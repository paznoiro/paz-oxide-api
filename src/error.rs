//! `ApiError` → RFC 9457 `application/problem+json`, plus engine error mapping.
//!
//! Engine error messages go to the caller (they describe the caller's own
//! document) but never into logs: the request middleware logs the problem
//! slug and static title from a response extension.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

#[derive(Clone)]
pub(crate) struct ProblemLog {
    pub slug: &'static str,
    pub title: &'static str,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub slug: &'static str,
    pub title: &'static str,
    pub detail: String,
    pub retry_after: Option<u64>,
    pub extra: Map<String, Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, slug: &'static str, title: &'static str, detail: impl Into<String>) -> Self {
        Self { status, slug, title, detail: detail.into(), retry_after: None, extra: Map::new() }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_string(), value.into());
        self
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad-request", "Bad request", detail)
    }

    pub fn invalid_param(name: &str, value: &str, accepted: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid-parameter",
            "Invalid parameter",
            format!("invalid value {value:?} for `{name}`; accepted: {accepted}"),
        )
        .with("parameter", name)
        .with("accepted", accepted)
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Unauthorized",
            "a valid `Authorization: Bearer <key>` header is required",
        )
    }

    pub fn body_timeout() -> Self {
        Self::new(StatusCode::REQUEST_TIMEOUT, "upload-timeout", "Upload timed out", "the request body was not received in time")
    }

    pub fn too_large(limit: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload-too-large",
            "Payload too large",
            format!("the request body exceeds the {limit}-byte limit"),
        )
        .with("limit_bytes", limit)
    }

    pub fn busy(retry_after: u64) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too-many-requests",
            "Too many requests",
            "all processing slots are busy; retry later",
        );
        e.retry_after = Some(retry_after);
        e
    }

    pub fn draining() -> Self {
        let mut e = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting-down",
            "Service is shutting down",
            "this instance is draining; retry on another instance",
        );
        e.retry_after = Some(1);
        e
    }

    pub fn unprocessable(slug: &'static str, title: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, slug, title, detail)
    }

    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::unprocessable("unsupported-format", "Unsupported document format", detail)
    }

    pub fn processing_timeout(detail: impl Into<String>) -> Self {
        Self::unprocessable("processing-timeout", "Processing timed out", detail)
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal-error", "Internal error", detail)
    }

    pub fn from_pdf(err: &pdf_oxide::Error) -> Self {
        use pdf_oxide::Error as E;
        match err {
            E::EncryptedPdf => Self::unprocessable(
                "encrypted-document",
                "Encrypted document",
                "the PDF is password-protected and cannot be opened without the user password",
            ),
            E::RecursionLimitExceeded(_) | E::CircularReference(_) => {
                Self::unprocessable("corrupt-document", "Corrupt document", format!("the PDF structure is invalid: {err}"))
            }
            _ => Self::unprocessable("corrupt-document", "Unreadable document", format!("the PDF could not be read: {err}")),
        }
    }

    pub fn from_office(err: &office_oxide::OfficeError) -> Self {
        use office_oxide::OfficeError as E;
        match err {
            E::UnsupportedFormat(msg) if msg.contains("encrypted") || msg.contains("password") => Self::unprocessable(
                "encrypted-document",
                "Encrypted document",
                "the Office document is password-protected (encrypted OOXML); decryption is not supported",
            ),
            E::UnsupportedFormat(msg) => Self::unsupported(msg.clone()),
            E::Panic(_) => Self::unprocessable("corrupt-document", "Corrupt document", "the document could not be parsed"),
            _ => Self::unprocessable("corrupt-document", "Unreadable document", format!("the document could not be read: {err}")),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.slug, self.status)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "type": format!("/problems/{}", self.slug),
            "title": self.title,
            "status": self.status.as_u16(),
            "detail": self.detail,
        });
        if let Value::Object(m) = &mut body {
            m.extend(self.extra);
        }
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        let mut resp = (self.status, bytes).into_response();
        resp.extensions_mut().insert(ProblemLog { slug: self.slug, title: self.title });
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if let Some(secs) = self.retry_after {
            h.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        if self.status == StatusCode::UNAUTHORIZED {
            h.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        resp
    }
}
