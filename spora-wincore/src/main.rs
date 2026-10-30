mod backend;
mod paths;
mod relays;
mod secret;
mod service;
mod session;
#[cfg(windows)]
mod pipe;
#[cfg(windows)]
mod winsvc;

pub mod proto {
    tonic::include_proto!("spora");
}

use std::sync::Arc;

use log::info;
use tonic::transport::Server;

use crate::proto::spora_server::SporaServer;
use crate::service::SporaService;
use crate::session::SessionStore;

/// Control-channel endpoint on Windows. Must match the client constant in
/// spora-winui/src/grpc.rs.
#[cfg(windows)]
const PIPE_NAME: &str = r"\\.\pipe\spora-wincore";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let service_mode = args.iter().any(|a| a == "--service");
    let log_file = args
        .windows(2)
        .find(|w| w[0] == "--log-file")
        .map(|w| w[1].clone());

    init_logging(log_file, service_mode);
    log_build();

    #[cfg(windows)]
    if service_mode {
        // Hand the process over to the SCM dispatcher; blocks until the
        // service is stopped.
        if let Err(e) = winsvc::run() {
            log::error!("service dispatcher failed: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(not(windows))]
    if service_mode {
        log::warn!("--service is only meaningful on Windows; running in console mode");
    }

    // Console mode (Linux dev builds, or manual debugging on Windows).
    let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
    let shutdown = Arc::new(tokio::sync::Notify::new());
    if let Err(e) = rt.block_on(run_server(shutdown)) {
        log::error!("server error: {e}");
        std::process::exit(1);
    }
}

/// The process logger, behind spora-service's composite logger: the
/// diagnostic log file (`%ProgramData%\Spora\diag`, JSON Lines, its own
/// fixed levels, share-link secrets redacted — what bug reports and uploads
/// carry) plus the human-readable log as `RUST_LOG` says (default info):
/// `spora-wincore.log` under the SCM, stderr in console mode unless
/// `--log-file` names a file. Crash files from a panic hook land beside the
/// diagnostic log.
fn init_logging(log_file: Option<String>, service_mode: bool) {
    use spora_service::diag::{self, DiagConfig, Secondary};

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder
        .filter_module("quinn", log::LevelFilter::Warn)
        .filter_module("quinn_proto", log::LevelFilter::Warn)
        .filter_module("quinn_udp", log::LevelFilter::Warn)
        .filter_module("smoltcp", log::LevelFilter::Warn);
    // Raw HTTP traces carry the upload token: no sink gets them (the
    // composite logger withholds them whatever the filters say).
    for target in diag::WITHHELD_TARGETS {
        builder.filter_module(target, log::LevelFilter::Off);
    }

    // Under the SCM there is no console, so default to a file under
    // %ProgramData%\Spora.
    let path = log_file.map(std::path::PathBuf::from).or_else(|| {
        if service_mode {
            let _ = std::fs::create_dir_all(paths::data_dir());
            Some(paths::service_log())
        } else {
            None
        }
    });
    let mut file_error = None;
    if let Some(p) = &path {
        match std::fs::OpenOptions::new().create(true).append(true).open(p) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            // A logging failure must not keep the service from starting.
            Err(e) => file_error = Some(format!("failed to open log file {}: {e}", p.display())),
        }
    }
    let secondary = builder.build();
    let level = secondary.filter();

    let diag_dir = paths::diag_dir();
    let diag_error = paths::ensure_private_dir(&diag_dir)
        .err()
        .map(|e| format!("diagnostic log directory {}: {e}", diag_dir.display()));
    diag::init(
        Some(DiagConfig::in_dir(diag_dir).with_platform(backend::platform())),
        Some(Secondary::new(Box::new(secondary), level)),
    );
    diag::install_panic_hook();

    if let (Some(p), None) = (&path, &file_error) {
        info!("Logging to file: {}", p.display());
    }
    for e in [file_error, diag_error].into_iter().flatten() {
        log::warn!("{e}");
    }
}

/// Names this build at the top of every run, so the service log says which
/// release (and which core commit) produced what follows.
fn log_build() {
    let core = spora_core::record::build_info();
    info!(
        "spora-wincore {} ({}, core {} {}{})",
        env!("CARGO_PKG_VERSION"),
        core.profile,
        core.version,
        core.commit.as_deref().unwrap_or("unknown-commit"),
        if core.dirty { "-dirty" } else { "" },
    );
}

/// Runs the gRPC control server until `shutdown` fires, then tears down any
/// sessions that are still alive (cancelling tunnels and removing routes).
async fn run_server(
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sessions = Arc::new(SessionStore::new());
    let backend = Arc::new(backend::Backend::new());
    // The device identity, the relay access and the uploader: opened now so
    // the relay token is renewed and uploads run before any UI asks. Local
    // files only; a failure is retried when something needs it.
    {
        let backend = backend.clone();
        tokio::task::spawn_blocking(move || backend.telemetry());
    }
    let service = SporaService::new(sessions.clone(), backend, shutdown.clone());

    // Stop the sessions as part of the shutdown signal: their event streams
    // end, so tonic's graceful drain below isn't held open forever by
    // long-lived streaming RPCs.
    let drain = {
        let sessions = sessions.clone();
        async move {
            shutdown.notified().await;
            info!("shutdown requested — stopping all sessions");
            sessions.shutdown_all();
        }
    };

    #[cfg(windows)]
    {
        info!("spora-wincore gRPC server listening on {}", PIPE_NAME);
        Server::builder()
            .add_service(SporaServer::new(service))
            .serve_with_incoming_shutdown(pipe::incoming(PIPE_NAME)?, drain)
            .await?;
    }

    #[cfg(not(windows))]
    {
        let addr = "[::1]:50051".parse::<std::net::SocketAddr>()?;
        info!("spora-wincore gRPC server listening on {}", addr);
        Server::builder()
            .add_service(SporaServer::new(service))
            .serve_with_shutdown(addr, drain)
            .await?;
    }

    // Backstop for sessions created while the server was draining.
    sessions.shutdown_all();
    // One last upload pass (only with uploads on), so the records the stop
    // just closed go now rather than after the next start; short, since a
    // system shutdown gives services a few seconds.
    let _ = tokio::task::spawn_blocking(|| {
        spora_service::finish(FINAL_UPLOAD_WAIT);
        spora_service::diag::flush(std::time::Duration::from_secs(1));
    })
    .await;
    Ok(())
}

const FINAL_UPLOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
