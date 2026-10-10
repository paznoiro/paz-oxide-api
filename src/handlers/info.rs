//! `/v1/info`: format, metadata and structure without extracting text.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use super::{COMMON_PARAMS, await_job, format_param};
use crate::AppState;
use crate::admission::Admitted;
use crate::detect::{self, DocKind};
use crate::error::ApiError;
use crate::io;
use crate::respond::{self, Engine, Timing};

pub async fn info(State(st): State<Arc<AppState>>, req: Request) -> Response {
    run(st, req).await.unwrap_or_else(IntoResponse::into_response)
}

#[derive(Serialize)]
struct WithIgnored<T: Serialize> {
    #[serde(flatten)]
    inner: T,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ignored_params: Vec<String>,
}

async fn run(st: Arc<AppState>, mut req: Request) -> Result<Response, ApiError> {
    let admitted = req
        .extensions_mut()
        .remove::<Admitted>()
        .ok_or_else(|| ApiError::internal("admission state missing"))?;
    let input = tokio::time::timeout(st.cfg.body_timeout, io::read_input(req, st.cfg.max_body_bytes))
        .await
        .map_err(|_| ApiError::body_timeout())??;
    let hint = format_param(&input.params)?;
    let detected = detect::detect(&input.bytes, hint, input.filename.as_deref())?;
    let ignored: Vec<String> = input.params.keys().filter(|k| !COMMON_PARAMS.contains(&k.as_str())).cloned().collect();
    let deadline = admitted.admitted_at + st.cfg.process_timeout;
    let source = detected.source;
    let bytes = input.bytes;
    let (max_pages, max_chars, max_pixels) = (st.cfg.max_pdf_pages, st.cfg.max_text_chars, st.cfg.max_image_pixels);
    let t = Instant::now();
    let engine = match detected.kind {
        DocKind::Pdf => Engine::Pdf,
        DocKind::Office(f) if !f.is_odf() => Engine::Office,
        DocKind::Office(_) | DocKind::Image(_) => Engine::Native,
    };
    let rx = st.cpu.spawn(admitted.permit.clone(), st.cfg.process_timeout, move || match detected.kind {
        DocKind::Pdf => crate::pdf::info(bytes, max_pages, source).and_then(|i| to_value(&i)),
        DocKind::Office(fmt) => crate::office::info(bytes, fmt, source, max_chars).and_then(|i| to_value(&i)),
        DocKind::Image(fmt) => crate::raster::probe(bytes, fmt, max_pixels, max_pages)
            .and_then(|doc| to_value(&crate::raster::info(&doc, source))),
    });
    let value = await_job(rx, deadline).await?;
    let timing = Timing { queue: admitted.queue_wait, parse: t.elapsed(), ..Default::default() };
    Ok(respond::json(&WithIgnored { inner: value, ignored_params: ignored }, engine, &timing))
}

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(v).map_err(|_| ApiError::internal("failed to serialize document info"))
}
