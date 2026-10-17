//! PDF phase 1 (runs on the CPU pool): open → `page_count()` → per page:
//! classify + legacy-font check + native text. Also `/v1/info` for PDFs.

pub mod lang;
pub mod route;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pdf_oxide::PdfDocument;
use pdf_oxide::converters::ConversionOptions;
use pdf_oxide::object::Object;
use serde::Serialize;
use serde_json::Value;

use crate::error::ApiError;
use crate::raster::ImageDoc;
use crate::text::ScriptCounts;
use route::{OcrMode, OcrReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Markdown { detect_headings: bool },
}

pub struct PdfJob {
    pub mode: Mode,
    /// 1-based page selection as requested; `None` = all pages.
    pub pages: Option<Vec<usize>>,
    pub ocr: OcrMode,
    pub max_text_chars: usize,
    pub max_pdf_pages: usize,
    pub deadline: Instant,
}

pub struct NativePage {
    /// 0-based page index.
    pub index: usize,
    pub text: String,
    pub ocr: Option<OcrReason>,
    pub failed: bool,
}

/// What phase 2 OCRs: a PDF's pages or an image's pages (TIFF frames).
#[derive(Clone)]
pub enum PageSource {
    Pdf(Arc<PdfDocument>),
    Image(Arc<ImageDoc>),
}

/// Phase 1 output for paged documents (PDFs and images).
pub struct Phase1 {
    pub source: PageSource,
    pub page_count: usize,
    pub pages: Vec<NativePage>,
    /// The deadline passed before every requested page was processed.
    pub incomplete: bool,
    /// The native-text budget ran out before every requested page was processed.
    pub truncated: bool,
    pub legacy_font: bool,
    pub script_counts: ScriptCounts,
    pub parse_time: Duration,
    pub extract_time: Duration,
}

pub fn open(bytes: Bytes, max_pages: usize) -> Result<(PdfDocument, usize), ApiError> {
    // `Bytes` → `Vec<u8>` reuses the allocation when the buffer is unique.
    let doc = PdfDocument::from_bytes(Vec::from(bytes)).map_err(|e| ApiError::from_pdf(&e))?;
    // `page_count` is the one call that fails on a locked PDF; text
    // extraction would silently return "" instead. Always call it first.
    let n = doc.page_count().map_err(|e| ApiError::from_pdf(&e))?;
    if doc.is_encrypted() && !doc.is_authenticated() {
        return Err(ApiError::from_pdf(&pdf_oxide::Error::EncryptedPdf));
    }
    if n > max_pages {
        return Err(ApiError::unprocessable(
            "too-many-pages",
            "Too many pages",
            format!("the PDF has {n} pages; the limit is {max_pages}"),
        )
        .with("page_count", n)
        .with("limit", max_pages));
    }
    Ok((doc, n))
}

/// Text-mode native read for one page: geometric row-major order with
/// visual-order RTL lines restored against the x-verdict reference.
/// Falls back to `extract_text` when the geometric path fails; pages
/// without RTL text never pay for the reference read.
fn plain_text_page(doc: &PdfDocument, idx: usize) -> Result<String, pdf_oxide::Error> {
    let opts = ConversionOptions { extract_tables: true, ..Default::default() };
    let plain = match doc.to_plain_text(idx, &opts) {
        Ok(t) => t,
        Err(_) => return doc.extract_text(idx),
    };
    if !pdf_oxide::text::bidi::looks_rtl(&plain) {
        return Ok(plain);
    }
    match doc.extract_text(idx) {
        Ok(reference) => Ok(crate::text::restore_rtl_lines(&plain, &reference)),
        Err(_) => Ok(plain),
    }
}

/// Validates a 1-based page selection against the page count.
pub fn select_pages(sel: Option<&[usize]>, page_count: usize) -> Result<Vec<usize>, ApiError> {
    match sel {
        None => Ok((0..page_count).collect()),
        Some(pages) => {
            if let Some(bad) = pages.iter().find(|p| **p > page_count) {
                return Err(ApiError::bad_request(format!(
                    "page {bad} is out of range; the document has {page_count} page(s)"
                ))
                .with("page_count", page_count));
            }
            Ok(pages.iter().map(|p| p - 1).collect())
        }
    }
}

/// Parses `pages=1-3,5` (1-based, ascending, de-duplicated).
pub fn parse_pages(s: &str) -> Option<Vec<usize>> {
    let mut out = std::collections::BTreeSet::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<usize>().ok()?, b.trim().parse::<usize>().ok()?),
            None => {
                let n = part.parse::<usize>().ok()?;
                (n, n)
            }
        };
        if a == 0 || b < a || b - a > 100_000 {
            return None;
        }
        out.extend(a..=b);
    }
    (!out.is_empty()).then(|| out.into_iter().collect())
}

pub fn phase1(bytes: Bytes, job: &PdfJob) -> Result<Phase1, ApiError> {
    let t0 = Instant::now();
    let (doc, page_count) = open(bytes, job.max_pdf_pages)?;
    let selected = select_pages(job.pages.as_deref(), page_count)?;
    let parse_time = t0.elapsed();

    let t1 = Instant::now();
    let md_opts = match job.mode {
        Mode::Markdown { detect_headings } => Some(ConversionOptions { detect_headings, ..Default::default() }),
        Mode::Text => None,
    };
    let mut pages = Vec::with_capacity(selected.len());
    let mut incomplete = false;
    let mut truncated = false;
    let mut native_chars = 0usize;
    let mut legacy_any = false;
    let mut counts = ScriptCounts::default();
    // OCR-layer verdicts: (junk pages, good pages, positions in `pages` too
    // sparse to judge).
    let (mut junk_layers, mut good_layers, mut unjudged) = (0usize, 0usize, Vec::new());

    for idx in selected {
        if Instant::now() >= job.deadline {
            incomplete = true;
            break;
        }
        if native_chars >= job.max_text_chars {
            truncated = true;
            break;
        }
        let legacy = job.ocr != OcrMode::Off && page_has_legacy_hindi_font(&doc, idx);
        let class = if job.ocr == OcrMode::Auto && !legacy { doc.classify_page(idx).ok() } else { None };
        let mut plan = route::plan_page(job.ocr, class.map(|c| c.kind), legacy);
        let native = match &md_opts {
            Some(opts) => doc.to_markdown(idx, opts),
            // Plain-text reading order (structure tree for tagged PDFs,
            // XY-cut row-major for untagged ones) keeps side-by-side
            // columns on shared lines, so `Invoice No. GST-3425-26` stays
            // together for `/v1/extract/fields`. `extract_text` assembles
            // tables in content-stream order instead, which splits them.
            None => plain_text_page(&doc, idx),
        };
        let (mut text, failed) = match native {
            Ok(t) => (t, false),
            Err(_) => (String::new(), true),
        };
        legacy_any |= legacy;
        // A scan under an invisible OCR layer is kept as native text if the
        // layer's glyphs are mapped, even when the scanner OCR'd handwriting
        // into nonsense. Junk → OCR the scan, and drop the junk so a page
        // whose OCR fails or can't run comes back empty, not as "native".
        let ocr_layer = plan.is_none()
            && class.is_some_and(|c| c.signals.image_area_ratio >= 0.80 && c.signals.invisible_text_ratio >= 0.50);
        let mut unjudged_layer = false;
        if ocr_layer {
            match crate::text::ocr_layer_is_junk(&text) {
                Some(true) => {
                    junk_layers += 1;
                    plan = Some(OcrReason::Scanned);
                    text.clear();
                }
                Some(false) => good_layers += 1,
                None => unjudged_layer = true,
            }
        }
        // A scanned page's text layer is overlay, not content (a browser's
        // print header/footer, a "Scanned with CamScanner" stamp, Bates
        // numbers) and almost always Latin: counting it would OCR a Hindi
        // or Arabic scan with `en`. Leave the language to the probe.
        if !legacy && plan != Some(OcrReason::Scanned) && !unjudged_layer && counts.letters() < 8000 {
            for ch in text.chars().filter(|c| !c.is_whitespace()).take(8000 - counts.letters()) {
                counts.add(ch);
            }
        }
        native_chars += text.chars().count();
        if unjudged_layer {
            unjudged.push(pages.len());
        }
        pages.push(NativePage { index: idx, text, ocr: plan, failed });
    }
    // One scanner wrote every page's layer: a page with too few words to
    // judge (a lone formula) takes the verdict of the pages that could be.
    if junk_layers > good_layers {
        junk_layers += unjudged.len();
        for i in unjudged {
            pages[i].ocr = Some(OcrReason::Scanned);
            pages[i].text.clear();
        }
    }
    if junk_layers > 0 {
        metrics::counter!("oxide_junk_ocr_layer_pages_total").increment(junk_layers as u64);
    }

    if !pages.is_empty() && pages.iter().all(|p| p.failed) {
        return Err(ApiError::unprocessable(
            "corrupt-document",
            "Unreadable document",
            "no page of the PDF could be read",
        ));
    }

    Ok(Phase1 {
        source: PageSource::Pdf(Arc::new(doc)),
        page_count,
        pages,
        incomplete,
        truncated,
        legacy_font: legacy_any,
        script_counts: counts,
        parse_time,
        extract_time: t1.elapsed(),
    })
}

/// Font face names on one page, from its `/Resources /Font` dictionary.
/// (`page_font_face_lookups()` would walk every page of the document; this
/// costs only the requested pages.)
fn page_font_faces(doc: &PdfDocument, idx: usize) -> Vec<String> {
    let resolve = |o: &Object| doc.resolve_object(o).ok();
    // `get_page` merges inherited attributes, `/Resources` included.
    let Some(res) = doc
        .get_page(idx)
        .ok()
        .and_then(|p| p.as_dict().and_then(|d| d.get("Resources")).and_then(resolve))
    else {
        return Vec::new();
    };
    let Some(fonts) = res.as_dict().and_then(|d| d.get("Font")).and_then(resolve) else {
        return Vec::new();
    };
    let Some(fonts) = fonts.as_dict() else { return Vec::new() };
    fonts
        .values()
        .filter_map(|f| {
            let f = resolve(f)?;
            let base = f.as_dict()?.get("BaseFont").and_then(resolve)?;
            let name = base.as_name()?;
            Some(name.split_once('+').map_or(name, |(_, rest)| rest).to_string())
        })
        .collect()
}

fn page_has_legacy_hindi_font(doc: &PdfDocument, idx: usize) -> bool {
    page_font_faces(doc, idx).iter().any(|f| route::is_legacy_hindi_font(f))
}

// ---------------------------------------------------------------------------
// /v1/info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PdfInfo {
    pub kind: &'static str,
    pub format: &'static str,
    pub format_source: crate::detect::FormatSource,
    pub version: String,
    pub page_count: usize,
    pub encrypted: bool,
    pub metadata: BTreeMap<String, Value>,
    pub pages: Vec<PageGeometry>,
}

#[derive(Serialize)]
pub struct PageGeometry {
    pub page: usize,
    pub width: f32,
    pub height: f32,
    pub rotation: i32,
}

pub fn info(bytes: Bytes, max_pages: usize, source: crate::detect::FormatSource) -> Result<PdfInfo, ApiError> {
    let (doc, page_count) = open(bytes, max_pages)?;
    let pages = (0..page_count)
        .map(|i| {
            let (x0, y0, x1, y1) = doc.get_page_media_box(i).unwrap_or((0.0, 0.0, 0.0, 0.0));
            PageGeometry { page: i + 1, width: x1 - x0, height: y1 - y0, rotation: doc.get_page_rotation(i).unwrap_or(0) }
        })
        .collect();
    let (major, minor) = doc.version();
    Ok(PdfInfo {
        kind: "pdf",
        format: "pdf",
        format_source: source,
        version: format!("{major}.{minor}"),
        page_count,
        encrypted: doc.is_encrypted(),
        metadata: info_dict(&doc),
        pages,
    })
}

fn info_dict(doc: &PdfDocument) -> BTreeMap<String, Value> {
    const KEYS: [(&str, &str); 8] = [
        ("Title", "title"),
        ("Author", "author"),
        ("Subject", "subject"),
        ("Keywords", "keywords"),
        ("Creator", "creator"),
        ("Producer", "producer"),
        ("CreationDate", "created"),
        ("ModDate", "modified"),
    ];
    let mut out = BTreeMap::new();
    let Some(info) = doc.trailer().as_dict().and_then(|t| t.get("Info")).and_then(|i| doc.resolve_object(i).ok()) else {
        return out;
    };
    let Some(dict) = info.as_dict() else { return out };
    for (pdf_key, key) in KEYS {
        let Some(v) = dict.get(pdf_key).and_then(|v| doc.resolve_object(v).ok()) else { continue };
        if let Some(bytes) = v.as_string() {
            let s = decode_text_string(bytes);
            let s = s.trim_matches(char::from(0)).trim();
            if !s.is_empty() {
                out.insert(key.to_string(), Value::String(s.to_string()));
            }
        }
    }
    out
}

/// PDF text strings: UTF-16BE with BOM, UTF-8 with BOM, else PDFDocEncoding
/// (approximated as Latin-1).
pub fn decode_text_string(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    b.iter().map(|&c| c as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_param() {
        assert_eq!(parse_pages("1-3,5"), Some(vec![1, 2, 3, 5]));
        assert_eq!(parse_pages("5,1-2,2"), Some(vec![1, 2, 5]));
        assert_eq!(parse_pages(" 7 "), Some(vec![7]));
        assert_eq!(parse_pages("0"), None);
        assert_eq!(parse_pages("3-1"), None);
        assert_eq!(parse_pages("a"), None);
        assert_eq!(parse_pages(""), None);
    }

    #[test]
    fn page_selection_bounds() {
        assert_eq!(select_pages(None, 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(select_pages(Some(&[1, 3]), 3).unwrap(), vec![0, 2]);
        assert!(select_pages(Some(&[4]), 3).is_err());
    }

    #[test]
    fn text_strings() {
        assert_eq!(decode_text_string(&[0xFE, 0xFF, 0x00, 0x41, 0x09, 0x28]), "Aन");
        assert_eq!(decode_text_string(b"Caf\xe9"), "Café");
    }
}
