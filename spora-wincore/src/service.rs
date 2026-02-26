use std::sync::atomic::Ordering;
use std::sync::Arc;

use log::{error, info};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use url::Url;

use crate::proto::*;
use crate::proto::spora_server::Spora;
use crate::route::RouteManager;
use crate::session::{ConnectSession, SessionStore, ShareSession};
use crate::tun::TunDevice;

/// Hardcoded keepalive interval for Windows client tunnels.
const KEEPALIVE_SECS: u64 = 20;

pub struct SporaService {
    sessions: Arc<SessionStore>,
}

impl SporaService {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(SessionStore::new()),
        }
    }
}

#[tonic::async_trait]
impl Spora for SporaService {
    type ShareStream = ReceiverStream<Result<ShareEvent, Status>>;
    type ConnectStream = ReceiverStream<Result<ConnectEvent, Status>>;

    async fn share(
        &self,
        request: Request<ShareRequest>,
    ) -> Result<Response<Self::ShareStream>, Status> {
        let key = request.into_inner().key;
        info!("gRPC Share(key={})", key);

        let (tx, rx) = mpsc::channel(16);
        let sessions = self.sessions.clone();

        tokio::spawn(async move {
            let config = spora_core::Config::default();
            match spora_core::share(key, config).await {
                Ok(session) => {
                    let url = format!(
                        "https://spora.to/s/{}?r={}",
                        session.key, session.endpoint
                    );
                    let cancel = session.cancel.clone();
                    let task = session.task;

                    let handle = sessions.insert_share(ShareSession { cancel, task });

                    let _ = tx
                        .send(Ok(ShareEvent {
                            event: Some(share_event::Event::Ready(ShareReady {
                                handle,
                                url,
                            })),
                        }))
                        .await;

                    // Hold stream open until client disconnects or calls StopShare.
                    tx.closed().await;
                }
                Err(e) => {
                    let _ = tx
                        .send(Ok(ShareEvent {
                            event: Some(share_event::Event::Error(ShareError {
                                message: e.to_string(),
                            })),
                        }))
                        .await;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn connect(
        &self,
        request: Request<ConnectRequest>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let url_str = request.into_inner().url;
        info!("gRPC Connect(url={})", url_str);

        let url = Url::parse(&url_str).map_err(|_| Status::invalid_argument("invalid URL"))?;
        if url.scheme() != "https" {
            return Err(Status::invalid_argument("URL scheme must be https"));
        }

        let (tx, rx) = mpsc::channel(16);
        let sessions = self.sessions.clone();

        tokio::spawn(async move {
            // MTU callback: on Windows, updates the TUN adapter MTU directly via netsh.
            // Gated by `tun_ready` flag so early callbacks (before TUN creation) are deferred.
            let tun_name = "Spora";
            let tun_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let pending_mtu = Arc::new(std::sync::atomic::AtomicU16::new(0));
            let mtu_callback: spora_core::transport::relay::MtuCallback = {
                let name = tun_name.to_string();
                let tun_ready = tun_ready.clone();
                let pending_mtu = pending_mtu.clone();
                Some(Arc::new(move |mtu: u16| {
                    if tun_ready.load(Ordering::Relaxed) {
                        info!("MTU callback: setting TUN MTU to {}", mtu);
                        if let Err(e) = crate::tun::set_mtu(&name, mtu as u32) {
                            error!("Failed to set TUN MTU: {}", e);
                        }
                    } else {
                        info!("MTU callback: TUN not ready yet, deferring MTU {}", mtu);
                        pending_mtu.store(mtu, Ordering::Relaxed);
                    }
                }))
            };

            let config = spora_core::Config {
                mtu_callback,
                ..spora_core::Config::default()
            };

            let result = match spora_core::connect(url, &config).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(Ok(ConnectEvent {
                            event: Some(connect_event::Event::Error(ConnectError {
                                message: e.to_string(),
                            })),
                        }))
                        .await;
                    return;
                }
            };

            let cancel = result.cancel;
            let transport = result.transport;

            // Hardcode keepalive to 20s
            result.keepalive_knob.store(KEEPALIVE_SECS, Ordering::Relaxed);
            if let Some(w) = result.keepalive_waker.lock().unwrap().take() {
                w.wake();
            }

            // Create TUN device (Windows-only; returns Err on Linux)
            let default_mtu = 1400u32;
            let tun = match TunDevice::create(tun_name, default_mtu) {
                Ok(t) => t,
                Err(e) => {
                    info!("TUN creation skipped/failed: {}", e);
                    // On non-Windows, register session without tunnel so Share-side testing works.
                    let routes = RouteManager::new();
                    let task = tokio::spawn(async {});
                    let handle = sessions.insert_connect(ConnectSession {
                        cancel,
                        task,
                        routes,
                    });
                    let _ = tx
                        .send(Ok(ConnectEvent {
                            event: Some(connect_event::Event::Connected(ConnectConnected {
                                handle,
                            })),
                        }))
                        .await;
                    tx.closed().await;
                    return;
                }
            };

            // TUN is ready — enable MTU callback and apply any deferred MTU
            tun_ready.store(true, Ordering::Relaxed);
            let deferred_mtu = pending_mtu.load(Ordering::Relaxed);
            if deferred_mtu > 0 {
                info!("Applying deferred MTU: {}", deferred_mtu);
                if let Err(e) = crate::tun::set_mtu(tun_name, deferred_mtu as u32) {
                    error!("Failed to set deferred TUN MTU: {}", e);
                }
            }

            // Set up routes: exclude relay IP via original gateway, then default via TUN
            let mut routes = RouteManager::new();
            match setup_routes(&mut routes, &url_str, &tun) {
                Ok(()) => {}
                Err(e) => {
                    error!("Route setup failed (tunnel will run without routing): {}", e);
                }
            }

            // Spawn tunnel bridge task
            let tunnel_cancel = cancel.clone();
            let task = tokio::spawn(async move {
                info!("Tunnel task starting");
                tokio::select! {
                    result = crate::tun::run_tunnel(transport, &tun) => {
                        match result {
                            Ok(()) => info!("Tunnel task exited normally"),
                            Err(e) => error!("Tunnel task error: {}", e),
                        }
                    }
                    _ = tunnel_cancel.cancelled() => {
                        info!("Tunnel task cancelled");
                    }
                }
            });

            let handle = sessions.insert_connect(ConnectSession {
                cancel,
                task,
                routes,
            });

            let _ = tx
                .send(Ok(ConnectEvent {
                    event: Some(connect_event::Event::Connected(ConnectConnected {
                        handle,
                    })),
                }))
                .await;

            // Hold stream open until client disconnects
            tx.closed().await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn stop_share(
        &self,
        request: Request<StopShareRequest>,
    ) -> Result<Response<StopShareResponse>, Status> {
        let handle = request.into_inner().handle;
        info!("gRPC StopShare(handle={})", handle);

        let session = self
            .sessions
            .remove_share(handle)
            .ok_or_else(|| Status::not_found("invalid share handle"))?;
        session.cancel.cancel();
        session.task.abort();

        Ok(Response::new(StopShareResponse {}))
    }

    async fn disconnect(
        &self,
        request: Request<DisconnectRequest>,
    ) -> Result<Response<DisconnectResponse>, Status> {
        let handle = request.into_inner().handle;
        info!("gRPC Disconnect(handle={})", handle);

        let session = self
            .sessions
            .remove_connect(handle)
            .ok_or_else(|| Status::not_found("invalid connect handle"))?;
        session.cancel.cancel();
        session.task.abort();
        // session.routes dropped here → routes cleaned up

        Ok(Response::new(DisconnectResponse {}))
    }

    async fn make_secret_key(
        &self,
        _request: Request<MakeSecretKeyRequest>,
    ) -> Result<Response<MakeSecretKeyResponse>, Status> {
        let key = spora_core::make_secret_key();
        info!("gRPC MakeSecretKey → {}", key);
        Ok(Response::new(MakeSecretKeyResponse { key }))
    }
}

/// Set up routing: exclude the relay host via the original gateway, then add a default
/// route through the TUN adapter. This is the Windows equivalent of Android's
/// VpnService.protect() + addRoute("0.0.0.0/0").
fn setup_routes(
    routes: &mut RouteManager,
    url_str: &str,
    tun: &TunDevice,
) -> Result<(), String> {
    // Extract relay host IP from URL (?r=host:port)
    let url = Url::parse(url_str).map_err(|e| format!("invalid URL: {}", e))?;
    let relay = url
        .query_pairs()
        .find(|(k, _)| k == "r")
        .map(|(_, v)| v.into_owned())
        .ok_or("URL missing ?r= parameter")?;
    let (relay_host, _) = relay
        .rsplit_once(':')
        .ok_or("invalid ?r= format")?;

    use std::net::ToSocketAddrs;

    // Resolve relay hostname to IP if needed
    let relay_ip = if relay_host.parse::<std::net::IpAddr>().is_ok() {
        relay_host.to_string()
    } else {
        let addr = format!("{}:0", relay_host)
            .to_socket_addrs()
            .map_err(|e| format!("failed to resolve relay host '{}': {}", relay_host, e))?
            .find(|a| a.is_ipv4())
            .ok_or_else(|| format!("no IPv4 address for relay host '{}'", relay_host))?;
        addr.ip().to_string()
    };

    let gw = crate::route::detect_default_gateway()?;
    info!(
        "Original default gateway: {} via IF {}",
        gw.gateway, gw.interface_index
    );

    let tun_if = tun.interface_index();

    // Exclude relay IP so its traffic goes through the real NIC (prevents routing loop)
    routes.exclude_host(&relay_ip, &gw.gateway, gw.interface_index)?;

    // Exclude STUN server so direct upgrade probes don't loop through tunnel
    if let Ok(addrs) = ("stun.l.google.com", 19302u16).to_socket_addrs() {
        for addr in addrs.filter(|a| a.is_ipv4()) {
            let ip = addr.ip().to_string();
            if let Err(e) = routes.exclude_host(&ip, &gw.gateway, gw.interface_index) {
                info!("Failed to exclude STUN IP {}: {}", ip, e);
            }
        }
    }

    // Route all traffic through TUN
    routes.add_default_route(tun_if)?;

    Ok(())
}
