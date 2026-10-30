use std::sync::Arc;
use std::sync::atomic::Ordering;

use log::{error, info, warn};
use spora_core::identity::{Credential, IDENTITY_MISMATCH, Share};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use url::Url;

use crate::backend::Backend;
use crate::proto::spora_server::Spora;
use crate::proto::*;
use crate::relays::{self, AddressError};
use crate::session::{ConnectSession, SessionStore, ShareSession};

/// Hardcoded keepalive interval for Windows client tunnels.
const KEEPALIVE_SECS: u64 = 20;

/// The client's address inside the tunnel (wincore's traditional one; any
/// private address does, the exit learns it from the packets).
const TUN_ADDR: &str = "10.0.85.1/24";

/// Bug reports reach the UI in chunks of this size (gRPC's default message
/// limit is 4 MiB; a report can be 24).
const REPORT_CHUNK: usize = 256 * 1024;

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
    backend: Arc<Backend>,
    shutdown: Arc<tokio::sync::Notify>,
}

impl SporaService {
    pub fn new(
        sessions: Arc<SessionStore>,
        backend: Arc<Backend>,
        shutdown: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            sessions,
            backend,
            shutdown,
        }
    }

    /// The service layer, opened if need be (off the async threads: opening
    /// reads and may write the state files).
    async fn telemetry(&self) -> Option<&'static spora_service::Telemetry> {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.telemetry())
            .await
            .ok()
            .flatten()
    }

    /// Keep the relay access fresh while new shares go on Spora's relays or
    /// a running share is on them.
    fn update_relay_access(&self) {
        if let Some(t) = self.backend.opened() {
            t.set_relay_access(
                self.backend.relays.get().use_spora || self.sessions.any_share_uses_spora(),
            );
        }
    }

    fn relay_settings(&self) -> RelaySettings {
        let s = self.backend.relays.get();
        let access = match self.backend.opened() {
            Some(t) => {
                let a = t.relay_access_status();
                RelayAccess {
                    available: true,
                    cert_hash: a.cert_hash,
                    token_expires_unix_ms: a.token_expires_unix_ms.unwrap_or(0),
                    last_success_unix_ms: a.last_success_unix_ms.unwrap_or(0),
                    last_error: a.last_error.unwrap_or_default(),
                    advised: a
                        .advised
                        .iter()
                        .filter_map(spora_service::access::advised_endpoint)
                        .map(|e| e.to_url_param())
                        .collect(),
                    fetching: self.backend.fetching(),
                    fetch_failed: self.backend.fetch_failed(),
                }
            }
            None => RelayAccess::default(),
        };
        RelaySettings {
            use_spora: s.use_spora,
            own: s
                .own
                .iter()
                .map(|r| OwnRelay {
                    endpoint: r.endpoint.clone(),
                    has_token: r.token.is_some(),
                })
                .collect(),
            access: Some(access),
        }
    }

    fn telemetry_status(&self) -> TelemetryStatus {
        match self.backend.opened() {
            Some(t) => telemetry_status(t.status()),
            None => TelemetryStatus {
                required_notice: spora_service::REQUIRED_NOTICE.to_string(),
                ..TelemetryStatus::default()
            },
        }
    }
}

fn telemetry_status(s: spora_service::TelemetryStatus) -> TelemetryStatus {
    TelemetryStatus {
        available: true,
        upload_enabled: s.upload_enabled,
        required: s.required,
        required_notice_pending: s.required_notice_pending,
        device_id: s.device_id.unwrap_or_default(),
        last_upload_unix_ms: s.last_upload_unix_ms.unwrap_or(0),
        last_error: s.last_error.unwrap_or_default(),
        required_notice: spora_service::REQUIRED_NOTICE.to_string(),
    }
}

async fn send_share_error(
    tx: &mpsc::Sender<Result<ShareEvent, Status>>,
    code: ShareErrorCode,
    message: impl Into<String>,
) {
    let message = message.into();
    error!("gRPC Share: {message}");
    let _ = tx
        .send(Ok(ShareEvent {
            event: Some(share_event::Event::Error(ShareError {
                message,
                code: code as i32,
            })),
        }))
        .await;
}

fn address_error(e: AddressError) -> RelayAddressError {
    match e {
        AddressError::Format => RelayAddressError::Format,
        AddressError::Protocol => RelayAddressError::Protocol,
        AddressError::Ipv6Brackets => RelayAddressError::Ipv6Brackets,
        AddressError::Port => RelayAddressError::Port,
    }
}

#[tonic::async_trait]
impl Spora for SporaService {
    type ShareStream = ReceiverStream<Result<ShareEvent, Status>>;
    type ConnectStream = ReceiverStream<Result<ConnectEvent, Status>>;
    type ExportBugReportStream = ReceiverStream<Result<BugReportChunk, Status>>;

    async fn share(
        &self,
        request: Request<ShareRequest>,
    ) -> Result<Response<Self::ShareStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = mpsc::channel(16);
        let sessions = self.sessions.clone();
        let telemetry = self.telemetry().await;

        tokio::spawn(async move {
            let share = match Share::from_blob(&req.share) {
                Ok(s) => s,
                Err(e) => {
                    return send_share_error(&tx, ShareErrorCode::InvalidShare, format!("not a share: {e}"))
                        .await;
                }
            };
            // The device identity every share is served under lives in the
            // service state.
            let Some(svc) = telemetry else {
                return send_share_error(
                    &tx,
                    ShareErrorCode::ServiceUnavailable,
                    "the service state is unavailable (see the service log)",
                )
                .await;
            };
            let device = svc.device_identity();
            if share.cert_hash != device.cert_hash() {
                return send_share_error(
                    &tx,
                    ShareErrorCode::IdentityMismatch,
                    "the share was made under another device identity",
                )
                .await;
            }
            let share_id = share.share_id;
            let uses_spora = share.relays.iter().any(|r| r.credential == Credential::Spora);

            // Connection logging is on by default (the sharer's IP fronts the
            // client's traffic); the UI can opt out or override dir/retention.
            let conn_log = if req.disable_conn_log {
                None
            } else {
                let dir = if req.conn_log_dir.is_empty() {
                    crate::paths::connlog_dir(&share.routing_hash())
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
            info!(
                "gRPC Share({}): {} relay(s){}, connection log {}",
                spora_core::identity::hex(&share_id),
                share.relays.len(),
                if uses_spora { ", Spora's among them" } else { "" },
                conn_log
                    .as_ref()
                    .map(|cl| cl.dir.display().to_string())
                    .unwrap_or_else(|| "disabled".into()),
            );

            // Forward the share-side lifecycle events the UI cares about. The
            // hook must be non-blocking, so a full stream buffer drops the
            // event.
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

            let mut config = spora_core::Config {
                conn_log,
                event_hook,
                record: Some(crate::backend::record_config()),
                ..spora_core::Config::default()
            };
            if uses_spora {
                // Renewal runs from now on; the share starts with the cached
                // token (none: degraded until the service gets one, and the
                // share gets it the moment it does).
                svc.set_relay_access(true);
                config.relay_token = svc.relay_token();
                if config.relay_token.is_none() {
                    warn!("gRPC Share: no valid relay token yet; Spora's relays refuse the share until one comes");
                }
            }

            match spora_core::share(device, share, config).await {
                Ok(session) => {
                    let url = session.url.to_string();
                    let cancel = session.cancel.clone();

                    let handle = match sessions.insert_share(ShareSession {
                        cancel: cancel.clone(),
                        task: session.task,
                        share_id,
                        credentials: session.credentials,
                        uses_spora,
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
                            event: Some(share_event::Event::Ready(ShareReady { handle, url })),
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
                    let code = if e.starts_with(IDENTITY_MISMATCH) {
                        ShareErrorCode::IdentityMismatch
                    } else {
                        ShareErrorCode::Unspecified
                    };
                    send_share_error(&tx, code, e).await;
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
        // Not the URL itself: it carries the share's secret, and this log
        // (unlike the diagnostic log) is not redacted.
        info!("gRPC Connect()");

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
            let mut config = spora_core::Config {
                record: Some(crate::backend::record_config()),
                ..spora_core::Config::default()
            };
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

    async fn make_share(
        &self,
        _request: Request<MakeShareRequest>,
    ) -> Result<Response<MakeShareResponse>, Status> {
        let settings = self.backend.relays.get();
        let failed = |error: MakeShareError, detail: String| {
            warn!("gRPC MakeShare: {error:?}: {detail}");
            Response::new(MakeShareResponse {
                error: error as i32,
                detail,
                ..MakeShareResponse::default()
            })
        };
        if !settings.has_relays() {
            return Ok(failed(MakeShareError::NoRelays, "no relays configured".into()));
        }
        let Some(svc) = self.telemetry().await else {
            return Ok(failed(
                MakeShareError::ServiceUnavailable,
                "the service state is unavailable (see the service log)".into(),
            ));
        };
        // Network only when Spora's relays are wanted and none were fetched
        // yet (a fresh install, or the API was never reachable); otherwise
        // offline, on the relays last fetched.
        if settings.use_spora && svc.relay_access_status().advised.is_empty() {
            let backend = self.backend.clone();
            let fetched = tokio::task::spawn_blocking(move || backend.fetch(svc))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            if let Err(e) = fetched {
                return Ok(failed(MakeShareError::RelaysUnreachable, e));
            }
        }
        let own: Vec<_> = settings.own.iter().filter_map(relays::OwnRelay::entry).collect();
        match svc.make_share(own, settings.use_spora) {
            Ok(share) => {
                info!(
                    "gRPC MakeShare: new share {} on {}{} own relay(s)",
                    spora_core::identity::hex(&share.share_id),
                    if settings.use_spora { "Spora's relays + " } else { "" },
                    settings.own.len()
                );
                Ok(Response::new(MakeShareResponse {
                    url: share.url().to_string(),
                    share: share.to_blob(),
                    ..MakeShareResponse::default()
                }))
            }
            Err(spora_service::Error::NoRelayAdvice) => Ok(failed(
                MakeShareError::NoSporaRelays,
                "the API advised no relays".into(),
            )),
            Err(e) => Ok(failed(MakeShareError::Failed, e.to_string())),
        }
    }

    async fn get_relay_settings(
        &self,
        _request: Request<GetRelaySettingsRequest>,
    ) -> Result<Response<RelaySettings>, Status> {
        // Opening (if it failed before) is what a settings page needs to
        // show anything at all.
        let _ = self.telemetry().await;
        Ok(Response::new(self.relay_settings()))
    }

    async fn set_spora_relays(
        &self,
        request: Request<SetSporaRelaysRequest>,
    ) -> Result<Response<RelaySettings>, Status> {
        let use_spora = request.into_inner().use_spora;
        self.backend.relays.update(|s| s.use_spora = use_spora);
        info!("Spora's relays {} for new shares", if use_spora { "on" } else { "off" });
        self.update_relay_access();
        Ok(Response::new(self.relay_settings()))
    }

    async fn put_own_relay(
        &self,
        request: Request<PutOwnRelayRequest>,
    ) -> Result<Response<PutOwnRelayResponse>, Status> {
        let req = request.into_inner();
        let refuse = |error: RelayAddressError| {
            Response::new(PutOwnRelayResponse {
                settings: Some(self.relay_settings()),
                error: error as i32,
            })
        };
        let endpoint = match relays::parse(&relays::normalize(&req.endpoint)) {
            Ok(e) => e,
            Err(e) => return Ok(refuse(address_error(e))),
        };
        let token_action = TokenAction::try_from(req.token_action).unwrap_or(TokenAction::Keep);
        let token = req.token.trim().to_string();
        let outcome = self.backend.relays.update(|s| {
            let index = if req.original.is_empty() {
                None
            } else {
                match s.own.iter().position(|r| r.endpoint == req.original) {
                    Some(i) => Some(i),
                    None => return Err(RelayAddressError::Gone),
                }
            };
            if s
                .own
                .iter()
                .enumerate()
                .any(|(i, r)| Some(i) != index && r.endpoint.eq_ignore_ascii_case(&endpoint))
            {
                return Err(RelayAddressError::Duplicate);
            }
            let kept = index.and_then(|i| s.own[i].token.clone());
            let relay = relays::OwnRelay {
                endpoint: endpoint.clone(),
                token: match token_action {
                    TokenAction::Set if !token.is_empty() => Some(token.clone()),
                    TokenAction::Set | TokenAction::Clear => None,
                    TokenAction::Keep => kept,
                },
            };
            match index {
                Some(i) => s.own[i] = relay,
                None => s.own.push(relay),
            }
            Ok(())
        });
        if let Err(e) = outcome {
            return Ok(refuse(e));
        }
        info!(
            "own relay {} {}",
            endpoint,
            if req.original.is_empty() { "added" } else { "changed" }
        );
        Ok(Response::new(PutOwnRelayResponse {
            settings: Some(self.relay_settings()),
            error: RelayAddressError::Ok as i32,
        }))
    }

    async fn remove_own_relay(
        &self,
        request: Request<RemoveOwnRelayRequest>,
    ) -> Result<Response<RelaySettings>, Status> {
        let endpoint = request.into_inner().endpoint;
        self.backend.relays.update(|s| s.own.retain(|r| r.endpoint != endpoint));
        info!("own relay {endpoint} removed");
        Ok(Response::new(self.relay_settings()))
    }

    async fn refresh_relays(
        &self,
        _request: Request<RefreshRelaysRequest>,
    ) -> Result<Response<RelaySettings>, Status> {
        if let Some(svc) = self.telemetry().await {
            let backend = self.backend.clone();
            let _ = tokio::task::spawn_blocking(move || backend.fetch(svc)).await;
        }
        Ok(Response::new(self.relay_settings()))
    }

    async fn get_telemetry(
        &self,
        _request: Request<GetTelemetryRequest>,
    ) -> Result<Response<TelemetryStatus>, Status> {
        let _ = self.telemetry().await;
        Ok(Response::new(self.telemetry_status()))
    }

    async fn set_telemetry(
        &self,
        request: Request<SetTelemetryRequest>,
    ) -> Result<Response<SetTelemetryResponse>, Status> {
        let enabled = request.into_inner().enabled;
        let Some(svc) = self.telemetry().await else {
            return Ok(Response::new(SetTelemetryResponse {
                status: Some(self.telemetry_status()),
                refused_required: false,
                error: "the service state is unavailable".into(),
            }));
        };
        let result = tokio::task::spawn_blocking(move || svc.set_upload_enabled(enabled))
            .await
            .map_err(|e| spora_service::Error::Config(e.to_string()))
            .and_then(|r| r);
        let (refused_required, error) = match result {
            Ok(()) => (false, String::new()),
            Err(spora_service::Error::Required) => {
                info!("telemetry: switching off refused, the account requires it");
                (true, String::new())
            }
            Err(e) => {
                warn!("telemetry: cannot switch uploads {}: {e}", if enabled { "on" } else { "off" });
                (false, e.to_string())
            }
        };
        Ok(Response::new(SetTelemetryResponse {
            status: Some(self.telemetry_status()),
            refused_required,
            error,
        }))
    }

    async fn acknowledge_telemetry_notice(
        &self,
        _request: Request<AcknowledgeTelemetryNoticeRequest>,
    ) -> Result<Response<TelemetryStatus>, Status> {
        if let Some(svc) = self.backend.opened() {
            let r = tokio::task::spawn_blocking(move || svc.acknowledge_notice()).await;
            if let Ok(Err(e)) = r {
                warn!("telemetry: acknowledging the notice: {e}");
            }
        }
        Ok(Response::new(self.telemetry_status()))
    }

    async fn export_bug_report(
        &self,
        request: Request<ExportBugReportRequest>,
    ) -> Result<Response<Self::ExportBugReportStream>, Status> {
        let note = Some(request.into_inner().note).filter(|n| !n.trim().is_empty());
        let svc = self.telemetry().await;
        let summary = tokio::task::spawn_blocking(move || export_report(svc, note))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| {
                error!("bug report: {e}");
                Status::internal(e)
            })?;
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            for chunk in summary.chunks(REPORT_CHUNK) {
                let msg = BugReportChunk { data: chunk.to_vec() };
                if tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
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

/// Bundle the recent records, diagnostic log and crash files into a zip in
/// the service's private reports directory, read it back and delete it. The
/// UI saves the bytes where the user can reach them: the service never
/// writes into a user's folders (it runs as SYSTEM, and the path would be
/// the caller's to choose).
fn export_report(
    svc: Option<&'static spora_service::Telemetry>,
    note: Option<String>,
) -> Result<Vec<u8>, String> {
    use std::collections::BTreeMap;
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = crate::paths::reports_dir();
    crate::paths::ensure_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let out = dir.join(format!(
        "report-{}-{}.zip",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let platform = BTreeMap::from([("client".to_string(), "spora-winui".to_string())]);
    let summary = match svc {
        Some(t) => t.export_bug_report(&out, note, platform),
        // Without the service state: the same directories, no device id.
        None => {
            let mut opts = spora_service::BundleOptions::new(&out);
            opts.record_dirs = vec![crate::paths::records_dir()];
            opts.log_dir = Some(crate::paths::diag_dir());
            opts.note = note;
            opts.platform = platform;
            opts.platform
                .insert("platform".into(), crate::backend::platform().into());
            opts.platform
                .insert("app_version".into(), crate::backend::app_version());
            spora_service::diag::bundle::export(&opts)
        }
    }
    .map_err(|e| e.to_string());
    let data = summary.and_then(|s| {
        info!(
            "bug report: {} files ({} left out), {} bytes",
            s.included_files, s.omitted_files, s.bytes
        );
        std::fs::read(&out).map_err(|e| e.to_string())
    });
    let _ = std::fs::remove_file(&out);
    data
}
