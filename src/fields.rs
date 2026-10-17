//! Named values read out of a document's text with regular expressions, for
//! `/v1/extract/fields`.
//!
//! A request carries its field specs; they are parsed and compiled once,
//! before the document is read, so a bad pattern costs a `400` and no OCR.
//! The document is then extracted exactly as `/v1/extract/text` would, and
//! every field runs over that one final text: no field triggers OCR of its own.
//!
//! The patterns are Rust `regex` syntax. Matching is linear in the text, so
//! no pattern can backtrack catastrophically; the price is no look-around and
//! no backreferences, which are refused at parse time with the engine's reason.
//!
//! A field may have a `type` ([`kinds`]): a date, an amount, a GSTIN, a PAN or
//! an IBAN. A match that does not read as one is no match, and the value is
//! written in one form whatever the print.

mod kinds;

use axum::http::StatusCode;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde::ser::SerializeMap;
use serde_json::Value;

use crate::error::ApiError;
pub use kinds::Kind;

pub const MAX_FIELDS: usize = 50;
pub const MAX_PATTERNS: usize = 10;
/// Patterns across all fields of one request.
pub const MAX_TOTAL_PATTERNS: usize = 100;
/// Characters per pattern.
pub const MAX_PATTERN_CHARS: usize = 1000;
/// Upper bound for a field's `max_length`.
pub const MAX_VALUE_CHARS: usize = 10_000;
/// Values kept per `multiple` field; later ones are dropped.
pub const MAX_VALUES: usize = 1000;
const MAX_NAME_CHARS: usize = 64;
/// Compiled sizes a pattern is tried at, smallest first. Counted repetitions
/// multiply: the Unicode word class is ~50 KB compiled, so `\w{1,200}` is
/// ~10 MB while `[A-Za-z0-9]{1,200}` stays under 0.2 MB. A pattern that fits
/// the first step is free; a larger one is charged the step it fitted
/// against [`COMPILED_BUDGET`]. The last step is the `regex` crate's default.
const SIZE_STEPS: [usize; 4] = [256 << 10, 1 << 20, 4 << 20, 10 << 20];
/// What the larger patterns of one request may take together. With
/// [`MAX_TOTAL_PATTERNS`] free ones, a request's patterns stay under ~65 MiB.
const COMPILED_BUDGET: usize = 40 << 20;
/// The lazy DFA's cache per pattern (the crate default is 2 MiB); a pattern
/// that outgrows it clears it and carries on, a little slower.
const DFA_CACHE: usize = 1 << 20;

/// Keys a field spec may carry; anything else is refused, so a typo never
/// passes for a default.
pub const FIELD_KEYS: [&str; 10] = [
    "name", "patterns", "capture_group", "required", "type", "format", "normalization", "max_length", "multiple", "selection",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Normalization {
    /// Leading and trailing whitespace removed.
    Trim,
    /// The capture exactly as matched.
    None,
}

/// One field, compiled.
#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub required: bool,
    pub multiple: bool,
    kind: Kind,
    /// A `text` field's only; a typed one is written in its type's form.
    normalization: Normalization,
    max_length: Option<usize>,
    /// Each pattern with the capture group it reads, resolved per pattern.
    patterns: Vec<(Regex, usize)>,
}

/// What `capture_group` asked for, before it is resolved against a pattern.
enum GroupSpec {
    /// Omitted: group 1 when the pattern has one, else the whole match.
    Default,
    Index(usize),
    Name(String),
}

/// Values in request order, every requested field present: a value, an array
/// (`multiple`), or `null` when nothing matched.
#[derive(Debug, Default)]
pub struct FieldValues(pub Vec<(String, Value)>);

impl FieldValues {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

impl Serialize for FieldValues {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub values: FieldValues,
    /// The text each value was read from, as printed, keyed like `values`: a
    /// typed value (`2026-10-14`) is not in the text, its print (`14 Oct 2026`)
    /// is. A `multiple` field lists every distinct print of the values kept.
    pub matched: FieldValues,
    /// Required fields that matched nothing, in request order.
    pub missing_required: Vec<String>,
}

/// Parses and compiles the `fields` param: a JSON array, or a string holding
/// one (multipart parts and query params are strings).
pub fn parse(value: &Value) -> Result<Vec<Field>, ApiError> {
    let parsed;
    let value = match value {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).map_err(|e| {
                invalid("fields", format!("`fields` is not valid JSON: {e}"), "a JSON array of field specs")
            })?;
            &parsed
        }
        v => v,
    };
    let Value::Array(items) = value else {
        return Err(invalid("fields", "`fields` must be an array of field specs", "a JSON array of field specs"));
    };
    if items.is_empty() || items.len() > MAX_FIELDS {
        return Err(invalid(
            "fields",
            format!("`fields` must hold 1 to {MAX_FIELDS} field specs, not {}", items.len()),
            &format!("1 to {MAX_FIELDS} field specs"),
        ));
    }
    let mut fields: Vec<Field> = Vec::with_capacity(items.len());
    let mut budget = COMPILED_BUDGET;
    for (i, item) in items.iter().enumerate() {
        let field = parse_field(i, item, &mut budget)?;
        if fields.iter().any(|f| f.name == field.name) {
            return Err(invalid(
                &format!("fields[{i}].name"),
                format!("field name `{}` is used twice", field.name),
                "a name unique within the request",
            ));
        }
        fields.push(field);
    }
    let total: usize = fields.iter().map(|f| f.patterns.len()).sum();
    if total > MAX_TOTAL_PATTERNS {
        return Err(invalid(
            "fields",
            format!("the fields hold {total} patterns between them; at most {MAX_TOTAL_PATTERNS} are allowed"),
            &format!("at most {MAX_TOTAL_PATTERNS} patterns in all"),
        ));
    }
    Ok(fields)
}

fn parse_field(i: usize, item: &Value, budget: &mut usize) -> Result<Field, ApiError> {
    let at = |key: &str| format!("fields[{i}].{key}");
    let Value::Object(spec) = item else {
        return Err(invalid(&format!("fields[{i}]"), format!("fields[{i}] must be an object"), "a field spec object"));
    };
    if let Some(key) = spec.keys().find(|k| !FIELD_KEYS.contains(&k.as_str())) {
        return Err(invalid(
            &format!("fields[{i}].{key}"),
            format!("unknown key `{key}` in fields[{i}]"),
            &FIELD_KEYS.join(", "),
        ));
    }

    let name = match spec.get("name") {
        Some(Value::String(s)) if valid_name(s) => s.clone(),
        _ => {
            return Err(invalid(
                &at("name"),
                format!("fields[{i}].name must be a letter followed by up to {} letters, digits or underscores", MAX_NAME_CHARS - 1),
                "e.g. invoice_number",
            ));
        }
    };

    let group = match spec.get("capture_group") {
        None | Some(Value::Null) => GroupSpec::Default,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(g) => GroupSpec::Index(g as usize),
            None => return Err(invalid(&at("capture_group"), "capture_group must be 0 or more", "a group number or name")),
        },
        Some(Value::String(s)) if !s.is_empty() => match s.parse::<usize>() {
            Ok(g) => GroupSpec::Index(g),
            Err(_) => GroupSpec::Name(s.clone()),
        },
        Some(_) => return Err(invalid(&at("capture_group"), "capture_group must be a number or a group name", "a group number or name")),
    };

    let required = flag(spec.get("required"), &at("required"))?;
    let multiple = flag(spec.get("multiple"), &at("multiple"))?;

    let kind = kinds::parse(spec.get("type"), spec.get("format"), &at)?;

    let normalization = match spec.get("normalization") {
        None | Some(Value::Null) => Normalization::Trim,
        Some(Value::String(s)) if s == "trim" => Normalization::Trim,
        Some(Value::String(s)) if s == "none" => Normalization::None,
        Some(v) => return Err(invalid(&at("normalization"), format!("invalid value {v} for `{}`", at("normalization")), "trim, none")),
    };

    let max_length = match spec.get("max_length") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(m) if (1..=MAX_VALUE_CHARS as u64).contains(&m) => Some(m as usize),
            _ => {
                return Err(invalid(&at("max_length"), format!("max_length must be 1 to {MAX_VALUE_CHARS}"), &format!("1 to {MAX_VALUE_CHARS}")));
            }
        },
        Some(_) => return Err(invalid(&at("max_length"), "max_length must be a number", &format!("1 to {MAX_VALUE_CHARS}"))),
    };

    match spec.get("selection") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s == "first" => {}
        Some(v) => return Err(invalid(&at("selection"), format!("invalid value {v} for `{}`", at("selection")), "first")),
    }

    let sources: Vec<&str> = match spec.get("patterns") {
        Some(Value::Array(ps)) if !ps.is_empty() && ps.len() <= MAX_PATTERNS => {
            let mut out = Vec::with_capacity(ps.len());
            for (j, p) in ps.iter().enumerate() {
                match p {
                    Value::String(s) if !s.is_empty() && s.chars().count() <= MAX_PATTERN_CHARS => out.push(s.as_str()),
                    _ => {
                        return Err(invalid(
                            &format!("fields[{i}].patterns[{j}]"),
                            format!("pattern {} of `{name}` must be a non-empty string of at most {MAX_PATTERN_CHARS} characters", j + 1),
                            "a regular expression",
                        ));
                    }
                }
            }
            out
        }
        _ => {
            return Err(invalid(
                &at("patterns"),
                format!("`{name}` needs 1 to {MAX_PATTERNS} patterns"),
                &format!("an array of 1 to {MAX_PATTERNS} regular expressions"),
            ));
        }
    };

    let mut patterns = Vec::with_capacity(sources.len());
    for (j, src) in sources.into_iter().enumerate() {
        let regex = compile(src, budget).map_err(|e| {
            let reason = match e {
                Compile::Invalid(e) => format!("is not a valid regular expression: {}", regex_reason(&e)),
                Compile::TooBig => format!(
                    "is too large once compiled (over {} MiB); counted repetitions of broad classes cost most, \
                     so narrow the class or the count (`[A-Za-z0-9]{{1,200}}`, not `\\w{{1,200}}`)",
                    SIZE_STEPS[SIZE_STEPS.len() - 1] >> 20
                ),
                Compile::OverBudget => format!(
                    "takes this request's patterns past {} MiB compiled between them; \
                     narrow the broad counted classes (`\\w{{1,200}}` is ~10 MiB)",
                    COMPILED_BUDGET >> 20
                ),
            };
            invalid(
                &format!("fields[{i}].patterns[{j}]"),
                format!("pattern {} of `{name}` {reason}", j + 1),
                "a regular expression in Rust regex syntax (no look-around, no backreferences)",
            )
        })?;
        let index = resolve_group(&regex, &group).ok_or_else(|| {
            let wanted = match &group {
                GroupSpec::Index(g) => format!("group {g}"),
                GroupSpec::Name(n) => format!("a group named `{n}`"),
                GroupSpec::Default => unreachable!("the default group always resolves"),
            };
            invalid(
                &at("capture_group"),
                format!("pattern {} of `{name}` has no {wanted}; it has {} numbered group(s)", j + 1, regex.captures_len() - 1),
                "0 (the whole match), a group the pattern defines, or one of its group names",
            )
        })?;
        patterns.push((regex, index));
    }

    Ok(Field { name, required, multiple, kind, normalization, max_length, patterns })
}

enum Compile {
    Invalid(regex::Error),
    TooBig,
    OverBudget,
}

/// Compiles at the smallest size step the pattern fits, charging `budget`.
fn compile(src: &str, budget: &mut usize) -> Result<Regex, Compile> {
    for (n, &limit) in SIZE_STEPS.iter().enumerate() {
        let charge = if n == 0 { 0 } else { limit };
        if charge > *budget {
            return Err(Compile::OverBudget);
        }
        match RegexBuilder::new(src).size_limit(limit).dfa_size_limit(DFA_CACHE).build() {
            Ok(regex) => {
                *budget -= charge;
                return Ok(regex);
            }
            Err(regex::Error::CompiledTooBig(_)) => continue,
            Err(e) => return Err(Compile::Invalid(e)),
        }
    }
    Err(Compile::TooBig)
}

fn resolve_group(regex: &Regex, group: &GroupSpec) -> Option<usize> {
    match group {
        GroupSpec::Default => Some(usize::from(regex.captures_len() > 1)),
        GroupSpec::Index(g) => (*g < regex.captures_len()).then_some(*g),
        GroupSpec::Name(n) => regex.capture_names().position(|c| c == Some(n.as_str())),
    }
}

fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.len() <= MAX_NAME_CHARS
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn flag(v: Option<&Value>, at: &str) -> Result<bool, ApiError> {
    match v {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(invalid(at, format!("invalid value {v} for `{at}`"), "true, false")),
    }
}

/// The engine's own one-line reason (`look-around ... is not supported`),
/// without the multi-line caret diagram around it.
fn regex_reason(e: &regex::Error) -> String {
    match e {
        regex::Error::Syntax(s) => s
            .lines()
            .rev()
            .find_map(|l| l.trim().strip_prefix("error: "))
            .unwrap_or("invalid syntax")
            .to_string(),
        other => other.to_string(),
    }
}

fn invalid(parameter: &str, detail: impl Into<String>, accepted: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid-parameter", "Invalid parameter", detail)
        .with("parameter", parameter)
        .with("accepted", accepted)
}

/// Runs every field over `text`.
///
/// A field's patterns are tried in order and the first valid match wins; with
/// `multiple`, every valid match of every pattern is kept, in that order,
/// each distinct value once. A match is valid when its group took part, is
/// not blank, reads as the field's `type`, and fits `max_length` as written;
/// an invalid match is skipped, never cut down. Values are the text as read,
/// only trimmed, or in their type's form: nothing is corrected (no `O` → `0`).
pub fn extract(fields: &[Field], text: &str) -> Extracted {
    let mut out = Extracted::default();
    for field in fields {
        let (value, matched) = if field.multiple {
            let mut seen: Vec<String> = Vec::new();
            let mut prints: Vec<String> = Vec::new();
            'patterns: for (regex, group) in &field.patterns {
                for m in matches(field, regex, *group, text) {
                    if prints.len() < MAX_VALUES && !prints.contains(&m.print) {
                        prints.push(m.print);
                    }
                    if !seen.contains(&m.value) {
                        seen.push(m.value);
                        if seen.len() == MAX_VALUES {
                            break 'patterns;
                        }
                    }
                }
            }
            let list = |v: Vec<String>| (!v.is_empty()).then(|| Value::Array(v.into_iter().map(Value::String).collect()));
            (list(seen), list(prints))
        } else {
            match field.patterns.iter().find_map(|(regex, group)| matches(field, regex, *group, text).next()) {
                Some(m) => (Some(Value::String(m.value)), Some(Value::String(m.print))),
                None => (None, None),
            }
        };
        if value.is_none() && field.required {
            out.missing_required.push(field.name.clone());
        }
        out.values.0.push((field.name.clone(), value.unwrap_or(Value::Null)));
        out.matched.0.push((field.name.clone(), matched.unwrap_or(Value::Null)));
    }
    out
}

/// One valid match: the value, and the text it was read from.
struct Match {
    value: String,
    print: String,
}

/// A pattern's valid matches, in text order.
fn matches<'a>(field: &'a Field, regex: &'a Regex, group: usize, text: &'a str) -> impl Iterator<Item = Match> + 'a {
    regex.captures_iter(text).filter_map(move |caps| {
        let raw = caps.get(group)?.as_str();
        if raw.trim().is_empty() {
            return None;
        }
        let (print, value) = match field.kind {
            Kind::Text => {
                let v = match field.normalization {
                    Normalization::Trim => raw.trim(),
                    Normalization::None => raw,
                };
                (v, v.to_string())
            }
            kind => (raw.trim(), kind.read(raw.trim())?),
        };
        if field.max_length.is_some_and(|max| value.chars().count() > max) {
            return None;
        }
        Some(Match { value, print: print.to_string() })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const INVOICE: &str = "ACME Trading LLC\n\
        Tax Invoice\n\
        Invoice No: INV-2026/0042\n\
        Invoice Date: 14 Oct 2026\n\
        PO Number\n  PO-77/A  \n\
        Ref: R-1, Ref: R-2, Ref: R-1\n\
        Total amount due: 1,250.00\n";

    fn run(fields: Value, text: &str) -> Extracted {
        extract(&parse(&fields).expect("valid specs"), text)
    }

    fn one(spec: Value, text: &str) -> Value {
        run(json!([spec]), text).values.0.remove(0).1
    }

    fn error(fields: Value) -> (String, String) {
        let e = parse(&fields).expect_err("invalid specs");
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        (e.extra["parameter"].as_str().unwrap().to_string(), e.detail)
    }

    #[test]
    fn same_line_label_and_value() {
        let v = one(json!({"name": "invoice_number", "patterns": [r"Invoice No:\s*([A-Z0-9 _./-]+)"]}), INVOICE);
        assert_eq!(v, "INV-2026/0042");
    }

    #[test]
    fn value_on_the_next_line() {
        // `\s+` crosses the line break; the trim drops the padding around the value.
        let v = one(json!({"name": "po", "patterns": [r"PO Number\s+([^\n]+)"]}), INVOICE);
        assert_eq!(v, "PO-77/A");
    }

    #[test]
    fn case_insensitive_flag() {
        let v = one(json!({"name": "total", "patterns": [r"(?i)TOTAL AMOUNT DUE:\s*([\d,]+\.\d{2})"]}), INVOICE);
        assert_eq!(v, "1,250.00");
    }

    #[test]
    fn patterns_are_tried_in_order() {
        let spec = json!({"name": "date", "patterns": [r"Due Date:\s*(.+)", r"Invoice Date:\s*(.+)", r"Date:\s*(.+)"]});
        assert_eq!(one(spec, INVOICE), "14 Oct 2026");
    }

    #[test]
    fn first_match_in_the_text_wins() {
        assert_eq!(one(json!({"name": "ref", "patterns": [r"Ref: (R-\d)"]}), INVOICE), "R-1");
    }

    #[test]
    fn each_field_reads_only_its_own_label() {
        // Both labels end in "No:", and the values have the same shape. A field's
        // patterns are its own: the invoice number never comes back as the PO's.
        let text = "PO No: PO-77\nInvoice No: INV-42\nDelivery No: D-9";
        let r = run(
            json!([
                {"name": "invoice_number", "patterns": [r"Invoice No:\s*(\S+)"]},
                {"name": "po_number", "patterns": [r"PO No:\s*(\S+)"]},
                {"name": "contract_ref", "patterns": [r"Contract No:\s*(\S+)"]},
            ]),
            text,
        );
        assert_eq!(r.values.get("invoice_number"), Some(&json!("INV-42")));
        assert_eq!(r.values.get("po_number"), Some(&json!("PO-77")));
        // One with nothing of its own stays empty rather than borrowing a neighbour's.
        assert_eq!(r.values.get("contract_ref"), Some(&Value::Null));
    }

    #[test]
    fn empty_text_finds_nothing() {
        let r = run(
            json!([
                {"name": "a", "patterns": [r"(\S*)"]},
                {"name": "b", "patterns": [r"x"], "required": true, "multiple": true},
            ]),
            "",
        );
        assert_eq!(r.values.get("a"), Some(&Value::Null));
        assert_eq!(r.values.get("b"), Some(&Value::Null));
        assert_eq!(r.missing_required, ["b"]);
    }

    #[test]
    fn missing_value_is_null_and_required_is_reported() {
        let r = run(
            json!([
                {"name": "iban", "patterns": [r"IBAN:\s*(\S+)"]},
                {"name": "vat", "patterns": [r"VAT No:\s*(\S+)"], "required": true},
                {"name": "invoice_number", "patterns": [r"Invoice No:\s*(\S+)"], "required": true},
            ]),
            INVOICE,
        );
        assert_eq!(r.values.get("iban"), Some(&Value::Null));
        assert_eq!(r.values.get("vat"), Some(&Value::Null));
        assert_eq!(r.values.get("invoice_number"), Some(&json!("INV-2026/0042")));
        assert_eq!(r.missing_required, ["vat"]);
    }

    #[test]
    fn values_keep_request_order() {
        let r = run(json!([{"name": "z", "patterns": ["Tax"]}, {"name": "a", "patterns": ["ACME"]}]), INVOICE);
        assert_eq!(serde_json::to_string(&r.values).unwrap(), r#"{"z":"Tax","a":"ACME"}"#);
    }

    #[test]
    fn capture_group_default_whole_match_number_and_name() {
        // No group in the pattern: the whole match.
        assert_eq!(one(json!({"name": "n", "patterns": [r"INV-\d+/\d+"]}), INVOICE), "INV-2026/0042");
        assert_eq!(one(json!({"name": "n", "patterns": [r"Invoice No:\s*(INV)-(\d+)"], "capture_group": 2}), INVOICE), "2026");
        assert_eq!(one(json!({"name": "n", "patterns": [r"Invoice No:\s*\S+"], "capture_group": 0}), INVOICE), "Invoice No: INV-2026/0042");
        let named = json!({"name": "n", "patterns": [r"Invoice No:\s*INV-(?<year>\d{4})"], "capture_group": "year"});
        assert_eq!(one(named, INVOICE), "2026");
    }

    #[test]
    fn invalid_capture_group_is_400() {
        let (param, detail) = error(json!([{"name": "n", "patterns": [r"(a)", r"(a)(b)"], "capture_group": 2}]));
        assert_eq!(param, "fields[0].capture_group");
        assert!(detail.contains("pattern 1 of `n` has no group 2"), "{detail}");
        let (param, _) = error(json!([{"name": "n", "patterns": [r"(?<x>a)"], "capture_group": "y"}]));
        assert_eq!(param, "fields[0].capture_group");
    }

    #[test]
    fn invalid_regex_is_400_with_the_engine_reason() {
        let (param, detail) = error(json!([{"name": "ok", "patterns": ["a"]}, {"name": "n", "patterns": ["a", r"(?<=Total)\d+"]}]));
        assert_eq!(param, "fields[1].patterns[1]");
        assert!(detail.contains("look-around"), "{detail}");
        assert!(!detail.contains('\n'), "{detail}");
        let (param, _) = error(json!([{"name": "n", "patterns": [r"(\d+"]}]));
        assert_eq!(param, "fields[0].patterns[0]");
        let (_, detail) = error(json!([{"name": "n", "patterns": [r"(?:\w{1000}){1000}"]}]));
        assert!(detail.contains("too large"), "{detail}");
    }

    #[test]
    fn large_patterns_share_a_budget() {
        // ~10 MiB each: four fit the request's budget, a fifth does not.
        let wide = r"(?i)name:\s*([\w .,'/-]{1,200})";
        assert!(parse(&json!([{"name": "n", "patterns": [wide, wide, wide, wide]}])).is_ok());
        let (param, detail) = error(json!([{"name": "n", "patterns": [wide, wide, wide]}, {"name": "m", "patterns": [wide, wide]}]));
        assert_eq!(param, "fields[1].patterns[1]");
        assert!(detail.contains("past 40 MiB"), "{detail}");
        // Narrow classes are free, however many.
        let narrow = vec![json!(r"(?i)invoice\s*no[:.]?\s*([A-Z0-9 _./-]{1,40})"); MAX_PATTERNS];
        let many: Vec<Value> = (0..MAX_TOTAL_PATTERNS / MAX_PATTERNS).map(|i| json!({"name": format!("f{i}"), "patterns": narrow})).collect();
        assert!(parse(&Value::Array(many)).is_ok());
    }

    #[test]
    fn max_length_skips_to_the_next_valid_match() {
        let text = "Code: ABCDEFGHIJKLMNOP\nCode: XY12";
        let v = one(json!({"name": "code", "patterns": [r"Code: (\w+)"], "max_length": 6}), text);
        assert_eq!(v, "XY12");
        assert_eq!(one(json!({"name": "code", "patterns": [r"Code: (\w{7,})"], "max_length": 6}), text), Value::Null);
    }

    #[test]
    fn trim_and_none_normalization() {
        let text = "Name:   Lia Shah   \n";
        assert_eq!(one(json!({"name": "n", "patterns": [r"Name:([^\n]*)"]}), text), "Lia Shah");
        assert_eq!(one(json!({"name": "n", "patterns": [r"Name:([^\n]*)"], "normalization": "none"}), text), "   Lia Shah   ");
        // A blank capture is no value, whatever the normalization.
        assert_eq!(one(json!({"name": "n", "patterns": [r"Name:(\s*)"], "normalization": "none"}), text), Value::Null);
    }

    #[test]
    fn multiple_keeps_each_distinct_value_in_order() {
        let v = one(json!({"name": "refs", "patterns": [r"Ref: (R-\d)", r"(PO-\S+)"], "multiple": true}), INVOICE);
        assert_eq!(v, json!(["R-1", "R-2", "PO-77/A"]));
        assert_eq!(one(json!({"name": "refs", "patterns": [r"Ref: (Q-\d)"], "multiple": true}), INVOICE), Value::Null);
    }

    #[test]
    fn values_are_never_corrected() {
        // OCR read a letter O for a zero: the value is reported as read.
        assert_eq!(one(json!({"name": "n", "patterns": [r"Invoice (\S+)"]}), "Invoice 44O1"), "44O1");
        assert_eq!(one(json!({"name": "n", "patterns": [r"Invoice (\d+)"]}), "Invoice 44O1"), "44");
    }

    #[test]
    fn unicode_text_and_digits() {
        let text = "رقم الفاتورة: ٤٤٧١\nबीजक संख्या: ४४७१";
        assert_eq!(one(json!({"name": "ar", "patterns": [r"رقم الفاتورة:\s*(\d+)"]}), text), "٤٤٧١");
        assert_eq!(one(json!({"name": "hi", "patterns": [r"बीजक संख्या:\s*(\d+)"]}), text), "४४७१");
    }

    #[test]
    fn malformed_specs_are_400() {
        for (fields, param) in [
            (json!({}), "fields"),
            (json!([]), "fields"),
            (json!("not json"), "fields"),
            (json!(["x"]), "fields[0]"),
            (json!([{"name": "1st", "patterns": ["a"]}]), "fields[0].name"),
            (json!([{"name": "a", "patterns": []}]), "fields[0].patterns"),
            (json!([{"name": "a", "patterns": [""]}]), "fields[0].patterns[0]"),
            (json!([{"name": "a", "patterns": ["a"], "pattern": "b"}]), "fields[0].pattern"),
            (json!([{"name": "a", "patterns": ["a"], "normalization": "upper"}]), "fields[0].normalization"),
            (json!([{"name": "a", "patterns": ["a"], "selection": "last"}]), "fields[0].selection"),
            (json!([{"name": "a", "patterns": ["a"], "max_length": 0}]), "fields[0].max_length"),
            (json!([{"name": "a", "patterns": ["a"], "required": "yes"}]), "fields[0].required"),
            (json!([{"name": "a", "patterns": ["a"]}, {"name": "a", "patterns": ["b"]}]), "fields[1].name"),
            (json!([{"name": "a", "patterns": ["a"], "type": "number"}]), "fields[0].type"),
            (json!([{"name": "a", "patterns": ["a"], "type": 1}]), "fields[0].type"),
            (json!([{"name": "a", "patterns": ["a"], "type": "date", "format": "point"}]), "fields[0].format"),
            (json!([{"name": "a", "patterns": ["a"], "type": "amount", "format": "dmy"}]), "fields[0].format"),
            (json!([{"name": "a", "patterns": ["a"], "format": "dmy"}]), "fields[0].format"),
            (json!([{"name": "a", "patterns": ["a"], "type": "gstin", "format": "dmy"}]), "fields[0].format"),
        ] {
            assert_eq!(error(fields.clone()).0, param, "{fields}");
        }
        let too_many: Vec<Value> = (0..=MAX_FIELDS).map(|i| json!({"name": format!("f{i}"), "patterns": ["a"]})).collect();
        assert_eq!(error(Value::Array(too_many)).0, "fields");
        let ten = vec![json!("a"); MAX_PATTERNS];
        let crowded: Vec<Value> = (0..=MAX_TOTAL_PATTERNS / MAX_PATTERNS).map(|i| json!({"name": format!("f{i}"), "patterns": ten})).collect();
        assert_eq!(error(Value::Array(crowded)).0, "fields");
    }

    #[test]
    fn a_typed_match_that_does_not_read_is_skipped() {
        // The first "Date:" holds no date; the field moves on to the next match.
        let text = "Due Date: see overleaf\nInvoice Date: 14 Oct 2026\nTotal: TBD\nTotal due: Rs. 1,25,000.50/-";
        let r = run(
            json!([
                {"name": "date", "patterns": [r"Date:\s*([^\n]+)"], "type": "date"},
                {"name": "total", "patterns": [r"Total[^:]*:\s*([^\n]+)"], "type": "amount"},
            ]),
            text,
        );
        assert_eq!(r.values.get("date"), Some(&json!("2026-10-14")));
        assert_eq!(r.values.get("total"), Some(&json!("125000.50")));
        // What was read, as printed: what to mark in the text.
        assert_eq!(r.matched.get("date"), Some(&json!("14 Oct 2026")));
        assert_eq!(r.matched.get("total"), Some(&json!("Rs. 1,25,000.50/-")));
    }

    #[test]
    fn typed_values_are_one_value_whatever_the_print() {
        let text = "Ref date 14/10/2026, again 14 Oct 2026, then 2026-11-01";
        let v = one(json!({"name": "d", "patterns": [r"(\d{1,4}[/-]\d{1,2}[/-]\d{1,4}|\d{1,2} [A-Za-z]{3} \d{4})"], "type": "date", "multiple": true}), text);
        assert_eq!(v, json!(["2026-10-14", "2026-11-01"]));
        let r = run(json!([{"name": "d", "patterns": [r"(\d{1,4}[/-]\d{1,2}[/-]\d{1,4}|\d{1,2} [A-Za-z]{3} \d{4})"], "type": "date", "multiple": true}]), text);
        assert_eq!(r.matched.get("d"), Some(&json!(["14/10/2026", "14 Oct 2026", "2026-11-01"])));
    }

    #[test]
    fn format_and_identifiers() {
        let text = "Datum: 03.04.2026\nBetrag: 1.250,00 EUR\nGSTIN: 27aapfu0939f1zv\nBad GSTIN: 27AAPFU0939F1ZW\nIBAN: GB82 WEST 1234 5698 7654 32";
        let r = run(
            json!([
                {"name": "dmy", "patterns": [r"Datum:\s*(\S+)"], "type": "date"},
                {"name": "mdy", "patterns": [r"Datum:\s*(\S+)"], "type": "date", "format": "mdy"},
                {"name": "betrag", "patterns": [r"Betrag:\s*([^\n]+)"], "type": "amount", "format": "comma"},
                {"name": "gstin", "patterns": [r"GSTIN:\s*(\S+)"], "type": "gstin", "multiple": true},
                {"name": "iban", "patterns": [r"IBAN:\s*([A-Z0-9 ]+)"], "type": "iban"},
                {"name": "pan", "patterns": [r"GSTIN:\s*\d\d(\S{10})"], "type": "pan"},
            ]),
            text,
        );
        assert_eq!(r.values.get("dmy"), Some(&json!("2026-04-03")));
        assert_eq!(r.values.get("mdy"), Some(&json!("2026-03-04")));
        assert_eq!(r.values.get("betrag"), Some(&json!("1250.00")));
        // The misread one fails its check character and is not kept.
        assert_eq!(r.values.get("gstin"), Some(&json!(["27AAPFU0939F1ZV"])));
        assert_eq!(r.values.get("iban"), Some(&json!("GB82WEST12345698765432")));
        assert_eq!(r.values.get("pan"), Some(&json!("AAPFU0939F")));
    }

    #[test]
    fn max_length_counts_the_typed_value() {
        // `125000.50` is 9 characters, its print 13.
        let spec = |max| json!({"name": "t", "patterns": [r"Total:\s*([^\n]+)"], "type": "amount", "max_length": max});
        assert_eq!(one(spec(9), "Total: ₹ 1,25,000.50"), "125000.50");
        assert_eq!(one(spec(8), "Total: ₹ 1,25,000.50"), Value::Null);
    }

    #[test]
    fn text_fields_match_as_printed() {
        let r = run(json!([{"name": "n", "patterns": [r"Name:([^\n]*)"], "normalization": "none"}]), "Name:  Lia  \n");
        assert_eq!(r.values.get("n"), Some(&json!("  Lia  ")));
        assert_eq!(r.matched.get("n"), Some(&json!("  Lia  ")));
        assert_eq!(r.matched.get("missing"), None);
    }

    #[test]
    fn fields_may_arrive_as_a_json_string() {
        let fields = parse(&json!(r#"[{"name": "n", "patterns": ["Invoice No:\\s*(\\S+)"]}]"#)).unwrap();
        assert_eq!(extract(&fields, INVOICE).values.get("n"), Some(&json!("INV-2026/0042")));
    }
}
