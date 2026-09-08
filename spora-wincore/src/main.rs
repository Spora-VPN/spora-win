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

fn init_logging(log_file: Option<String>, service_mode: bool) {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder
        .filter_module("quinn", log::LevelFilter::Warn)
        .filter_module("quinn_proto", log::LevelFilter::Warn)
        .filter_module("quinn_udp", log::LevelFilter::Warn);

    // Under the SCM there is no console, so default to a file under
    // %ProgramData%\Spora.
    let path = log_file.map(std::path::PathBuf::from).or_else(|| {
        if service_mode {
            let dir = std::path::PathBuf::from(
                std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string()),
            )
            .join("Spora");
            let _ = std::fs::create_dir_all(&dir);
            Some(dir.join("spora-wincore.log"))
        } else {
            None
        }
    });

    match path {
        Some(p) => match std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file))).init();
                info!("Logging to file: {}", p.display());
            }
            Err(e) => {
                // A logging failure must not keep the service from starting.
                builder.init();
                log::warn!("failed to open log file {}: {e}", p.display());
            }
        },
        None => builder.init(),
    }
}

/// Runs the gRPC control server until `shutdown` fires, then tears down any
/// sessions that are still alive (cancelling tunnels and removing routes).
async fn run_server(
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sessions = Arc::new(SessionStore::new());
    let service = SporaService::new(sessions.clone(), shutdown.clone());

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
    Ok(())
}
