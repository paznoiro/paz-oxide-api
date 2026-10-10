use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use mimalloc::MiMalloc;
use oxide_api::config::Config;
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(healthcheck());
    }
    init_logging();
    install_panic_hook();

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "invalid configuration; refusing to start");
            std::process::exit(2);
        }
    };

    // The async runtime only does I/O; cores belong to rayon and ONNX Runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("io")
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    if let Err(e) = rt.block_on(serve(cfg)) {
        tracing::error!(error = %e, "server error");
        std::process::exit(1);
    }
}

async fn serve(cfg: Config) -> std::io::Result<()> {
    let addr = cfg.addr;
    let drain_delay = cfg.drain_delay;
    if cfg.api_key.is_none() {
        tracing::warn!(%addr, "no OXIDE_API_API_KEY set: /v1/* is unauthenticated (loopback bind only)");
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let st = oxide_api::build_state(cfg);
    oxide_api::spawn_watchdog(&st);
    tracing::info!(
        %addr,
        ocr = cfg!(feature = "ocr"),
        cpu_threads = st.cpu.threads(),
        max_inflight = st.cfg.max_inflight,
        "oxide-api listening"
    );
    let health = st.health.clone();
    let app = oxide_api::build_router(st);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Flip /readyz and refuse new /v1 work first, give the load
            // balancer time to notice, then stop accepting connections and
            // let in-flight requests finish.
            tracing::info!(drain_secs = drain_delay.as_secs(), "shutdown requested; draining");
            health.start_draining();
            tokio::time::sleep(drain_delay).await;
        })
        .await?;
    tracing::info!("shut down cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}

fn init_logging() {
    // Engines log document-derived strings at info/warn; keep them quiet.
    let default = "info,pdf_oxide=error,office_oxide=error,ort=warn";
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt().json().with_env_filter(filter).with_current_span(true).with_span_list(false).with_target(false).init();
}

/// Engine panics are caught by the pools; log where, never the payload
/// (it can quote document content).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        tracing::error!(%location, %thread, "panic caught (payload suppressed)");
    }));
}

/// `oxide-api healthcheck`: the image has no shell or curl, so the binary
/// probes its own `/healthz` for Docker's HEALTHCHECK.
fn healthcheck() -> i32 {
    let addr_s = std::env::var("OXIDE_API_ADDR").unwrap_or_else(|_| oxide_api::config::DEFAULT_ADDR.into());
    let Ok(mut addr) = addr_s.parse::<SocketAddr>() else { return 1 };
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() { [127, 0, 0, 1].into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    let probe = || -> std::io::Result<bool> {
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf)?;
        let head = String::from_utf8_lossy(&buf[..n]);
        Ok(head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200"))
    };
    match probe() {
        Ok(true) => 0,
        _ => 1,
    }
}
