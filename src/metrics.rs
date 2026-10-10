//! Prometheus metrics: RED per route, queue wait, in-flight, 429s, documents
//! per kind/format, OCR pages by language, OCR page latency, `lang_source`,
//! engine builds.

use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<Option<PrometheusHandle>> = OnceLock::new();

const SECONDS_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// Installs the global recorder once per process.
pub fn install() -> Option<PrometheusHandle> {
    HANDLE
        .get_or_init(|| {
            let handle = PrometheusBuilder::new()
                .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), SECONDS_BUCKETS)
                .ok()?
                .install_recorder()
                .ok()?;
            describe();
            Some(handle)
        })
        .clone()
}

fn describe() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};
    describe_counter!("oxide_http_requests_total", "HTTP requests by route and status");
    describe_histogram!("oxide_http_request_duration_seconds", "HTTP request latency by route");
    describe_histogram!("oxide_admission_wait_seconds", "Time spent waiting for an admission permit");
    describe_gauge!("oxide_inflight_jobs", "Admitted requests whose work hasn't finished");
    describe_counter!("oxide_rejected_total", "Requests rejected by admission (429)");
    describe_counter!("oxide_documents_total", "Documents received, by kind and format");
    describe_counter!("oxide_ocr_pages_total", "OCR'd pages by language and outcome");
    describe_histogram!("oxide_ocr_page_duration_seconds", "OCR latency per page, by language");
    describe_counter!("oxide_lang_source_total", "How the OCR language was chosen");
    describe_counter!("oxide_ocr_engine_builds_total", "ONNX Runtime sessions built (startup only)");
    describe_counter!("oxide_processing_timeouts_total", "Requests that hit the processing deadline");
    describe_counter!("oxide_engine_panics_total", "Engine panics caught in worker pools");
    describe_counter!("oxide_auth_failures_total", "Requests with a missing or wrong bearer token");
}

pub async fn track(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_string(), |p| p.as_str().to_string());
    let start = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16().to_string();
    metrics::counter!("oxide_http_requests_total", "route" => route.clone(), "status" => status).increment(1);
    metrics::histogram!("oxide_http_request_duration_seconds", "route" => route).record(start.elapsed().as_secs_f64());
    resp
}
