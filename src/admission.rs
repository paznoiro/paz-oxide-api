//! Admission control: a semaphore permit is acquired *before* the request
//! body is read, so a waiting request costs a socket, not 32 MiB.
//!
//! The permit is handed to the handler, which moves it into the job(s) it
//! spawns. It is released only when the last holder (handler or job) drops
//! it, so work can never pile up beyond `OXIDE_API_MAX_INFLIGHT`, even when
//! a request times out while its job is still running.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::OwnedSemaphorePermit;

use crate::AppState;
use crate::error::ApiError;

/// One in-flight slot. Shared between the handler and its jobs.
pub struct Permit {
    _permit: OwnedSemaphorePermit,
}

impl Drop for Permit {
    fn drop(&mut self) {
        metrics::gauge!("oxide_inflight_jobs").decrement(1.0);
    }
}

pub type JobPermit = Arc<Permit>;

#[derive(Clone)]
pub struct Admitted {
    pub permit: JobPermit,
    pub admitted_at: Instant,
    pub queue_wait: Duration,
}

/// Answers without reading the body, draining it in the background so the
/// client actually receives the response.
pub fn reject(st: &AppState, req: Request, err: ApiError) -> Response {
    crate::io::drain_in_background(req.into_body(), st.cfg.max_body_bytes, st.cfg.body_timeout);
    err.into_response()
}

pub async fn admit(State(st): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    if st.health.is_draining() {
        return reject(&st, req, ApiError::draining());
    }
    let start = Instant::now();
    let sem = st.admission.clone();
    let acquired = match sem.clone().try_acquire_owned() {
        Ok(p) => Some(p),
        Err(_) => tokio::time::timeout(st.cfg.queue_wait, sem.acquire_owned()).await.ok().and_then(Result::ok),
    };
    let Some(permit) = acquired else {
        metrics::counter!("oxide_rejected_total", "reason" => "busy").increment(1);
        return reject(&st, req, ApiError::busy(st.cfg.retry_after_secs));
    };
    let queue_wait = start.elapsed();
    metrics::histogram!("oxide_admission_wait_seconds").record(queue_wait.as_secs_f64());
    metrics::gauge!("oxide_inflight_jobs").increment(1.0);
    req.extensions_mut().insert(Admitted {
        permit: Arc::new(Permit { _permit: permit }),
        admitted_at: Instant::now(),
        queue_wait,
    });
    next.run(req).await
}
