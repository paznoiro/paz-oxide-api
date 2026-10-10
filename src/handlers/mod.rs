pub mod extract;
pub mod info;

use std::time::Instant;

use tokio::sync::oneshot;

use crate::blocking::JobPanicked;
use crate::error::ApiError;
use crate::io::Params;

/// Waits for a CPU-pool job up to `deadline`. The job keeps running (and
/// keeps its permit) if the deadline passes first.
pub async fn await_job<T>(rx: oneshot::Receiver<Result<Result<T, ApiError>, JobPanicked>>, deadline: Instant) -> Result<T, ApiError> {
    match tokio::time::timeout_at(deadline.into(), rx).await {
        Err(_) => {
            metrics::counter!("oxide_processing_timeouts_total").increment(1);
            Err(ApiError::processing_timeout("the document could not be processed within the deadline"))
        }
        Ok(Err(_)) => Err(ApiError::internal("the worker dropped the job")),
        Ok(Ok(Err(JobPanicked))) => {
            metrics::counter!("oxide_engine_panics_total").increment(1);
            Err(ApiError::unprocessable(
                "corrupt-document",
                "Unreadable document",
                "the document triggered an internal parser error",
            ))
        }
        Ok(Ok(Ok(r))) => r,
    }
}

pub fn parse_bool(name: &str, v: &str) -> Result<bool, ApiError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ApiError::invalid_param(name, v, "true, false")),
    }
}

/// Params understood by any endpoint, regardless of document kind.
pub const COMMON_PARAMS: [&str; 2] = ["format", "filename"];

pub fn format_param(params: &Params) -> Result<Option<crate::detect::DocKind>, ApiError> {
    params
        .get("format")
        .map(|v| crate::detect::DocKind::from_name(v).ok_or_else(|| ApiError::invalid_param("format", v, crate::detect::ACCEPTED_FORMATS)))
        .transpose()
}
