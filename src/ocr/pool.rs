//! `OcrPool`: `OCR_WORKERS` threads, each owning one `OcrStack` (its own
//! ONNX Runtime sessions, so there's no cross-worker lock contention).
//! Pages (and uploaded images) are decoded inside the worker, bounding
//! decoded scans to one per worker.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{page, smoke, stack};
use crate::admission::JobPermit;
use crate::blocking::{WORKER_STACK, Watchdog};
use crate::pdf::PageSource;
use crate::pdf::document::{Block, PageFrame};
use crate::pdf::lang::Lang;

pub enum LangPlan {
    Fixed(Lang),
    Probe(Vec<Lang>),
}

pub struct OcrJob {
    pub source: PageSource,
    pub page: usize,
    /// PDF only: render the page instead of reading its embedded scan.
    pub render: bool,
    /// PDF only: a small embedded image (a stamp) is the target.
    pub accept_small_image: bool,
    /// Full-page rendering and positioned results for document extraction.
    pub structured: bool,
    /// Include recognizer boxes in opt-in diagnostics without changing routing.
    pub include_blocks: bool,
    pub lang: LangPlan,
    pub permit: JobPermit,
    pub reply: oneshot::Sender<Result<OcrPageResult, String>>,
}

pub struct OcrPageResult {
    pub text: String,
    pub blocks: Vec<Block>,
    pub lang: Lang,
    /// The language was chosen by the probe on this page.
    pub probed: bool,
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
enum Phase {
    Loading,
    Ready,
    Unavailable(String),
}

struct State {
    phase: Phase,
    langs: BTreeMap<Lang, Result<(), String>>,
    sessions: usize,
}

pub struct OcrService {
    state: RwLock<State>,
    tx: Mutex<Option<mpsc::Sender<OcrJob>>>,
    workers: usize,
    deadline: Duration,
}

pub struct OcrSettings {
    pub model_dir: PathBuf,
    pub langs: Vec<Lang>,
    pub workers: usize,
    pub threads_per_worker: usize,
    pub deadline: Duration,
}

impl OcrService {
    /// Starts the workers in the background. Each builds its stack and runs
    /// the golden smoke test per language; `on_ready` fires once every
    /// worker has reported (whether or not OCR ended up usable).
    pub fn start(settings: OcrSettings, watchdog: Arc<Watchdog>, on_ready: impl FnOnce() + Send + 'static) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<OcrJob>();
        let rx = Arc::new(Mutex::new(rx));
        let svc = Arc::new(Self {
            state: RwLock::new(State { phase: Phase::Loading, langs: BTreeMap::new(), sessions: 0 }),
            tx: Mutex::new(Some(tx)),
            workers: settings.workers,
            deadline: settings.deadline,
        });

        let (report_tx, report_rx) = mpsc::channel::<WorkerReport>();
        for w in 0..settings.workers {
            let rx = rx.clone();
            let report_tx = report_tx.clone();
            let dir = settings.model_dir.clone();
            let langs = settings.langs.clone();
            let threads = settings.threads_per_worker;
            let watchdog = watchdog.clone();
            let deadline = settings.deadline;
            let spawned = std::thread::Builder::new()
                .name(format!("ocr-{w}"))
                .stack_size(WORKER_STACK)
                .spawn(move || worker(w, dir, langs, threads, rx, report_tx, watchdog, deadline));
            if let Err(e) = spawned {
                tracing::error!(worker = w, error = %e, "failed to spawn OCR worker");
            }
        }
        drop(report_tx);

        let svc2 = svc.clone();
        let langs = settings.langs.clone();
        std::thread::Builder::new()
            .name("ocr-init".into())
            .spawn(move || {
                let mut reports = Vec::new();
                while let Ok(r) = report_rx.recv() {
                    reports.push(r);
                }
                svc2.finish_init(&langs, reports);
                on_ready();
            })
            .expect("failed to spawn OCR init thread");
        svc
    }

    fn finish_init(&self, langs: &[Lang], reports: Vec<WorkerReport>) {
        let mut st = self.state.write().unwrap_or_else(|e| e.into_inner());
        st.sessions = reports.iter().map(|r| r.sessions).sum();
        metrics::counter!("oxide_ocr_engine_builds_total").increment(st.sessions as u64);
        let live = reports.iter().filter(|r| r.fatal.is_none()).count();
        if live == 0 {
            let why = reports
                .iter()
                .find_map(|r| r.fatal.clone())
                .unwrap_or_else(|| "no OCR worker started".into());
            tracing::error!(reason = %why, "OCR unavailable; native extraction continues");
            st.phase = Phase::Unavailable(why);
            for &l in langs {
                st.langs.insert(l, Err("OCR unavailable".into()));
            }
            return;
        }
        for &l in langs {
            // A language is usable only if every live worker loaded it and
            // passed its golden test.
            let failure = reports.iter().filter(|r| r.fatal.is_none()).find_map(|r| r.failed.get(&l).cloned());
            match &failure {
                Some(why) => tracing::error!(lang = l.code(), reason = %why, "OCR language unavailable"),
                None => tracing::info!(lang = l.code(), "OCR language ready"),
            }
            st.langs.insert(l, failure.map_or(Ok(()), Err));
        }
        if live < reports.len() {
            tracing::warn!(live, total = reports.len(), "some OCR workers failed to start");
        }
        st.phase = Phase::Ready;
    }

    pub fn available_langs(&self) -> Vec<Lang> {
        let st = self.state.read().unwrap_or_else(|e| e.into_inner());
        if !matches!(st.phase, Phase::Ready) {
            return Vec::new();
        }
        st.langs.iter().filter(|(_, r)| r.is_ok()).map(|(l, _)| *l).collect()
    }

    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Queues a page. `None` if the pool is shut down.
    pub fn submit(&self, job: OcrJob) -> bool {
        let guard = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(tx) => tx.send(job).is_ok(),
            None => false,
        }
    }

    pub fn status_json(&self) -> Value {
        let st = self.state.read().unwrap_or_else(|e| e.into_inner());
        let (status, reason) = match &st.phase {
            Phase::Loading => ("loading", None),
            Phase::Ready => ("ready", None),
            Phase::Unavailable(r) => ("unavailable", Some(r.clone())),
        };
        let langs: serde_json::Map<String, Value> = st
            .langs
            .iter()
            .map(|(l, r)| {
                let v = match r {
                    Ok(()) => json!("available"),
                    Err(why) => json!({ "status": "unavailable", "reason": why }),
                };
                (l.code().to_string(), v)
            })
            .collect();
        json!({
            "compiled": true,
            "status": status,
            "reason": reason,
            "workers": self.workers,
            "sessions": st.sessions,
            "languages": langs,
        })
    }
}

struct WorkerReport {
    fatal: Option<String>,
    failed: BTreeMap<Lang, String>,
    sessions: usize,
}

#[allow(clippy::too_many_arguments)]
fn worker(
    id: usize,
    dir: PathBuf,
    langs: Vec<Lang>,
    threads: usize,
    rx: Arc<Mutex<mpsc::Receiver<OcrJob>>>,
    report_tx: mpsc::Sender<WorkerReport>,
    watchdog: Arc<Watchdog>,
    deadline: Duration,
) {
    let built = catch_unwind(AssertUnwindSafe(|| stack::build(&dir, &langs, threads)));
    let report = match built {
        Ok(r) => r,
        Err(_) => stack::BuildReport {
            stack: None,
            fatal: Some("OCR stack construction panicked".into()),
            failed_langs: BTreeMap::new(),
            sessions: 0,
        },
    };
    let mut failed = report.failed_langs;
    if let Some(stack) = &report.stack {
        for &l in &langs {
            if failed.contains_key(&l) {
                continue;
            }
            let res = catch_unwind(AssertUnwindSafe(|| smoke::run(stack, l)))
                .unwrap_or_else(|_| Err("golden test panicked".into()));
            if let Err(why) = res {
                failed.insert(l, why);
            }
        }
    }
    let _ = report_tx.send(WorkerReport { fatal: report.fatal, failed, sessions: report.sessions });
    drop(report_tx);
    let Some(stack) = report.stack else { return };
    tracing::debug!(worker = id, "OCR worker ready");

    loop {
        let job = {
            let rx = rx.lock().unwrap_or_else(|e| e.into_inner());
            rx.recv()
        };
        let Ok(job) = job else { break };
        if job.reply.is_closed() {
            continue; // the request is gone; don't spend inference on it
        }
        let _watch = watchdog.register(deadline);
        let start = Instant::now();
        let out = catch_unwind(AssertUnwindSafe(|| run_job(&stack, &job)))
            .unwrap_or_else(|_| Err("OCR engine panicked".into()));
        let elapsed = start.elapsed();
        let lang_label = match (&out, &job.lang) {
            (Ok((p, _)), _) => p.lang.code(),
            (Err(_), LangPlan::Fixed(l)) => l.code(),
            (Err(_), LangPlan::Probe(_)) => "probe",
        };
        metrics::counter!("oxide_ocr_pages_total", "lang" => lang_label, "outcome" => if out.is_ok() { "ok" } else { "error" })
            .increment(1);
        metrics::histogram!("oxide_ocr_page_duration_seconds", "lang" => lang_label).record(elapsed.as_secs_f64());
        let OcrJob { reply, permit, .. } = job;
        let _ = reply.send(out.map(|(p, probed)| OcrPageResult {
            text: p.text, blocks: if job.structured || job.include_blocks { p.blocks } else { Vec::new() }, lang: p.lang, probed, elapsed,
        }));
        drop(permit);
    }
}

fn run_job(stack: &stack::OcrStack, job: &OcrJob) -> Result<(page::PageOcr, bool), String> {
    let doc = match &job.source {
        PageSource::Image(img) => return ocr_one(stack, &img.decode(job.page)?, &job.lang),
        PageSource::Pdf(doc) => doc,
    };
    if job.structured {
        // Render once to include every image and to give native/OCR blocks
        // the same cropped, rotated page coordinate system. Reuse the same
        // detector, recognizers and language probe as plain-text extraction.
        let frame =
            PageFrame::read(doc, job.page).map_err(|e| format!("page geometry failed: {e}"))?;
        let img = page::rendered_image(doc, job.page)?;
        let (mut out, probed) = ocr_one(stack, &img, &job.lang)?;
        for block in &mut out.blocks {
            block.bbox = block.bbox.and_then(|b| frame.ocr_box(b, img.width(), img.height()));
        }
        return Ok((out, probed));
    }
    // Legacy-font pages are vector text: there is no scan to try, render only.
    if job.render {
        let img = page::rendered_image(doc, job.page)?;
        return ocr_one(stack, &img, &job.lang);
    }
    // Hybrid stamps: the small embedded image *is* the target; rendering the
    // whole page would re-OCR the native text we already have. Keep the old
    // embedded-or-render behaviour with no fallback.
    if job.accept_small_image {
        let img = page::page_image(doc, job.page, job.render, job.accept_small_image)?;
        return ocr_one(stack, &img, &job.lang);
    }
    // Scanned / forced pages: the largest embedded image should be the scan,
    // but a full-page background, QR code or logo can win that contest while
    // the real content is vector text. When embedded OCR comes back
    // suspiciously short, retry with a rendering and keep the longer text.
    // (Seen on a 3-page UCIC invoice set: embedded OCR gave ~11 chars/page
    // of fragments while the rendered page holds the full invoice.)
    if let Some(res) = page::embedded_image(doc, job.page, false) {
        let img = match res {
            Ok(img) => img,
            Err(why) => {
                tracing::warn!(page = job.page + 1, reason = %why, "embedded image failed; trying rendered page");
                let render = page::rendered_image(doc, job.page)?;
                return ocr_one(stack, &render, &job.lang);
            }
        };
        let first = match ocr_one(stack, &img, &job.lang) {
            Ok(out) => out,
            Err(why) => {
                tracing::warn!(page = job.page + 1, reason = %why, "embedded OCR failed; trying rendered page");
                let render = page::rendered_image(doc, job.page)?;
                return ocr_one(stack, &render, &job.lang);
            }
        };
        let first_len = first.0.text.trim().chars().count();
        if first_len >= FALLBACK_MIN_CHARS {
            return Ok(first);
        }
        tracing::warn!(page = job.page + 1, chars = first_len, "embedded OCR suspiciously short; retrying with rendered page");
        let render = page::rendered_image(doc, job.page)?;
        let second = ocr_one(stack, &render, &job.lang)?;
        let second_len = second.0.text.trim().chars().count();
        // Search needs recall: either side can hold unique lines (e.g. the
        // header only in embedded, the body only in the render). Union them
        // instead of keeping just the longer side.
        if second_len == 0 {
            return Ok(first);
        }
        if first_len == 0 {
            return Ok(second);
        }
        let (base, extra) = if second_len >= first_len { (second, first) } else { (first, second) };
        let merged_text = crate::text::merge_native_and_ocr(&base.0.text, &extra.0.text);
        tracing::info!(
            page = job.page + 1,
            embedded_chars = first_len,
            rendered_chars = second_len,
            merged_chars = merged_text.chars().count(),
            "merged embedded + rendered OCR"
        );
        let merged = page::PageOcr { text: merged_text, lang: base.0.lang, blocks: Vec::new() };
        return Ok((merged, base.1 || extra.1));
    }
    let img = page::rendered_image(doc, job.page)?;
    ocr_one(stack, &img, &job.lang)
}

/// Pages with fewer non-whitespace chars than this after embedded-image OCR
/// retry with a rendered page. Invoices carry hundreds of chars; blank pages
/// retry harmlessly and still come back empty.
const FALLBACK_MIN_CHARS: usize = 100;

fn ocr_one(
    stack: &stack::OcrStack,
    img: &image::DynamicImage,
    plan: &LangPlan,
) -> Result<(page::PageOcr, bool), String> {
    match plan {
        LangPlan::Fixed(l) => page::ocr_image(stack, img, *l).map(|p| (p, false)),
        LangPlan::Probe(candidates) => page::probe_and_ocr(stack, img, candidates),
    }
}
