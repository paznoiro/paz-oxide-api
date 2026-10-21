//! Compact, engine-independent layout for downstream schema mapping.
//! Coordinates are points in the displayed (cropped, rotated) page.

use pdf_oxide::PdfDocument;
use pdf_oxide::document::ReadingOrder;
use pdf_oxide::extractors::text::{ArtifactType, PaginationSubtype};
use pdf_oxide::geometry::Rect;
use serde::Serialize;

use crate::text::{TextBudget, collapse_whitespace, extra_ocr_lines, normalize_arabic_forms};

#[derive(Clone, Serialize)]
pub struct PageFrame {
    pub width: f32,
    pub height: f32,
    pub rotation: i32,
    #[serde(skip)]
    bounds: Rect,
}

impl PageFrame {
    pub fn read(doc: &PdfDocument, index: usize) -> Result<Self, pdf_oxide::Error> {
        let (x0, y0, x1, y1) = doc.get_page_media_box(index)?;
        let media = Rect::from_points(x0, y0, x1, y1);
        // get_page_info is gated behind the rendering feature upstream.
        // Read only CropBox here so native-only builds retain geometry too.
        let page = doc.get_page(index)?;
        let crop = page
            .as_dict()
            .and_then(|d| d.get("CropBox"))
            .and_then(|o| doc.resolve_object(o).ok())
            .and_then(|o| o.as_array().map(|a| a.to_vec()))
            .and_then(|a| {
                let values: Option<Vec<f32>> = a
                    .iter()
                    .take(4)
                    .map(|o| match doc.resolve_object(o).ok()? {
                        pdf_oxide::object::Object::Integer(v) => Some(v as f32),
                        pdf_oxide::object::Object::Real(v) => Some(v as f32),
                        _ => None,
                    })
                    .collect();
                let v = values?;
                (v.len() == 4).then(|| Rect::from_points(v[0], v[1], v[2], v[3]))
            });
        // Match PDF-Oxide's renderer: intersect CropBox with MediaBox,
        // falling back to MediaBox when they do not overlap.
        let bounds = crop
            .and_then(|crop| intersection(media, crop))
            .unwrap_or(media);
        let rotation = doc.get_page_rotation(index)?.rem_euclid(360);
        let (width, height) = match rotation {
            90 | 270 => (bounds.height, bounds.width),
            _ => (bounds.width, bounds.height),
        };
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(pdf_oxide::Error::InvalidPdf(
                "invalid page dimensions".into(),
            ));
        }
        Ok(Self {
            width,
            height,
            rotation,
            bounds,
        })
    }

    /// Transform and clip an axis-aligned PDF user-space rectangle.
    fn native_box(&self, rect: Rect) -> Option<Rect> {
        if ![rect.x, rect.y, rect.width, rect.height]
            .iter()
            .all(|v| v.is_finite())
        {
            return None;
        }
        let rect = intersection(self.bounds, rect)?;
        let (x, y) = (rect.x - self.bounds.x, rect.y - self.bounds.y);
        let (w, h) = (rect.width, rect.height);
        Some(match self.rotation {
            90 => Rect::new(y, x, h, w),
            180 => Rect::new(self.bounds.width - x - w, y, w, h),
            270 => Rect::new(self.bounds.height - y - h, self.bounds.width - x - w, h, w),
            _ => Rect::new(x, self.bounds.height - y - h, w, h),
        })
    }

    /// Rendered OCR uses the same visible page frame, with a pixel origin at top-left.
    pub fn ocr_box(&self, rect: Rect, image_width: u32, image_height: u32) -> Option<Rect> {
        if image_width == 0
            || image_height == 0
            || ![rect.x, rect.y, rect.width, rect.height]
                .iter()
                .all(|v| v.is_finite())
        {
            return None;
        }
        let (sx, sy) = (
            self.width / image_width as f32,
            self.height / image_height as f32,
        );
        intersection(
            Rect::new(0.0, 0.0, self.width, self.height),
            Rect::new(rect.x * sx, rect.y * sy, rect.width * sx, rect.height * sy),
        )
    }
}

fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let (right, bottom) = (
        (a.x + a.width).min(b.x + b.width),
        (a.y + a.height).min(b.y + b.height),
    );
    (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockSource {
    Native,
    Ocr,
}

#[derive(Clone, Serialize)]
pub struct Font {
    pub name: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Serialize)]
pub struct Block {
    pub id: String,
    pub reading_order: usize,
    pub kind: &'static str,
    pub text: String,
    /// Null when the source has no usable geometry.
    pub bbox: Option<Rect>,
    pub source: BlockSource,
    /// Recognizer confidence only; native text has no confidence score.
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading_level: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<Font>,
}

impl Block {
    pub fn ocr(text: String, bbox: Rect, confidence: f32) -> Self {
        Self {
            id: String::new(),
            reading_order: 0,
            kind: "text",
            text,
            bbox: Some(bbox),
            source: BlockSource::Ocr,
            confidence: (confidence.is_finite() && (0.0..=1.0).contains(&confidence))
                .then_some(confidence),
            heading_level: None,
            font: None,
        }
    }
}

#[derive(Serialize)]
pub struct DocumentPage {
    pub page: usize,
    #[serde(flatten)]
    pub frame: PageFrame,
    pub text: String,
    pub blocks: Vec<Block>,
    pub truncated: bool,
}

impl DocumentPage {
    pub fn new(page: usize, frame: PageFrame) -> Self {
        Self {
            page,
            frame,
            text: String::new(),
            blocks: Vec::new(),
            truncated: false,
        }
    }

    /// One positioned-span read; no second plain-text conversion or per-glyph output.
    pub fn read_native(&mut self, doc: &PdfDocument, index: usize) -> Result<(), pdf_oxide::Error> {
        let spans = doc.extract_spans_with_reading_order(index, ReadingOrder::Structure)?;
        for span in spans {
            if span.text.trim().is_empty() {
                continue;
            }
            let bbox = self.frame.native_box(span.bbox);
            // Text entirely outside the visible page is not part of its content.
            if bbox.is_none()
                && span.bbox.width > 0.0
                && span.bbox.height > 0.0
                && [span.bbox.x, span.bbox.y, span.bbox.width, span.bbox.height]
                    .iter()
                    .all(|v| v.is_finite())
            {
                continue;
            }
            let kind = match span.artifact_type {
                Some(ArtifactType::Pagination(PaginationSubtype::Header)) => "header",
                Some(ArtifactType::Pagination(PaginationSubtype::Footer)) => "footer",
                Some(ArtifactType::Pagination(PaginationSubtype::PageNumber)) => "page_number",
                Some(_) => "artifact",
                None if span.heading_level.is_some() => "heading",
                None => "text",
            };
            self.blocks.push(Block {
                id: String::new(),
                reading_order: 0,
                kind,
                text: span.text,
                bbox,
                source: BlockSource::Native,
                confidence: None,
                heading_level: span.heading_level,
                font: Some(Font {
                    name: span.font_name,
                    size: span.font_size,
                    bold: span.font_weight.is_bold(),
                    italic: span.is_italic,
                }),
            });
        }
        self.text = self
            .blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(())
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.blocks.clear();
    }

    pub fn apply_ocr(&mut self, blocks: Vec<Block>, merge: bool) {
        if !merge {
            self.blocks = blocks;
            return;
        }
        let ocr_text = blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut extra: std::collections::HashSet<&str> =
            extra_ocr_lines(&self.text, &ocr_text).into_iter().collect();
        let before = self.blocks.len();
        self.blocks
            .extend(blocks.into_iter().filter(|b| extra.remove(b.text.trim())));
        if self.blocks.len() > before {
            // Mixed pages have no shared structure tree for their OCR content.
            // Use displayed geometric order; the native-only path keeps engine order.
            self.blocks.sort_by(|a, b| match (a.bbox, b.bbox) {
                (Some(a), Some(b)) => a.y.total_cmp(&b.y).then_with(|| a.x.total_cmp(&b.x)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
        }
    }

    /// Charge text once across the document; page text is exactly the ordered
    /// blocks joined by newlines, including when the final block is shortened.
    pub fn finish(&mut self, budget: &mut TextBudget, clean: bool) {
        let mut blocks = Vec::new();
        self.text.clear();
        let total = self.blocks.len();
        for mut block in self.blocks.drain(..) {
            block.text = normalize_arabic_forms(block.text);
            if clean {
                block.text = collapse_whitespace(&block.text);
            }
            if block.text.is_empty() {
                continue;
            }
            if budget.exhausted() {
                self.truncated = true;
                break;
            }
            let sep = if blocks.is_empty() { "" } else { "\n" };
            let piece = budget.take(format!("{sep}{}", block.text));
            block.text = piece.strip_prefix(sep).unwrap_or(&piece).to_string();
            self.truncated |= budget.truncated;
            if block.text.is_empty() {
                self.truncated = true;
                break;
            }
            self.text.push_str(&piece);
            block.reading_order = blocks.len();
            block.id = format!("p{}-b{}", self.page, blocks.len());
            blocks.push(block);
        }
        self.truncated |= blocks.len() < total && budget.exhausted();
        self.blocks = blocks;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_coordinates_respect_origin_crop_and_rotation() {
        let bounds = Rect::new(10.0, 20.0, 200.0, 300.0);
        let input = Rect::new(30.0, 60.0, 50.0, 70.0);
        for (rotation, expected) in [
            (0, Rect::new(20.0, 190.0, 50.0, 70.0)),
            (90, Rect::new(40.0, 20.0, 70.0, 50.0)),
            (180, Rect::new(130.0, 40.0, 50.0, 70.0)),
            (270, Rect::new(190.0, 130.0, 70.0, 50.0)),
        ] {
            let frame = PageFrame {
                bounds,
                rotation,
                width: 200.0,
                height: 300.0,
            };
            assert_eq!(frame.native_box(input), Some(expected));
            assert_eq!(
                frame.native_box(Rect::new(-100.0, -100.0, 10.0, 10.0)),
                None
            );
        }
        let frame = PageFrame {
            bounds,
            rotation: 90,
            width: 300.0,
            height: 200.0,
        };
        assert_eq!(
            frame.ocr_box(Rect::new(80.0, 40.0, 140.0, 100.0), 600, 400),
            frame.native_box(input)
        );
        assert_eq!(
            frame.ocr_box(Rect::new(-2.0, -2.0, 10.0, 10.0), 600, 400),
            Some(Rect::new(0.0, 0.0, 4.0, 4.0))
        );
    }

    #[test]
    fn confidence_is_never_fabricated() {
        for value in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
            assert!(
                Block::ocr("x".into(), Rect::default(), value)
                    .confidence
                    .is_none()
            );
        }
        assert_eq!(
            Block::ocr("x".into(), Rect::default(), 0.87).confidence,
            Some(0.87)
        );
    }

    #[test]
    fn mixed_blocks_deduplicate_and_budget_unicode_consistently() {
        let frame = PageFrame {
            bounds: Rect::default(),
            rotation: 0,
            width: 200.0,
            height: 300.0,
        };
        let mut page = DocumentPage::new(2, frame);
        let mut native = Block::ocr("Invoice 42".into(), Rect::new(0.0, 10.0, 100.0, 10.0), 0.9);
        native.source = BlockSource::Native;
        native.confidence = None;
        page.blocks.push(native);
        page.text = "Invoice 42".into();
        page.apply_ocr(
            vec![
                Block::ocr("INVOICE 42".into(), Rect::new(0.0, 10.0, 100.0, 10.0), 0.8),
                Block::ocr("தமிழ்".into(), Rect::new(0.0, 0.0, 100.0, 10.0), 0.7),
                Block::ocr("தமிழ்".into(), Rect::new(0.0, 0.0, 100.0, 10.0), 0.6),
            ],
            true,
        );
        let mut budget = TextBudget::new(8);
        page.finish(&mut budget, false);
        assert_eq!(page.text, "தமிழ்\nIn");
        assert_eq!(page.blocks[0].confidence, Some(0.7));
        assert_eq!(page.blocks[0].id, "p2-b0");
        assert_eq!(page.blocks[1].text, "In");
        assert!(page.truncated && budget.truncated);
    }
}
