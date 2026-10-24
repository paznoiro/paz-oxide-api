//! Golden per-language startup test: a tiny embedded image must recognize
//! to its known string. Catches a missing ONNX Runtime, a corrupt model, or
//! a dict/model mismatch before `/readyz` turns green.

use super::page;
use super::stack::OcrStack;
use crate::pdf::lang::Lang;

struct Golden {
    png: &'static [u8],
    expected: &'static str,
}

fn golden(lang: Lang) -> Golden {
    match lang {
        Lang::En => Golden { png: include_bytes!("fixtures/en.png"), expected: "Invoice 2026 total due" },
        Lang::Hi => Golden { png: include_bytes!("fixtures/hi.png"), expected: "भारत का संविधान" },
        Lang::Ar => Golden { png: include_bytes!("fixtures/ar.png"), expected: "مرحبا بالعالم" },
        Lang::Ta => Golden { png: include_bytes!("fixtures/ta.png"), expected: "தமிழ்நாடு அரசு" },
    }
}

/// Minimum normalized similarity between the recognized and expected text.
const MIN_SIMILARITY: f32 = 0.8;

pub fn run(stack: &OcrStack, lang: Lang) -> Result<(), String> {
    let g = golden(lang);
    let img = image::load_from_memory(g.png).map_err(|e| format!("fixture decode failed: {e}"))?;
    let out = page::ocr_image(stack, &img, lang)?;
    let sim = similarity(&out.text, g.expected);
    if sim >= MIN_SIMILARITY {
        Ok(())
    } else {
        Err(format!(
            "golden text mismatch (similarity {sim:.2} < {MIN_SIMILARITY}): got {:?}, expected {:?}",
            out.text, g.expected
        ))
    }
}

fn norm(s: &str) -> Vec<char> {
    s.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

/// 1 − (Levenshtein distance / longer length), whitespace-insensitive.
pub fn similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (norm(a), norm(b));
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    1.0 - prev[b.len()] as f32 / longest as f32
}

#[cfg(test)]
mod tests {
    use super::similarity;

    #[test]
    fn similarity_metric() {
        assert_eq!(similarity("Hello World", "hello  world"), 1.0);
        assert!(similarity("Invoice 2026 total due", "Invoice 2026 tota1 due") > 0.9);
        assert!(similarity("abc", "xyz") < 0.1);
    }
}
