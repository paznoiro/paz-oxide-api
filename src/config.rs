//! `OXIDE_API_*` environment parsing, defaults, and the fail-closed auth check.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: SocketAddr,
    pub api_key: Option<String>,
    pub cpu_threads: usize,
    pub max_inflight: usize,
    pub queue_wait: Duration,
    pub retry_after_secs: u64,
    pub max_body_bytes: usize,
    pub body_timeout: Duration,
    pub process_timeout: Duration,
    pub max_text_chars: usize,
    pub max_debug_bytes: usize,
    pub max_pdf_pages: usize,
    pub max_image_pixels: u64,
    pub drain_delay: Duration,
    pub ocr_workers: usize,
    pub ocr_timeout: Duration,
    pub ocr_languages: Vec<String>,
    pub ocr_model_dir: PathBuf,
    pub indra_cli: PathBuf,
    pub indra_model_dir: PathBuf,
    pub indra_math: bool,
    pub indra_max_concurrent: usize,
}

pub const DEFAULT_ADDR: &str = "0.0.0.0:8080";

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());

        let addr_s = var("OXIDE_API_ADDR").unwrap_or_else(|| DEFAULT_ADDR.to_string());
        let addr: SocketAddr = addr_s
            .parse()
            .map_err(|_| format!("OXIDE_API_ADDR: invalid socket address {addr_s:?}"))?;

        let api_key = var("OXIDE_API_API_KEY");
        if let Some(k) = &api_key
            && k.len() < 16
        {
            return Err("OXIDE_API_API_KEY must be at least 16 characters".into());
        }
        if api_key.is_none() && !addr.ip().is_loopback() {
            return Err(format!(
                "OXIDE_API_API_KEY is required when binding a non-loopback address ({addr}); \
                 set a key or bind 127.0.0.1"
            ));
        }

        let parallelism = std::thread::available_parallelism().map_or(1, |n| n.get());
        let num = |k: &str, default: u64, min: u64| -> Result<u64, String> {
            match var(k) {
                None => Ok(default),
                Some(v) => {
                    let n: u64 = v.parse().map_err(|_| format!("{k}: expected an integer, got {v:?}"))?;
                    if n < min {
                        return Err(format!("{k}: must be >= {min}"));
                    }
                    Ok(n)
                }
            }
        };

        let ocr_languages: Vec<String> = var("OXIDE_API_OCR_LANGUAGES")
            .unwrap_or_else(|| "en,hi,ar".into())
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        for l in &ocr_languages {
            if !matches!(l.as_str(), "en" | "hi" | "ar") {
                return Err(format!("OXIDE_API_OCR_LANGUAGES: unsupported language {l:?} (accepted: en, hi, ar)"));
            }
        }

        let default_retry = if cfg!(feature = "ocr") { 10 } else { 1 };
        let indra_math = match var("OXIDE_API_INDRA_MATH").as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("true" | "1" | "yes" | "on") => true,
            Some("false" | "0" | "no" | "off") => false,
            Some(_) => return Err("OXIDE_API_INDRA_MATH: expected true or false".into()),
        };

        Ok(Self {
            addr,
            api_key,
            cpu_threads: num("OXIDE_API_CPU_THREADS", parallelism as u64, 1)? as usize,
            max_inflight: num("OXIDE_API_MAX_INFLIGHT", 8, 1)? as usize,
            queue_wait: Duration::from_millis(num("OXIDE_API_QUEUE_WAIT_MS", 1000, 0)?),
            retry_after_secs: num("OXIDE_API_RETRY_AFTER_SECS", default_retry, 1)?,
            max_body_bytes: num("OXIDE_API_MAX_BODY_BYTES", 32 * 1024 * 1024, 1024)? as usize,
            body_timeout: Duration::from_secs(num("OXIDE_API_BODY_TIMEOUT_SECS", 30, 1)?),
            process_timeout: Duration::from_secs(num("OXIDE_API_PROCESS_TIMEOUT_SECS", 30, 1)?),
            max_text_chars: num("OXIDE_API_MAX_TEXT_CHARS", 4 * 1024 * 1024, 1)? as usize,
            max_debug_bytes: num("OXIDE_API_MAX_DEBUG_BYTES", 1024 * 1024, 1)? as usize,
            max_pdf_pages: num("OXIDE_API_MAX_PDF_PAGES", 2000, 1)? as usize,
            max_image_pixels: num("OXIDE_API_MAX_IMAGE_PIXELS", 64_000_000, 1)?,
            drain_delay: Duration::from_secs(num("OXIDE_API_DRAIN_DELAY_SECS", 2, 0)?),
            ocr_workers: num("OXIDE_API_OCR_WORKERS", 2, 1)? as usize,
            ocr_timeout: Duration::from_secs(num("OXIDE_API_OCR_TIMEOUT_SECS", 120, 1)?),
            ocr_languages,
            ocr_model_dir: var("OXIDE_API_OCR_MODEL_DIR").map_or_else(|| PathBuf::from("/models"), PathBuf::from),
            indra_cli: var("OXIDE_API_INDRA_CLI").map_or_else(|| PathBuf::from("indra-ocr"), PathBuf::from),
            indra_model_dir: var("OXIDE_API_INDRA_MODEL_DIR").map_or_else(|| PathBuf::from("/models/indra"), PathBuf::from),
            indra_math,
            indra_max_concurrent: num("OXIDE_API_INDRA_MAX_CONCURRENT", 1, 1)? as usize,
        })
    }

    /// ONNX Runtime intra-op threads per OCR worker.
    pub fn ocr_threads_per_worker(&self) -> usize {
        (self.cpu_threads / self.ocr_workers.max(1)).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn refuses_public_bind_without_key() {
        let err = cfg(&[]).unwrap_err();
        assert!(err.contains("OXIDE_API_API_KEY is required"), "{err}");
        assert!(cfg(&[("OXIDE_API_ADDR", "0.0.0.0:9000")]).is_err());
    }

    #[test]
    fn loopback_without_key_is_allowed() {
        let c = cfg(&[("OXIDE_API_ADDR", "127.0.0.1:8080")]).unwrap();
        assert!(c.api_key.is_none());
    }

    #[test]
    fn defaults() {
        let c = cfg(&[("OXIDE_API_API_KEY", "0123456789abcdef")]).unwrap();
        assert_eq!(c.max_inflight, 8);
        assert_eq!(c.max_body_bytes, 33_554_432);
        assert_eq!(c.max_text_chars, 4_194_304);
        assert_eq!(c.max_pdf_pages, 2000);
        assert_eq!(c.max_image_pixels, 64_000_000);
        assert_eq!(c.ocr_languages, vec!["en", "hi", "ar"]);
        assert!(c.indra_math);
        assert_eq!(c.indra_max_concurrent, 1);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(cfg(&[("OXIDE_API_API_KEY", "short")]).is_err());
        assert!(cfg(&[("OXIDE_API_API_KEY", "0123456789abcdef"), ("OXIDE_API_MAX_INFLIGHT", "x")]).is_err());
        assert!(cfg(&[("OXIDE_API_API_KEY", "0123456789abcdef"), ("OXIDE_API_OCR_LANGUAGES", "en,fr")]).is_err());
        assert!(cfg(&[("OXIDE_API_ADDR", "127.0.0.1:8080"), ("OXIDE_API_INDRA_MATH", "maybe")]).is_err());
        assert!(cfg(&[("OXIDE_API_ADDR", "127.0.0.1:8080"), ("OXIDE_API_INDRA_MAX_CONCURRENT", "0")]).is_err());
    }

    #[test]
    fn indra_math_can_be_disabled() {
        let c = cfg(&[("OXIDE_API_ADDR", "127.0.0.1:8080"), ("OXIDE_API_INDRA_MATH", "false")]).unwrap();
        assert!(!c.indra_math);
    }
}
