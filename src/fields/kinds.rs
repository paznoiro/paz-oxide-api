//! A field's `type`: what its value must be, checked, and written one way.
//!
//! A typed field's match either reads as that type or is not a match: the
//! next match, then the next pattern, is tried, as for one past `max_length`.
//! The value is written in one form whatever the print: dates `YYYY-MM-DD`,
//! amounts a plain decimal, identifiers upper-case without spaces. Nothing is
//! corrected: an `O` read for a `0` fails the check rather than becoming one.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::invalid;
use crate::error::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The capture as read, after `normalization`.
    Text,
    Date(DateOrder),
    Amount(DecimalMark),
    /// Indian GST number: 15 characters, the last a check character.
    Gstin,
    /// Indian Permanent Account Number: `AAAAA9999A`.
    Pan,
    /// International Bank Account Number, mod-97 checked.
    Iban,
}

/// How an all-numeric date is read; `2026-10-14` (four digits first) is
/// always year-month-day, and a month written as a name is never in doubt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOrder {
    Dmy,
    Mdy,
    Ymd,
}

/// The decimal mark: `1,250.00` (`point`) or `1.250,00` (`comma`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalMark {
    Point,
    Comma,
}

/// Longest capture a typed field reads; anything longer is not one value.
const MAX_TYPED_CHARS: usize = 64;

/// Reads `type` and `format` from a spec.
pub(super) fn parse(type_: Option<&Value>, format: Option<&Value>, at: &dyn Fn(&str) -> String) -> Result<Kind, ApiError> {
    let name = match type_ {
        None | Some(Value::Null) => "text",
        Some(Value::String(s)) => s.as_str(),
        Some(v) => return Err(bad_type(v, at)),
    };
    let format = match format {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(v) => return Err(invalid(&at("format"), format!("invalid value {v} for `{}`", at("format")), "a string")),
    };
    let bad_format = |accepted: &str| {
        invalid(
            &at("format"),
            format!("invalid value \"{}\" for `{}` of a {name} field", format.unwrap_or_default(), at("format")),
            accepted,
        )
    };
    let kind = match name {
        "date" => Kind::Date(match format {
            None | Some("dmy") => DateOrder::Dmy,
            Some("mdy") => DateOrder::Mdy,
            Some("ymd") => DateOrder::Ymd,
            Some(_) => return Err(bad_format("dmy, mdy, ymd")),
        }),
        "amount" => Kind::Amount(match format {
            None | Some("point") => DecimalMark::Point,
            Some("comma") => DecimalMark::Comma,
            Some(_) => return Err(bad_format("point, comma")),
        }),
        "text" | "gstin" | "pan" | "iban" => {
            if format.is_some() {
                return Err(invalid(
                    &at("format"),
                    format!("`{}` applies to date and amount fields, not {name}", at("format")),
                    "no format for this type",
                ));
            }
            match name {
                "gstin" => Kind::Gstin,
                "pan" => Kind::Pan,
                "iban" => Kind::Iban,
                _ => Kind::Text,
            }
        }
        _ => return Err(bad_type(&Value::String(name.to_string()), at)),
    };
    Ok(kind)
}

fn bad_type(v: &Value, at: &dyn Fn(&str) -> String) -> ApiError {
    invalid(&at("type"), format!("invalid value {v} for `{}`", at("type")), "text, date, amount, gstin, pan, iban")
}

impl Kind {
    /// The value a trimmed, non-blank capture stands for, or `None` when it is
    /// not one of this type.
    pub fn read(self, raw: &str) -> Option<String> {
        if raw.chars().count() > MAX_TYPED_CHARS {
            return None;
        }
        match self {
            Kind::Text => Some(raw.to_string()),
            Kind::Date(order) => date(raw, order),
            Kind::Amount(mark) => amount(raw, mark),
            Kind::Gstin => gstin(raw),
            Kind::Pan => pan(raw),
            Kind::Iban => iban(raw),
        }
    }
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december",
];
const WEEKDAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

enum Tok<'a> {
    Num(&'a str),
    Word(&'a str),
}

/// `14/10/2026`, `14-Oct-2026`, `14th October, 2026`, `Oct 14, 2026`,
/// `Wed, 14 Oct 2026`, `2026-10-14`, `14.10.26` → `2026-10-14`. English month
/// names, whole or cut to three letters or more. A two-digit year is 2000-2069
/// below 70, else 1900s. A day the month does not have is not a date.
fn date(raw: &str, order: DateOrder) -> Option<String> {
    let mut toks = Vec::with_capacity(4);
    let mut rest = raw;
    while let Some(c) = rest.chars().next() {
        let run = |pred: fn(&char) -> bool| rest.find(|c: char| !pred(&c)).unwrap_or(rest.len());
        if c.is_ascii_digit() {
            let end = run(char::is_ascii_digit);
            toks.push(Tok::Num(&rest[..end]));
            rest = &rest[end..];
        } else if c.is_ascii_alphabetic() {
            let end = run(char::is_ascii_alphabetic);
            let word = &rest[..end];
            let ordinal = matches!(toks.last(), Some(Tok::Num(_)))
                && ["st", "nd", "rd", "th"].iter().any(|o| word.eq_ignore_ascii_case(o));
            let filler = word.eq_ignore_ascii_case("of") || (toks.is_empty() && weekday(word));
            if !ordinal && !filler {
                toks.push(Tok::Word(word));
            }
            rest = &rest[end..];
        } else if c.is_whitespace() || matches!(c, '-' | '/' | '.' | ',' | '\'') {
            rest = &rest[c.len_utf8()..];
        } else {
            return None;
        }
        if toks.len() > 3 {
            return None;
        }
    }
    let (y, m, d) = match toks.as_slice() {
        [Tok::Num(a), Tok::Num(b), Tok::Num(c)] => {
            let (y, m, d) = match order {
                _ if a.len() == 4 => (a, b, c),
                DateOrder::Dmy => (c, b, a),
                DateOrder::Mdy => (c, a, b),
                DateOrder::Ymd => (a, b, c),
            };
            (year(y)?, small(m)?, small(d)?)
        }
        [Tok::Num(a), Tok::Word(m), Tok::Num(b)] if a.len() == 4 => (year(a)?, month(m)?, small(b)?),
        [Tok::Num(d), Tok::Word(m), Tok::Num(y)] => (year(y)?, month(m)?, small(d)?),
        [Tok::Word(m), Tok::Num(d), Tok::Num(y)] => (year(y)?, month(m)?, small(d)?),
        _ => return None,
    };
    ((1..=12).contains(&m) && (1..=days_in(m, y)).contains(&d)).then(|| format!("{y:04}-{m:02}-{d:02}"))
}

fn year(s: &str) -> Option<u32> {
    let n: u32 = s.parse().ok()?;
    match s.len() {
        2 => Some(if n < 70 { 2000 + n } else { 1900 + n }),
        4 if n >= 1000 => Some(n),
        _ => None,
    }
}

/// A day or a month: one or two digits.
fn small(s: &str) -> Option<u32> {
    if s.len() > 2 {
        return None;
    }
    s.parse().ok()
}

fn month(word: &str) -> Option<u32> {
    let w = word.to_ascii_lowercase();
    if w.len() < 3 {
        return None;
    }
    MONTHS.iter().position(|m| m.starts_with(&w)).map(|i| i as u32 + 1)
}

fn weekday(word: &str) -> bool {
    let w = word.to_ascii_lowercase();
    w.len() >= 3 && WEEKDAYS.iter().any(|d| d.starts_with(&w))
}

fn days_in(month: u32, year: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// ---------------------------------------------------------------------------
// Amounts
// ---------------------------------------------------------------------------

/// An optional sign and currency (`-`, `Rs.`, `₹`, `USD`, `US$`) before the
/// number; after it, `/-` (Indian "no paise"), a currency (`AED`, `€`), or a
/// trailing minus. A currency after the number is three letters, so a letter
/// misread for a last digit (`1,250.0O`) is not taken for one.
static AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?<s1>[-−+])?\s*(?:[A-Za-z]{2,3}\.?)?\s*\p{Sc}?\s*(?<s2>[-−+])?\s*",
        r"(?<num>\d(?:[\d.,'\x{A0}\x{202F} ]*\d)?)",
        r"\s*(?:/-)?\s*(?:[A-Za-z]{3}|\p{Sc})?\s*(?<s3>[-−])?$",
    ))
    .expect("amount pattern compiles")
});

/// `1,250.00`, `₹ 1,25,000.50`, `USD 1,250`, `(1,250.00)`, `1 250,00 €` →
/// `1250.00`, `125000.50`, `1250`, `-1250.00`, `1250.00`. Digits are grouped
/// in threes, or Indian style (`1,25,000`), with one kind of separator; the
/// decimal digits are kept as printed, never rounded.
fn amount(raw: &str, mark: DecimalMark) -> Option<String> {
    let (inner, paren) = match raw.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        Some(inner) => (inner.trim(), true),
        None => (raw, false),
    };
    let caps = AMOUNT.captures(inner)?;
    let signs: Vec<&str> = ["s1", "s2", "s3"].iter().filter_map(|s| caps.name(s).map(|m| m.as_str())).collect();
    if signs.len() + usize::from(paren) > 1 {
        return None;
    }
    let negative = paren || signs.first().is_some_and(|s| *s != "+");
    let dec = match mark {
        DecimalMark::Point => '.',
        DecimalMark::Comma => ',',
    };
    let num = &caps["num"];
    let (int, frac) = match num.rfind(dec) {
        Some(i) => (&num[..i], Some(&num[i + 1..])),
        None => (num, None),
    };
    if frac.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let digits = grouped(int, dec)?;
    let digits = digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let sign = if negative { "-" } else { "" };
    Some(match frac {
        Some(f) => format!("{sign}{digits}.{f}"),
        None => format!("{sign}{digits}"),
    })
}

/// The digits of a whole part, when it is grouped the way amounts are.
fn grouped(int: &str, dec: char) -> Option<String> {
    let mut seps = int.chars().filter(|c| !c.is_ascii_digit());
    let Some(sep) = seps.next() else {
        return Some(int.to_string());
    };
    if sep == dec || seps.any(|c| c != sep) {
        return None;
    }
    let groups: Vec<&str> = int.split(sep).collect();
    let (first, rest) = groups.split_first()?;
    let (last, middle) = rest.split_last()?;
    let thousands = (1..=3).contains(&first.len()) && rest.iter().all(|g| g.len() == 3);
    let lakhs = (1..=2).contains(&first.len()) && last.len() == 3 && middle.iter().all(|g| g.len() == 2);
    (thousands || lakhs).then(|| groups.concat())
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Upper-case, with the spaces and hyphens identifiers are printed with removed.
fn compact(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect()
}

const BASE36: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn base36(c: u8) -> Option<u32> {
    BASE36.iter().position(|&b| b == c).map(|i| i as u32)
}

/// Two digits of state code, ten of PAN (or TAN), an entity number, a letter
/// (usually `Z`), then a check character: Luhn mod 36 over the first fourteen.
fn gstin(raw: &str) -> Option<String> {
    let id = compact(raw);
    let b = id.as_bytes();
    if b.len() != 15 || !b[..2].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut sum = 0;
    for (i, &c) in b[..14].iter().enumerate() {
        let p = base36(c)? * if i % 2 == 0 { 1 } else { 2 };
        sum += p / 36 + p % 36;
    }
    let check = BASE36[((36 - sum % 36) % 36) as usize];
    (base36(b[14]).is_some() && b[14] == check).then_some(id)
}

/// Five letters, four digits, a letter. Its check letter's rule is not
/// published, so the shape is all that is checked.
fn pan(raw: &str) -> Option<String> {
    let id = compact(raw);
    let b = id.as_bytes();
    let shape = b.len() == 10
        && b[..5].iter().all(u8::is_ascii_uppercase)
        && b[5..9].iter().all(u8::is_ascii_digit)
        && b[9].is_ascii_uppercase();
    shape.then_some(id)
}

/// Country letters, two check digits, up to 30 letters or digits; the whole
/// number, rotated four places, is 1 mod 97 (ISO 13616).
fn iban(raw: &str) -> Option<String> {
    let id = compact(raw);
    let b = id.as_bytes();
    if !(15..=34).contains(&b.len())
        || !b[..2].iter().all(u8::is_ascii_uppercase)
        || !b[2..4].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let mut n: u32 = 0;
    for &c in b[4..].iter().chain(&b[..4]) {
        let v = base36(c)?;
        n = if v < 10 { (n * 10 + v) % 97 } else { (n * 100 + v) % 97 };
    }
    (n == 1).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(raw: &str) -> Option<String> {
        date(raw, DateOrder::Dmy)
    }

    #[test]
    fn dates_in_their_printed_forms() {
        for raw in [
            "14/10/2026", "14-10-2026", "14.10.2026", "14.10.26", "14 Oct 2026", "14-Oct-2026", "14 October 2026",
            "14th October, 2026", "14th of October 2026", "Oct 14, 2026", "October 14 2026", "Oct. 14, 2026",
            "Wed, 14 Oct 2026", "Wednesday 14 October 2026", "2026-10-14", "2026/10/14", "2026 Oct 14", "14 SEPT 2026",
            "14\u{a0}Oct\u{a0}2026",
        ] {
            let want = if raw.contains("SEPT") { "2026-09-14" } else { "2026-10-14" };
            assert_eq!(d(raw).as_deref(), Some(want), "{raw}");
        }
    }

    #[test]
    fn numeric_dates_follow_the_order() {
        assert_eq!(date("03/04/2026", DateOrder::Dmy).as_deref(), Some("2026-04-03"));
        assert_eq!(date("03/04/2026", DateOrder::Mdy).as_deref(), Some("2026-03-04"));
        assert_eq!(date("26/04/03", DateOrder::Ymd).as_deref(), Some("2026-04-03"));
        // Four digits first is a year, whatever the order.
        assert_eq!(date("2026-04-03", DateOrder::Mdy).as_deref(), Some("2026-04-03"));
        // Day first, as the order says, even where only the other way would fit.
        assert_eq!(date("10/14/2026", DateOrder::Dmy), None);
    }

    #[test]
    fn two_digit_years_pivot_at_70() {
        assert_eq!(d("01/01/69").as_deref(), Some("2069-01-01"));
        assert_eq!(d("01/01/70").as_deref(), Some("1970-01-01"));
    }

    #[test]
    fn not_dates() {
        for raw in [
            "31/04/2026", "29/02/2026", "0/10/2026", "14/13/2026", "14 Foo 2026", "see overleaf", "14/10", "14/10/2026 10:30",
            "1,250.00", "1.2.3", "14/10/202", "14 Oct 2026 Oct", "Ma 14 2026", "INV-2026/0042", "0014/10/2026",
        ] {
            assert_eq!(d(raw), None, "{raw}");
        }
        assert_eq!(d("29/02/2024").as_deref(), Some("2024-02-29"));
        assert_eq!(d("29/02/2000").as_deref(), Some("2000-02-29"));
        assert_eq!(d("29/02/1900"), None);
    }

    fn a(raw: &str) -> Option<String> {
        amount(raw, DecimalMark::Point)
    }

    #[test]
    fn amounts_in_their_printed_forms() {
        for (raw, want) in [
            ("1,250.00", "1250.00"),
            ("1250", "1250"),
            ("0.50", "0.50"),
            ("₹ 1,25,000.50", "125000.50"),
            ("Rs. 1,250/-", "1250"),
            ("INR 12,34,56,789.00", "123456789.00"),
            ("USD 1,250", "1250"),
            ("US$1,250.5", "1250.5"),
            ("1,250.00 AED", "1250.00"),
            ("1 250.00", "1250.00"),
            ("1'250.00", "1250.00"),
            ("12.500", "12.500"),
            ("-1,250.00", "-1250.00"),
            ("₹ -1,250.00", "-1250.00"),
            ("(1,250.00)", "-1250.00"),
            ("1,250.00-", "-1250.00"),
            ("+1,250", "1250"),
            ("007", "7"),
        ] {
            assert_eq!(a(raw).as_deref(), Some(want), "{raw}");
        }
    }

    #[test]
    fn decimal_comma() {
        let c = |raw| amount(raw, DecimalMark::Comma);
        assert_eq!(c("1.250,00").as_deref(), Some("1250.00"));
        assert_eq!(c("1 250,00 €").as_deref(), Some("1250.00"));
        assert_eq!(c("1250,5").as_deref(), Some("1250.5"));
        // Read with a decimal point, the same print is not a well-grouped amount.
        assert_eq!(a("1.250,00"), None);
    }

    #[test]
    fn not_amounts() {
        for raw in [
            "1,2,50", "12,50", "1,250.00.00", "1,250 000", "1,250.", "total 1,250", "--1,250", "-(1,250)", "-1,250-", "N/A",
            "1,250.0O", "TBD", "1.250,00",
        ] {
            assert_eq!(a(raw), None, "{raw}");
        }
    }

    #[test]
    fn gstin_checks_its_check_character() {
        assert_eq!(gstin("27AAPFU0939F1ZV").as_deref(), Some("27AAPFU0939F1ZV"));
        assert_eq!(gstin("29aagcb7383j1z4").as_deref(), Some("29AAGCB7383J1Z4"));
        assert_eq!(gstin("27 AAPFU 0939F 1ZV").as_deref(), Some("27AAPFU0939F1ZV"));
        // One misread character, wherever it is, fails the check.
        assert_eq!(gstin("27AAPFU0939F1ZW"), None);
        assert_eq!(gstin("27AAPFUO939F1ZV"), None);
        assert_eq!(gstin("27AAPFU0939F1Z"), None);
        assert_eq!(gstin("AAAAPFU0939F1ZV"), None);
    }

    #[test]
    fn pan_is_its_shape() {
        assert_eq!(pan("aapfu0939f").as_deref(), Some("AAPFU0939F"));
        assert_eq!(pan("AAPFU0939"), None);
        assert_eq!(pan("AAPF00939F"), None);
        assert_eq!(pan("AAPFU09391"), None);
    }

    #[test]
    fn iban_checks_mod_97() {
        assert_eq!(iban("GB82 WEST 1234 5698 7654 32").as_deref(), Some("GB82WEST12345698765432"));
        assert_eq!(iban("DE89370400440532013000").as_deref(), Some("DE89370400440532013000"));
        assert_eq!(iban("AE070331234567890123456").as_deref(), Some("AE070331234567890123456"));
        assert_eq!(iban("GB82WEST12345698765433"), None);
        assert_eq!(iban("GB82WEST"), None);
        assert_eq!(iban("1282WEST12345698765432"), None);
    }

    #[test]
    fn long_captures_are_never_one_value() {
        assert_eq!(Kind::Amount(DecimalMark::Point).read(&"1".repeat(65)), None);
        assert_eq!(Kind::Amount(DecimalMark::Point).read(&"1".repeat(64)).map(|v| v.len()), Some(64));
    }
}
