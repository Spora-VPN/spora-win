use std::sync::atomic::Ordering;
use std::sync::Arc;

use log::{error, info, warn};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use url::Url;

use crate::proto::*;
use crate::proto::spora_server::Spora;
use crate::session::{ConnectSession, SessionStore, ShareSession};

/// Hardcoded keepalive interval for Windows client tunnels.
const KEEPALIVE_SECS: u64 = 20;

/// The client's address inside the tunnel (wincore's traditional one; any
/// private address does, the exit learns it from the packets).
const TUN_ADDR: &str = "10.0.85.1/24";

/// The host-side tunnel policy: full tunnel (v4 and v6), the sharer's DNS
/// forwarder as the resolver, an MTU that follows the path — `spora use`'s
/// defaults, except for the address.
fn tunnel_options() -> Result<spora_vpn::Options, String> {
    let dns: Vec<String> = spora_vpn::DEFAULT_DNS.iter().map(|s| s.to_string()).collect();
    spora_vpn::Options::parse(
        TUN_ADDR,
        spora_vpn::DEFAULT_TUN_ADDR6,
        false,
        &[],
        false,
        &dns,
        false,
        None,
    )
}

pub struct SporaService {
    sessions: Arc<SessionStore>,
    shutdown: Arc<tokio::sync::Notify>,
}

impl SporaService {
    pub fn new(sessions: Arc<SessionStore>, shutdown: Arc<tokio::sync::Notify>) -> Self {
        Self { sessions, shutdown }
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

                    let handle = match sessions.insert_share(ShareSession {
                        cancel: cancel.clone(),
                        task,
                    }) {
                        Some(h) => h,
                        None => {
                            // Service is shutting down — stop the share we just
                            // brought up and let the stream close.
                            info!("gRPC Share: service shutting down, aborting new share");
                            cancel.cancel();
                            return;
                        }
                    };

                    let _ = tx
                        .send(Ok(ShareEvent {
                            event: Some(share_event::Event::Ready(ShareReady {
                                handle,
                                url,
                            })),
                        }))
                        .await;

                    // The event stream is the session's lease: the service is
                    // long-lived, so when the UI goes away (clean exit or
                    // crash) the share must stop rather than leak. When
                    // StopShare or service shutdown cancels the session, the
                    // stream closes so the client sees it end.
                    tokio::select! {
                        _ = tx.closed() => {
                            if let Some(s) = sessions.remove_share(handle) {
                                info!("share {} client disconnected — stopping share", handle);
                                s.cancel.cancel();
                                s.task.abort();
                            }
                        }
                        _ = cancel.cancelled() => {}
                    }
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
            // Phase 1: the tunnel interface exists with its address and MTU
            // but no routes, so the relay dial and the STUN queries use the
            // normal network — spora-vpn's two-phase bring-up, the same as
            // `spora use` and the Android client. A Linux dev build has no
            // TUN; the session then runs without a tunnel so the control API
            // can still be exercised.
            let vpn = match tunnel_options().and_then(spora_vpn::Session::setup) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    info!("TUN setup skipped/failed: {}", e);
                    None
                }
            };
            let mut config = spora_core::Config::default();
            if let Some(s) = &vpn {
                // Every outer socket (relay, STUN, punched peer) is bound to
                // the uplink, so nothing loops into the tunnel whatever the
                // routes say.
                config.protector = s.protector();
            }
            config.mtu_callback = spora_vpn::mtu_callback(vpn.as_ref(), |event| match event {
                spora_vpn::MtuEvent::Reported(mtu) => info!("path MTU budget reported: {}", mtu),
                spora_vpn::MtuEvent::Applied(mtu) => info!("TUN MTU set to {}", mtu),
                spora_vpn::MtuEvent::Failed { reported, error } => {
                    error!("could not apply MTU {}: {}", reported, error)
                }
            });
            config.event_hook =
                spora_vpn::event_hook(vpn.as_ref(), |event| info!("tunnel event: {:?}", event));

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

            let Some(vpn) = vpn else {
                // No tunnel on this platform: register the session so the
                // control API round-trips, and hold the transport until the
                // client goes away.
                let task = tokio::spawn(async move {
                    let _transport = transport;
                    std::future::pending::<()>().await;
                });
                let session_cancel = cancel.clone();
                let handle = match sessions.insert_connect(ConnectSession {
                    cancel,
                    task,
                    vpn: None,
                }) {
                    Some(h) => h,
                    None => {
                        info!("gRPC Connect: service shutting down, aborting new session");
                        session_cancel.cancel();
                        return;
                    }
                };
                let _ = tx
                    .send(Ok(ConnectEvent {
                        event: Some(connect_event::Event::Connected(ConnectConnected {
                            handle,
                        })),
                    }))
                    .await;
                tokio::select! {
                    _ = tx.closed() => {
                        if let Some(s) = sessions.remove_connect(handle) {
                            info!("connect {} client disconnected — tearing down", handle);
                            s.cancel.cancel();
                            s.task.abort();
                        }
                    }
                    _ = session_cancel.cancelled() => {}
                }
                return;
            };

            // Phase 2: the session is up and its sockets are protected — now
            // point the host at the tunnel.
            match vpn.activate() {
                Ok(act) => info!(
                    "tunnel up on {} ({}/{}{}), MTU {}: routes [{}], resolver [{}]{}",
                    vpn.tun_name(),
                    vpn.options().tun_addr,
                    vpn.options().tun_prefix,
                    vpn.options()
                        .tun_addr6
                        .map(|(a, p)| format!(", {a}/{p}"))
                        .unwrap_or_default(),
                    vpn.current_mtu(),
                    act.routes
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    act.dns
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    act.dns_method
                        .map(|m| format!(" via {m}"))
                        .unwrap_or_default(),
                ),
                Err(e) => error!(
                    "route/resolver setup failed (tunnel will run without routing): {}",
                    e
                ),
            }
            let pump_handle = match vpn.pump_handle() {
                Ok(h) => h,
                Err(e) => {
                    error!("tunnel pump: {}", e);
                    let _ = tx
                        .send(Ok(ConnectEvent {
                            event: Some(connect_event::Event::Error(ConnectError {
                                message: e,
                            })),
                        }))
                        .await;
                    cancel.cancel();
                    return;
                }
            };

            // Spawn tunnel bridge task
            let tunnel_cancel = cancel.clone();
            let task = tokio::spawn(async move {
                info!("Tunnel task starting");
                tokio::select! {
                    result = spora_vpn::run_pump(transport, pump_handle) => {
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

            let session_cancel = cancel.clone();
            let handle = match sessions.insert_connect(ConnectSession {
                cancel,
                task,
                vpn: Some(vpn),
            }) {
                Some(h) => h,
                None => {
                    // Service is shutting down. Dropping the session above
                    // dropped its spora-vpn session, already undoing the
                    // routes and resolver we just installed; cancel the
                    // tunnel task and end.
                    info!("gRPC Connect: service shutting down, tearing down new tunnel");
                    session_cancel.cancel();
                    return;
                }
            };

            let _ = tx
                .send(Ok(ConnectEvent {
                    event: Some(connect_event::Event::Connected(ConnectConnected {
                        handle,
                    })),
                }))
                .await;

            // Tie the tunnel to the event stream (see share() above): a UI
            // that crashed or was closed must not leave the host's traffic
            // routing into a dead tunnel. Dropping the session also drops its
            // spora-vpn session, which undoes the routes and resolver.
            tokio::select! {
                _ = tx.closed() => {
                    if let Some(s) = sessions.remove_connect(handle) {
                        info!("connect {} client disconnected — tearing down tunnel", handle);
                        s.cancel.cancel();
                        s.task.abort();
                    }
                }
                _ = session_cancel.cancelled() => {}
            }
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
        // session.vpn dropped here → routes and resolver undone, adapter closed

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
        // On Windows the process is a Windows service whose lifecycle belongs
        // to the SCM: a clean self-exit would report SERVICE_STOPPED (so the
        // failure/restart actions never fire) and leave every user's tunnels
        // down until reboot. The pipe admits unelevated interactive users, so
        // honoring this RPC would let any local process stop the machine-wide
        // VPN. Ignore it — the UI on Windows never calls it. Stop the service
        // via the SCM (`sc stop Spora` / Services) instead.
        #[cfg(windows)]
        {
            warn!("gRPC Shutdown ignored on Windows — service is stopped via the SCM");
            let _ = &self.shutdown;
        }
        #[cfg(not(windows))]
        {
            info!("gRPC Shutdown — exiting");
            self.shutdown.notify_one();
        }
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

