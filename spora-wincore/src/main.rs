mod service;
mod session;
mod tun;
mod route;

pub mod proto {
    tonic::include_proto!("spora");
}

use log::info;
use tonic::transport::Server;

use crate::proto::spora_server::SporaServer;
use crate::service::SporaService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let log_file = args.windows(2).find(|w| w[0] == "--log-file").map(|w| w[1].clone());

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder
        .filter_module("quinn", log::LevelFilter::Warn)
        .filter_module("quinn_proto", log::LevelFilter::Warn)
        .filter_module("quinn_udp", log::LevelFilter::Warn);

    if let Some(path) = log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("failed to open log file");
        builder
            .target(env_logger::Target::Pipe(Box::new(file)))
            .init();
        info!("Logging to file: {}", path);
    } else {
        builder.init();
    }

    let addr = "[::1]:50051".parse()?;
    let service = SporaService::new();

    info!("spora-wincore gRPC server listening on {}", addr);
    Server::builder()
        .add_service(SporaServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
