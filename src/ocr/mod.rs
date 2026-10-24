//! OCR subsystem (`ocr` feature): ONNX Runtime via `pdf_oxide`'s detector
//! and recognizers, composed as one detector + `en`/`hi`/`ar`/`ta` recognizers
//! per worker.

pub mod page;
pub mod indra;
pub mod pool;
pub mod smoke;
pub mod stack;

pub use pool::{LangPlan, OcrJob, OcrPageResult, OcrService, OcrSettings};
