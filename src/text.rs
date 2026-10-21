//! Arabic presentation-form normalization, script counting, char budget.

use unicode_normalization::UnicodeNormalization;

fn is_arabic_presentation_form(c: char) -> bool {
    matches!(c, '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}')
}

/// NFKC applied only to Arabic Presentation Forms (A: U+FB50–U+FDFF,
/// B: U+FE70–U+FEFF), so glyph-shaped letters come out as base letters.
/// Nothing else is rewritten. (U+FEFF, the BOM, is dropped.)
pub fn normalize_arabic_forms(s: String) -> String {
    if !s.chars().any(is_arabic_presentation_form) {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\u{FEFF}' {
            continue;
        }
        if is_arabic_presentation_form(c) {
            out.extend(std::iter::once(c).nfkc());
        } else {
            out.push(c);
        }
    }
    out
}

/// Restores logical order to RTL lines that the geometric plain-text
/// pipeline emits in visual order, using `reference` — the same page
/// through `extract_text`, whose per-run x-verdicts keep it RTL-correct —
/// as the oracle.
///
/// A line is only changed when its corrected form is fully attested by
/// reference lines, so LTR pages, already-correct lines, and unattested
/// lines come back byte-identical. The geometric path may also group
/// spans differently (two reference lines on one output line), so the
/// corrected line may match several reference lines pieced together —
/// in which case their order is restored to the output line's own.
/// Pure-RTL lines use the same number-preserving reversal the reference
/// path uses; mixed lines (Latin labels beside RTL values) go through
/// the full UAX #9 reorder.
pub fn restore_rtl_lines(text: &str, reference: &str) -> String {
    use pdf_oxide::text::bidi::{looks_rtl, reorder_visual_to_logical, reverse_rtl_keep_numbers};
    use std::collections::HashSet;
    if !looks_rtl(text) {
        return text.to_string();
    }
    let key = |l: &str| l.split_whitespace().collect::<Vec<_>>().join(" ");
    let known: HashSet<String> = reference.lines().map(&key).collect();
    // Split a corrected line into reference lines, longest pieces first.
    // Returns the pieces in the corrected line's order, if fully covered.
    let partition = |corrected: &str| -> Option<Vec<String>> {
        let words: Vec<&str> = corrected.split_whitespace().collect();
        let n = words.len();
        if n == 0 {
            return None;
        }
        let mut parent: Vec<Option<usize>> = vec![None; n + 1];
        parent[0] = Some(0);
        for j in 1..=n {
            for i in (0..j).rev() {
                if parent[i].is_some() && known.contains(words[i..j].join(" ").as_str()) {
                    parent[j] = Some(i);
                    break;
                }
            }
        }
        parent[n]?;
        let mut pieces = Vec::new();
        let mut j = n;
        while j > 0 {
            let i = parent[j].unwrap();
            pieces.push(words[i..j].join(" "));
            j = i;
        }
        pieces.reverse();
        Some(pieces)
    };
    let mut out = String::with_capacity(text.len());
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if looks_rtl(line) && !known.contains(key(line).as_str()) {
            let corrected = if line.chars().any(|c| c.is_ascii_alphabetic()) {
                reorder_visual_to_logical(line)
            } else {
                reverse_rtl_keep_numbers(line)
            };
            if let Some(mut pieces) = partition(&corrected) {
                // The correction mirrors the output line, so its pieces run
                // backwards against the output order: flip them back.
                pieces.reverse();
                out.push_str(&pieces.join(" "));
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScriptCounts {
    pub latin: usize,
    pub arabic: usize,
    pub devanagari: usize,
}

impl ScriptCounts {
    pub fn of(s: &str, limit: usize) -> Self {
        let mut c = Self::default();
        for ch in s.chars().filter(|c| !c.is_whitespace()).take(limit) {
            c.add(ch);
        }
        c
    }

    pub fn add(&mut self, ch: char) {
        match ch {
            'A'..='Z' | 'a'..='z' | '\u{00C0}'..='\u{024F}' => self.latin += 1,
            '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFE}' => {
                self.arabic += 1
            }
            '\u{0900}'..='\u{097F}' => self.devanagari += 1,
            _ => {}
        }
    }

    pub fn letters(&self) -> usize {
        self.latin + self.arabic + self.devanagari
    }
}

/// Whether a scanner's OCR text layer is garbage: the scanner OCR'd
/// handwriting or maths and kept what it guessed ("IxO'C't. oA cxAtl").
/// pdf_oxide's quality gate only catches unmapped glyphs (U+FFFD, private
/// use); this is printable Latin and passes it. Judged on words of 3+
/// letters, shorter tokens proving nothing either way: real words are
/// lower, Title or UPPER case and of one script; junk mixes case, script
/// and symbols inside a word. `None` when there are too few words to judge.
pub fn ocr_layer_is_junk(text: &str) -> Option<bool> {
    const MIN_WORDS: usize = 6;
    let (mut words, mut good) = (0usize, 0usize);
    for tok in text.split_whitespace() {
        if tok.chars().filter(|c| c.is_alphabetic()).count() < 3 {
            continue;
        }
        words += 1;
        good += usize::from(is_well_formed_word(tok));
    }
    // Real text scores 0.8+, a scanner's handwriting OCR under 0.3.
    (words >= MIN_WORDS).then(|| good * 2 < words)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Devanagari,
    Arabic,
    Other,
}

fn script_of(c: char) -> Option<Script> {
    match c {
        '\u{0900}'..='\u{097F}' => Some(Script::Devanagari),
        '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFE}' => {
            Some(Script::Arabic)
        }
        // Combining accents (decomposed é) belong to the Latin word.
        '\u{0300}'..='\u{036F}' | '\u{1E00}'..='\u{1EFF}' => Some(Script::Latin),
        c if c.is_alphabetic() => Some(if (c as u32) < 0x250 { Script::Latin } else { Script::Other }),
        _ => None,
    }
}

fn is_well_formed_word(tok: &str) -> bool {
    const EDGE: &[char] = &['(', ')', '[', ']', '"', '\'', '.', ',', ';', ':', '!', '?', '“', '”', '‘', '’'];
    let word = tok.trim_matches(EDGE);
    !word.is_empty() && word.split(['-', '\'', '’']).all(is_well_formed_part)
}

fn is_well_formed_part(part: &str) -> bool {
    // ZWJ/ZWNJ shape Devanagari and Arabic; they're not symbols.
    let letters: Vec<char> = part.chars().filter(|c| !matches!(c, '\u{200C}' | '\u{200D}')).collect();
    let digits = letters.iter().take_while(|c| c.is_ascii_digit()).count();
    let rest = &letters[digits..];
    if rest.is_empty() {
        return digits > 0; // "19" in "COVID-19"
    }
    let Some(script) = script_of(rest[0]) else { return false };
    if !rest.iter().all(|&c| script_of(c) == Some(script)) {
        return false; // a symbol, digit or second script inside the word
    }
    let no_upper = |cs: &[char]| cs.iter().all(|c| !c.is_uppercase());
    let no_lower = |cs: &[char]| cs.iter().all(|c| !c.is_lowercase());
    if digits > 0 {
        return no_upper(rest); // "3rd", "1990s"
    }
    // Only Latin has case here: lower, UPPER or Title.
    script != Script::Latin || no_upper(rest) || no_lower(rest) || no_upper(&rest[1..])
}

/// Collects document text under a per-document character budget.
pub struct TextBudget {
    remaining: usize,
    pub truncated: bool,
}

impl TextBudget {
    pub fn new(max_chars: usize) -> Self {
        Self { remaining: max_chars, truncated: false }
    }

    pub fn exhausted(&self) -> bool {
        self.truncated || self.remaining == 0
    }

    /// Charges `s` against the budget, truncating it at a char boundary if
    /// it doesn't fit. Returns the (possibly shortened) text.
    pub fn take(&mut self, mut s: String) -> String {
        if self.truncated {
            return String::new();
        }
        let n = s.chars().count();
        if n <= self.remaining {
            self.remaining -= n;
            return s;
        }
        let cut = s.char_indices().nth(self.remaining).map_or(s.len(), |(i, _)| i);
        s.truncate(cut);
        self.remaining = 0;
        self.truncated = true;
        s
    }
}

/// Collapses extra whitespace for `clean=true`: runs of horizontal
/// whitespace (spaces, tabs and other Unicode spaces, never newlines)
/// become a single ASCII space, trailing whitespace on each line is
/// removed, blank lines become truly empty, 3+ newlines collapse to a
/// single blank line, and leading/trailing blank lines are trimmed.
/// Single leading spaces are kept (so indented lines stay indented),
/// except on the first/last line where the final trim removes them.
pub fn collapse_whitespace(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = Vec::new();
    for line in s.split('\n') {
        let mut out = String::with_capacity(line.len());
        let mut in_ws = false;
        for c in line.chars() {
            if c.is_whitespace() {
                if !in_ws {
                    out.push(' ');
                    in_ws = true;
                }
            } else {
                out.push(c);
                in_ws = false;
            }
        }
        let trimmed = out.trim_end();
        if trimmed.trim().is_empty() {
            lines.push(String::new());
        } else {
            lines.push(trimmed.to_string());
        }
    }
    let joined = lines.join("\n");
    // Cap blank runs at one blank line (two newlines).
    let mut out = String::with_capacity(joined.len());
    let mut newlines = 0usize;
    for c in joined.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(c);
            }
        } else {
            newlines = 0;
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Upstream's hybrid merge rule, reimplemented: keep the native text and
/// append OCR lines not already contained in it (whitespace-normalized,
/// case-insensitive), de-duplicated within the OCR block itself.
pub fn merge_native_and_ocr(native: &str, ocr: &str) -> String {
    let native_trimmed = native.trim_end();
    if ocr.trim().is_empty() {
        return native.to_string();
    }
    if native_trimmed.trim().is_empty() {
        return ocr.to_string();
    }
    let extra = extra_ocr_lines(native_trimmed, ocr);
    if extra.is_empty() {
        return native.to_string();
    }
    format!("{native_trimmed}\n{}", extra.join("\n"))
}

/// OCR additions shared by text assembly and structured block merging.
pub fn extra_ocr_lines<'a>(native: &str, ocr: &'a str) -> Vec<&'a str> {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let native_norm = norm(native);
    let mut extra: Vec<&str> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in ocr.lines() {
        let lt = line.trim();
        let ln = norm(lt);
        if ln.is_empty() || native_norm.contains(&ln) || seen.contains(&ln) {
            continue;
        }
        seen.push(ln);
        extra.push(lt);
    }
    extra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtl_lines_restored_only_with_oracle_support() {
        let reference = "مرحبا بالعالم\nPure LTR";
        // Visual-order line, as the geometric pipeline emits it.
        let visual = "ملاعلاب ابحرم\nPure LTR";
        assert_eq!(restore_rtl_lines(visual, reference), reference);
        // Already-logical lines are in the reference: untouched.
        assert_eq!(restore_rtl_lines(reference, reference), reference);
        // No RTL at all: fast path, no allocation surprises.
        assert_eq!(restore_rtl_lines("Hello", reference), "Hello");
        // Oracle has no support: keep, never guess.
        assert_eq!(restore_rtl_lines(visual, "unrelated"), visual);
        // The geometric path may group two reference lines onto one output
        // line: the correction pieces them back apart, in output order.
        let merged_visual = "ةيبرع ةقيثو ملاعلاب ابحرم";
        assert_eq!(
            restore_rtl_lines(merged_visual, "وثيقة عربية\nمرحبا بالعالم"),
            "وثيقة عربية مرحبا بالعالم"
        );
        // Mixed line present in the reference: untouched.
        let mixed = "Total 2024 الإجمالي";
        assert_eq!(restore_rtl_lines(mixed, mixed), mixed);
    }

    #[test]
    fn arabic_presentation_forms_become_base_letters() {
        // "سلام" written with presentation forms B (initial/medial/final glyphs).
        let shaped = "\u{FEB3}\u{FEE0}\u{FE8E}\u{FEE1}".to_string();
        assert_eq!(normalize_arabic_forms(shaped), "سلام");
        // Lam-alef ligature decomposes into two letters.
        assert_eq!(normalize_arabic_forms("\u{FEFB}".into()), "لا");
        // Everything else untouched (including other compatibility chars).
        assert_eq!(normalize_arabic_forms("ﬁ x²".into()), "ﬁ x²");
    }

    #[test]
    fn budget_truncates_at_char_boundary() {
        let mut b = TextBudget::new(5);
        assert_eq!(b.take("नमस".into()), "नमस");
        assert_eq!(b.take("abcdef".into()), "ab");
        assert!(b.truncated);
        assert_eq!(b.take("more".into()), "");
    }

    #[test]
    fn scripts() {
        let c = ScriptCounts::of("Hello مرحبا नमस्ते", 8000);
        assert_eq!(c.latin, 5);
        assert_eq!(c.arabic, 5);
        assert!(c.devanagari >= 5);
    }

    #[test]
    fn junk_ocr_layer() {
        // A scanner's OCR of handwritten lecture notes.
        let junk = "IxO'C't. oA cxAtl Voraion.\ninoNie, (le.r'A^cc:^ Wt. rre.n^V\n\
                    r oyAt. oo^ ^ vVs (AeA\\J<xV\\vje. -HjacWoAI)\n(2or\\)<xVur€. cx.aA 'Vo^'S^oHj";
        assert_eq!(ocr_layer_is_junk(junk), Some(true));
        for real in [
            "We have now derived the Frenet frame and its derivative functions, curvature and torsion.",
            "INVOICE No. 42 — Total due: 1,250.00 USD (net 30 days). Thank you for your business!",
            "The COVID-19 report for the 3rd quarter of the 1990s, O'Brien's well-known McDonald analysis.",
            "Über die Größe der Straße: Ça coûte très cher, n'est-ce pas? El niño está aquí mañana.",
            "यह एक परीक्षण दस्तावेज़ है जिसमें हिंदी पाठ लिखा गया है और यह पढ़ने योग्य है",
            "هذا مستند اختبار يحتوي على نص عربي مكتوب بشكل صحيح ويمكن قراءته بسهولة",
        ] {
            assert_eq!(ocr_layer_is_junk(real), Some(false), "{real}");
        }
        // Too few words of 3+ letters to say either way.
        assert_eq!(ocr_layer_is_junk("M^Ver some, , uot Ae,V 3 i"), None);
        assert_eq!(ocr_layer_is_junk(""), None);
    }

    #[test]
    fn hybrid_merge() {
        let native = "Invoice 42\nTotal due: 100";
        let ocr = "INVOICE 42\nStamp: PAID\nstamp:   paid\n";
        assert_eq!(merge_native_and_ocr(native, ocr), "Invoice 42\nTotal due: 100\nStamp: PAID");
        assert_eq!(merge_native_and_ocr("", "a"), "a");
        assert_eq!(merge_native_and_ocr("a", " "), "a");
    }

    #[test]
    fn whitespace_collapse() {
        assert_eq!(collapse_whitespace("Hello    World"), "Hello World");
        assert_eq!(collapse_whitespace("Hello\t\tWorld"), "Hello World");
        assert_eq!(collapse_whitespace("Hello   \n   World"), "Hello\n World");
        assert_eq!(collapse_whitespace("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_whitespace("a\n   \n\n   \nb"), "a\n\nb");
        assert_eq!(collapse_whitespace("\n\n  Hello  \n\n"), "Hello");
        assert_eq!(collapse_whitespace("line with trailing   "), "line with trailing");
        assert_eq!(collapse_whitespace("a\r\n\r\n\r\nb"), "a\n\nb");
        assert_eq!(collapse_whitespace("a\rb"), "a\nb");
        assert_eq!(collapse_whitespace(""), "");
        assert_eq!(collapse_whitespace("   \n  \n "), "");
        // Unicode spaces collapse to a single ASCII space.
        assert_eq!(collapse_whitespace("Hello\u{00A0}\u{00A0}World"), "Hello World");
        // Already-clean text is unchanged.
        assert_eq!(collapse_whitespace("a\n\nb\nc"), "a\n\nb\nc");
    }
}
