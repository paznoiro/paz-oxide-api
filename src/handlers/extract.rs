//! `/v1/extract/{text,markdown,fields,document}`: phase 1 (native, CPU pool) + phase 2
//! (OCR pool) orchestration. Images take the PDF path with every page
//! routed to OCR. `fields` reads the document exactly as `text` does, then
//! runs its field specs over that one text.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;
use serde_json::Value;

use super::{COMMON_PARAMS, await_job, format_param, parse_bool};
use crate::AppState;
use crate::admission::Admitted;
use crate::detect::{self, DocKind, FormatSource};
use crate::error::ApiError;
use crate::fields::{self, Extracted, Field, FieldValues};
use crate::io::{self, Params, Shape};
use crate::office::OfficeFormat;
use crate::pdf::document::{Block, DocumentPage};
use crate::pdf::lang::{Lang, LangParam, LangSource};
use crate::pdf::route::{OcrMode, OcrReason};
use crate::pdf::{self, Mode, Phase1};
use crate::raster::ImageFormat;
use crate::respond::{self, Engine, Output, Timing};
use crate::text::{TextBudget, collapse_whitespace, normalize_arabic_forms};

/// Slack for a page already in progress when its deadline passes.
const GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Text,
    Markdown,
    Fields,
    Document,
}

impl Endpoint {
    fn raw_type(self) -> &'static str {
        match self {
            Endpoint::Text | Endpoint::Fields | Endpoint::Document => "text/plain",
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

pub async fn fields(State(st): State<Arc<AppState>>, req: Request) -> Response {
    extract_fields(st, req).await.unwrap_or_else(IntoResponse::into_response)
}

pub async fn document(State(st): State<Arc<AppState>>, req: Request) -> Response {
    extract(st, req, Endpoint::Document).await.unwrap_or_else(IntoResponse::into_response)
}

struct ExtractParams {
    format: Option<DocKind>,
    pages: Option<Vec<usize>>,
    ocr: OcrMode,
    lang: LangParam,
    detect_headings: bool,
    clean: bool,
    include_text: bool,
    provided: Vec<String>,
    structured: bool,
}

/// Params for paged kinds (PDF, images).
const PAGED_PARAMS: [&str; 3] = ["pages", "ocr", "lang"];

/// Params that apply to every extraction (any kind, every endpoint).
const EXTRACT_PARAMS: [&str; 1] = ["clean"];

/// Params of `/v1/extract/fields` alone.
const FIELDS_PARAMS: [&str; 1] = ["include_text"];

impl ExtractParams {
    fn parse(p: &Params, ep: Endpoint) -> Result<Self, ApiError> {
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
        let clean = p.get("clean").map(|v| parse_bool("clean", v)).transpose()?.unwrap_or(false);
        let include_text = match ep {
            Endpoint::Fields => p.get("include_text").map(|v| parse_bool("include_text", v)).transpose()?.unwrap_or(false),
            Endpoint::Text | Endpoint::Markdown | Endpoint::Document => false,
        };
        Ok(Self { format: format_param(p)?, pages, ocr, lang, detect_headings, clean, include_text, provided: p.keys().cloned().collect(), structured: ep == Endpoint::Document })
    }

    /// Params that don't apply to the detected kind (or aren't known at all).
    /// `kind` is `None` for fields read from supplied text: no document.
    fn ignored(&self, kind: Option<DocKind>, ep: Endpoint) -> Vec<String> {
        self.provided
            .iter()
            .filter(|k| {
                let k = k.as_str();
                let applies = EXTRACT_PARAMS.contains(&k)
                    || (ep == Endpoint::Fields && FIELDS_PARAMS.contains(&k))
                    || kind.is_some_and(|kind| {
                        COMMON_PARAMS.contains(&k)
                            || (kind.is_paged() && PAGED_PARAMS.contains(&k))
                            || (kind == DocKind::Pdf && ep == Endpoint::Markdown && k == "detect_headings")
                    });
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
    let params = ExtractParams::parse(&input.params, ep)?;
    let detected = detect::detect(&input.bytes, params.format, input.filename.as_deref())?;
    if ep == Endpoint::Document && detected.kind != DocKind::Pdf {
        return Err(ApiError::unsupported(
            "structured document extraction accepts PDF files; use /v1/extract/text for other formats",
        ));
    }
    count_document(detected.kind);
    let ignored = params.ignored(Some(detected.kind), ep);
    let mode = match ep {
        Endpoint::Text | Endpoint::Fields | Endpoint::Document => Mode::Text,
        Endpoint::Markdown => Mode::Markdown { detect_headings: params.detect_headings },
    };
    let ctx = Ctx { st, admitted, clean: params.clean };
    let ext = read_document(&ctx, input.bytes, detected.kind, &params, mode).await?;
    Ok(if ep == Endpoint::Document {
        render_document(ext, detected.source, &ignored)
    } else {
        render(ext, detected.source, output, &ignored)
    })
}

fn count_document(kind: DocKind) {
    metrics::counter!("oxide_documents_total", "kind" => kind.kind_str(), "format" => kind.format_str()).increment(1);
}

struct Ctx {
    st: Arc<AppState>,
    admitted: Admitted,
    clean: bool,
}

/// A document's final text — normalized, cleaned when asked, within the text
/// budget — and how it was read.
struct Extraction {
    kind: DocKind,
    text: String,
    truncated: bool,
    /// PDFs and images only.
    paged: Option<Paged>,
    engine: Engine,
    timing: Timing,
    document: Option<Vec<DocumentPage>>,
}

#[derive(Serialize)]
struct DocumentResponse<'a> {
    schema_version: &'static str,
    kind: &'static str,
    format: &'static str,
    format_source: FormatSource,
    coordinate_system: CoordinateSystem,
    char_count: usize,
    truncated: bool,
    #[serde(flatten)]
    paged: Option<&'a Paged>,
    pages: &'a [DocumentPage],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    ignored_params: &'a [String],
}

#[derive(Serialize)]
struct CoordinateSystem {
    unit: &'static str,
    origin: &'static str,
    x_direction: &'static str,
    y_direction: &'static str,
}

fn render_document(ext: Extraction, source: FormatSource, ignored: &[String]) -> Response {
    let pages = ext.document.as_deref().unwrap_or_default();
    respond::json(
        &DocumentResponse {
            schema_version: "1.0",
            kind: "pdf",
            format: "pdf",
            format_source: source,
            coordinate_system: CoordinateSystem {
                unit: "pt",
                origin: "top_left",
                x_direction: "right",
                y_direction: "down",
            },
            char_count: pages.iter().map(|p| p.text.chars().count()).sum(),
            truncated: ext.truncated,
            paged: ext.paged.as_ref(),
            pages,
            ignored_params: ignored,
        },
        ext.engine,
        &ext.timing,
    )
}

/// What a PDF or image response adds to the text.
#[derive(Serialize)]
struct Paged {
    page_count: usize,
    pages_extracted: Vec<usize>,
    incomplete: bool,
    ocr: OcrInfo,
    /// Pages whose native text could not be extracted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed_pages: Vec<usize>,
}

/// Reads a document to its final text, the one way every endpoint here does.
async fn read_document(ctx: &Ctx, bytes: Bytes, kind: DocKind, params: &ExtractParams, mode: Mode) -> Result<Extraction, ApiError> {
    match kind {
        DocKind::Office(fmt) => office_flow(ctx, bytes, fmt, mode).await,
        DocKind::Pdf => pdf_flow(ctx, bytes, params, mode).await,
        DocKind::Image(fmt) => image_flow(ctx, bytes, fmt, params).await,
    }
}

/// The `/v1/extract/{text,markdown}` response: JSON, or the raw text with
/// its metadata in `X-Oxide-*` headers.
#[derive(Serialize)]
struct ExtractResponse<'a> {
    kind: &'static str,
    format: &'static str,
    format_source: FormatSource,
    text: &'a str,
    char_count: usize,
    truncated: bool,
    #[serde(flatten)]
    paged: Option<&'a Paged>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    ignored_params: &'a [String],
}

fn render(ext: Extraction, source: FormatSource, output: Output, ignored: &[String]) -> Response {
    let char_count = ext.text.chars().count();
    let (kind, format) = (ext.kind.kind_str(), ext.kind.format_str());
    match output {
        Output::Json => respond::json(
            &ExtractResponse {
                kind,
                format,
                format_source: source,
                text: &ext.text,
                char_count,
                truncated: ext.truncated,
                paged: ext.paged.as_ref(),
                ignored_params: ignored,
            },
            ext.engine,
            &ext.timing,
        ),
        Output::Raw(media) => {
            let mut meta = vec![
                ("x-oxide-kind", kind.to_string()),
                ("x-oxide-format", format.to_string()),
                ("x-oxide-format-source", source_str(source).to_string()),
                ("x-oxide-truncated", ext.truncated.to_string()),
            ];
            if let Some(paged) = &ext.paged {
                let join = |v: &[usize]| v.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
                meta.push(("x-oxide-incomplete", paged.incomplete.to_string()));
                meta.push(("x-oxide-ocr-status", paged.ocr.status.as_str().to_string()));
                meta.push(("x-oxide-page-count", paged.page_count.to_string()));
                meta.push(("x-oxide-pages-extracted", join(&paged.pages_extracted)));
                if let Some(l) = paged.ocr.lang {
                    meta.push(("x-oxide-lang", l.code().to_string()));
                }
                if !paged.ocr.pages_needing_ocr.is_empty() {
                    meta.push(("x-oxide-pages-needing-ocr", join(&paged.ocr.pages_needing_ocr)));
                }
            }
            meta.push(("x-oxide-char-count", char_count.to_string()));
            respond::raw(ext.text, media, ext.engine, &ext.timing, &meta)
        }
    }
}

// ---------------------------------------------------------------------------
// Fields
// ---------------------------------------------------------------------------

/// The `/v1/extract/fields` response. Always JSON: the values are the point,
/// and a raw-text mode would have nowhere to put them.
#[derive(Serialize)]
struct FieldsResponse<'a> {
    /// `pdf`, `office` or `image`; `text` when the caller sent the text.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format_source: Option<FormatSource>,
    fields: &'a FieldValues,
    /// What each value was read from, as printed: a typed value is written in
    /// its type's form, so this is what to look for in the text.
    matched: &'a FieldValues,
    missing_required: &'a [String],
    char_count: usize,
    truncated: bool,
    #[serde(flatten)]
    paged: Option<&'a Paged>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    ignored_params: &'a [String],
}

/// Keys `/v1/extract/fields` takes whole rather than as scalar params.
const FIELDS_SHAPE: Shape = Shape { whole: &["fields", "text"], document_optional: true };

async fn extract_fields(st: Arc<AppState>, mut req: Request) -> Result<Response, ApiError> {
    let admitted = req
        .extensions_mut()
        .remove::<Admitted>()
        .ok_or_else(|| ApiError::internal("admission state missing"))?;
    let input = tokio::time::timeout(st.cfg.body_timeout, io::read_input_shaped(req, st.cfg.max_body_bytes, FIELDS_SHAPE))
        .await
        .map_err(|_| ApiError::body_timeout())??;
    // Specs first: a bad pattern must cost a 400, not an OCR run.
    let specs = input
        .whole
        .get("fields")
        .ok_or_else(|| ApiError::bad_request("`fields` is required: a JSON array of field specs").with("parameter", "fields"))?;
    let fields = fields::parse(specs)?;
    let params = ExtractParams::parse(&input.params, Endpoint::Fields)?;
    let ctx = Ctx { st, admitted, clean: params.clean };

    match (input.bytes.is_empty(), input.whole.get("text")) {
        (false, Some(_)) => Err(ApiError::bad_request("send either a document or `text`, not both").with("parameter", "text")),
        (true, None) => Err(ApiError::bad_request(
            "nothing to read: send the document (multipart `file`, JSON `file_base64` or the raw body) or its `text`",
        )
        .with("parameter", "text")
        .with("accepted", "a document, or `text` to run the fields over")),
        (true, Some(Value::String(supplied))) => {
            let mut budget = TextBudget::new(ctx.st.cfg.max_text_chars);
            let text = budget.take(normalize_arabic_forms(supplied.clone()));
            let text = if ctx.clean { collapse_whitespace(&text) } else { text };
            let ignored = params.ignored(None, Endpoint::Fields);
            let (found, text, took) = match_fields(&ctx, fields, text).await?;
            let timing = Timing { queue: ctx.admitted.queue_wait, extract: took, ..Default::default() };
            let body = FieldsResponse {
                kind: "text",
                format: None,
                format_source: None,
                fields: &found.values,
                matched: &found.matched,
                missing_required: &found.missing_required,
                char_count: text.chars().count(),
                truncated: budget.truncated,
                paged: None,
                text: params.include_text.then_some(text.as_str()),
                ignored_params: &ignored,
            };
            Ok(respond::json(&body, Engine::Native, &timing))
        }
        (true, Some(_)) => Err(ApiError::bad_request("`text` must be a string").with("parameter", "text")),
        (false, None) => {
            let detected = detect::detect(&input.bytes, params.format, input.filename.as_deref())?;
            count_document(detected.kind);
            let ignored = params.ignored(Some(detected.kind), Endpoint::Fields);
            let ext = read_document(&ctx, input.bytes, detected.kind, &params, Mode::Text).await?;
            let Extraction { kind, text, truncated, paged, engine, mut timing, .. } = ext;
            let (found, text, took) = match_fields(&ctx, fields, text).await?;
            timing.extract += took;
            let body = FieldsResponse {
                kind: kind.kind_str(),
                format: Some(kind.format_str()),
                format_source: Some(detected.source),
                fields: &found.values,
                matched: &found.matched,
                missing_required: &found.missing_required,
                char_count: text.chars().count(),
                truncated,
                paged: paged.as_ref(),
                text: params.include_text.then_some(text.as_str()),
                ignored_params: &ignored,
            };
            Ok(respond::json(&body, engine, &timing))
        }
    }
}

/// Runs the fields over the text on the CPU pool, with a deadline of its own:
/// the document's may already be spent on OCR. So a request that OCRs can run
/// past `ocr_timeout` by up to `process_timeout` here; matching is linear in
/// the text, so in practice it takes milliseconds.
async fn match_fields(ctx: &Ctx, fields: Vec<Field>, text: String) -> Result<(Extracted, String, Duration), ApiError> {
    let timeout = ctx.st.cfg.process_timeout;
    let deadline = Instant::now() + timeout;
    let rx = ctx.st.cpu.spawn(ctx.admitted.permit.clone(), timeout, move || {
        let t = Instant::now();
        let found = fields::extract(&fields, &text);
        Ok((found, text, t.elapsed()))
    });
    await_job(rx, deadline).await
}

// ---------------------------------------------------------------------------
// Office
// ---------------------------------------------------------------------------

async fn office_flow(ctx: &Ctx, bytes: Bytes, fmt: OfficeFormat, mode: Mode) -> Result<Extraction, ApiError> {
    let deadline = ctx.admitted.admitted_at + ctx.st.cfg.process_timeout;
    let max_chars = ctx.st.cfg.max_text_chars;
    let clean = ctx.clean;
    let rx = ctx.st.cpu.spawn(ctx.admitted.permit.clone(), ctx.st.cfg.process_timeout, move || {
        let t = Instant::now();
        let (text, cut_short) = crate::office::extract(bytes, fmt, mode, max_chars)?;
        let parse = t.elapsed();
        let t = Instant::now();
        let mut budget = TextBudget::new(max_chars);
        let text = budget.take(normalize_arabic_forms(text));
        let text = if clean { collapse_whitespace(&text) } else { text };
        Ok((text, budget.truncated || cut_short, parse, t.elapsed()))
    });
    let (text, truncated, parse, extract) = await_job(rx, deadline).await?;
    Ok(Extraction {
        kind: DocKind::Office(fmt),
        text,
        truncated,
        paged: None,
        engine: if fmt.is_odf() { Engine::Native } else { Engine::Office },
        timing: Timing { queue: ctx.admitted.queue_wait, parse, extract, ocr: None },
        document: None,
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

/// Per-page outcome after phase 2.
struct OutPage {
    index: usize,
    text: String,
    ocr_applied: bool,
    merged: bool,
    document: Option<DocumentPage>,
}

async fn pdf_flow(ctx: &Ctx, bytes: Bytes, params: &ExtractParams, mode: Mode) -> Result<Extraction, ApiError> {
    let cfg = &ctx.st.cfg;
    let native_deadline = ctx.admitted.admitted_at + cfg.process_timeout;
    let job = pdf::PdfJob {
        mode,
        structured: params.structured,
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

async fn image_flow(ctx: &Ctx, bytes: Bytes, fmt: ImageFormat, params: &ExtractParams) -> Result<Extraction, ApiError> {
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
async fn paged_flow(ctx: &Ctx, p1: Phase1, params: &ExtractParams, kind: DocKind) -> Result<Extraction, ApiError> {
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
        run_ocr(ctx, &p1, &needing, params.lang, strict).await?
    };

    finish(ctx, p1, phase2, reasons, kind, params.structured)
}

/// Phase 2 outcome.
struct Phase2 {
    status_hint: Option<OcrStatus>,
    lang: Option<(Lang, LangSource)>,
    /// page index → OCR text (replacement or merge per the page's reason).
    texts: Vec<(usize, String)>,
    blocks: std::collections::HashMap<usize, Vec<Block>>,
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
            blocks: Default::default(),
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
            structured: p1.pages.iter().any(|p| p.document.is_some()),
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
                    if !res.blocks.is_empty() {
                        out.blocks.insert(idx, res.blocks);
                    }
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

fn finish(ctx: &Ctx, p1: Phase1, mut p2: Phase2, reasons: Vec<PageReason>, kind: DocKind, structured: bool) -> Result<Extraction, ApiError> {
    let Phase1 { pages, page_count, incomplete: p1_incomplete, truncated: p1_truncated, parse_time, extract_time, .. } = p1;
    let mut ocr_texts: std::collections::HashMap<usize, String> = p2.texts.into_iter().collect();
    let mut out_pages: Vec<OutPage> = Vec::with_capacity(pages.len());
    let mut native_failed = Vec::new();
    for mut p in pages {
        if p2.timed_out.contains(&p.index) {
            continue; // not done: the caller can re-request it with `pages=`
        }
        if p.failed {
            native_failed.push(p.index + 1);
        }
        match (ocr_texts.remove(&p.index), p.ocr) {
            (Some(ocr), Some(reason)) => {
                if let Some(page) = &mut p.document {
                    page.apply_ocr(
                        p2.blocks.remove(&p.index).unwrap_or_default(),
                        reason.merges(),
                    );
                }
                let merged = reason.merges();
                // Structured output is assembled from blocks; avoid a second
                // merge of the same content into an unused plain-text string.
                let text = if structured {
                    String::new()
                } else if merged {
                    crate::text::merge_native_and_ocr(&p.text, &ocr)
                } else {
                    ocr
                };
                out_pages.push(OutPage { index: p.index, text, ocr_applied: true, merged, document: p.document });
            }
            _ => out_pages.push(OutPage { index: p.index, text: p.text, ocr_applied: false, merged: false, document: p.document }),
        }
    }

    let incomplete = p1_incomplete || !p2.timed_out.is_empty();
    if incomplete && out_pages.is_empty() {
        return Err(ApiError::processing_timeout("no page could be processed within the deadline"));
    }

    let mut budget = TextBudget::new(ctx.st.cfg.max_text_chars);
    let mut text = String::new();
    let mut document = structured.then(Vec::new);
    if let Some(document) = &mut document {
        let mut kept = 0;
        for p in &mut out_pages {
            if budget.exhausted() {
                budget.truncated = true;
                break;
            }
            if let Some(mut page) = p.document.take() {
                page.finish(&mut budget, ctx.clean);
                document.push(page);
            }
            kept += 1;
        }
        out_pages.truncate(kept);
    } else {
        for (i, p) in out_pages.iter().enumerate() {
            if budget.exhausted() {
                break;
            }
            let sep = if i > 0 { "\n\n" } else { "" };
            let piece = budget.take(format!("{sep}{}", p.text.trim_end()));
            text.push_str(&piece);
        }
    }
    let text = normalize_arabic_forms(text);
    let text = if ctx.clean { collapse_whitespace(&text) } else { text };
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
    Ok(Extraction {
        kind,
        text,
        truncated,
        paged: Some(Paged { page_count, pages_extracted, incomplete, ocr, failed_pages: native_failed }),
        engine: Engine::Pdf,
        timing,
        document,
    })
}
