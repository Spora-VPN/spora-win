mod backend;
mod paths;
#[cfg(windows)]
mod pipe;
mod relays;
mod secret;
mod service;
mod session;
mod users;
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
use crate::users::Users;

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
/// diagnostic log file (JSON Lines, its own fixed levels, share-link secrets
/// redacted — what bug reports and uploads carry) plus the human-readable
/// log as `RUST_LOG` says (default info): `spora-wincore.log` under the SCM,
/// stderr in console mode unless `--log-file` names a file. The diagnostic
/// log is the served user's, in their directory (crash files from the
/// panic hook beside it): none until a user is served (see `users.rs`).
fn init_logging(log_file: Option<String>, service_mode: bool) {
    use spora_service::diag::{self, Secondary};

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

    // Everything the service keeps is private to it, the human-readable log
    // (every user's sessions) included: closed before anything is written.
    let data = paths::data_dir();
    let data_error = paths::ensure_private_dir(&data)
        .err()
        .map(|e| format!("data directory {}: {e}", data.display()));
    // Under the SCM there is no console, so default to a file under
    // %ProgramData%\Spora.
    let path = log_file
        .map(std::path::PathBuf::from)
        .or_else(|| service_mode.then(paths::service_log));
    let mut file_error = None;
    if let Some(p) = &path {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
        {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            // A logging failure must not keep the service from starting.
            Err(e) => file_error = Some(format!("failed to open log file {}: {e}", p.display())),
        }
    }
    let secondary = builder.build();
    let level = secondary.filter();

    diag::init(None, Some(Secondary::new(Box::new(secondary), level)));
    diag::install_panic_hook();

    if let (Some(p), None) = (&path, &file_error) {
        info!("Logging to file: {}", p.display());
    }
    for e in [data_error, file_error].into_iter().flatten() {
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
    // Whose state is open, and whose calls are refused meanwhile.
    let users = Users::new(sessions.clone());
    let service = SporaService::new(sessions.clone(), users.clone(), shutdown.clone());

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
            .serve_with_incoming_shutdown(pipe::incoming(PIPE_NAME, users.clone())?, drain)
            .await?;
    }

    #[cfg(not(windows))]
    {
        let addr = "[::1]:50051".parse::<std::net::SocketAddr>()?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!("spora-wincore gRPC server listening on {}", addr);
        Server::builder()
            .add_service(SporaServer::new(service))
            .serve_with_incoming_shutdown(users::tcp_incoming(listener, users.clone()), drain)
            .await?;
    }

    // Backstop for sessions created while the server was draining.
    sessions.shutdown_all();
    // The user served is let go: one last upload pass (only with uploads
    // on), so the records the stop just closed go now rather than after the
    // next start, and their log file is written out.
    users.shutdown().await;
    Ok(())
}
