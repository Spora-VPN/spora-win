use std::sync::atomic::Ordering;
use std::sync::Arc;

use log::{error, info, warn};
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
    shutdown: Arc<tokio::sync::Notify>,
}

impl SporaService {
    pub fn new(shutdown: Arc<tokio::sync::Notify>) -> Self {
        Self {
            sessions: Arc::new(SessionStore::new()),
            shutdown,
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
        // TODO(redesign): the gRPC contract still has a `key: String` field
        // from the old design. Decode it as base64(identity.to_bytes()) and
        // generate a fresh identity if empty. A proper proto update should
        // change this to `bytes identity = 1`.
        let req = request.into_inner();
        let raw_key = req.key;
        let identity = if raw_key.is_empty() {
            spora_core::identity::Identity::generate()
        } else {
            use base64::Engine as _;
            match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&raw_key) {
                Ok(bytes) => match spora_core::identity::Identity::from_bytes(&bytes) {
                    Ok(id) => id,
                    Err(e) => {
                        error!("gRPC Share: invalid persisted identity: {}", e);
                        return Err(Status::invalid_argument(e));
                    }
                },
                Err(e) => {
                    error!("gRPC Share: identity not valid base64url: {}", e);
                    return Err(Status::invalid_argument(e.to_string()));
                }
            }
        };
        // Connection logging is on by default (the sharer's IP fronts the
        // client's traffic); the UI can opt out or override dir/retention.
        let conn_log = if req.disable_conn_log {
            None
        } else {
            let dir = if req.conn_log_dir.is_empty() {
                default_connlog_dir(&identity)
            } else {
                std::path::PathBuf::from(&req.conn_log_dir)
            };
            let mut cl = spora_core::connlog::ConnLogConfig::in_dir(&dir);
            if req.conn_log_retention_days > 0 {
                cl.retention = std::time::Duration::from_secs(
                    u64::from(req.conn_log_retention_days) * 86_400,
                );
            }
            cl.log_destinations = !req.conn_log_sessions_only;
            Some(cl)
        };
        match &conn_log {
            Some(cl) => info!("gRPC Share() — connection log at {}", cl.dir.display()),
            None => info!("gRPC Share() — connection log disabled"),
        }

        let (tx, rx) = mpsc::channel(16);
        let sessions = self.sessions.clone();

        // Forward the share-side lifecycle events the UI cares about. The
        // hook must be non-blocking, so a full stream buffer drops the event.
        let event_tx = tx.clone();
        let event_hook: spora_core::EventHook = Some(Arc::new(move |ev| {
            let mapped = match ev {
                spora_core::TunnelEvent::SessionEnded { reason } => {
                    share_event::Event::SessionEnded(ShareSessionEnded { reason })
                }
                spora_core::TunnelEvent::ConnLogDegraded { detail } => {
                    share_event::Event::ConnLogDegraded(ShareConnLogDegraded { detail })
                }
                _ => return,
            };
            if event_tx.try_send(Ok(ShareEvent { event: Some(mapped) })).is_err() {
                // ConnLogDegraded in particular must not vanish silently.
                warn!("gRPC Share: event stream full or gone, dropping lifecycle event");
            }
        }));

        tokio::spawn(async move {
            let config = spora_core::Config {
                conn_log,
                event_hook,
                ..spora_core::Config::default()
            };
            match spora_core::share(identity, config).await {
                Ok(session) => {
                    let url = session.url.to_string();
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
            // MTU callback: on Windows, updates the TUN adapter MTU via Win32 API.
            // Gated by `tun_ready` flag so early callbacks (before TUN creation) are deferred.
            let tun_name = "Spora";
            let tun_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let pending_mtu = Arc::new(std::sync::atomic::AtomicU16::new(0));
            let tun_if_index = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let mtu_callback: spora_core::MtuCallback = {
                let tun_ready = tun_ready.clone();
                let pending_mtu = pending_mtu.clone();
                let tun_if_index = tun_if_index.clone();
                Some(Arc::new(move |mtu: u16| {
                    if tun_ready.load(Ordering::Relaxed) {
                        let if_index = tun_if_index.load(Ordering::Relaxed);
                        info!("MTU callback: setting TUN MTU to {}", mtu);
                        if let Err(e) = crate::tun::set_mtu(if_index, mtu as u32) {
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
            tun_if_index.store(tun.interface_index(), Ordering::Relaxed);
            tun_ready.store(true, Ordering::Relaxed);
            let deferred_mtu = pending_mtu.load(Ordering::Relaxed);
            if deferred_mtu > 0 {
                info!("Applying deferred MTU: {}", deferred_mtu);
                if let Err(e) = crate::tun::set_mtu(tun.interface_index(), deferred_mtu as u32) {
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
        // Legacy RPC kept for protocol compat; the new share() flow generates
        // its identity internally and doesn't accept a pre-supplied key.
        info!("gRPC MakeSecretKey (deprecated, ignored)");
        Ok(Response::new(MakeSecretKeyResponse { key: String::new() }))
    }

    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        info!("gRPC Shutdown — exiting");
        self.shutdown.notify_one();
        Ok(Response::new(ShutdownResponse {}))
    }
}

/// Default connection-log directory:
/// `%PROGRAMDATA%\Spora\connlog\<routing_key_hex>\`. Per-identity so two
/// shares with different identities never interleave their logs. Kept
/// cfg-portable (plain env lookup, no Win32) so the crate still builds and
/// runs share-side tests on Linux.
fn default_connlog_dir(identity: &spora_core::identity::Identity) -> std::path::PathBuf {
    let program_data =
        std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
    let rk_hex: String = identity
        .routing_key
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    std::path::PathBuf::from(program_data)
        .join("Spora")
        .join("connlog")
        .join(rk_hex)
}

/// Set up routing: exclude the relay host via the original gateway, then add a default
/// route through the TUN adapter. This is the Windows equivalent of Android's
/// VpnService.protect() + addRoute("0.0.0.0/0").
fn setup_routes(
    routes: &mut RouteManager,
    url_str: &str,
    tun: &TunDevice,
) -> Result<(), String> {
    use std::net::ToSocketAddrs;

    // Parse every relay endpoint out of the URL (a share URL can carry
    // several; a hostname may resolve to more than one IPv4). Each must be
    // excluded from the tunnel so its traffic reaches the real NIC, or the
    // relay path loops through the VPN it bootstraps.
    let url = Url::parse(url_str).map_err(|e| format!("invalid URL: {}", e))?;
    let token =
        spora_core::identity::Token::from_url(&url).map_err(|e| format!("invalid share URL: {}", e))?;
    let mut relay_ips: Vec<String> = Vec::new();
    for relay in &token.relays {
        if let Ok(ip) = relay.host.parse::<std::net::IpAddr>() {
            // Windows split-tunnel routing here is IPv4-only; a v6 relay
            // literal needs a v6 route exclusion this code doesn't do yet.
            if ip.is_ipv4() {
                relay_ips.push(ip.to_string());
            } else {
                info!("skipping IPv6 relay {} (v6 route exclusion unimplemented)", ip);
            }
        } else {
            match format!("{}:0", relay.host).to_socket_addrs() {
                Ok(addrs) => relay_ips.extend(addrs.filter(|a| a.is_ipv4()).map(|a| a.ip().to_string())),
                Err(e) => info!("failed to resolve relay host '{}': {}", relay.host, e),
            }
        }
    }
    if relay_ips.is_empty() {
        return Err("no IPv4 relay address resolved from the share URL".into());
    }

    let gw = crate::route::detect_default_gateway()?;
    info!(
        "Original default gateway: {} via IF {}",
        gw.gateway, gw.interface_index
    );

    let tun_if = tun.interface_index();

    // Exclude each relay IP so its traffic goes through the real NIC (prevents routing loop)
    for relay_ip in &relay_ips {
        routes.exclude_host(relay_ip, &gw.gateway, gw.interface_index)?;
    }

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
