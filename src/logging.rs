//! One completion event per request, except the frequent liveness probe.

use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;

use crate::error::ProblemLog;

pub async fn requests(req: Request, next: Next) -> Response {
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }

    // Never log query strings, authorization, bodies or engine error details:
    // those can contain credentials, filenames or extracted document content.
    let id = req.headers().get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or("");
    let span = tracing::info_span!("request", method = %req.method(), path = %req.uri().path(), request_id = %id);
    async move {
        let start = Instant::now();
        let response = next.run(req).await;
        let status = response.status().as_u16();
        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        let problem = response.extensions().get::<ProblemLog>();
        let problem_type = problem.map(|p| p.slug);
        let error = problem.map(|p| p.title);
        if response.status().is_server_error() {
            tracing::error!(status, duration_ms, problem = problem_type, error, "request failed");
        } else if response.status().is_client_error() {
            tracing::warn!(status, duration_ms, problem = problem_type, error, "request rejected");
        } else {
            tracing::info!(status, duration_ms, "request completed");
        }
        response
    }
    .instrument(span)
    .await
}
