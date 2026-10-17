//! Oxide API: text and markdown extraction from PDF, Office and OpenDocument
//! files, with OCR for scanned PDFs and images, and named fields read out of
//! that text by regular expressions.

pub mod admission;
pub mod auth;
pub mod blocking;
pub mod config;
pub mod detect;
pub mod error;
pub mod fields;
pub mod handlers;
pub mod io;
pub mod metrics;
#[cfg(feature = "ocr")]
pub mod ocr;
pub mod odf;
pub mod office;
pub mod pdf;
pub mod raster;
pub mod respond;
pub mod text;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;
use tokio::sync::Semaphore;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::blocking::{CpuPool, Watchdog};
use crate::config::Config;

#[derive(Default)]
pub struct Health {
    ready: AtomicBool,
    draining: AtomicBool,
}

impl Health {
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    pub fn start_draining(&self) {
        self.draining.store(true, Ordering::Release);
    }
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }
}

pub struct AppState {
    pub cfg: Config,
    pub admission: Arc<Semaphore>,
    pub cpu: CpuPool,
    pub watchdog: Arc<Watchdog>,
    pub health: Arc<Health>,
    pub metrics: Option<PrometheusHandle>,
    pub started: Instant,
    #[cfg(feature = "ocr")]
    pub ocr: Option<Arc<ocr::OcrService>>,
}

/// Builds shared state and starts background work (OCR workers). Readiness
/// flips once OCR has finished loading (immediately on `:slim`).
pub fn build_state(cfg: Config) -> Arc<AppState> {
    office_oxide::limits::set_max_text_chars(cfg.max_text_chars);
    let watchdog = Arc::new(Watchdog::default());
    let health = Arc::new(Health::default());

    #[cfg(feature = "ocr")]
    let ocr = {
        let langs: Vec<pdf::lang::Lang> = cfg.ocr_languages.iter().filter_map(|l| pdf::lang::Lang::from_code(l)).collect();
        let h = health.clone();
        let settings = ocr::OcrSettings {
            model_dir: cfg.ocr_model_dir.clone(),
            langs,
            workers: cfg.ocr_workers,
            threads_per_worker: cfg.ocr_threads_per_worker(),
            deadline: cfg.ocr_timeout,
        };
        Some(ocr::OcrService::start(settings, watchdog.clone(), move || h.set_ready()))
    };
    #[cfg(not(feature = "ocr"))]
    health.set_ready();

    Arc::new(AppState {
        admission: Arc::new(Semaphore::new(cfg.max_inflight)),
        cpu: CpuPool::new(cfg.cpu_threads, watchdog.clone()),
        watchdog,
        health,
        metrics: metrics::install(),
        started: Instant::now(),
        #[cfg(feature = "ocr")]
        ocr,
        cfg,
    })
}

/// Periodically checks the watchdog (jobs running > 10x their deadline).
pub fn spawn_watchdog(st: &Arc<AppState>) {
    let dog = st.watchdog.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if dog.check() {
                break;
            }
        }
    });
}

const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

pub fn build_router(st: Arc<AppState>) -> Router {
    let v1 = Router::new()
        .route("/v1/extract/text", post(handlers::extract::text))
        .route("/v1/extract/markdown", post(handlers::extract::markdown))
        .route("/v1/extract/fields", post(handlers::extract::fields))
        .route("/v1/info", post(handlers::info::info))
        // Layers run outside-in: auth first, then admission, so an
        // unauthenticated request never takes a permit.
        .route_layer(axum::middleware::from_fn_with_state(st.clone(), admission::admit))
        .route_layer(axum::middleware::from_fn_with_state(st.clone(), auth::require_bearer));

    let ops = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/metrics", get(metrics_handler))
        .route("/openapi.json", get(openapi_json))
        .route("/openapi.yaml", get(openapi_yaml));
    #[cfg(feature = "docs")]
    let ops = ops.route("/docs", get(docs));

    let x_request_id = HeaderName::from_static("x-request-id");
    v1.merge(ops)
        .layer(DefaultBodyLimit::max(st.cfg.max_body_bytes))
        .layer(axum::middleware::from_fn(metrics::track))
        .layer(
            TraceLayer::new_for_http()
                // Method + path + request id only: never query strings,
                // bodies, filenames or extracted text.
                .make_span_with(|req: &axum::http::Request<_>| {
                    let id = req.headers().get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or("");
                    tracing::info_span!("request", method = %req.method(), path = %req.uri().path(), request_id = %id)
                })
                .on_request(())
                .on_response(tower_http::trace::DefaultOnResponse::new().level(tracing::Level::INFO)),
        )
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .layer(SetSensitiveRequestHeadersLayer::new([axum::http::header::AUTHORIZATION]))
        .layer(SetResponseHeaderLayer::if_not_present(CONTENT_SECURITY_POLICY, HeaderValue::from_static(API_CSP)))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(HeaderName::from_static("x-frame-options"), HeaderValue::from_static("DENY")))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .with_state(st)
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn healthz(State(st): State<Arc<AppState>>) -> Response {
    let r = if st.watchdog.is_tripped() {
        (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"status": "unhealthy", "reason": "watchdog"}))).into_response()
    } else {
        axum::Json(json!({"status": "ok"})).into_response()
    };
    no_store(r)
}

async fn readyz(State(st): State<Arc<AppState>>) -> Response {
    let reason = if st.health.is_draining() {
        Some("draining")
    } else if st.watchdog.is_tripped() {
        Some("watchdog")
    } else if !st.health.is_ready() {
        Some("starting")
    } else {
        None
    };
    no_store(match reason {
        Some(r) => (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"status": "not_ready", "reason": r}))).into_response(),
        None => axum::Json(json!({"status": "ready"})).into_response(),
    })
}

async fn version(State(st): State<Arc<AppState>>) -> Response {
    #[cfg(feature = "ocr")]
    let ocr = st.ocr.as_ref().map_or_else(|| json!({"compiled": true, "status": "disabled"}), |o| o.status_json());
    #[cfg(not(feature = "ocr"))]
    let ocr = json!({"compiled": false, "status": "unavailable"});
    let c = &st.cfg;
    no_store(
        axum::Json(json!({
            "name": "oxide-api",
            "version": env!("CARGO_PKG_VERSION"),
            "engines": { "pdf_oxide": pdf_oxide::VERSION, "office_oxide": office_oxide::VERSION },
            "uptime_secs": st.started.elapsed().as_secs(),
            "limits": {
                "max_body_bytes": c.max_body_bytes,
                "max_inflight": c.max_inflight,
                "queue_wait_ms": c.queue_wait.as_millis() as u64,
                "process_timeout_secs": c.process_timeout.as_secs(),
                "ocr_timeout_secs": c.ocr_timeout.as_secs(),
                "max_text_chars": c.max_text_chars,
                "max_pdf_pages": c.max_pdf_pages,
                "max_image_pixels": c.max_image_pixels,
                "cpu_threads": st.cpu.threads(),
            },
            "auth": c.api_key.is_some(),
            "ocr": ocr,
        }))
        .into_response(),
    )
}

async fn metrics_handler(State(st): State<Arc<AppState>>) -> Response {
    match &st.metrics {
        Some(h) => {
            let mut r = h.render().into_response();
            r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"));
            no_store(r)
        }
        None => (StatusCode::NOT_FOUND, "metrics recorder not installed").into_response(),
    }
}

pub const OPENAPI_JSON: &str = include_str!("../openapi.json");
pub const OPENAPI_YAML: &str = include_str!("../openapi.yaml");

async fn openapi_json() -> Response {
    ([(CONTENT_TYPE, "application/json")], OPENAPI_JSON).into_response()
}

async fn openapi_yaml() -> Response {
    ([(CONTENT_TYPE, "application/yaml")], OPENAPI_YAML).into_response()
}

#[cfg(feature = "docs")]
async fn docs() -> Response {
    const PAGE: &str = r#"<!doctype html>
<html>
<head>
  <title>Oxide API</title>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
</head>
<body>
  <script id="api-reference" data-url="/openapi.json"></script>
  <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1"></script>
</body>
</html>"#;
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src https://cdn.jsdelivr.net 'unsafe-inline'; \
                 style-src https://cdn.jsdelivr.net https://fonts.googleapis.com 'unsafe-inline'; \
                 font-src https://cdn.jsdelivr.net https://fonts.gstatic.com data:; img-src 'self' data: https:; \
                 connect-src 'self'; frame-ancestors 'none'",
            ),
        ],
        PAGE,
    )
        .into_response()
}
