//! Opt-in, bounded response diagnostics. No upload bytes, filenames,
//! credentials, subprocess stderr or local paths enter this trace.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize)]
pub struct DebugTrace {
    pub steps: Vec<FlowStep>,
    pub truncated: bool,
    #[serde(skip)]
    remaining: usize,
}

#[derive(Serialize)]
pub struct FlowStep {
    step: &'static str,
    status: &'static str,
    elapsed_ms: u64,
    input: Value,
    output: Value,
}

impl DebugTrace {
    pub fn new(limit: usize) -> Self {
        Self {
            steps: Vec::new(),
            truncated: false,
            remaining: limit,
        }
    }

    pub fn push(
        &mut self,
        step: &'static str,
        status: &'static str,
        elapsed: Duration,
        input: Value,
        output: Value,
    ) {
        let input = self.bounded(input);
        let output = self.bounded(output);
        self.steps.push(FlowStep {
            step,
            status,
            elapsed_ms: elapsed.as_millis() as u64,
            input,
            output,
        });
    }

    fn bounded(&mut self, value: Value) -> Value {
        let encoded = value.to_string();
        if encoded.len() <= self.remaining {
            self.remaining -= encoded.len();
            return value;
        }
        self.truncated = true;
        let mut end = self.remaining.min(encoded.len());
        while !encoded.is_char_boundary(end) {
            end -= 1;
        }
        self.remaining = 0;
        json!({"truncated":true,"original_bytes":encoded.len(),"preview":&encoded[..end]})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_is_bounded_and_marks_truncation() {
        let mut trace = DebugTrace::new(30);
        trace.push(
            "parse",
            "ok",
            Duration::ZERO,
            json!({"page":1}),
            json!({"text":"தமிழ்".repeat(30)}),
        );
        let j = serde_json::to_value(trace).unwrap();
        assert_eq!(j["truncated"], true);
        assert_eq!(j["steps"][0]["output"]["truncated"], true);
        assert!(j["steps"][0]["output"]["preview"].as_str().unwrap().len() <= 30);
    }
}
