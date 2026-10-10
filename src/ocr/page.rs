//! Page OCR from public upstream parts: page image → detect → crop →
//! recognize → reading order. Plus the language probe.

use image::DynamicImage;
use pdf_oxide::PdfDocument;
use pdf_oxide::ocr::{DetectedBox, crop_text_region};
use pdf_oxide::rendering::{RenderOptions, render_page};

use super::stack::OcrStack;
use crate::pdf::lang::Lang;
use crate::text::ScriptCounts;

/// Embedded images smaller than this (pixels) aren't trusted to be the
/// page scan; the page is rendered instead.
const MIN_SCAN_PIXELS: u64 = 500_000;
/// Rendered at 300 DPI so 6–8 pt invoice table text survives detection
/// downscaling (was 200 DPI, which left small glyphs under ~9 px high).
const RENDER_DPI: u32 = 300;

/// Where the OCR input image came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSource {
    Embedded,
    Rendered,
}

/// Largest embedded image, when it is big enough to be the page scan.
/// Returns `None` when there is no usable scan (caller renders instead).
/// `accept_small` is for hybrid stamps: any size goes.
pub fn embedded_image(doc: &PdfDocument, page: usize, accept_small: bool) -> Option<Result<DynamicImage, String>> {
    let largest = doc
        .extract_images(page)
        .map_err(|e| format!("image extraction failed: {e}"))
        .map(|imgs| {
            imgs.into_iter()
                .max_by_key(|img| u64::from(img.width()) * u64::from(img.height()))
        });
    match largest {
        Err(why) => Some(Err(why)),
        Ok(None) => None,
        Ok(Some(img)) => {
            let px = u64::from(img.width()) * u64::from(img.height());
            if px >= MIN_SCAN_PIXELS || accept_small {
                tracing::debug!(page = page + 1, width = img.width(), height = img.height(), pixels = px, "ocr embedded image");
                Some(img.to_dynamic_image().map_err(|e| format!("image decode failed: {e}")))
            } else {
                tracing::debug!(page = page + 1, width = img.width(), height = img.height(), pixels = px, "embedded image too small; rendering");
                None
            }
        }
    }
}

pub fn rendered_image(doc: &PdfDocument, page: usize) -> Result<DynamicImage, String> {
    let opts = RenderOptions::with_dpi(RENDER_DPI).as_raw();
    let r = render_page(doc, page, &opts).map_err(|e| format!("page render failed: {e}"))?;
    tracing::debug!(page = page + 1, width = r.width, height = r.height, dpi = RENDER_DPI, "ocr rendered page");
    image::RgbaImage::from_raw(r.width, r.height, r.data)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| "page render returned a malformed buffer".to_string())
}

/// Gets the image to OCR: the page's largest embedded image (scans), or a
/// rendering of the page (legacy-font pages, pages without a usable scan).
pub fn page_image(doc: &PdfDocument, page: usize, render: bool, accept_small: bool) -> Result<DynamicImage, String> {
    page_image_with_source(doc, page, render, accept_small).map(|(img, _)| img)
}

pub fn page_image_with_source(
    doc: &PdfDocument,
    page: usize,
    render: bool,
    accept_small: bool,
) -> Result<(DynamicImage, ImageSource), String> {
    if !render && let Some(res) = embedded_image(doc, page, accept_small) {
        return res.map(|img| (img, ImageSource::Embedded));
    }
    rendered_image(doc, page).map(|img| (img, ImageSource::Rendered))
}

#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    pub confidence: f32,
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

impl Span {
    fn new(text: String, confidence: f32, poly: &[[f32; 2]; 4]) -> Self {
        let xs = poly.iter().map(|p| p[0]);
        let ys = poly.iter().map(|p| p[1]);
        Self {
            text,
            confidence,
            min_x: xs.clone().fold(f32::MAX, f32::min),
            max_x: xs.fold(f32::MIN, f32::max),
            min_y: ys.clone().fold(f32::MAX, f32::min),
            max_y: ys.fold(f32::MIN, f32::max),
        }
    }
    fn cy(&self) -> f32 {
        (self.min_y + self.max_y) / 2.0
    }
    fn h(&self) -> f32 {
        (self.max_y - self.min_y).max(1.0)
    }
}

fn box_area(b: &DetectedBox) -> f32 {
    // Shoelace formula over the quadrilateral.
    let p = &b.polygon;
    let mut a = 0.0;
    for i in 0..4 {
        let j = (i + 1) % 4;
        a += p[i][0] * p[j][1] - p[j][0] * p[i][1];
    }
    (a / 2.0).abs()
}

/// PaddleOCR's `pred_reverse` for Arabic models, which emit characters in
/// visual (left-to-right) order: reverse the run order while keeping runs
/// of Latin letters, digits and number punctuation intact. Unlike upstream,
/// the spaces at the edges of a kept run are mirrored with it, so
/// "2026 ابحرم" becomes "مرحبا 2026", not "مرحبا2026 ".
pub fn arabic_pred_reverse(s: &str) -> String {
    let keep = |c: char| c.is_ascii_alphanumeric() || " :*./%+-".contains(c);
    let mut runs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, runs: &mut Vec<String>| {
        if cur.is_empty() {
            return;
        }
        let core = cur.trim_matches(' ');
        let lead = cur.len() - cur.trim_start_matches(' ').len();
        let trail = cur.len() - cur.trim_end_matches(' ').len();
        let mirrored = if core.is_empty() {
            cur.clone()
        } else {
            format!("{}{core}{}", " ".repeat(trail), " ".repeat(lead))
        };
        runs.push(mirrored);
        cur.clear();
    };
    for c in s.chars() {
        if keep(c) {
            cur.push(c);
        } else {
            flush(&mut cur, &mut runs);
            runs.push(c.to_string());
        }
    }
    flush(&mut cur, &mut runs);
    runs.reverse();
    runs.concat()
}

/// Inter-word gap, as a fraction of the line's ink height, above which a
/// line crop is split into words before recognition. The PP-OCRv3
/// Devanagari and Arabic recognizers rarely emit the space class, so words
/// run together unless each is recognized on its own. Measured on Noto
/// renderings: Devanagari words are joined by the headline (intra-word gaps
/// ≤ 0.04, spaces ≥ 0.18); Arabic has small gaps after non-joining letters
/// (≤ 0.19) but spaces ≥ 0.31. On 88 labelled lines (two fonts per script,
/// blur + noise) splitting raised space-aware accuracy from 0.68 to 0.82
/// (hi) and 0.87 to 0.94 (ar) with no loss in character accuracy.
/// `None`: the recognizer spaces words itself.
fn word_gap(lang: Lang) -> Option<f32> {
    match lang {
        Lang::En => None,
        Lang::Hi => Some(0.11),
        Lang::Ar => Some(0.25),
    }
}

fn recognize_boxes(stack: &OcrStack, img: &DynamicImage, boxes: &[DetectedBox], lang: Lang) -> Result<Vec<Span>, String> {
    let rec = stack.recognizers.get(&lang).ok_or_else(|| format!("recognizer {} not loaded", lang.code()))?;
    let threshold = stack.config.rec_threshold;
    let mut spans = Vec::with_capacity(boxes.len());
    for b in boxes {
        let crop = crop_text_region(img, &b.polygon).map_err(|e| format!("crop failed: {e}"))?;
        let segments = word_gap(lang).map_or_else(Vec::new, |gap| word_segments(&crop, gap));
        let pieces: Vec<DynamicImage> = if segments.len() > 1 {
            segments.iter().map(|&(x0, x1)| crop.crop_imm(x0, 0, x1 - x0, crop.height())).collect()
        } else {
            vec![crop]
        };
        let mut words: Vec<String> = Vec::with_capacity(pieces.len());
        let (mut conf_sum, mut chars) = (0.0f32, 0usize);
        for piece in &pieces {
            let r = rec.recognize(piece).map_err(|e| format!("recognition failed: {e}"))?;
            let n = r.text.trim().chars().count();
            if n == 0 || r.confidence < threshold {
                continue;
            }
            conf_sum += r.confidence * n as f32;
            chars += n;
            words.push(postprocess(lang, r.text.trim()));
        }
        if words.is_empty() {
            continue;
        }
        if lang == Lang::Ar {
            words.reverse(); // segments run left to right; Arabic reads right to left
        }
        spans.push(Span::new(words.join(" "), conf_sum / chars as f32, &b.polygon));
    }
    Ok(spans)
}

pub fn postprocess(lang: Lang, text: &str) -> String {
    match lang {
        Lang::En => text.to_string(),
        Lang::Hi => reorder_devanagari_i_sign(text),
        Lang::Ar => arabic_pred_reverse(text),
    }
}

/// Splits a (rectified) line crop into word x-ranges at ink gaps wider than
/// `rel_gap` × the line's ink height. Returns fewer than two ranges when
/// the line shouldn't be split.
pub fn word_segments(crop: &DynamicImage, rel_gap: f32) -> Vec<(u32, u32)> {
    let gray = crop.to_luma8();
    let (w, h) = gray.dimensions();
    if w < 8 || h < 8 {
        return Vec::new();
    }
    let threshold = otsu(&gray);
    let dark_text = {
        let mean = gray.pixels().map(|p| u64::from(p.0[0])).sum::<u64>() / (u64::from(w) * u64::from(h));
        mean as u8 > threshold
    };
    let ink = |x: u32, y: u32| {
        let v = gray.get_pixel(x, y).0[0];
        if dark_text { v <= threshold } else { v > threshold }
    };
    // Ink rows (ignoring single-pixel noise) give the line's ink height.
    let row_ink: Vec<u32> = (0..h).map(|y| (0..w).filter(|&x| ink(x, y)).count() as u32).collect();
    let min_row = (w / 100).max(1);
    let (Some(top), Some(bottom)) = (row_ink.iter().position(|&c| c >= min_row), row_ink.iter().rposition(|&c| c >= min_row)) else {
        return Vec::new();
    };
    let ink_h = (bottom - top + 1) as u32;
    let noise = (ink_h / 30).max(1);
    let col_ink: Vec<bool> = (0..w).map(|x| (top as u32..=bottom as u32).filter(|&y| ink(x, y)).count() as u32 > noise).collect();
    let (Some(first), Some(last)) = (col_ink.iter().position(|&c| c), col_ink.iter().rposition(|&c| c)) else {
        return Vec::new();
    };
    let min_gap = (rel_gap * ink_h as f32).ceil() as usize;
    let pad = (ink_h as usize / 6).max(2);
    let mut segments = Vec::new();
    let mut seg_start = first;
    let mut x = first;
    while x <= last {
        if col_ink[x] {
            x += 1;
            continue;
        }
        let gap_start = x;
        while x <= last && !col_ink[x] {
            x += 1;
        }
        if x - gap_start >= min_gap {
            segments.push((seg_start.saturating_sub(pad) as u32, (gap_start + pad).min(w as usize) as u32));
            seg_start = x;
        }
    }
    segments.push((seg_start.saturating_sub(pad) as u32, (last + 1 + pad).min(w as usize) as u32));
    segments
}

/// Otsu's threshold over an 8-bit histogram.
fn otsu(gray: &image::GrayImage) -> u8 {
    let mut hist = [0u64; 256];
    for p in gray.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u64 = hist.iter().sum();
    let sum_all: f64 = hist.iter().enumerate().map(|(i, &c)| i as f64 * c as f64).sum();
    let (mut w_b, mut sum_b, mut best, mut best_t) = (0u64, 0f64, -1f64, 127u8);
    for (t, &c) in hist.iter().enumerate() {
        w_b += c;
        if w_b == 0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0 {
            break;
        }
        sum_b += t as f64 * c as f64;
        let (m_b, m_f) = (sum_b / w_b as f64, (sum_all - sum_b) / w_f as f64);
        let between = w_b as f64 * w_f as f64 * (m_b - m_f).powi(2);
        if between > best {
            best = between;
            best_t = t as u8;
        }
    }
    best_t
}

fn is_deva_consonant(c: char) -> bool {
    matches!(c, '\u{0915}'..='\u{0939}' | '\u{0958}'..='\u{095F}' | '\u{0978}'..='\u{097F}')
}

/// The short-i sign ि (U+093F) is drawn *left* of its consonant, and the
/// recognizer sometimes emits it in that visual order. In Unicode a vowel
/// sign always follows its consonant cluster, so a ि that doesn't follow a
/// consonant is moved after the cluster it precedes. (A ि after a consonant
/// is already valid and left alone.)
pub fn reorder_devanagari_i_sign(s: &str) -> String {
    const I_SIGN: char = '\u{093F}';
    const NUKTA: char = '\u{093C}';
    const VIRAMA: char = '\u{094D}';
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let after_consonant = i > 0 && (is_deva_consonant(chars[i - 1]) || chars[i - 1] == NUKTA);
        if c == I_SIGN && !after_consonant && chars.get(i + 1).is_some_and(|&n| is_deva_consonant(n)) {
            // Cluster: C (nukta)? (virama C (nukta)?)*
            let mut j = i + 2;
            if chars.get(j) == Some(&NUKTA) {
                j += 1;
            }
            while chars.get(j) == Some(&VIRAMA) && chars.get(j + 1).is_some_and(|&n| is_deva_consonant(n)) {
                j += 2;
                if chars.get(j) == Some(&NUKTA) {
                    j += 1;
                }
            }
            out.extend(&chars[i + 1..j]);
            out.push(I_SIGN);
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

pub fn detect(stack: &OcrStack, img: &DynamicImage) -> Result<Vec<DetectedBox>, String> {
    stack.detector.detect(img).map_err(|e| format!("detection failed: {e}"))
}

pub struct PageOcr {
    pub text: String,
    pub lang: Lang,
    pub spans: usize,
}

/// OCR one image with a fixed language.
pub fn ocr_image(stack: &OcrStack, img: &DynamicImage, lang: Lang) -> Result<PageOcr, String> {
    let boxes = detect(stack, img)?;
    let spans = recognize_boxes(stack, img, &boxes, lang)?;
    tracing::debug!(boxes = boxes.len(), spans = spans.len(), chars = spans.iter().map(|s| s.text.len()).sum::<usize>(), lang = lang.code(), "ocr fixed");
    Ok(PageOcr { spans: spans.len(), text: assemble(spans, lang == Lang::Ar), lang })
}

/// Language probe: detect once, recognize the 8 largest boxes with every
/// candidate recognizer, keep the best mean confidence among outputs
/// dominated by their own script. The detection is reused for the page.
pub fn probe_and_ocr(stack: &OcrStack, img: &DynamicImage, candidates: &[Lang]) -> Result<(PageOcr, bool), String> {
    let boxes = detect(stack, img)?;
    let mut by_size: Vec<&DetectedBox> = boxes.iter().collect();
    by_size.sort_by(|a, b| box_area(b).total_cmp(&box_area(a)));
    let sample: Vec<DetectedBox> = by_size.into_iter().take(8).cloned().collect();

    let mut best: Option<(Lang, f32)> = None;
    if !sample.is_empty() {
        let crops: Vec<DynamicImage> = sample
            .iter()
            .filter_map(|b| crop_text_region(img, &b.polygon).ok())
            .collect();
        for &lang in candidates {
            let Some(rec) = stack.recognizers.get(&lang) else { continue };
            let mut conf_sum = 0.0;
            let mut counts = ScriptCounts::default();
            for crop in &crops {
                if let Ok(r) = rec.recognize(crop) {
                    conf_sum += r.confidence;
                    r.text.chars().for_each(|c| counts.add(c));
                }
            }
            let mean = conf_sum / crops.len().max(1) as f32;
            let dominated = lang.script_share(&counts) >= 0.5;
            tracing::debug!(lang = lang.code(), mean_conf = mean, dominated, "ocr probe");
            if dominated && best.is_none_or(|(_, c)| mean > c) {
                best = Some((lang, mean));
            }
        }
    }
    let (lang, probed) = match best {
        Some((l, _)) => (l, true),
        None => (Lang::En, false),
    };
    let spans = recognize_boxes(stack, img, &boxes, lang)?;
    tracing::debug!(boxes = boxes.len(), spans = spans.len(), lang = lang.code(), probed, "ocr probe done");
    Ok((PageOcr { spans: spans.len(), text: assemble(spans, lang == Lang::Ar), lang }, probed))
}

/// Groups spans into lines (vertical overlap), orders words within a line
/// (right-to-left for Arabic), and inserts paragraph breaks at large gaps.
pub fn assemble(mut spans: Vec<Span>, rtl: bool) -> String {
    if spans.is_empty() {
        return String::new();
    }
    spans.sort_by(|a, b| a.cy().total_cmp(&b.cy()));
    let mut lines: Vec<Vec<Span>> = Vec::new();
    for s in spans {
        if let Some(line) = lines.last_mut() {
            let (cy, h) = line_metrics(line);
            if (s.cy() - cy).abs() <= 0.5 * h.max(s.h()) {
                line.push(s);
                continue;
            }
        }
        lines.push(vec![s]);
    }
    let mut heights: Vec<f32> = lines.iter().map(|l| line_metrics(l).1).collect();
    heights.sort_by(f32::total_cmp);
    let median_h = heights[heights.len() / 2];

    let mut out = String::new();
    let mut prev_bottom: Option<f32> = None;
    for mut line in lines {
        if rtl {
            line.sort_by(|a, b| b.max_x.total_cmp(&a.max_x));
        } else {
            line.sort_by(|a, b| a.min_x.total_cmp(&b.min_x));
        }
        let top = line.iter().map(|s| s.min_y).fold(f32::MAX, f32::min);
        let bottom = line.iter().map(|s| s.max_y).fold(f32::MIN, f32::max);
        if let Some(pb) = prev_bottom {
            out.push_str(if top - pb > median_h { "\n\n" } else { "\n" });
        }
        let words: Vec<&str> = line.iter().map(|s| s.text.trim()).collect();
        out.push_str(&words.join(" "));
        prev_bottom = Some(bottom);
    }
    out
}

fn line_metrics(line: &[Span]) -> (f32, f32) {
    let n = line.len() as f32;
    let cy = line.iter().map(Span::cy).sum::<f32>() / n;
    let h = line.iter().map(Span::h).sum::<f32>() / n;
    (cy, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(t: &str, x: f32, y: f32, w: f32, h: f32) -> Span {
        Span::new(t.into(), 0.9, &[[x, y], [x + w, y], [x + w, y + h], [x, y + h]])
    }

    #[test]
    fn lines_and_paragraphs() {
        let spans = vec![
            span("world", 120.0, 12.0, 80.0, 20.0),
            span("Hello", 10.0, 10.0, 90.0, 20.0),
            span("next", 10.0, 36.0, 60.0, 20.0),
            span("para", 10.0, 120.0, 60.0, 20.0),
        ];
        assert_eq!(assemble(spans, false), "Hello world\nnext\n\npara");
    }

    #[test]
    fn rtl_line_order() {
        let spans = vec![span("A", 10.0, 10.0, 40.0, 20.0), span("B", 100.0, 10.0, 40.0, 20.0)];
        assert_eq!(assemble(spans, true), "B A");
    }

    #[test]
    fn devanagari_i_sign_reordering() {
        // Visual-order output → logical order.
        assert_eq!(reorder_devanagari_i_sign("केिलए"), "केलिए");
        assert_eq!(reorder_devanagari_i_sign("िहंदी"), "हिंदी");
        assert_eq!(reorder_devanagari_i_sign("संिवधान"), "संविधान");
        assert_eq!(reorder_devanagari_i_sign("ि\u{0938}\u{094D}\u{0925}ति"), "\u{0938}\u{094D}\u{0925}िति");
        // Already-logical text is untouched.
        assert_eq!(reorder_devanagari_i_sign("किला लिए"), "किला लिए");
        assert_eq!(reorder_devanagari_i_sign("hello"), "hello");
    }

    #[test]
    fn word_segmentation_on_gaps() {
        // Three ink blocks: a narrow gap (same word) and a wide one (space).
        let mut img = image::GrayImage::from_pixel(200, 40, image::Luma([255]));
        for (x0, x1) in [(10, 60), (62, 100), (140, 190)] {
            for x in x0..x1 {
                for y in 10..30 {
                    img.put_pixel(x, y, image::Luma([0]));
                }
            }
        }
        let segs = word_segments(&DynamicImage::ImageLuma8(img), 0.25);
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert!(segs[0].1 <= 140 && segs[1].0 >= 100, "{segs:?}");
    }

    #[test]
    fn arabic_reverse_keeps_numbers() {
        assert_eq!(arabic_pred_reverse("abc"), "abc");
        assert_eq!(arabic_pred_reverse("مرحبا"), "ابحرم");
        assert_eq!(arabic_pred_reverse("2026 ابحرم"), "مرحبا 2026");
        assert_eq!(arabic_pred_reverse("ملاع hello world ابحرم"), "مرحبا hello world عالم");
    }
}
