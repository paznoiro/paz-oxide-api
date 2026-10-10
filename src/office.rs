//! Office extraction, fully in memory. DOCX/XLSX/PPTX/DOC/XLS/PPT go through
//! `office_oxide` (which parses on its own 16 MB-stack thread); OpenDocument
//! goes through `crate::odf`, which builds the same IR so both render alike.
//! Runs on the CPU pool.

use std::io::Cursor;

use bytes::Bytes;
use office_oxide::ir::{DocumentIR, Element};
use office_oxide::ir_render::MarkdownOptions;
use office_oxide::{Document, DocumentFormat};
use serde::Serialize;
use serde_json::Value;

use crate::detect::FormatSource;
use crate::error::ApiError;
use crate::odf;
use crate::pdf::Mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeFormat {
    Docx,
    Xlsx,
    Pptx,
    Doc,
    Xls,
    Ppt,
    Odt,
    Ods,
    Odp,
}

/// What a document is made of, whatever its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Text,
    Sheets,
    Slides,
}

impl OfficeFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Doc => "doc",
            Self::Xls => "xls",
            Self::Ppt => "ppt",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
        }
    }

    /// Canonical names plus the template and macro-enabled siblings that
    /// share each container (`dotx`, `xlsm`, `ott`, ...).
    pub fn from_extension(ext: &str) -> Option<Self> {
        if let Some(f) = DocumentFormat::from_extension(ext) {
            return Some(Self::from_engine(f));
        }
        Some(match ext.to_ascii_lowercase().as_str() {
            "odt" | "ott" => Self::Odt,
            "ods" | "ots" => Self::Ods,
            "odp" | "otp" => Self::Odp,
            _ => return None,
        })
    }

    fn from_engine(f: DocumentFormat) -> Self {
        match f {
            DocumentFormat::Docx => Self::Docx,
            DocumentFormat::Xlsx => Self::Xlsx,
            DocumentFormat::Pptx => Self::Pptx,
            DocumentFormat::Doc => Self::Doc,
            DocumentFormat::Xls => Self::Xls,
            DocumentFormat::Ppt => Self::Ppt,
        }
    }

    /// The `office_oxide` reader for this format; `None` for OpenDocument.
    fn engine(self) -> Option<DocumentFormat> {
        Some(match self {
            Self::Docx => DocumentFormat::Docx,
            Self::Xlsx => DocumentFormat::Xlsx,
            Self::Pptx => DocumentFormat::Pptx,
            Self::Doc => DocumentFormat::Doc,
            Self::Xls => DocumentFormat::Xls,
            Self::Ppt => DocumentFormat::Ppt,
            Self::Odt | Self::Ods | Self::Odp => return None,
        })
    }

    pub fn is_odf(self) -> bool {
        self.engine().is_none()
    }

    pub fn family(self) -> Family {
        match self {
            Self::Docx | Self::Doc | Self::Odt => Family::Text,
            Self::Xlsx | Self::Xls | Self::Ods => Family::Sheets,
            Self::Pptx | Self::Ppt | Self::Odp => Family::Slides,
        }
    }
}

fn open(bytes: Bytes, format: DocumentFormat) -> Result<Document, ApiError> {
    Document::from_reader(Cursor::new(bytes), format).map_err(|e| ApiError::from_office(&e))
}

/// The document's text, and whether the reader stopped before its end
/// (OpenDocument only; `office_oxide` enforces the same budget silently).
pub fn extract(bytes: Bytes, format: OfficeFormat, mode: Mode, max_chars: usize) -> Result<(String, bool), ApiError> {
    let Some(engine) = format.engine() else {
        let ir = odf::read(bytes, max_chars)?;
        let text = match mode {
            Mode::Text => ir.plain_text(),
            Mode::Markdown { .. } => ir.to_markdown_with(MarkdownOptions::default()),
        };
        return Ok((text, ir.metadata.text_truncated));
    };
    let doc = open(bytes, engine)?;
    Ok((
        match mode {
            Mode::Text => doc.plain_text(),
            Mode::Markdown { .. } => doc.to_markdown_with(MarkdownOptions::default()),
        },
        false,
    ))
}

#[derive(Serialize)]
pub struct OfficeInfo {
    pub kind: &'static str,
    pub format: &'static str,
    pub format_source: FormatSource,
    pub metadata: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheets: Option<Vec<SheetInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slide_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_count: Option<usize>,
}

#[derive(Serialize)]
pub struct SheetInfo {
    pub name: String,
    pub rows: usize,
    pub cols: usize,
}

pub fn info(bytes: Bytes, format: OfficeFormat, source: FormatSource, max_chars: usize) -> Result<OfficeInfo, ApiError> {
    let ir = match format.engine() {
        Some(engine) => open(bytes, engine)?.to_ir(),
        None => odf::read(bytes, max_chars)?,
    };
    Ok(build_info(&ir, format, source))
}

fn build_info(ir: &DocumentIR, format: OfficeFormat, source: FormatSource) -> OfficeInfo {
    let mut metadata = serde_json::to_value(&ir.metadata).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut metadata {
        m.remove("format"); // echoed at the top level
    }
    let (mut sheets, mut slide_count, mut section_count) = (None, None, None);
    match format.family() {
        Family::Sheets => {
            sheets = Some(
                ir.sections
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let (rows, cols) = s
                            .elements
                            .iter()
                            .find_map(|e| match e {
                                Element::Table(t) => {
                                    Some((t.rows.len(), t.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0)))
                                }
                                _ => None,
                            })
                            .unwrap_or((0, 0));
                        SheetInfo { name: s.title.clone().unwrap_or_else(|| format!("Sheet{}", i + 1)), rows, cols }
                    })
                    .collect(),
            );
        }
        Family::Slides => slide_count = Some(ir.sections.len()),
        Family::Text => section_count = Some(ir.sections.len()),
    }
    OfficeInfo { kind: "office", format: format.extension(), format_source: source, metadata, sheets, slide_count, section_count }
}
