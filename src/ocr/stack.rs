//! `OcrStack`: one shared `TextDetector` + one `TextRecognizer` per language.
//! Built once per OCR worker at startup, never per request.

use std::collections::BTreeMap;
use std::path::Path;

use pdf_oxide::extractors::auto::OcrLanguage;
use pdf_oxide::ocr::{OcrConfig, TextDetector, TextRecognizer};

use crate::pdf::lang::Lang;

pub struct OcrStack {
    pub detector: TextDetector,
    pub recognizers: BTreeMap<Lang, TextRecognizer>,
    pub config: OcrConfig,
}

pub const DETECTOR_FILE: &str = "det.onnx";

/// Model file names come from upstream's `OcrLanguage::spec()`, so the
/// loader and the fetch script can't drift.
pub fn model_files(lang: Lang) -> (String, String) {
    let ocr_lang = match lang {
        Lang::En => OcrLanguage::English,
        Lang::Hi => OcrLanguage::Devanagari,
        Lang::Ar => OcrLanguage::Arabic,
    };
    let spec = ocr_lang.spec();
    (spec.rec_file, spec.dict_file)
}

pub struct BuildReport {
    pub stack: Option<OcrStack>,
    /// Set when the detector (or ONNX Runtime itself) failed: no OCR at all.
    pub fatal: Option<String>,
    pub failed_langs: BTreeMap<Lang, String>,
    pub sessions: usize,
}

pub fn build(model_dir: &Path, langs: &[Lang], threads: usize) -> BuildReport {
    // Dense invoices set 6–8 pt table text: at the upstream default
    // MaxSide 960 an A4 scan downscales to ~0.27x and small glyphs drop
    // under ~9 px, below what the v4 detector reliably finds. 1500 keeps
    // the same strategy but holds small text (same tensor as a 200 DPI
    // render; ~2.4x pixels of 960).
    let config = OcrConfig {
        num_threads: threads,
        det_resize_strategy: pdf_oxide::ocr::DetResizeStrategy::MaxSide { max_side: 1500 },
        ..OcrConfig::default()
    };
    let detector = match TextDetector::new(model_dir.join(DETECTOR_FILE), config.clone()) {
        Ok(d) => d,
        Err(e) => {
            return BuildReport { stack: None, fatal: Some(format!("detector: {e}")), failed_langs: BTreeMap::new(), sessions: 0 };
        }
    };
    let mut sessions = 1;
    let mut recognizers = BTreeMap::new();
    let mut failed_langs = BTreeMap::new();
    for &lang in langs {
        let (rec, dict) = model_files(lang);
        match TextRecognizer::new(model_dir.join(&rec), model_dir.join(&dict), config.clone()) {
            Ok(r) => {
                sessions += 1;
                recognizers.insert(lang, r);
            }
            Err(e) => {
                failed_langs.insert(lang, format!("{rec}/{dict}: {e}"));
            }
        }
    }
    BuildReport { stack: Some(OcrStack { detector, recognizers, config }), fatal: None, failed_langs, sessions }
}
