//! Bearer token on `/v1/*`, compared in constant time.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;
use crate::error::ApiError;

pub async fn require_bearer(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Some(key) = st.cfg.api_key.as_deref() else {
        // Only reachable on a loopback bind (config refuses otherwise).
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        });
    match presented {
        Some(token) if ct_eq(token.as_bytes(), key.as_bytes()) => next.run(req).await,
        _ => {
            metrics::counter!("oxide_auth_failures_total").increment(1);
            crate::admission::reject(&st, req, ApiError::unauthorized())
        }
    }
}

/// Constant-time equality: runs over the whole expected key regardless of
/// where (or whether) the inputs differ.
fn ct_eq(presented: &[u8], expected: &[u8]) -> bool {
    let mut diff = (presented.len() != expected.len()) as u8;
    for (i, e) in expected.iter().enumerate() {
        diff |= presented.get(i).copied().unwrap_or(!e) ^ e;
    }
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::ct_eq;

    #[test]
    fn compares() {
        assert!(ct_eq(b"secret-token-1234", b"secret-token-1234"));
        assert!(!ct_eq(b"secret-token-1235", b"secret-token-1234"));
        assert!(!ct_eq(b"secret", b"secret-token-1234"));
        assert!(!ct_eq(b"secret-token-1234-extra", b"secret-token-1234"));
        assert!(!ct_eq(b"", b"secret-token-1234"));
    }
}
