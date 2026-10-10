//! JSON vs raw output (by `Accept`), `X-Oxide-*`, `X-Engine`, `Server-Timing`.

use std::time::Duration;

use axum::http::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Json,
    Raw(&'static str),
}

/// Picks JSON (default) or the endpoint's raw media type from `Accept`,
/// honouring q-values. `raw` is `text/plain` or `text/markdown`.
pub fn negotiate(headers: &HeaderMap, raw: &'static str) -> Output {
    let Some(accept) = headers.get(ACCEPT).and_then(|v| v.to_str().ok()) else {
        return Output::Json;
    };
    let (mut json_q, mut raw_q) = (-1.0f32, -1.0f32);
    for item in accept.split(',') {
        let mut parts = item.split(';');
        let mt = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .find_map(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        match mt.as_str() {
            "application/json" | "application/*" | "*/*" => json_q = json_q.max(q),
            m if m == raw => raw_q = raw_q.max(q),
            "text/*" => raw_q = raw_q.max(q * 0.99),
            _ => {}
        }
    }
    if raw_q > 0.0 && raw_q > json_q { Output::Raw(raw) } else { Output::Json }
}

#[derive(Default)]
pub struct Timing {
    pub queue: Duration,
    pub parse: Duration,
    pub extract: Duration,
    pub ocr: Option<Duration>,
}

impl Timing {
    fn header(&self) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let mut s = format!(
            "queue;dur={:.1}, parse;dur={:.1}, extract;dur={:.1}",
            ms(self.queue),
            ms(self.parse),
            ms(self.extract)
        );
        if let Some(o) = self.ocr {
            s.push_str(&format!(", ocr;dur={:.1}", ms(o)));
        }
        s
    }
}

/// What produced a response, for `X-Engine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// `pdf_oxide`: PDFs, and OCR of images.
    Pdf,
    /// `office_oxide`: DOCX/XLSX/PPTX/DOC/XLS/PPT.
    Office,
    /// This service's own readers: OpenDocument, image headers.
    Native,
}

pub fn engine_header(engine: Engine) -> HeaderValue {
    let v = match engine {
        Engine::Pdf => format!("pdf_oxide/{}", pdf_oxide::VERSION),
        Engine::Office => format!("office_oxide/{}", office_oxide::VERSION),
        Engine::Native => format!("oxide-api/{}", env!("CARGO_PKG_VERSION")),
    };
    HeaderValue::from_str(&v).unwrap_or(HeaderValue::from_static("unknown"))
}

/// Common headers on every data response.
fn data_headers(h: &mut HeaderMap, engine: Engine, timing: &Timing) {
    h.insert(HeaderName::from_static("x-engine"), engine_header(engine));
    if let Ok(v) = HeaderValue::from_str(&timing.header()) {
        h.insert(HeaderName::from_static("server-timing"), v);
    }
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

pub fn json<T: Serialize>(body: &T, engine: Engine, timing: &Timing) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, bytes).into_response();
            let h = resp.headers_mut();
            h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            data_headers(h, engine, timing);
            resp
        }
        Err(_) => crate::error::ApiError::internal("failed to serialize the response").into_response(),
    }
}

/// Raw text body; metadata travels in `X-Oxide-*` headers.
pub fn raw(text: String, media: &'static str, engine: Engine, timing: &Timing, meta: &[(&'static str, String)]) -> Response {
    let mut resp = (StatusCode::OK, text).into_response();
    let h = resp.headers_mut();
    let ctype = if media == "text/markdown" { "text/markdown; charset=utf-8" } else { "text/plain; charset=utf-8" };
    h.insert(CONTENT_TYPE, HeaderValue::from_static(ctype));
    data_headers(h, engine, timing);
    for (name, value) in meta {
        if let Ok(v) = HeaderValue::from_str(value) {
            h.insert(HeaderName::from_static(name), v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(ACCEPT, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn negotiation() {
        assert_eq!(negotiate(&HeaderMap::new(), "text/plain"), Output::Json);
        assert_eq!(negotiate(&accept("*/*"), "text/plain"), Output::Json);
        assert_eq!(negotiate(&accept("text/plain"), "text/plain"), Output::Raw("text/plain"));
        assert_eq!(negotiate(&accept("text/markdown"), "text/plain"), Output::Json);
        assert_eq!(negotiate(&accept("text/markdown, */*;q=0.1"), "text/markdown"), Output::Raw("text/markdown"));
        assert_eq!(negotiate(&accept("application/json, text/plain;q=0.5"), "text/plain"), Output::Json);
        assert_eq!(negotiate(&accept("text/plain;q=0"), "text/plain"), Output::Json);
    }
}
