//! `/v1/extract/{text,markdown}`: phase 1 (native, CPU pool) + phase 2
//! (OCR pool) orchestration. Images take the PDF path with every page
//! routed to OCR.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;

use super::{COMMON_PARAMS, await_job, format_param, parse_bool};
use crate::AppState;
use crate::admission::Admitted;
use crate::detect::{self, DocKind, FormatSource};
use crate::error::ApiError;
use crate::io::{self, Params};
use crate::office::OfficeFormat;
use crate::pdf::lang::{Lang, LangParam, LangSource};
use crate::pdf::route::{OcrMode, OcrReason};
use crate::pdf::{self, Mode, Phase1};
use crate::raster::ImageFormat;
use crate::respond::{self, Engine, Output, Timing};
use crate::text::{TextBudget, normalize_arabic_forms};

/// Slack for a page already in progress when its deadline passes.
const GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Text,
    Markdown,
}

impl Endpoint {
    fn raw_type(self) -> &'static str {
        match self {
            Endpoint::Text => "text/plain",
            Endpoint::Markdown => "text/markdown",
        }
    }
}

pub async fn text(State(st): State<Arc<AppState>>, req: Request) -> Response {
    extract(st, req, Endpoint::Text).await.unwrap_or_else(IntoResponse::into_response)
}

pub async fn markdown(State(st): State<Arc<AppState>>, req: Request) -> Response {
    extract(st, req, Endpoint::Markdown).await.unwrap_or_else(IntoResponse::into_response)
}

struct ExtractParams {
    format: Option<DocKind>,
    pages: Option<Vec<usize>>,
    ocr: OcrMode,
    lang: LangParam,
    detect_headings: bool,
    provided: Vec<String>,
}

/// Params for paged kinds (PDF, images).
const PAGED_PARAMS: [&str; 3] = ["pages", "ocr", "lang"];

impl ExtractParams {
    fn parse(p: &Params) -> Result<Self, ApiError> {
        let pages = p
            .get("pages")
            .map(|v| pdf::parse_pages(v).ok_or_else(|| ApiError::invalid_param("pages", v, "1-based pages and ranges, e.g. 1-3,5")))
            .transpose()?;
        let ocr = p
            .get("ocr")
            .map(|v| OcrMode::parse(v).ok_or_else(|| ApiError::invalid_param("ocr", v, "auto, force, off")))
            .transpose()?
            .unwrap_or(OcrMode::Auto);
        let lang = p
            .get("lang")
            .map(|v| LangParam::parse(v).ok_or_else(|| ApiError::invalid_param("lang", v, "auto, en, hi, ar")))
            .transpose()?
            .unwrap_or(LangParam::Auto);
        let detect_headings = p.get("detect_headings").map(|v| parse_bool("detect_headings", v)).transpose()?.unwrap_or(true);
        Ok(Self { format: format_param(p)?, pages, ocr, lang, detect_headings, provided: p.keys().cloned().collect() })
    }

    /// Params that don't apply to the detected kind (or aren't known at all).
    fn ignored(&self, kind: DocKind, ep: Endpoint) -> Vec<String> {
        self.provided
            .iter()
            .filter(|k| {
                let k = k.as_str();
                let applies = COMMON_PARAMS.contains(&k)
                    || (kind.is_paged() && PAGED_PARAMS.contains(&k))
                    || (kind == DocKind::Pdf && ep == Endpoint::Markdown && k == "detect_headings");
                !applies
            })
            .cloned()
            .collect()
    }
}

async fn extract(st: Arc<AppState>, mut req: Request, ep: Endpoint) -> Result<Response, ApiError> {
    let admitted = req
        .extensions_mut()
        .remove::<Admitted>()
        .ok_or_else(|| ApiError::internal("admission state missing"))?;
    let output = respond::negotiate(req.headers(), ep.raw_type());
    let input = tokio::time::timeout(st.cfg.body_timeout, io::read_input(req, st.cfg.max_body_bytes))
        .await
        .map_err(|_| ApiError::body_timeout())??;
    let params = ExtractParams::parse(&input.params)?;
    let detected = detect::detect(&input.bytes, params.format, input.filename.as_deref())?;
    metrics::counter!("oxide_documents_total", "kind" => detected.kind.kind_str(), "format" => detected.kind.format_str())
        .increment(1);
    let ignored = params.ignored(detected.kind, ep);
    let mode = match ep {
        Endpoint::Text => Mode::Text,
        Endpoint::Markdown => Mode::Markdown { detect_headings: params.detect_headings },
    };
    let ctx = Ctx { st, admitted, output, source: detected.source, ignored };
    match detected.kind {
        DocKind::Office(fmt) => office_flow(ctx, input.bytes, fmt, mode).await,
        DocKind::Pdf => pdf_flow(ctx, input.bytes, &params, mode).await,
        DocKind::Image(fmt) => image_flow(ctx, input.bytes, fmt, &params).await,
    }
}

struct Ctx {
    st: Arc<AppState>,
    admitted: Admitted,
    output: Output,
    source: FormatSource,
    ignored: Vec<String>,
}

// ---------------------------------------------------------------------------
// Office
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OfficeResponse<'a> {
    kind: &'static str,
    format: &'static str,
    format_source: FormatSource,
    text: &'a str,
    char_count: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    ignored_params: &'a [String],
}

async fn office_flow(ctx: Ctx, bytes: Bytes, fmt: OfficeFormat, mode: Mode) -> Result<Response, ApiError> {
    let deadline = ctx.admitted.admitted_at + ctx.st.cfg.process_timeout;
    let max_chars = ctx.st.cfg.max_text_chars;
    let rx = ctx.st.cpu.spawn(ctx.admitted.permit.clone(), ctx.st.cfg.process_timeout, move || {
        let t = Instant::now();
        let (text, cut_short) = crate::office::extract(bytes, fmt, mode, max_chars)?;
        let parse = t.elapsed();
        let t = Instant::now();
        let mut budget = TextBudget::new(max_chars);
        let text = budget.take(normalize_arabic_forms(text));
        Ok((text, budget.truncated || cut_short, parse, t.elapsed()))
    });
    let (text, truncated, parse, extract) = await_job(rx, deadline).await?;
    let timing = Timing { queue: ctx.admitted.queue_wait, parse, extract, ocr: None };
    let char_count = text.chars().count();
    let format = fmt.extension();
    let engine = if fmt.is_odf() { Engine::Native } else { Engine::Office };
    Ok(match ctx.output {
        Output::Json => respond::json(
            &OfficeResponse {
                kind: "office",
                format,
                format_source: ctx.source,
                text: &text,
                char_count,
                truncated,
                ignored_params: &ctx.ignored,
            },
            engine,
            &timing,
        ),
        Output::Raw(media) => respond::raw(
            text,
            media,
            engine,
            &timing,
            &[
                ("x-oxide-kind", "office".into()),
                ("x-oxide-format", format.into()),
                ("x-oxide-format-source", source_str(ctx.source).into()),
                ("x-oxide-truncated", truncated.to_string()),
                ("x-oxide-char-count", char_count.to_string()),
            ],
        ),
    })
}

fn source_str(s: FormatSource) -> &'static str {
    match s {
        FormatSource::Sniff => "sniff",
        FormatSource::Param => "param",
        FormatSource::Filename => "filename",
    }
}

// ---------------------------------------------------------------------------
// PDF and images
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OcrStatus {
    Native,
    Ocr,
    Hybrid,
    Unavailable,
    Off,
}

impl OcrStatus {
    fn as_str(self) -> &'static str {
        match self {
            OcrStatus::Native => "native",
            OcrStatus::Ocr => "ocr",
            OcrStatus::Hybrid => "hybrid",
            OcrStatus::Unavailable => "unavailable",
            OcrStatus::Off => "off",
        }
    }
}

#[derive(Serialize)]
struct PageReason {
    page: usize,
    reason: OcrReason,
}

#[derive(Serialize)]
struct OcrInfo {
    status: OcrStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<Lang>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lang_source: Option<LangSource>,
    /// Pages whose output includes OCR text.
    pages: Vec<usize>,
    /// Pages that need OCR but didn't get it (OCR unavailable here).
    pages_needing_ocr: Vec<usize>,
    /// Pages whose OCR failed; they carry native text, or none when the
    /// page's OCR layer was junk.
    failed_pages: Vec<usize>,
    /// Why each page was routed to OCR.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reasons: Vec<PageReason>,
}

/// The response for paged kinds: PDFs and images.
#[derive(Serialize)]
struct PagedResponse<'a> {
    kind: &'static str,
    format: &'static str,
    format_source: FormatSource,
    text: &'a str,
    char_count: usize,
    truncated: bool,
    page_count: usize,
    pages_extracted: Vec<usize>,
    incomplete: bool,
    ocr: OcrInfo,
    /// Pages whose native text could not be extracted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed_pages: Vec<usize>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    ignored_params: &'a [String],
}

/// Per-page outcome after phase 2.
struct OutPage {
    index: usize,
    text: String,
    ocr_applied: bool,
    merged: bool,
}

async fn pdf_flow(ctx: Ctx, bytes: Bytes, params: &ExtractParams, mode: Mode) -> Result<Response, ApiError> {
    let cfg = &ctx.st.cfg;
    let native_deadline = ctx.admitted.admitted_at + cfg.process_timeout;
    let job = pdf::PdfJob {
        mode,
        pages: params.pages.clone(),
        ocr: params.ocr,
        max_text_chars: cfg.max_text_chars,
        max_pdf_pages: cfg.max_pdf_pages,
        deadline: native_deadline,
    };
    let rx = ctx.st.cpu.spawn(ctx.admitted.permit.clone(), cfg.process_timeout, move || pdf::phase1(bytes, &job));
    let p1 = await_job(rx, native_deadline + GRACE).await?;
    paged_flow(ctx, p1, params, DocKind::Pdf).await
}

async fn image_flow(ctx: Ctx, bytes: Bytes, fmt: ImageFormat, params: &ExtractParams) -> Result<Response, ApiError> {
    let cfg = &ctx.st.cfg;
    let deadline = ctx.admitted.admitted_at + cfg.process_timeout;
    let (pages, ocr, max_pixels, max_pages) = (params.pages.clone(), params.ocr, cfg.max_image_pixels, cfg.max_pdf_pages);
    let rx = ctx.st.cpu.spawn(ctx.admitted.permit.clone(), cfg.process_timeout, move || {
        crate::raster::phase1(bytes, fmt, pages.as_deref(), ocr, max_pixels, max_pages)
    });
    let p1 = await_job(rx, deadline).await?;
    paged_flow(ctx, p1, params, DocKind::Image(fmt)).await
}

/// Phase 2 (OCR where the plan needs it) and the response.
async fn paged_flow(ctx: Ctx, p1: Phase1, params: &ExtractParams, kind: DocKind) -> Result<Response, ApiError> {
    let needing: Vec<(usize, OcrReason)> = p1.pages.iter().filter_map(|p| p.ocr.map(|r| (p.index, r))).collect();
    let reasons: Vec<PageReason> = needing.iter().map(|(i, r)| PageReason { page: i + 1, reason: *r }).collect();

    let phase2 = if params.ocr == OcrMode::Off {
        Phase2::skipped(OcrStatus::Off)
    } else if needing.is_empty() {
        Phase2::skipped(OcrStatus::Native)
    } else {
        // Images have no native text to fall back on, so an image whose
        // every page failed is an error, as with `ocr=force`.
        let strict = params.ocr == OcrMode::Force || matches!(kind, DocKind::Image(_));
        run_ocr(&ctx, &p1, &needing, params.lang, strict).await?
    };

    finish(ctx, p1, phase2, reasons, kind)
}

/// Phase 2 outcome.
struct Phase2 {
    status_hint: Option<OcrStatus>,
    lang: Option<(Lang, LangSource)>,
    /// page index → OCR text (replacement or merge per the page's reason).
    texts: Vec<(usize, String)>,
    failed: Vec<usize>,
    /// Why the first failed page failed (engine wording, not content).
    #[cfg_attr(not(feature = "ocr"), allow(dead_code))]
    first_error: Option<String>,
    not_run: Vec<usize>,
    /// OCR'd pages skipped because the OCR deadline passed.
    timed_out: Vec<usize>,
    elapsed: Option<Duration>,
}

impl Phase2 {
    fn skipped(status: OcrStatus) -> Self {
        Self {
            status_hint: Some(status),
            lang: None,
            texts: vec![],
            failed: vec![],
            first_error: None,
            not_run: vec![],
            timed_out: vec![],
            elapsed: None,
        }
    }

    #[cfg_attr(feature = "ocr", allow(dead_code))]
    fn unavailable(needing: &[(usize, OcrReason)], lang: Option<(Lang, LangSource)>) -> Self {
        Self { not_run: needing.iter().map(|(i, _)| *i).collect(), lang, ..Self::skipped(OcrStatus::Unavailable) }
    }
}

#[cfg(not(feature = "ocr"))]
async fn run_ocr(_ctx: &Ctx, p1: &Phase1, needing: &[(usize, OcrReason)], lang: LangParam, _strict: bool) -> Result<Phase2, ApiError> {
    // `:slim` still classifies pages: report what needs OCR, plus the
    // language the `:ocr` image would use when the text layer decides it.
    let resolved = crate::pdf::lang::resolve_static(lang, p1.legacy_font, &p1.script_counts);
    Ok(Phase2::unavailable(needing, resolved))
}

#[cfg(feature = "ocr")]
async fn run_ocr(ctx: &Ctx, p1: &Phase1, needing: &[(usize, OcrReason)], lang_param: LangParam, strict: bool) -> Result<Phase2, ApiError> {
    use crate::ocr::{LangPlan, OcrJob};
    use tokio::sync::oneshot;

    let resolved = crate::pdf::lang::resolve_static(lang_param, p1.legacy_font, &p1.script_counts);
    let Some(svc) = ctx.st.ocr.as_ref() else {
        return Ok(Phase2::unavailable(needing, resolved));
    };
    let avail = svc.available_langs();
    if avail.is_empty() || resolved.is_some_and(|(l, _)| !avail.contains(&l)) {
        return Ok(Phase2::unavailable(needing, resolved));
    }
    let ocr_deadline = ctx.admitted.admitted_at + ctx.st.cfg.ocr_timeout;
    let mut lang = resolved;
    let mut out = Phase2::skipped(OcrStatus::Native);
    out.status_hint = None;
    let mut elapsed = Duration::ZERO;

    // Pages are submitted one at a time per request, so concurrent requests
    // interleave page by page on the shared OCR workers.
    for (pos, &(idx, reason)) in needing.iter().enumerate() {
        if Instant::now() >= ocr_deadline {
            out.timed_out.extend(needing[pos..].iter().map(|(i, _)| *i));
            break;
        }
        let plan = match lang {
            Some((l, _)) => LangPlan::Fixed(l),
            None => LangPlan::Probe(avail.clone()),
        };
        let (tx, rx) = oneshot::channel();
        let job = OcrJob {
            source: p1.source.clone(),
            page: idx,
            render: reason.renders(),
            accept_small_image: reason == OcrReason::Hybrid,
            lang: plan,
            permit: ctx.admitted.permit.clone(),
            reply: tx,
        };
        if !svc.submit(job) {
            out.not_run.extend(needing[pos..].iter().map(|(i, _)| *i));
            out.status_hint = Some(OcrStatus::Unavailable);
            break;
        }
        match tokio::time::timeout_at((ocr_deadline + GRACE).into(), rx).await {
            Ok(Ok(Ok(res))) => {
                if lang.is_none() {
                    let source = if res.probed { LangSource::Probe } else { LangSource::Default };
                    metrics::counter!("oxide_lang_source_total", "source" => if res.probed { "probe" } else { "default" }).increment(1);
                    lang = Some((res.lang, source));
                }
                elapsed += res.elapsed;
                let has_native = p1.pages.iter().any(|p| p.index == idx && !p.text.trim().is_empty());
                if res.text.trim().is_empty() && has_native {
                    // Nothing recognized (e.g. a page rendered without its
                    // fonts): never trade real native text for nothing.
                    tracing::warn!(page = idx + 1, "OCR produced no text; keeping native text");
                    out.failed.push(idx);
                } else {
                    out.texts.push((idx, res.text));
                }
            }
            Ok(Ok(Err(why))) => {
                // Engine error strings describe the failure, not the content.
                tracing::warn!(page = idx + 1, reason = %why, "OCR failed on page; keeping native text");
                out.failed.push(idx);
                out.first_error.get_or_insert(why);
            }
            Ok(Err(_)) => out.failed.push(idx),
            Err(_) => {
                out.timed_out.extend(needing[pos..].iter().map(|(i, _)| *i));
                break;
            }
        }
    }
    if let Some((_, source)) = resolved {
        metrics::counter!("oxide_lang_source_total", "source" => lang_source_str(source)).increment(1);
    }
    out.lang = lang;
    out.elapsed = Some(elapsed);

    if strict && out.texts.is_empty() && !out.failed.is_empty() && out.timed_out.is_empty() {
        if let Some(why) = out.first_error.as_deref().filter(|w| w.starts_with("image decode failed"))
            && matches!(p1.source, crate::pdf::PageSource::Image(_))
        {
            return Err(ApiError::unprocessable("corrupt-document", "Unreadable document", format!("the image could not be read: {why}")));
        }
        return Err(ApiError::unprocessable("ocr-failed", "OCR failed", "OCR failed on every requested page"));
    }
    Ok(out)
}

#[cfg(feature = "ocr")]
fn lang_source_str(s: LangSource) -> &'static str {
    match s {
        LangSource::Param => "param",
        LangSource::Font => "font",
        LangSource::Text => "text",
        LangSource::Probe => "probe",
        LangSource::Default => "default",
    }
}

fn finish(ctx: Ctx, p1: Phase1, p2: Phase2, reasons: Vec<PageReason>, kind: DocKind) -> Result<Response, ApiError> {
    let Phase1 { pages, page_count, incomplete: p1_incomplete, truncated: p1_truncated, parse_time, extract_time, .. } = p1;
    let mut ocr_texts: std::collections::HashMap<usize, String> = p2.texts.into_iter().collect();
    let mut out_pages: Vec<OutPage> = Vec::with_capacity(pages.len());
    let mut native_failed = Vec::new();
    for p in pages {
        if p2.timed_out.contains(&p.index) {
            continue; // not done: the caller can re-request it with `pages=`
        }
        if p.failed {
            native_failed.push(p.index + 1);
        }
        match (ocr_texts.remove(&p.index), p.ocr) {
            (Some(ocr), Some(reason)) => {
                // OCR text has no structure beyond lines and paragraphs, so
                // it reads the same as text or markdown.
                let (text, merged) = if reason.merges() {
                    (crate::text::merge_native_and_ocr(&p.text, &ocr), true)
                } else {
                    (ocr, false)
                };
                out_pages.push(OutPage { index: p.index, text, ocr_applied: true, merged });
            }
            _ => out_pages.push(OutPage { index: p.index, text: p.text, ocr_applied: false, merged: false }),
        }
    }

    let incomplete = p1_incomplete || !p2.timed_out.is_empty();
    if incomplete && out_pages.is_empty() {
        return Err(ApiError::processing_timeout("no page could be processed within the deadline"));
    }

    let mut budget = TextBudget::new(ctx.st.cfg.max_text_chars);
    let mut text = String::new();
    for (i, p) in out_pages.iter().enumerate() {
        if budget.exhausted() {
            break;
        }
        let sep = if i > 0 { "\n\n" } else { "" };
        let piece = budget.take(format!("{sep}{}", p.text.trim_end()));
        text.push_str(&piece);
    }
    let text = normalize_arabic_forms(text);
    let truncated = p1_truncated || budget.truncated;

    let ocr_pages: Vec<usize> = out_pages.iter().filter(|p| p.ocr_applied).map(|p| p.index + 1).collect();
    let status = p2.status_hint.unwrap_or_else(|| {
        if ocr_pages.is_empty() {
            OcrStatus::Native
        } else if ocr_pages.len() == out_pages.len() && !out_pages.iter().any(|p| p.merged) {
            OcrStatus::Ocr
        } else {
            OcrStatus::Hybrid
        }
    });
    let pages_extracted: Vec<usize> = out_pages.iter().map(|p| p.index + 1).collect();
    let ocr = OcrInfo {
        status,
        lang: p2.lang.map(|(l, _)| l),
        lang_source: p2.lang.map(|(_, s)| s),
        pages: ocr_pages,
        pages_needing_ocr: p2.not_run.iter().map(|i| i + 1).collect(),
        failed_pages: p2.failed.iter().map(|i| i + 1).collect(),
        reasons,
    };
    let timing = Timing { queue: ctx.admitted.queue_wait, parse: parse_time, extract: extract_time, ocr: p2.elapsed };
    let char_count = text.chars().count();
    let (kind_str, format) = (kind.kind_str(), kind.format_str());

    Ok(match ctx.output {
        Output::Json => respond::json(
            &PagedResponse {
                kind: kind_str,
                format,
                format_source: ctx.source,
                text: &text,
                char_count,
                truncated,
                page_count,
                pages_extracted,
                incomplete,
                ocr,
                failed_pages: native_failed,
                ignored_params: &ctx.ignored,
            },
            Engine::Pdf,
            &timing,
        ),
        Output::Raw(media) => {
            let join = |v: &[usize]| v.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
            let mut meta = vec![
                ("x-oxide-kind", kind_str.to_string()),
                ("x-oxide-format", format.to_string()),
                ("x-oxide-format-source", source_str(ctx.source).to_string()),
                ("x-oxide-truncated", truncated.to_string()),
                ("x-oxide-incomplete", incomplete.to_string()),
                ("x-oxide-ocr-status", ocr.status.as_str().to_string()),
                ("x-oxide-page-count", page_count.to_string()),
                ("x-oxide-pages-extracted", join(&pages_extracted)),
                ("x-oxide-char-count", char_count.to_string()),
            ];
            if let Some(l) = ocr.lang {
                meta.push(("x-oxide-lang", l.code().to_string()));
            }
            if !ocr.pages_needing_ocr.is_empty() {
                meta.push(("x-oxide-pages-needing-ocr", join(&ocr.pages_needing_ocr)));
            }
            respond::raw(text, media, Engine::Pdf, &timing, &meta)
        }
    })
}
