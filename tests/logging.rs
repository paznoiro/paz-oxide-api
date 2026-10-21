//! Exercise JSON access logs, severity, correlation, and sensitive-data handling.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use oxide_api::error::ApiError;
use serde_json::Value;
use tower::ServiceExt;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tracing::instrument::WithSubscriber;

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn logs_requests_and_errors_but_never_health_probes_or_sensitive_data() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    async {
        let request_id = axum::http::HeaderName::from_static("x-request-id");
        let app = Router::new()
            .route("/healthz", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
            .route("/readyz", get(|| async { StatusCode::OK }))
            .route("/version", get(|| async { StatusCode::OK }))
            .route("/rejected", get(|| async { ApiError::unauthorized() }))
            .route("/failed", get(|| async { ApiError::internal("private-document-contents") }))
            .layer(axum::middleware::from_fn(oxide_api::logging::requests))
            .layer(PropagateRequestIdLayer::new(request_id.clone()))
            .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid));
        for (path, status) in [
            ("/healthz?token=private-query", 503),
            ("/readyz", 200),
            ("/version", 200),
            ("/rejected", 401),
            ("/failed", 500),
            ("/missing", 404),
        ] {
            let response = app.clone().oneshot(
                Request::builder().uri(path).header("authorization", "Bearer private-token")
                    .header("x-request-id", "log-correlation-id")
                    .body(Body::from("private-body")).unwrap(),
            ).await.unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.headers()["x-request-id"], "log-correlation-id");
        }
    }
    .with_subscriber(subscriber)
    .await;
    let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let events: Vec<Value> = output.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(events.len(), 5, "{output}");
    for (event, (path, status, level)) in events.iter().zip([
        ("/readyz", 200, "INFO"), ("/version", 200, "INFO"),
        ("/rejected", 401, "WARN"), ("/failed", 500, "ERROR"), ("/missing", 404, "WARN"),
    ]) {
        assert_eq!(event["level"], level);
        assert_eq!(event["fields"]["status"], status);
        assert!(event["fields"]["duration_ms"].is_number());
        assert_eq!(event["span"]["path"], path);
        assert_eq!(event["span"]["method"], "GET");
        assert_eq!(event["span"]["request_id"], "log-correlation-id");
    }
    assert_eq!(events[2]["fields"]["problem"], "unauthorized");
    assert_eq!(events[3]["fields"]["problem"], "internal-error");
    assert_eq!(events[3]["fields"]["error"], "Internal error");
    assert!(!output.contains("private-"), "{output}");
    assert!(!output.contains("/healthz"), "{output}");
}
