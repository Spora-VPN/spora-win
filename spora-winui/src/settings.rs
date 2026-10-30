//! Settings: the relays new links are made with (Spora's and the user's
//! own) and the telemetry upload switch, both kept by the service for the
//! Windows user (like the device identity they belong to). This module
//! holds what the service last said, turns it into what the Settings pane
//! shows, polls it while the pane is open, and handles the pane's actions —
//! plus the force notice (the account requires telemetry) and saving a
//! diagnostic report where the user can reach it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use slint::ComponentHandle as _;

use super::grpc::proto::*;
use super::{AppState, AppWindow, Banner, Ctx, OwnRelayItem, Pane, fmt_datetime, render};

/// The share's link no longer works: the service's state (the device
/// identity every share is served under) was reset.
pub const LINK_RESET: &str =
    "This link no longer works: Spora's data for this Windows user was reset.";

const SUPPORT_MAIL: &str = "support@spora.to";

/// While the pane is open its status (relay access renewing, the last
/// upload) is re-read at this pace; otherwise only now and then, for the
/// force notice.
const POLL_OPEN: Duration = Duration::from_secs(2);
const POLL_IDLE: Duration = Duration::from_secs(60);
/// While the service serves another Windows user: how soon to ask again.
const POLL_REFUSED: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct SettingsState {
    pub relays: Option<RelaySettings>,
    pub telemetry: Option<TelemetryStatus>,
    /// The Relays tab is the one showing (else Telemetry).
    pub telemetry_tab: bool,
    /// A switch change is on its way to the service.
    pub telemetry_busy: bool,
    /// The last switch change failed (not for the account's requirement).
    pub switch_failed: bool,
    /// One fetch of Spora's relays per visit, when they were never fetched.
    pub fetch_asked: bool,
    /// The force notice was shown this session for a pending requirement.
    pub required_shown: bool,
    /// Show the force notice at the next render.
    pub show_required: bool,
    /// The service serves another Windows user: what it said, shown in the
    /// banner for as long as that lasts.
    pub refused: Option<String>,
    pub report: Report,
}

#[derive(Default)]
pub struct Report {
    pub state: ReportState,
    pub text: String,
    pub path: Option<PathBuf>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    #[default]
    None,
    Busy,
    Saved,
    Error,
}

/// What the banner says when a link cannot be made, and whether the
/// relay settings are the fix.
pub fn make_share_error(code: i32, detail: &str) -> (String, bool) {
    match MakeShareError::try_from(code).unwrap_or(MakeShareError::Failed) {
        MakeShareError::NoRelays => (
            "There are no relays to make a link with. Turn on Spora's relays or add your own in settings.".into(),
            true,
        ),
        MakeShareError::RelaysUnreachable => (
            "Couldn't get relays from Spora. Check your internet connection and try again.".into(),
            false,
        ),
        MakeShareError::NoSporaRelays => (
            "Spora has no relays for you right now. Try again later, or add your own in settings.".into(),
            true,
        ),
        MakeShareError::ServiceUnavailable => (
            "Couldn't create a link: the Spora service can't open its data right now. Try again in a minute.".into(),
            false,
        ),
        MakeShareError::Ok | MakeShareError::Failed => (format!("Couldn't create a link. {detail}"), false),
    }
}

fn address_error(code: i32) -> &'static str {
    match RelayAddressError::try_from(code).unwrap_or(RelayAddressError::Format) {
        RelayAddressError::Ok => "",
        RelayAddressError::Format => "Enter it as host:port, for example relay.example.org:443.",
        RelayAddressError::Protocol => {
            "Unknown protocol. Use nz/ or tcptls/, or nothing for the default."
        }
        RelayAddressError::Ipv6Brackets => "Put an IPv6 address in brackets: [2001:db8::1]:443.",
        RelayAddressError::Port => "The port must be a number from 1 to 65535.",
        RelayAddressError::Duplicate => "This relay is already on the list.",
        RelayAddressError::Gone => "This relay was removed in the meantime.",
    }
}

/// The status line under "Use Spora's relays".
fn spora_status(r: Option<&RelaySettings>, now_ms: u64) -> (&'static str, &'static str) {
    const CHECKING: (&str, &str) = ("pending", "Checking…");
    let Some(a) = r.and_then(|r| r.access.as_ref()) else {
        return CHECKING;
    };
    if !a.available {
        return (
            "error",
            "Spora's relays aren't available right now. Spora will keep trying.",
        );
    }
    if a.fetching {
        return CHECKING;
    }
    if a.token_expires_unix_ms > now_ms && !a.advised.is_empty() {
        return ("active", "Ready.");
    }
    if !a.last_error.is_empty() || a.fetch_failed {
        return ("error", "Couldn't reach Spora. Spora will keep trying.");
    }
    // The API answered, and gave no token (or no relays).
    if a.last_success_unix_ms > 0 {
        return (
            "error",
            "Spora's relays aren't available to this PC right now.",
        );
    }
    CHECKING
}

fn never_fetched(r: &RelaySettings) -> bool {
    r.use_spora
        && r.access.as_ref().is_some_and(|a| {
            a.available && a.advised.is_empty() && a.last_success_unix_ms == 0 && !a.fetching
        })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Push the settings into the window (part of the one `render`).
pub fn push(app: &AppWindow, s: &mut AppState) {
    let st = &mut s.settings;
    let relays = st.relays.as_ref();
    app.set_use_spora(relays.is_none_or(|r| r.use_spora));
    app.set_relays_available(
        relays
            .and_then(|r| r.access.as_ref())
            .is_some_and(|a| a.available),
    );
    let (shape, text) = spora_status(relays, now_ms());
    app.set_spora_shape(shape.into());
    app.set_spora_text(text.into());
    let own: Vec<OwnRelayItem> = relays
        .map(|r| {
            r.own
                .iter()
                .map(|o| OwnRelayItem {
                    endpoint: o.endpoint.as_str().into(),
                    has_token: o.has_token,
                })
                .collect()
        })
        .unwrap_or_default();
    app.set_own_relays(slint::VecModel::from_slice(&own));
    app.set_settings_alert(relays.is_some_and(|r| !r.use_spora && r.own.is_empty()));

    let t = st.telemetry.as_ref();
    let available = t.is_some_and(|t| t.available);
    app.set_telemetry_available(available);
    app.set_telemetry_on(t.is_some_and(|t| t.upload_enabled));
    app.set_telemetry_busy(st.telemetry_busy);
    let (shape, text) = match t {
        Some(t) if !t.available => (
            "error",
            "Telemetry isn't available right now. Spora will keep trying; a diagnostic report still works.",
        ),
        _ if st.switch_failed => ("error", "Couldn't change the setting. Try again."),
        Some(t) if t.upload_enabled && !t.last_error.is_empty() => (
            "pending",
            "The last upload didn't go through. Spora will try again.",
        ),
        _ => ("", ""),
    };
    app.set_telemetry_shape(shape.into());
    app.set_telemetry_text(text.into());
    app.set_device_id(t.map(|t| t.device_id.clone()).unwrap_or_default().into());
    let last_upload = match t {
        Some(t) if t.last_upload_unix_ms > 0 => fmt_datetime((t.last_upload_unix_ms / 1000) as i64),
        Some(t) if t.upload_enabled => "Not yet".into(),
        _ => String::new(),
    };
    app.set_last_upload(last_upload.into());
    app.set_report_state(
        match st.report.state {
            ReportState::None => "",
            ReportState::Busy => "busy",
            ReportState::Saved => "saved",
            ReportState::Error => "error",
        }
        .into(),
    );
    app.set_report_text(st.report.text.as_str().into());

    if std::mem::take(&mut st.show_required) {
        let notice = t
            .map(|t| t.required_notice.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| REQUIRED_NOTICE.to_string());
        app.set_required_notice(notice.into());
        app.set_show_required_dialog(true);
    }
}

/// The agreed wording, for when the service has not said it yet.
const REQUIRED_NOTICE: &str = "Your account requires that telemetry is enabled (most likely because you've signed up to a promotion). You can keep it enabled, sign in to a different account, or contact support.";

/// Re-read the settings from the service: every [`POLL_OPEN`] while the pane
/// is open, every [`POLL_IDLE`] otherwise (the force notice can arrive any
/// time), and at once when poked.
pub async fn poll(ctx: Ctx) {
    loop {
        let Some(grpc) = ctx.client() else { return };
        let relays = grpc.relay_settings().await;
        let telemetry = grpc.telemetry().await;
        let refused = grpc.refused();
        let (open, fetch) = {
            let mut s = ctx.lock();
            match (&relays, refused) {
                (Err(why), true) => {
                    s.banner = Some(Banner::plain(why.clone()));
                    s.settings.refused = Some(why.clone());
                }
                (_, false) => {
                    if let Some(why) = s.settings.refused.take()
                        && s.banner.as_ref().is_some_and(|b| b.text == why)
                    {
                        s.banner = None;
                    }
                }
                _ => {}
            }
            if let Ok(r) = relays {
                s.settings.relays = Some(r);
            }
            if let Ok(t) = telemetry {
                if t.required_notice_pending && !s.settings.required_shown {
                    s.settings.required_shown = true;
                    s.settings.show_required = true;
                }
                s.settings.telemetry = Some(t);
            }
            let open = s.pane == Pane::Settings;
            // Spora's relays on and never fetched (a fresh install, or the
            // API was never reachable): fetch now, so the status says
            // whether they can be reached before the user makes a link.
            let fetch = open
                && !s.settings.telemetry_tab
                && !s.settings.fetch_asked
                && s.settings.relays.as_ref().is_some_and(never_fetched);
            if fetch {
                s.settings.fetch_asked = true;
            }
            (open, fetch)
        };
        ctx.refresh();
        if fetch {
            let ctx = ctx.clone();
            let grpc = grpc.clone();
            tokio::spawn(async move {
                if let Ok(r) = grpc.refresh_relays().await {
                    ctx.lock().settings.relays = Some(r);
                    ctx.refresh();
                }
            });
        }
        tokio::select! {
            _ = tokio::time::sleep(if open {
                POLL_OPEN
            } else if refused {
                POLL_REFUSED
            } else {
                POLL_IDLE
            }) => {}
            _ = ctx.poke.notified() => {}
        }
    }
}

/// Open the Settings pane (on `tab`, or where it was).
pub fn open(ctx: &Ctx, app: &AppWindow, tab: Option<&str>) {
    if let Some(tab) = tab {
        app.set_settings_tab(tab.into());
    }
    let mut s = ctx.lock();
    s.creating = None; // navigating away ends the handoff, not the share
    s.pane = Pane::Settings;
    s.settings.telemetry_tab = app.get_settings_tab() == "telemetry";
    s.settings.fetch_asked = false;
    s.settings.switch_failed = false;
    drop(s);
    ctx.poke.notify_one();
    render(ctx);
}

/// Run `f` with the service client on the runtime, unless this is a demo.
fn with_client<F, Fut>(ctx: &Ctx, f: F)
where
    F: FnOnce(Ctx, super::GrpcClient) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Some(grpc) = ctx.client() else {
        ctx.lock().banner = Some(Banner::plain(
            "Can't reach the Spora service yet. Retrying…",
        ));
        render(ctx);
        return;
    };
    ctx.rt.spawn(f(ctx.clone(), grpc));
}

/// The Settings pane's callbacks.
pub fn wire(ctx: &Ctx, app: &AppWindow) {
    {
        let ctx = ctx.clone();
        let weak = app.as_weak();
        app.on_open_settings(move || {
            if let Some(app) = weak.upgrade() {
                open(&ctx, &app, None);
            }
        });
    }
    {
        let ctx = ctx.clone();
        let weak = app.as_weak();
        app.on_banner_act(move || {
            let Some(app) = weak.upgrade() else { return };
            ctx.lock().banner = None;
            open(&ctx, &app, Some("relays"));
        });
    }
    {
        let ctx = ctx.clone();
        app.on_settings_tab_changed(move |tab| {
            let mut s = ctx.lock();
            s.settings.telemetry_tab = tab == "telemetry";
            s.settings.switch_failed = false;
            drop(s);
            ctx.poke.notify_one();
        });
    }
    {
        let ctx = ctx.clone();
        app.on_set_use_spora(move |on| {
            let mut s = ctx.lock();
            // Shown at once; the service's answer follows.
            if let Some(r) = s.settings.relays.as_mut() {
                r.use_spora = on;
            }
            s.settings.fetch_asked = false;
            let demo = s.demo;
            drop(s);
            render(&ctx);
            if demo {
                return;
            }
            with_client(&ctx, move |ctx, grpc| async move {
                match grpc.set_spora_relays(on).await {
                    Ok(r) => ctx.lock().settings.relays = Some(r),
                    Err(e) => log::error!("{e}"),
                }
                ctx.poke.notify_one();
                ctx.refresh();
            });
        });
    }
    {
        let ctx = ctx.clone();
        let weak = app.as_weak();
        app.on_relay_save(move |original, endpoint, token, clear_token| {
            let Some(app) = weak.upgrade() else { return };
            let token = token.trim().to_string();
            let action = if !token.is_empty() {
                TokenAction::Set
            } else if clear_token {
                TokenAction::Clear
            } else {
                TokenAction::Keep
            };
            if ctx.lock().demo {
                demo_put_relay(&ctx, &original, &endpoint, action);
                app.set_show_relay_dialog(false);
                render(&ctx);
                return;
            }
            app.set_relay_busy(true);
            app.set_relay_error("".into());
            let (original, endpoint) = (original.to_string(), endpoint.to_string());
            with_client(&ctx, move |ctx, grpc| async move {
                let r = grpc.put_own_relay(original, endpoint, action, token).await;
                let ctx2 = ctx.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(app) = ctx2.app.upgrade() else {
                        return;
                    };
                    app.set_relay_busy(false);
                    match r {
                        Ok(resp) => {
                            if let Some(settings) = resp.settings {
                                ctx2.lock().settings.relays = Some(settings);
                            }
                            if resp.error == RelayAddressError::Ok as i32 {
                                app.set_show_relay_dialog(false);
                            } else {
                                app.set_relay_error(address_error(resp.error).into());
                            }
                        }
                        Err(e) => {
                            app.set_relay_error(format!("Couldn't save the relay. {e}").into())
                        }
                    }
                    render(&ctx2);
                })
                .ok();
            });
        });
    }
    {
        let ctx = ctx.clone();
        app.on_relay_remove(move |endpoint| {
            let endpoint = endpoint.to_string();
            let mut s = ctx.lock();
            if let Some(r) = s.settings.relays.as_mut() {
                r.own.retain(|o| o.endpoint != endpoint);
            }
            let demo = s.demo;
            drop(s);
            render(&ctx);
            if demo {
                return;
            }
            with_client(&ctx, move |ctx, grpc| async move {
                match grpc.remove_own_relay(endpoint).await {
                    Ok(r) => ctx.lock().settings.relays = Some(r),
                    Err(e) => log::error!("{e}"),
                }
                ctx.refresh();
            });
        });
    }
    {
        let ctx = ctx.clone();
        app.on_set_telemetry(move |on| set_telemetry(&ctx, on));
    }
    {
        let ctx = ctx.clone();
        app.on_save_report(move || save_report(&ctx));
    }
    {
        let ctx = ctx.clone();
        app.on_show_report(move || {
            if let Some(path) = ctx.lock().settings.report.path.clone() {
                reveal(&path);
            }
        });
    }
    {
        let ctx = ctx.clone();
        app.on_required_closed(move |contact| {
            if contact {
                open_url(&format!(
                    "mailto:{SUPPORT_MAIL}?subject=Telemetry%20required"
                ));
            }
            let pending = ctx
                .lock()
                .settings
                .telemetry
                .as_ref()
                .is_some_and(|t| t.required_notice_pending);
            if pending && !ctx.lock().demo {
                with_client(&ctx, |ctx, grpc| async move {
                    match grpc.acknowledge_telemetry_notice().await {
                        Ok(t) => ctx.lock().settings.telemetry = Some(t),
                        Err(e) => log::warn!("{e}"),
                    }
                    ctx.refresh();
                });
            }
        });
    }
}

fn set_telemetry(ctx: &Ctx, on: bool) {
    let mut s = ctx.lock();
    if s.settings.telemetry_busy {
        return; // a double click landing before the answer
    }
    s.settings.switch_failed = false;
    if s.demo {
        if let Some(t) = s.settings.telemetry.as_mut() {
            if !on && t.required {
                s.settings.show_required = true;
            } else {
                t.upload_enabled = on;
            }
        }
        drop(s);
        render(ctx);
        return;
    }
    s.settings.telemetry_busy = true;
    drop(s);
    render(ctx);
    with_client(ctx, move |ctx, grpc| async move {
        let r = grpc.set_telemetry(on).await;
        let mut s = ctx.lock();
        s.settings.telemetry_busy = false;
        match r {
            Ok(resp) => {
                if let Some(t) = resp.status {
                    s.settings.telemetry = Some(t);
                }
                if resp.refused_required {
                    // The account requires it: the switch stays on, and the
                    // user is owed the reason.
                    s.settings.show_required = true;
                } else if !resp.error.is_empty() {
                    log::error!("telemetry switch: {}", resp.error);
                    s.settings.switch_failed = true;
                }
            }
            Err(e) => {
                log::error!("{e}");
                s.settings.switch_failed = true;
            }
        }
        drop(s);
        ctx.refresh();
    });
}

/// The service bundles the report (it can read the records and logs; the
/// user cannot), the UI writes it to the user's Downloads folder.
fn save_report(ctx: &Ctx) {
    let mut s = ctx.lock();
    if s.settings.report.state == ReportState::Busy {
        return;
    }
    let demo = s.demo;
    s.settings.report = Report {
        state: ReportState::Busy,
        text: String::new(),
        path: None,
    };
    drop(s);
    render(ctx);
    if demo {
        let ctx = ctx.clone();
        slint::Timer::single_shot(Duration::from_millis(900), move || {
            let path = report_dir().join("spora-report-20261029-141205.zip");
            ctx.lock().settings.report = saved(path);
            render(&ctx);
        });
        return;
    }
    with_client(ctx, |ctx, grpc| async move {
        let result = match grpc.export_bug_report(String::new()).await {
            Ok(zip) => tokio::task::spawn_blocking(move || write_report(&zip))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r),
            Err(e) => Err(e),
        };
        ctx.lock().settings.report = match result {
            Ok(path) => {
                log::info!("diagnostic report saved to {}", path.display());
                saved(path)
            }
            Err(e) => {
                log::error!("diagnostic report: {e}");
                Report {
                    state: ReportState::Error,
                    text: format!("Couldn't save the report. {e}"),
                    path: None,
                }
            }
        };
        ctx.refresh();
    });
}

fn saved(path: PathBuf) -> Report {
    Report {
        state: ReportState::Saved,
        text: format!(
            "Saved as {}. Send it to {SUPPORT_MAIL} with a few words about what went wrong.",
            path.display()
        ),
        path: Some(path),
    }
}

/// The user's Downloads folder (else their home, else the app's own folder).
fn report_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| {
            d.download_dir()
                .map(Path::to_path_buf)
                .or_else(|| Some(d.home_dir().to_path_buf()))
        })
        .unwrap_or_else(super::log_dir)
}

fn write_report(zip: &[u8]) -> Result<PathBuf, String> {
    let dir = report_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("spora-report-{stamp}.zip"));
    std::fs::write(&path, zip).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Show `path` selected in Explorer.
fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer wants the quotes around the path only, not the switch.
        let r = std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn();
        if let Err(e) = r {
            log::warn!("cannot open Explorer: {e}");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }
}

/// Hand a URL (mailto:) to the user's default handler.
fn open_url(url: &str) {
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(not(windows))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = r {
        log::warn!("cannot open {url}: {e}");
    }
}

// ── Demo (SPORA_UI_DEMO) ─────────────────────────────────────────────

fn demo_put_relay(ctx: &Ctx, original: &str, endpoint: &str, action: TokenAction) {
    let mut s = ctx.lock();
    let Some(r) = s.settings.relays.as_mut() else {
        return;
    };
    let endpoint = endpoint.trim().to_string();
    let had = r
        .own
        .iter()
        .find(|o| o.endpoint == original)
        .is_some_and(|o| o.has_token);
    let relay = OwnRelay {
        endpoint,
        has_token: match action {
            TokenAction::Set => true,
            TokenAction::Clear => false,
            TokenAction::Keep => had,
        },
    };
    match r.own.iter().position(|o| o.endpoint == original) {
        Some(i) if !original.is_empty() => r.own[i] = relay,
        _ => r.own.push(relay),
    }
}

/// Seed the settings for the screenshot matrix: `settings` (Relays, ready,
/// two own relays), `relaydialog` (editing one, with an address error),
/// `telemetry` (on, registered), `required` (the force notice over the
/// Telemetry tab), `norelays` (no relays: the alert and the banner a new
/// link gets).
pub fn seed_demo(s: &mut AppState, app: &AppWindow, mode: &str) {
    let now = now_ms();
    let access = RelayAccess {
        available: true,
        cert_hash: "3f9a0c1d2e4b5a6978c0d1e2f3a4b5c6d7e8f901".into(),
        token_expires_unix_ms: now + 3 * 86_400_000,
        last_success_unix_ms: now - 3_600_000,
        advised: vec!["[2001:db8::1]:443".into(), "192.0.2.1:443".into()],
        ..RelayAccess::default()
    };
    s.settings.relays = Some(RelaySettings {
        use_spora: mode != "norelays",
        own: if mode == "norelays" {
            Vec::new()
        } else {
            vec![
                OwnRelay {
                    endpoint: "relay.example.org:443".into(),
                    has_token: true,
                },
                OwnRelay {
                    endpoint: "nz/[2001:db8::7]:8443".into(),
                    has_token: false,
                },
            ]
        },
        access: Some(access),
    });
    s.settings.telemetry = Some(TelemetryStatus {
        available: true,
        upload_enabled: matches!(mode, "telemetry" | "required"),
        required: mode == "required",
        device_id: if matches!(mode, "telemetry" | "required") {
            "6c1e0d52-8a3f-4b7e-9d21-0f4a7c3e5b18".into()
        } else {
            String::new()
        },
        last_upload_unix_ms: if mode == "telemetry" {
            now - 25 * 60_000
        } else {
            0
        },
        required_notice: REQUIRED_NOTICE.into(),
        ..TelemetryStatus::default()
    });
    match mode {
        "settings" => s.pane = Pane::Settings,
        "telemetry" | "required" => {
            s.pane = Pane::Settings;
            s.settings.telemetry_tab = true;
            app.set_settings_tab("telemetry".into());
            s.settings.show_required = mode == "required";
        }
        "relaydialog" => {
            s.pane = Pane::Settings;
            app.set_relay_original("relay.example.org:443".into());
            app.set_relay_had_token(true);
            app.set_relay_endpoint("relay.example.org".into());
            app.set_relay_error(address_error(RelayAddressError::Format as i32).into());
            app.set_show_relay_dialog(true);
        }
        "norelays" => {
            let (text, fix) = make_share_error(MakeShareError::NoRelays as i32, "");
            s.banner = Some(Banner {
                text,
                settings: fix,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(f: impl FnOnce(&mut RelayAccess)) -> RelaySettings {
        let mut a = RelayAccess {
            available: true,
            ..RelayAccess::default()
        };
        f(&mut a);
        RelaySettings {
            use_spora: true,
            own: vec![],
            access: Some(a),
        }
    }

    #[test]
    fn spora_status_follows_the_access() {
        let now = 1_000_000;
        assert_eq!(spora_status(None, now).1, "Checking…");
        let ready = access(|a| {
            a.token_expires_unix_ms = now + 1;
            a.advised = vec!["1.2.3.4:443".into()];
            a.last_success_unix_ms = now - 1;
        });
        assert_eq!(spora_status(Some(&ready), now), ("active", "Ready."));
        let expired = access(|a| {
            a.token_expires_unix_ms = now - 1;
            a.advised = vec!["1.2.3.4:443".into()];
            a.last_success_unix_ms = now - 10;
        });
        assert_eq!(spora_status(Some(&expired), now).0, "error");
        let unreachable = access(|a| a.last_error = "no answer".into());
        assert!(
            spora_status(Some(&unreachable), now)
                .1
                .starts_with("Couldn't reach")
        );
        let fresh = access(|_| {});
        assert!(never_fetched(&fresh));
        assert_eq!(spora_status(Some(&fresh), now).1, "Checking…");
        let down = access(|a| a.available = false);
        assert!(!never_fetched(&down));
        assert_eq!(spora_status(Some(&down), now).0, "error");
    }
}
