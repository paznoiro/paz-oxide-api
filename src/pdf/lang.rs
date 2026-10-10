//! `lang=auto`: explicit param → legacy font → native-text script → probe → `en`.

use serde::Serialize;

use crate::text::ScriptCounts;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    En,
    Hi,
    Ar,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Hi, Lang::Ar];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Hi => "hi",
            Lang::Ar => "ar",
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "en" => Lang::En,
            "hi" => Lang::Hi,
            "ar" => Lang::Ar,
            _ => return None,
        })
    }

    /// Share of `counts` written in this language's own script.
    pub fn script_share(self, counts: &ScriptCounts) -> f32 {
        let total = counts.letters();
        if total == 0 {
            return 0.0;
        }
        let own = match self {
            Lang::En => counts.latin,
            Lang::Hi => counts.devanagari,
            Lang::Ar => counts.arabic,
        };
        own as f32 / total as f32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LangSource {
    Param,
    Font,
    Text,
    Probe,
    Default,
}

/// The `lang` request param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangParam {
    Auto,
    Fixed(Lang),
}

impl LangParam {
    pub fn parse(s: &str) -> Option<Self> {
        if s.trim().eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        Lang::from_code(s).map(Self::Fixed)
    }
}

/// Minimum letters of native text before the text layer is trusted to
/// decide the language; below this the document is treated as a pure scan.
const MIN_TEXT_LETTERS: usize = 32;

/// Steps 1–3 of language resolution (everything that needs no inference).
/// `None` means "probe the first OCR page".
pub fn resolve_static(param: LangParam, legacy_font: bool, native_text_counts: &ScriptCounts) -> Option<(Lang, LangSource)> {
    if let LangParam::Fixed(l) = param {
        return Some((l, LangSource::Param));
    }
    if legacy_font {
        return Some((Lang::Hi, LangSource::Font));
    }
    if let Some(l) = script_heuristic(native_text_counts) {
        return Some((l, LangSource::Text));
    }
    if native_text_counts.letters() >= MIN_TEXT_LETTERS {
        return Some((Lang::En, LangSource::Text));
    }
    None
}

/// Upstream's heuristic: Arabic or Devanagari wins when its count is ≥ 4
/// and at least half the Latin count.
pub fn script_heuristic(c: &ScriptCounts) -> Option<Lang> {
    let (n, lang) = if c.arabic >= c.devanagari { (c.arabic, Lang::Ar) } else { (c.devanagari, Lang::Hi) };
    (n >= 4 && n * 2 >= c.latin).then_some(lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic() {
        let c = ScriptCounts::of("مرحبا بالعالم and some english", 8000);
        assert_eq!(script_heuristic(&c), Some(Lang::Ar));
        let c = ScriptCounts::of("A long English paragraph with one word नमस्ते", 8000);
        assert_eq!(script_heuristic(&c), None);
        let c = ScriptCounts::of("भारत एक देश है", 8000);
        assert_eq!(script_heuristic(&c), Some(Lang::Hi));
    }

    #[test]
    fn resolution_order() {
        let empty = ScriptCounts::default();
        assert_eq!(resolve_static(LangParam::Fixed(Lang::Ar), true, &empty), Some((Lang::Ar, LangSource::Param)));
        assert_eq!(resolve_static(LangParam::Auto, true, &empty), Some((Lang::Hi, LangSource::Font)));
        assert_eq!(resolve_static(LangParam::Auto, false, &empty), None);
        let en = ScriptCounts::of(&"english words ".repeat(10), 8000);
        assert_eq!(resolve_static(LangParam::Auto, false, &en), Some((Lang::En, LangSource::Text)));
    }

    #[test]
    fn parse() {
        assert_eq!(LangParam::parse("AUTO"), Some(LangParam::Auto));
        assert_eq!(LangParam::parse("hi"), Some(LangParam::Fixed(Lang::Hi)));
        assert_eq!(LangParam::parse("fr"), None);
    }
}
