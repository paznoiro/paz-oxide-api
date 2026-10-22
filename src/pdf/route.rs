//! Page routing: `classify_page` + legacy-Hindi-font check → page plan.

use pdf_oxide::extractors::auto::PageKind;
use serde::Serialize;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OcrEngine {
    #[default]
    Paddle,
    Indra,
}

impl OcrEngine {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "paddle" => Some(Self::Paddle),
            "indra" => Some(Self::Indra),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paddle => "paddle",
            Self::Indra => "indra",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrMode {
    Auto,
    Force,
    Off,
}

impl OcrMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "force" => Self::Force,
            "off" => Self::Off,
            _ => return None,
        })
    }
}

/// Why a page is sent to OCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrReason {
    /// Image-dominated page, or a text layer too broken to use.
    Scanned,
    /// Native text plus text-bearing images: native + new OCR lines.
    Hybrid,
    /// Text set in a legacy (non-Unicode) Hindi font: the text layer is
    /// mojibake, so the rendered page is OCR'd with `hi`.
    LegacyFont,
    /// `ocr=force`.
    Forced,
    /// An image upload: OCR is the only way to its text.
    Image,
}

impl OcrReason {
    /// Hybrid pages keep their native text and gain OCR lines; every other
    /// reason replaces the native text with the OCR output.
    pub fn merges(self) -> bool {
        matches!(self, OcrReason::Hybrid)
    }

    /// Legacy-font pages carry vector glyphs, not a scan: render the page.
    pub fn renders(self) -> bool {
        matches!(self, OcrReason::LegacyFont)
    }
}

pub fn plan_page(mode: OcrMode, kind: Option<PageKind>, legacy_font: bool) -> Option<OcrReason> {
    match mode {
        OcrMode::Off => None,
        OcrMode::Force => Some(if legacy_font { OcrReason::LegacyFont } else { OcrReason::Forced }),
        OcrMode::Auto if legacy_font => Some(OcrReason::LegacyFont),
        OcrMode::Auto => match kind? {
            PageKind::Scanned => Some(OcrReason::Scanned),
            PageKind::ImageText | PageKind::Mixed => Some(OcrReason::Hybrid),
            _ => None, // TextLayer, Empty (and any future kinds)
        },
    }
}

/// Legacy (pre-Unicode) Devanagari font families whose glyphs are mapped
/// onto Latin code points: Kruti Dev, Chanakya, DevLys, Walkman-Chanakya,
/// Shusha, Agra, APS-DV.
pub fn is_legacy_hindi_font(face: &str) -> bool {
    let lower = face.to_ascii_lowercase();
    let squashed: String = lower.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    const CONTAINS: [&str; 5] = ["krutidev", "chanakya", "devlys", "shusha", "apsdv"];
    if CONTAINS.iter().any(|f| squashed.contains(f)) {
        return true;
    }
    // "Agra" is a short word: match it only as the leading token
    // ("Agra", "Agra-Bold", "AgraBold"), never inside e.g. "Agrandir".
    let first_token: String = lower.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    match first_token.strip_prefix("agra") {
        Some(rest) => matches!(rest, "" | "bold" | "italic" | "bolditalic" | "regular" | "normal" | "mt"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_fonts() {
        for f in ["Kruti Dev 010", "KrutiDev010Bold", "Walkman-Chanakya905", "DevLys 010", "Shusha02", "Agra", "Agra-Bold", "APS-DV-Priyanka"] {
            assert!(is_legacy_hindi_font(f), "{f}");
        }
        for f in ["Arial", "NotoSansDevanagari", "Mangal", "Agrandir-Regular", "TimesNewRomanPSMT"] {
            assert!(!is_legacy_hindi_font(f), "{f}");
        }
    }

    #[test]
    fn routing() {
        assert_eq!(plan_page(OcrMode::Auto, Some(PageKind::TextLayer), false), None);
        assert_eq!(plan_page(OcrMode::Auto, Some(PageKind::Scanned), false), Some(OcrReason::Scanned));
        assert_eq!(plan_page(OcrMode::Auto, Some(PageKind::Mixed), false), Some(OcrReason::Hybrid));
        assert_eq!(plan_page(OcrMode::Auto, Some(PageKind::TextLayer), true), Some(OcrReason::LegacyFont));
        assert_eq!(plan_page(OcrMode::Force, Some(PageKind::TextLayer), false), Some(OcrReason::Forced));
        assert_eq!(plan_page(OcrMode::Off, Some(PageKind::Scanned), true), None);
        assert_eq!(plan_page(OcrMode::Auto, None, false), None);
    }
}
