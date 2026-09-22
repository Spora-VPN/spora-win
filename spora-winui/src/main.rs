#![cfg_attr(windows, windows_subsystem = "windows")]

//! Spora Windows client — the UI process. A pure gRPC client of the
//! `Spora` service (spora-wincore) over its named pipe; it owns no tunnel
//! state of its own. The window follows the "master-detail instrument"
//! design (canvas 1b): the sidebar is the connection list, the detail pane
//! is the status surface; a tray icon + flyout keep sessions reachable while
//! the window is closed.

mod grpc;
mod store;
mod toast;
mod tray;

use grpc::GrpcClient;
use grpc::proto::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use store::Store;
use tray::{Tray, TrayEvent, TrayState};

slint::include_modules!();

const SINGLE_INSTANCE_PORT: u16 = 21453;

// ── Domain model ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Role {
    Share,
    Use,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Share => "share",
            Role::Use => "use",
        }
    }
    fn parse(s: &str) -> Option<Role> {
        match s {
            "share" => Some(Role::Share),
            "use" => Some(Role::Use),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Status {
    #[default]
    Inactive,
    Starting,
    Sharing,
    Connecting,
    Connected,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Inactive => "inactive",
            Status::Starting => "starting",
            Status::Sharing => "sharing",
            Status::Connecting => "connecting",
            Status::Connected => "connected",
            Status::Error => "error",
        }
    }
    fn is_active(self) -> bool {
        matches!(self, Status::Sharing | Status::Connected)
    }
    fn is_pending(self) -> bool {
        matches!(self, Status::Starting | Status::Connecting)
    }
    fn is_on(self) -> bool {
        self.is_active() || self.is_pending()
    }
}

/// Per-connection session state that is not persisted.
#[derive(Default)]
struct Runtime {
    status: Status,
    /// (friendly sentence, underlying detail) while `status == Error`.
    error: Option<(String, String)>,
    started_at: Option<Instant>,
    /// Bumped each time a session task starts for this id; a task only
    /// touches state on teardown if it is still the current generation, so a
    /// superseded task (quick off→on) never clobbers the newer session.
    generation: u64,
    /// The session task; aborting it drops the event stream, which makes the
    /// service tear the session (and its routes) down.
    task: Option<tokio::task::AbortHandle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Empty,
    Detail,
    NewShare,
    Creating,
}

struct AppState {
    store: Store,
    runtime: HashMap<String, Runtime>,
    selected: Option<(Role, String)>,
    pane: Pane,
    /// Id of the share whose link is being created (canvas 3a); None once
    /// the handoff pane (2c) took over or creation was cancelled.
    creating: Option<String>,
    /// Unattributed error banner above the detail content (canvas 2d).
    banner: Option<String>,
    service_online: bool,
    /// SPORA_UI_DEMO: seeded states, no service — for screenshot matrices.
    demo: bool,
}

impl AppState {
    fn rt(&mut self, id: &str) -> &mut Runtime {
        self.runtime.entry(id.to_string()).or_default()
    }
    fn status(&self, id: &str) -> Status {
        self.runtime.get(id).map(|r| r.status).unwrap_or_default()
    }
    fn label(&self, role: Role, id: &str) -> String {
        match role {
            Role::Share => self.store.shared.iter().find(|c| c.id == id).map(|c| c.label.clone()),
            Role::Use => self.store.uses.iter().find(|c| c.id == id).map(|c| c.label.clone()),
        }
        .unwrap_or_default()
    }
    fn exists(&self, role: Role, id: &str) -> bool {
        match role {
            Role::Share => self.store.shared.iter().any(|c| c.id == id),
            Role::Use => self.store.uses.iter().any(|c| c.id == id),
        }
    }
    /// Select something sensible after a delete/cancel: keep the current
    /// selection if it still exists, else the first share, else the first
    /// link, else the empty state.
    fn select_fallback(&mut self) {
        if let Some((role, id)) = &self.selected
            && self.exists(*role, id)
        {
            self.pane = Pane::Detail;
            return;
        }
        self.selected = self
            .store
            .shared
            .first()
            .map(|c| (Role::Share, c.id.clone()))
            .or_else(|| self.store.uses.first().map(|c| (Role::Use, c.id.clone())));
        self.pane = if self.selected.is_some() { Pane::Detail } else { Pane::Empty };
    }
    /// The share being created is no longer in creation (it failed, was
    /// stopped, or the user navigated away): drop the marker and leave the
    /// creation panes if they are showing.
    fn leave_creating(&mut self, id: &str) {
        if self.creating.as_deref() == Some(id) {
            self.creating = None;
            if matches!(self.pane, Pane::Creating | Pane::NewShare) {
                self.pane = if self.selected.is_some() { Pane::Detail } else { Pane::Empty };
            }
        }
    }
    fn set_error(&mut self, id: &str, friendly: &str, detail: impl Into<String>) {
        let rt = self.rt(id);
        rt.status = Status::Error;
        rt.error = Some((friendly.to_string(), detail.into()));
        rt.started_at = None;
        rt.task = None;
    }
}

/// Everything a background task needs to update the UI.
#[derive(Clone)]
struct Ctx {
    state: Arc<Mutex<AppState>>,
    app: slint::Weak<AppWindow>,
    flyout: slint::Weak<TrayFlyout>,
    rt: tokio::runtime::Handle,
    client: Arc<Mutex<Option<GrpcClient>>>,
}

impl Ctx {
    fn lock(&self) -> MutexGuard<'_, AppState> {
        self.state.lock().unwrap()
    }
    fn client(&self) -> Option<GrpcClient> {
        self.client.lock().unwrap().clone()
    }
    /// Re-render from any thread.
    fn refresh(&self) {
        let ctx = self.clone();
        slint::invoke_from_event_loop(move || render(&ctx)).ok();
    }
}

/// Click position and tray icon rect (physical pixels) a flyout opened from.
#[derive(Clone, Copy)]
struct FlyoutAnchor {
    x: f64,
    y: f64,
    icon: (f64, f64, f64, f64),
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    static LAST_TRAY: RefCell<Option<(TrayState, String)>> = const { RefCell::new(None) };
    /// Where the flyout was opened from.
    static FLYOUT_ANCHOR: std::cell::Cell<FlyoutAnchor> = const { std::cell::Cell::new(FlyoutAnchor { x: 0.0, y: 0.0, icon: (0.0, 0.0, 0.0, 0.0) }) };
    /// When the flyout last hid because it lost focus. A tray click first
    /// takes focus away (hiding the flyout) and only then reports itself,
    /// so a click right after such a hide is the one that closed it.
    static FLYOUT_HIDDEN_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

// ── URL handling ─────────────────────────────────────────────────────

/// Normalise what a user (or the OS) hands us into an `https://spora.to/…`
/// URL: the `spora:` URI scheme, a bare `spora.to/…`, or the URL itself.
fn convert_spora_url(input: &str) -> Option<String> {
    let s = input.trim();
    let rest = s
        .strip_prefix("https://spora.to/")
        .or_else(|| s.strip_prefix("http://spora.to/"))
        .or_else(|| s.strip_prefix("spora.to/"))
        .or_else(|| s.strip_prefix("spora:").map(|r| r.trim_start_matches('/')))?;
    // Only a share link has the shape s/<key>[?relay]; anything else is
    // "not a Spora link" (the dialog's validation error).
    let key = rest.strip_prefix("s/")?;
    let key = key.split(['?', '/', '#']).next().unwrap_or("");
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return None;
    }
    Some(format!("https://spora.to/{rest}"))
}

/// The link as shown in the UI: scheme stripped.
fn display_link(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string()
}

// ── Formatting ───────────────────────────────────────────────────────

fn fmt_date(ts: i64) -> String {
    use chrono::TimeZone;
    if ts <= 0 {
        return String::new();
    }
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%-d %B %Y").to_string())
        .unwrap_or_default()
}

fn fmt_datetime(ts: i64) -> String {
    use chrono::{Datelike, TimeZone};
    if ts <= 0 {
        return String::new();
    }
    let Some(d) = chrono::Local.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    let today = chrono::Local::now().date_naive();
    let day = d.date_naive();
    let time = d.format("%H:%M");
    if day == today {
        format!("today, {time}")
    } else if day.succ_opt() == Some(today) {
        format!("yesterday, {time}")
    } else if day.year() == today.year() {
        format!("{}, {time}", d.format("%-d %B"))
    } else {
        format!("{}, {time}", d.format("%-d %B %Y"))
    }
}

fn fmt_elapsed(since: Instant) -> String {
    let s = since.elapsed().as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Deterministic barcode: 2px bars in three quantized heights, seeded per
/// connection (the design system's mulberry32 sequence).
fn barcode(seed: u32, bars: usize) -> Vec<f32> {
    let mut t = seed;
    let mut next = move || {
        t = t.wrapping_add(0x6d2b_79f5);
        let mut r = (t ^ (t >> 15)).wrapping_mul(1 | t);
        r ^= r.wrapping_add((r ^ (r >> 7)).wrapping_mul(61 | r));
        ((r ^ (r >> 14)) as f64) / 4_294_967_296.0
    };
    (0..bars)
        .map(|_| {
            let v = next();
            if v > 0.82 {
                1.0
            } else if v > 0.5 {
                0.6
            } else {
                0.3
            }
        })
        .collect()
}

fn seed_for(id: &str) -> u32 {
    id.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32))
}

// ── Rendering: state → UI ────────────────────────────────────────────

fn item_for(s: &AppState, role: Role, id: &str) -> Option<ConnectionItem> {
    let rt = s.runtime.get(id);
    let status = rt.map(|r| r.status).unwrap_or_default();
    let (error, detail) = rt
        .and_then(|r| r.error.clone())
        .filter(|_| status == Status::Error)
        .unwrap_or_default();
    let (label, url, created, last) = match role {
        Role::Share => {
            let c = s.store.shared.iter().find(|c| c.id == id)?;
            (c.label.clone(), c.url.clone(), c.created_at, c.last_activity)
        }
        Role::Use => {
            let c = s.store.uses.iter().find(|c| c.id == id)?;
            (c.label.clone(), c.url.clone(), c.created_at, c.last_activity)
        }
    };
    Some(ConnectionItem {
        id: id.into(),
        role: role.as_str().into(),
        label: label.into(),
        link: display_link(&url).into(),
        url: url.into(),
        status: status.as_str().into(),
        error: error.into(),
        error_detail: detail.into(),
        created: fmt_date(created).into(),
        last_activity: fmt_datetime(last).into(),
        seed: seed_for(id) as i32,
    })
}

fn status_lines(s: &AppState) -> Vec<StatusLine> {
    // `text` is the window form (with names, canvas 1h/3b), `short` the
    // flyout header form (canvas 1d/2e: the names are in the rows below).
    let line = |shape: &str, text: String, short: &str, tone: &str| StatusLine {
        shape: shape.into(),
        text: text.into(),
        short: short.into(),
        tone: tone.into(),
    };
    let mut lines = Vec::new();
    for c in &s.store.uses {
        match s.status(&c.id) {
            Status::Connected => lines.push(line(
                "active",
                format!("↓ CONNECTED — {}", c.label.to_uppercase()),
                "↓ CONNECTED",
                "success",
            )),
            Status::Connecting => lines.push(line(
                "pending",
                format!("↓ CONNECTING VIA «{}»…", c.label.to_uppercase()),
                "↓ CONNECTING…",
                "pending",
            )),
            _ => {}
        }
    }
    let sharing: Vec<String> = s
        .store
        .shared
        .iter()
        .filter(|c| s.status(&c.id) == Status::Sharing)
        .map(|c| c.label.to_uppercase())
        .collect();
    if !sharing.is_empty() {
        // Names up to two, a count past that (canvas 3b).
        let tail = if sharing.len() <= 2 { sharing.join(", ") } else { sharing.len().to_string() };
        lines.push(line("active", format!("↑ SHARING — {tail}"), "↑ SHARING", "success"));
    }
    if s.store.shared.iter().any(|c| s.status(&c.id) == Status::Starting) {
        lines.push(line("pending", "↑ STARTING…".into(), "↑ STARTING…", "pending"));
    }
    if lines.is_empty() {
        // Nothing live: an error is the headline (canvas 1h), else OFFLINE.
        let error = s
            .runtime
            .values()
            .find(|r| r.status == Status::Error)
            .and_then(|r| r.error.as_ref())
            .map(|(friendly, _)| friendly.clone());
        match error {
            Some(sentence) => lines.push(line("error", sentence.clone(), &sentence, "error")),
            None => lines.push(line("", "OFFLINE".into(), "OFFLINE", "muted")),
        }
    }
    lines
}

fn tray_state(s: &AppState) -> TrayState {
    let statuses = s.runtime.values().map(|r| r.status);
    let mut state = TrayState::Offline;
    for st in statuses {
        state = match (state, st) {
            (_, Status::Error) => TrayState::Error,
            (TrayState::Error, _) => TrayState::Error,
            (_, Status::Starting | Status::Connecting) => TrayState::Pending,
            (TrayState::Pending, _) => TrayState::Pending,
            (_, Status::Sharing | Status::Connected) => TrayState::Active,
            (prev, _) => prev,
        };
    }
    state
}

/// Push the whole state into both windows and the tray. Cheap: lists are
/// small, and it is the only way anything reaches the screen.
fn render(ctx: &Ctx) {
    let Some(app) = ctx.app.upgrade() else { return };
    let s = ctx.lock();

    let shares: Vec<ConnectionItem> = s
        .store
        .shared
        .iter()
        .filter_map(|c| item_for(&s, Role::Share, &c.id))
        .collect();
    let uses: Vec<ConnectionItem> = s
        .store
        .uses
        .iter()
        .filter_map(|c| item_for(&s, Role::Use, &c.id))
        .collect();
    let lines = status_lines(&s);

    app.set_shares(slint::VecModel::from_slice(&shares));
    app.set_uses(slint::VecModel::from_slice(&uses));
    app.set_status_lines(slint::VecModel::from_slice(&lines));

    match &s.selected {
        Some((role, id)) => {
            app.set_selected_role(role.as_str().into());
            app.set_selected_id(id.as_str().into());
            if let Some(item) = item_for(&s, *role, id) {
                let seed = item.seed as u32;
                app.set_selected(item);
                app.set_selected_bars(slint::VecModel::from_slice(&barcode(seed, 220)));
            }
            let elapsed = s
                .runtime
                .get(id)
                .and_then(|r| r.started_at)
                .map(fmt_elapsed)
                .unwrap_or_default();
            app.set_selected_elapsed(elapsed.into());
        }
        None => {
            app.set_selected_role("".into());
            app.set_selected_id("".into());
            app.set_selected_elapsed("".into());
        }
    }
    app.set_pane(
        match s.pane {
            Pane::Empty => "empty",
            Pane::Detail => "detail",
            Pane::NewShare => "new-share",
            Pane::Creating => "creating",
        }
        .into(),
    );
    app.set_banner(s.banner.clone().unwrap_or_default().into());

    if let Some(flyout) = ctx.flyout.upgrade() {
        flyout.set_shares(slint::VecModel::from_slice(&shares));
        flyout.set_uses(slint::VecModel::from_slice(&uses));
        flyout.set_status_lines(slint::VecModel::from_slice(&lines));
    }

    let tray = tray_state(&s);
    let tooltip = {
        let text: Vec<String> = lines.iter().map(|l| l.text.to_string()).collect();
        format!("Spora — {}", text.join(" · "))
    };
    drop(s);
    LAST_TRAY.with(|last| {
        let mut last = last.borrow_mut();
        if last.as_ref().is_none_or(|(st, tip)| *st != tray || *tip != tooltip) {
            TRAY.with(|t| {
                if let Some(t) = t.borrow().as_ref() {
                    t.set_state(tray, &tooltip);
                }
            });
            *last = Some((tray, tooltip));
        }
    });
}

// ── Window helpers ───────────────────────────────────────────────────

fn show_main(app: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    app.window().show().ok();
    app.window().with_winit_window(|w| {
        w.set_minimized(false);
        w.focus_window();
    });
    // hide() drops the winit window and show() makes a new one, so the
    // platform finish has to be applied again.
    apply_window_chrome(app.as_weak(), 0);
}

/// Closing the window keeps the client (and its sessions) alive in the
/// tray; without a tray there is nothing to come back from, so quit.
fn hide_or_quit(app: &AppWindow, tray_ok: bool) {
    if tray_ok && cfg!(windows) {
        app.window().hide().ok();
    } else {
        slint::quit_event_loop().ok();
    }
}

/// The frameless window's platform finish: the DWM drop shadow (and the 1px
/// frame line it comes with) and Windows 11's rounded corners. The winit
/// window only exists once the event loop runs, hence the retry.
fn apply_window_chrome(weak: slint::Weak<AppWindow>, attempt: u32) {
    let Some(app) = weak.upgrade() else { return };
    #[allow(unused_variables)]
    let applied = {
        use slint::winit_030::WinitWindowAccessor;
        app.window()
            .with_winit_window(|w| {
                #[cfg(windows)]
                {
                    use slint::winit_030::winit::platform::windows::{CornerPreference, WindowExtWindows};
                    w.set_undecorated_shadow(true);
                    w.set_corner_preference(CornerPreference::Round);
                }
                let _ = w;
            })
            .is_some()
    };
    if !applied && attempt < 40 {
        slint::Timer::single_shot(Duration::from_millis(50), move || apply_window_chrome(weak, attempt + 1));
    }
}

/// Mirror the OS maximised state into the UI (restore glyph, no resize
/// borders). Read on the next loop turn: winit updates its flags from the
/// same WM_SIZE that produced this event.
fn track_maximized(app: &AppWindow) {
    use slint::winit_030::{WinitWindowAccessor, winit};
    let weak = app.as_weak();
    app.window().on_winit_window_event(move |_, ev| {
        if let winit::event::WindowEvent::Resized(_) = ev {
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(app) = weak.upgrade() {
                    let maximized = app.window().with_winit_window(|w| w.is_maximized()).unwrap_or(false);
                    if app.get_maximized() != maximized {
                        app.set_maximized(maximized);
                    }
                }
            });
        }
        slint::winit_030::EventResult::Propagate
    });
}

/// Place the flyout next to its anchor: right-aligned to the tray icon
/// (or the click, if the icon rect is unknown), opening upwards from a
/// bottom taskbar and downwards from a top one. Called on show and again
/// whenever the window is resized, because right after `show()` the size
/// can still be the one from the previous open.
fn place_flyout(flyout: &TrayFlyout) {
    let FlyoutAnchor { x, y, icon } = FLYOUT_ANCHOR.with(|a| a.get());
    let size = flyout.window().size();
    let (w, h) = (size.width as f64, size.height as f64);
    let (ix, iy, iw, ih) = if icon.2 > 0.0 { icon } else { (x, y, 0.0, 0.0) };
    let px = (ix + iw - w).max(0.0);
    let py = if iy > h + 8.0 { iy - h - 8.0 } else { iy + ih + 8.0 };
    log::debug!("flyout placed: size {w}x{h} at {px},{py} (icon {icon:?})");
    flyout
        .window()
        .set_position(slint::PhysicalPosition::new(px as i32, py as i32));
}

fn show_flyout(flyout: &TrayFlyout, x: f64, y: f64, icon: (f64, f64, f64, f64)) {
    use slint::winit_030::WinitWindowAccessor;
    FLYOUT_ANCHOR.with(|a| a.set(FlyoutAnchor { x, y, icon }));
    if let Err(e) = flyout.window().show() {
        log::warn!("flyout show failed: {e}");
    }
    place_flyout(flyout);
    flyout.window().with_winit_window(|w| {
        #[cfg(windows)]
        {
            use slint::winit_030::winit::platform::windows::WindowExtWindows;
            w.set_skip_taskbar(true);
        }
        w.focus_window();
    });
}

// ── Session tasks ────────────────────────────────────────────────────

/// Start (or restart) a share session for `id`; runs until its stream ends.
async fn run_share(ctx: Ctx, grpc: GrpcClient, id: String, key: String) {
    let my_gen = {
        let mut s = ctx.lock();
        let rt = s.rt(&id);
        rt.generation += 1;
        rt.status = Status::Starting;
        rt.error = None;
        rt.started_at = None;
        rt.generation
    };
    ctx.refresh();

    let mut stream = match grpc.share(key).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Share RPC failed: {e}");
            let mut s = ctx.lock();
            if s.rt(&id).generation == my_gen {
                s.set_error(&id, "Couldn't start sharing.", e);
                s.leave_creating(&id);
            }
            drop(s);
            ctx.refresh();
            return;
        }
    };

    let mut ended_in_error = false;
    loop {
        let event = match stream.message().await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(status) => {
                // The stream broke under us (service restart, pipe reset):
                // a stop we did not ask for, so it is shown as an error.
                log::error!("Share stream failed: {status}");
                let mut s = ctx.lock();
                if s.rt(&id).generation == my_gen {
                    let was_live = s.status(&id).is_active();
                    let label = s.label(Role::Share, &id);
                    s.set_error(&id, "Sharing was interrupted.", status.to_string());
                    s.leave_creating(&id);
                    ended_in_error = true;
                    drop(s);
                    if was_live {
                        toast::show("Sharing interrupted", &format!("«{label}» is no longer shared."));
                    }
                }
                break;
            }
        };
        let Some(event) = event.event else { continue };
        // One lock per event: the generation check and the mutation it
        // guards happen under the same guard.
        let mut s = ctx.lock();
        if s.rt(&id).generation != my_gen {
            break; // superseded: the newer task owns the state now
        }
        match event {
            share_event::Event::Ready(ready) => {
                let rt = s.rt(&id);
                rt.status = Status::Sharing;
                rt.started_at = Some(Instant::now());
                s.store.set_share_url(&id, &ready.url);
                s.store.touch_shared(&id);
                if s.creating.as_deref() == Some(&id) {
                    // The handoff pane (canvas 2c) shows THIS share.
                    s.creating = None;
                    s.selected = Some((Role::Share, id.clone()));
                    s.pane = Pane::NewShare;
                }
                drop(s);
                ctx.refresh();
            }
            share_event::Event::Connected(_) => {
                s.store.touch_shared(&id);
                let label = s.label(Role::Share, &id);
                drop(s);
                toast::show("Sharing", &format!("«{label}» is now using your internet."));
                ctx.refresh();
            }
            share_event::Event::Error(err) => {
                log::error!("Share error: {}", err.message);
                let label = s.label(Role::Share, &id);
                s.set_error(&id, "Couldn't start sharing.", err.message.clone());
                s.leave_creating(&id);
                drop(s);
                toast::show("Couldn't start sharing", &format!("«{label}»: {}", err.message));
                ended_in_error = true;
                break;
            }
            share_event::Event::SessionEnded(ended) => {
                log::info!("Share session ended: {}", ended.reason);
                s.store.touch_shared(&id);
                drop(s);
                ctx.refresh();
            }
            share_event::Event::ConnLogDegraded(d) => {
                log::error!("Connection log degraded: {}", d.detail);
                s.banner = Some(format!("The connection log couldn't be written: {}", d.detail));
                drop(s);
                ctx.refresh();
            }
        }
    }

    // Stream ended — mark inactive, unless a newer task for this id has
    // superseded us (then its state is not ours to touch).
    let mut s = ctx.lock();
    if s.rt(&id).generation == my_gen {
        let rt = s.rt(&id);
        if !ended_in_error {
            rt.status = Status::Inactive;
            rt.error = None;
        }
        rt.started_at = None;
        rt.task = None;
        s.store.touch_shared(&id);
        s.leave_creating(&id);
    }
    drop(s);
    ctx.refresh();
}

/// Start a connect ("use") session for `id`; runs until its stream ends.
async fn run_use(ctx: Ctx, grpc: GrpcClient, id: String, url: String) {
    let my_gen = {
        let mut s = ctx.lock();
        let rt = s.rt(&id);
        rt.generation += 1;
        rt.status = Status::Connecting;
        rt.error = None;
        rt.started_at = None;
        rt.generation
    };
    ctx.refresh();

    let mut stream = match grpc.connect_vpn(url).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Connect RPC failed: {e}");
            let mut s = ctx.lock();
            if s.rt(&id).generation == my_gen {
                s.set_error(&id, "Couldn't connect.", e);
            }
            drop(s);
            ctx.refresh();
            return;
        }
    };

    let mut ended_in_error = false;
    loop {
        let event = match stream.message().await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(status) => {
                log::error!("Connect stream failed: {status}");
                let mut s = ctx.lock();
                if s.rt(&id).generation == my_gen {
                    let was_live = s.status(&id).is_active();
                    let label = s.label(Role::Use, &id);
                    s.set_error(&id, "Connection lost.", status.to_string());
                    ended_in_error = true;
                    drop(s);
                    if was_live {
                        toast::show("Connection lost", &format!("Couldn't stay connected via «{label}»."));
                    }
                }
                break;
            }
        };
        let Some(event) = event.event else { continue };
        let mut s = ctx.lock();
        if s.rt(&id).generation != my_gen {
            break;
        }
        match event {
            connect_event::Event::Connected(_) => {
                let rt = s.rt(&id);
                rt.status = Status::Connected;
                rt.started_at = Some(Instant::now());
                s.store.touch_use(&id);
                let label = s.label(Role::Use, &id);
                drop(s);
                toast::show("Connected", &format!("Your traffic goes through «{label}»."));
                ctx.refresh();
            }
            connect_event::Event::Disconnected(d) => {
                log::info!("Disconnected: {}", d.reason);
                let was_connected = s.status(&id) == Status::Connected;
                let label = s.label(Role::Use, &id);
                // A drop we did not ask for is an error the user must see.
                s.set_error(&id, "Connection lost.", d.reason.clone());
                drop(s);
                if was_connected {
                    toast::show("Connection lost", &format!("Couldn't stay connected via «{label}»."));
                }
                ended_in_error = true;
                break;
            }
            connect_event::Event::Error(err) => {
                log::error!("Connect error: {}", err.message);
                let label = s.label(Role::Use, &id);
                s.set_error(&id, "Couldn't connect.", err.message.clone());
                drop(s);
                toast::show("Couldn't connect", &format!("Couldn't connect via «{label}»."));
                ended_in_error = true;
                break;
            }
        }
    }

    let mut s = ctx.lock();
    if s.rt(&id).generation == my_gen {
        let rt = s.rt(&id);
        if !ended_in_error {
            rt.status = Status::Inactive;
            rt.error = None;
        }
        rt.started_at = None;
        rt.task = None;
        s.store.touch_use(&id);
    }
    drop(s);
    ctx.refresh();
}

// ── Actions (UI thread) ──────────────────────────────────────────────

/// Turn a connection on: start its session (or, in demo mode, pretend).
fn start(ctx: &Ctx, role: Role, id: &str) {
    let mut s = ctx.lock();
    if s.demo {
        let rt = s.rt(id);
        rt.status = match role {
            Role::Share => Status::Sharing,
            Role::Use => Status::Connected,
        };
        rt.error = None;
        rt.started_at = Some(Instant::now());
        drop(s);
        render(ctx);
        return;
    }
    let Some(grpc) = ctx.client() else {
        s.banner = Some("Can't reach the Spora service yet. Retrying…".into());
        drop(s);
        render(ctx);
        return;
    };
    let task = match role {
        Role::Share => {
            let Some(key) = s.store.shared.iter().find(|c| c.id == id).map(|c| c.secret_key.clone()) else { return };
            ctx.rt.spawn(run_share(ctx.clone(), grpc, id.to_string(), key))
        }
        Role::Use => {
            let Some(url) = s.store.uses.iter().find(|c| c.id == id).map(|c| c.url.clone()) else { return };
            ctx.rt.spawn(run_use(ctx.clone(), grpc, id.to_string(), url))
        }
    };
    s.rt(id).task = Some(task.abort_handle());
    drop(s);
    render(ctx);
}

/// Turn a connection off: stop the session. Works for pending sessions too
/// (the toggle stays live = cancel). Aborting the task drops the event
/// stream, and the service tears a session down with its stream, so no
/// Stop/Disconnect RPC is needed (it would only race the teardown).
fn stop(ctx: &Ctx, role: Role, id: &str) {
    let mut s = ctx.lock();
    let rt = s.rt(id);
    rt.generation += 1; // orphan the running task's teardown
    if let Some(task) = rt.task.take() {
        task.abort();
    }
    rt.status = Status::Inactive;
    rt.error = None;
    rt.started_at = None;
    match role {
        Role::Share => s.store.touch_shared(id),
        Role::Use => s.store.touch_use(id),
    }
    // A share stopped mid-creation is no longer being created, and the
    // handoff pane makes no sense for a share that is off.
    s.leave_creating(id);
    if s.pane == Pane::NewShare && s.selected.as_ref().is_some_and(|(_, sel)| sel == id) {
        s.pane = Pane::Detail;
    }
    drop(s);
    render(ctx);
}

fn share_new(ctx: &Ctx, app: &AppWindow) {
    let mut s = ctx.lock();
    if s.creating.is_some() {
        return;
    }
    if s.demo {
        // Simulate the busy moment, then the handoff.
        let label = s.store.next_label(true);
        let id = s.store.add_shared(label, String::new());
        s.selected = Some((Role::Share, id.clone()));
        s.pane = Pane::Creating;
        s.creating = Some(id.clone());
        drop(s);
        app.set_new_share_name("".into());
        render(ctx);
        let ctx2 = ctx.clone();
        slint::Timer::single_shot(Duration::from_millis(1500), move || {
            let mut s = ctx2.lock();
            if s.creating.as_deref() != Some(&id) {
                return;
            }
            s.creating = None;
            s.pane = Pane::NewShare;
            s.store.set_share_url(&id, "https://spora.to/s/9q1x84hh2m");
            let rt = s.rt(&id);
            rt.status = Status::Sharing;
            rt.started_at = Some(Instant::now());
            drop(s);
            render(&ctx2);
        });
        return;
    }
    let Some(grpc) = ctx.client() else {
        s.banner = Some("Can't reach the Spora service yet. Retrying…".into());
        drop(s);
        render(ctx);
        return;
    };
    // Reserve the busy pane right away; the id arrives with the key.
    s.creating = Some(String::new());
    s.pane = Pane::Creating;
    drop(s);
    app.set_new_share_name("".into());
    render(ctx);

    let ctx2 = ctx.clone();
    ctx.rt.spawn(async move {
        let key = grpc.make_secret_key().await;
        let mut s = ctx2.lock();
        if s.creating.as_deref() != Some("") {
            return; // cancelled while the key was being made
        }
        match key {
            Ok(key) => {
                let label = s.store.next_label(true);
                let id = s.store.add_shared(label, key.clone());
                s.selected = Some((Role::Share, id.clone()));
                s.creating = Some(id.clone());
                let task = ctx2.rt.spawn(run_share(ctx2.clone(), grpc, id.clone(), key));
                s.rt(&id).task = Some(task.abort_handle());
            }
            Err(e) => {
                log::error!("MakeSecretKey failed: {e}");
                s.creating = None;
                s.banner = Some(format!("Couldn't create a link. {e}"));
                s.select_fallback();
            }
        }
        drop(s);
        ctx2.refresh();
    });
}

fn cancel_new_share(ctx: &Ctx) {
    let mut s = ctx.lock();
    let Some(id) = s.creating.take() else { return };
    if !id.is_empty() {
        drop(s);
        stop(ctx, Role::Share, &id);
        s = ctx.lock();
        s.store.remove_shared(&id);
        s.runtime.remove(&id);
        if s.selected.as_ref().is_some_and(|(_, sel)| *sel == id) {
            s.selected = None;
        }
    }
    s.select_fallback();
    drop(s);
    render(ctx);
}

fn open_connect_dialog(app: &AppWindow, url: &str) {
    app.set_dialog_url(url.into());
    app.set_dialog_name("".into());
    app.set_dialog_url_error("".into());
    // The dialog focuses the link field on open, or the name field when
    // the link came prefilled.
    app.set_dialog_focus_name(!url.is_empty());
    app.set_show_connect_dialog(true);
}

/// Save the dialog: validate the link, reuse a saved entry with the same
/// link, otherwise add one; then connect.
fn dialog_save(ctx: &Ctx, app: &AppWindow, url: &str, name: &str) {
    let Some(url) = convert_spora_url(url) else {
        app.set_dialog_url_error("That doesn't look like a Spora link.".into());
        return;
    };
    app.set_show_connect_dialog(false);
    app.set_dialog_url_error("".into());
    let mut s = ctx.lock();
    let existing = s.store.uses.iter().find(|c| c.url == url).map(|c| c.id.clone());
    let id = match existing {
        Some(id) => {
            let name = name.trim();
            if !name.is_empty() {
                s.store.rename_use(&id, name.to_string());
            }
            id
        }
        None => {
            let label = if name.trim().is_empty() { s.store.next_label(false) } else { name.trim().to_string() };
            s.store.add_use(label, url)
        }
    };
    s.selected = Some((Role::Use, id.clone()));
    s.pane = Pane::Detail;
    s.creating = None;
    let on = s.status(&id).is_on();
    drop(s);
    if !on {
        start(ctx, Role::Use, &id);
    } else {
        render(ctx);
    }
}

/// A `spora:` link arrived (command line or a second instance): a known
/// link is selected and connected; an unknown one opens the dialog prefilled.
fn handle_incoming_url(ctx: &Ctx, app: &AppWindow, url: &str) {
    log::info!("Handling incoming URL: {url}");
    show_main(app);
    let mut s = ctx.lock();
    s.creating = None;
    let known = s.store.uses.iter().find(|c| c.url == url).map(|c| c.id.clone());
    match known {
        Some(id) => {
            s.selected = Some((Role::Use, id.clone()));
            s.pane = Pane::Detail;
            let on = s.status(&id).is_on();
            drop(s);
            if !on {
                start(ctx, Role::Use, &id);
            } else {
                render(ctx);
            }
        }
        None => {
            drop(s);
            render(ctx);
            open_connect_dialog(app, url);
        }
    }
}

fn delete(ctx: &Ctx, role: Role, id: &str) {
    if ctx.lock().status(id).is_on() {
        stop(ctx, role, id);
    }
    let mut s = ctx.lock();
    match role {
        Role::Share => s.store.remove_shared(id),
        Role::Use => s.store.remove_use(id),
    }
    s.runtime.remove(id);
    if s.selected.as_ref().is_some_and(|(_, sel)| sel == id) {
        s.selected = None;
    }
    s.select_fallback();
    drop(s);
    render(ctx);
}

fn rename(ctx: &Ctx, role: Role, id: &str, label: &str) {
    let label = label.trim();
    if label.is_empty() {
        return;
    }
    let mut s = ctx.lock();
    match role {
        Role::Share => s.store.rename_shared(id, label.to_string()),
        Role::Use => s.store.rename_use(id, label.to_string()),
    }
    drop(s);
    render(ctx);
}

// ── Demo seeding (SPORA_UI_DEMO) ─────────────────────────────────────

fn seed_demo(s: &mut AppState, mode: &str) {
    s.demo = true;
    s.service_online = true;
    if mode == "empty" {
        return;
    }
    let day = 86_400;
    let t = store::now();
    let mut share = |label: &str, status: Status, url: &str, created: i64, last: i64, err: Option<(&str, &str)>| {
        let id = s.store.add_shared(label.into(), String::new());
        s.store.set_share_url(&id, url);
        if let Some(c) = s.store.shared.iter_mut().find(|c| c.id == id) {
            c.created_at = created;
            c.last_activity = last;
        }
        let rt = s.rt(&id);
        rt.status = status;
        if status.is_active() {
            rt.started_at = Some(Instant::now() - Duration::from_secs(3792));
        }
        if let Some((f, d)) = err {
            rt.error = Some((f.into(), d.into()));
        }
        id
    };
    let mom = share("Mom's phone", Status::Sharing, "https://spora.to/s/4kd02mze7q", t - 141 * day, t - 600, None);
    let alesya = share("Alesya's phone", Status::Starting, "https://spora.to/s/8h2ks0qz1a", t - 30 * day, t - 3 * day, None);
    let work = share("Work laptop", Status::Error, "https://spora.to/s/0pl3mx7bq2", t - 12 * day, t - day, Some(("Couldn't start sharing.", "relay handshake timed out (relay.spora.to:443)")));
    share("Old tablet", Status::Inactive, "https://spora.to/s/x91ke2n0ma", t - 200 * day, t - 40 * day, None);
    let mut link = |label: &str, status: Status, url: &str, created: i64, last: i64| {
        let id = s.store.add_use(label.into(), url.into());
        if let Some(c) = s.store.uses.iter_mut().find(|c| c.id == id) {
            c.created_at = created;
            c.last_activity = last;
        }
        let rt = s.rt(&id);
        rt.status = status;
        if status.is_active() {
            rt.started_at = Some(Instant::now() - Duration::from_secs(8077));
        }
        id
    };
    let dana = link("Dana's home", Status::Connected, "https://spora.to/s/7f3kq2w8r1?r=relay.spora.to:443", t - 101 * day, t - 120);
    link("Flat in Berlin", Status::Inactive, "https://spora.to/s/2nn7x0ab3c", t - 60 * day, t - 9 * day);
    match mode {
        "share" => {
            s.selected = Some((Role::Share, mom));
            s.pane = Pane::Detail;
        }
        "new" => {
            s.selected = Some((Role::Share, mom));
            s.pane = Pane::NewShare;
        }
        "creating" => {
            s.selected = Some((Role::Share, mom));
            s.pane = Pane::Creating;
            s.creating = Some(String::new());
        }
        "banner" => {
            s.selected = Some((Role::Use, dana));
            s.pane = Pane::Detail;
            s.banner = Some("Couldn't connect. Ask your friend for a new link.".into());
        }
        "error" => {
            // Nothing live, one failed share: the error is the headline
            // (canvas 1h) in the sidebar, the flyout and the tray.
            for id in [&mom, &alesya, &dana] {
                let rt = s.rt(id);
                rt.status = Status::Inactive;
                rt.started_at = None;
            }
            s.selected = Some((Role::Share, work));
            s.pane = Pane::Detail;
        }
        _ => {
            s.selected = Some((Role::Use, dana));
            s.pane = Pane::Detail;
        }
    }
}

// ── main ─────────────────────────────────────────────────────────────

fn main() {
    // A `spora:` URL on the command line (URI-scheme launch).
    let initial_url = std::env::args().skip(1).find_map(|arg| convert_spora_url(&arg));

    // Single-instance enforcement: try to bind a known port. If we can't,
    // another instance is running — hand it the URL and exit.
    let instance_listener = match std::net::TcpListener::bind(("127.0.0.1", SINGLE_INSTANCE_PORT)) {
        Ok(listener) => {
            listener.set_nonblocking(true).ok();
            Some(listener)
        }
        Err(bind_err) => {
            use std::io::Write;
            match std::net::TcpStream::connect(("127.0.0.1", SINGLE_INSTANCE_PORT)) {
                Ok(mut stream) => {
                    // An empty line just raises the other instance's window.
                    let _ = writeln!(stream, "{}", initial_url.clone().unwrap_or_default());
                    std::process::exit(0);
                }
                Err(_) => {
                    // The port is taken by something that is not Spora:
                    // run anyway, just without single-instance enforcement.
                    eprintln!("single-instance port {SINGLE_INSTANCE_PORT} unavailable ({bind_err}); continuing");
                    None
                }
            }
        }
    };

    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir).expect("failed to create log directory");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("spora-winui.log"))
        .expect("failed to open log file");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .init();

    let demo_mode = std::env::var("SPORA_UI_DEMO").ok();

    // On Windows, spora-wincore runs as the "Spora" Windows service; on Linux
    // dev builds, spawn it as a plain child process.
    #[cfg(not(windows))]
    if demo_mode.is_none() {
        launch_wincore();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let app = AppWindow::new().unwrap();
    let flyout = TrayFlyout::new().unwrap();

    let mut state = AppState {
        store: if demo_mode.is_some() { Store::ephemeral() } else { Store::load() },
        runtime: HashMap::new(),
        selected: None,
        pane: Pane::Empty,
        creating: None,
        banner: None,
        service_online: false,
        demo: false,
    };
    match &demo_mode {
        Some(mode) => seed_demo(&mut state, mode),
        None => state.select_fallback(),
    }

    let ctx = Ctx {
        state: Arc::new(Mutex::new(state)),
        app: app.as_weak(),
        flyout: flyout.as_weak(),
        rt: rt.handle().clone(),
        client: Arc::new(Mutex::new(None)),
    };

    // ── Tray + toasts ────────────────────────────────────────────────
    toast::init();
    let tray_ok = {
        let ctx2 = ctx.clone();
        match Tray::new(move |ev| {
            log::debug!("tray event: {ev:?}");
            let ctx = ctx2.clone();
            if let Err(e) = slint::invoke_from_event_loop(move || on_tray_event(&ctx, ev)) {
                log::warn!("tray event could not reach the UI thread: {e}");
            }
        }) {
            Ok(tray) => {
                TRAY.with(|t| *t.borrow_mut() = Some(tray));
                true
            }
            Err(e) => {
                log::warn!("tray icon unavailable: {e}");
                false
            }
        }
    };

    // The flyout hides when it loses focus (a click anywhere else).
    {
        use slint::winit_030::{WinitWindowAccessor, winit};
        let weak = flyout.as_weak();
        flyout.window().on_winit_window_event(move |_, ev| {
            match ev {
                winit::event::WindowEvent::Focused(false) if std::env::var_os("SPORA_UI_DEMO_FLYOUT").is_none() => {
                    let weak = weak.clone();
                    slint::Timer::single_shot(Duration::ZERO, move || {
                        if let Some(f) = weak.upgrade()
                            && f.window().is_visible()
                        {
                            f.window().hide().ok();
                            FLYOUT_HIDDEN_AT.with(|t| t.set(Some(Instant::now())));
                        }
                    });
                }
                // The size settles after show(); place it again then.
                winit::event::WindowEvent::Resized(_) => {
                    let weak = weak.clone();
                    slint::Timer::single_shot(Duration::ZERO, move || {
                        if let Some(f) = weak.upgrade()
                            && f.window().is_visible()
                        {
                            place_flyout(&f);
                        }
                    });
                }
                _ => {}
            }
            slint::winit_030::EventResult::Propagate
        });
    }

    // Alt+F4 / the taskbar's close: same as our close button.
    {
        let weak = app.as_weak();
        app.window().on_close_requested(move || {
            if let Some(app) = weak.upgrade() {
                hide_or_quit(&app, tray_ok);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    track_maximized(&app);

    // ── Service connection ───────────────────────────────────────────
    if demo_mode.is_none() {
        let ctx2 = ctx.clone();
        rt.spawn(async move {
            let mut attempts = 0u32;
            loop {
                match GrpcClient::connect().await {
                    Ok(c) => {
                        *ctx2.client.lock().unwrap() = Some(c);
                        let mut s = ctx2.lock();
                        s.service_online = true;
                        if s.banner.as_deref().is_some_and(|b| b.starts_with("Can't reach the Spora service")) {
                            s.banner = None;
                        }
                        drop(s);
                        ctx2.refresh();
                        break;
                    }
                    Err(e) => {
                        attempts += 1;
                        log::warn!("gRPC connect failed, retrying in 2s: {e}");
                        if attempts == 3 {
                            ctx2.lock().banner = Some("Can't reach the Spora service. Retrying…".into());
                            ctx2.refresh();
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
    }

    // ── Incoming URLs (command line + later instances) ───────────────
    {
        let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Some(url) = initial_url {
            let _ = url_tx.send(url);
        }
        if let Some(std_listener) = instance_listener {
            let url_tx = url_tx.clone();
            rt.spawn(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener).expect("failed to convert TcpListener");
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let url_tx = url_tx.clone();
                    tokio::spawn(async move {
                        use tokio::io::AsyncBufReadExt;
                        let mut buf = Vec::new();
                        let mut reader = tokio::io::BufReader::new(stream);
                        if reader.read_until(b'\n', &mut buf).await.is_ok() {
                            let line = String::from_utf8_lossy(&buf).trim().to_string();
                            let _ = url_tx.send(line);
                        }
                    });
                }
            });
        }
        let ctx2 = ctx.clone();
        rt.spawn(async move {
            while let Some(line) = url_rx.recv().await {
                let url = convert_spora_url(&line);
                // Wait for the service before acting on a link (a bare line
                // from a second instance just raises the window).
                while url.is_some() && ctx2.client().is_none() && !ctx2.lock().demo {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                let ctx = ctx2.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(app) = ctx.app.upgrade() else { return };
                    match url {
                        Some(url) => handle_incoming_url(&ctx, &app, &url),
                        None => show_main(&app),
                    }
                })
                .ok();
            }
        });
    }

    // ── Window callbacks ─────────────────────────────────────────────
    {
        let ctx = ctx.clone();
        app.on_select(move |role, id| {
            let Some(role) = Role::parse(&role) else { return };
            let mut s = ctx.lock();
            s.creating = None; // navigating away ends the handoff, not the share
            s.selected = Some((role, id.to_string()));
            s.pane = Pane::Detail;
            drop(s);
            render(&ctx);
        });
    }
    {
        let ctx = ctx.clone();
        let weak = app.as_weak();
        app.on_share_new(move || {
            if let Some(app) = weak.upgrade() {
                share_new(&ctx, &app);
            }
        });
    }
    {
        let weak = app.as_weak();
        app.on_connect_new(move || {
            if let Some(app) = weak.upgrade() {
                open_connect_dialog(&app, "");
            }
        });
    }
    {
        let ctx = ctx.clone();
        app.on_toggle(move |id, role, on| {
            let Some(role) = Role::parse(&role) else { return };
            if on { start(&ctx, role, &id) } else { stop(&ctx, role, &id) }
        });
    }
    {
        let ctx = ctx.clone();
        app.on_retry(move |role, id| {
            if let Some(role) = Role::parse(&role) {
                start(&ctx, role, &id);
            }
        });
    }
    {
        let ctx = ctx.clone();
        app.on_delete(move |role, id| {
            if let Some(role) = Role::parse(&role) {
                delete(&ctx, role, &id);
            }
        });
    }
    {
        let ctx = ctx.clone();
        app.on_rename(move |role, id, label| {
            if let Some(role) = Role::parse(&role) {
                rename(&ctx, role, &id, &label);
            }
        });
    }
    {
        let ctx = ctx.clone();
        app.on_cancel_new_share(move || cancel_new_share(&ctx));
    }
    {
        let ctx = ctx.clone();
        app.on_finish_new_share(move |id, label| {
            rename(&ctx, Role::Share, &id, &label);
            let mut s = ctx.lock();
            s.pane = Pane::Detail;
            drop(s);
            render(&ctx);
        });
    }
    {
        let ctx = ctx.clone();
        let weak = app.as_weak();
        app.on_dialog_save(move |url, name| {
            if let Some(app) = weak.upgrade() {
                dialog_save(&ctx, &app, &url, &name);
            }
        });
    }
    {
        let ctx = ctx.clone();
        app.on_dismiss_banner(move || {
            ctx.lock().banner = None;
            render(&ctx);
        });
    }
    app.on_copy(move |text| copy_to_clipboard(&text));

    // ── Window chrome (frameless: the caption and controls are ours) ──
    {
        use slint::winit_030::{WinitWindowAccessor, winit};
        let weak = app.as_weak();
        app.on_start_drag(move || {
            if let Some(app) = weak.upgrade() {
                app.window().with_winit_window(|w| {
                    if let Err(e) = w.drag_window() {
                        log::warn!("drag_window: {e}");
                    }
                });
            }
        });
        let weak = app.as_weak();
        app.on_show_window_menu(move |x, y| {
            if let Some(app) = weak.upgrade() {
                app.window().with_winit_window(|w| {
                    w.show_window_menu(winit::dpi::LogicalPosition::new(x as f64, y as f64));
                });
            }
        });
        let weak = app.as_weak();
        app.on_minimize(move || {
            if let Some(app) = weak.upgrade() {
                app.window().with_winit_window(|w| w.set_minimized(true));
            }
        });
        let weak = app.as_weak();
        app.on_toggle_maximize(move || {
            if let Some(app) = weak.upgrade() {
                app.window().with_winit_window(|w| w.set_maximized(!w.is_maximized()));
            }
        });
        let weak = app.as_weak();
        app.on_close_window(move || {
            if let Some(app) = weak.upgrade() {
                hide_or_quit(&app, tray_ok);
            }
        });
    }

    // ── Flyout callbacks ─────────────────────────────────────────────
    {
        let ctx = ctx.clone();
        flyout.on_toggle(move |id, role, on| {
            let Some(role) = Role::parse(&role) else { return };
            if on { start(&ctx, role, &id) } else { stop(&ctx, role, &id) }
        });
    }
    {
        let ctx = ctx.clone();
        flyout.on_retry(move |role, id| {
            if let Some(role) = Role::parse(&role) {
                start(&ctx, role, &id);
            }
        });
    }
    {
        let ctx = ctx.clone();
        flyout.on_open_main(move || on_tray_event(&ctx, TrayEvent::OpenWindow));
    }
    {
        let ctx = ctx.clone();
        flyout.on_share_new(move || on_tray_event(&ctx, TrayEvent::NewShare));
    }
    {
        let ctx = ctx.clone();
        flyout.on_connect_new(move || on_tray_event(&ctx, TrayEvent::NewConnect));
    }

    // ── Elapsed-time tick ────────────────────────────────────────────
    let ticker = slint::Timer::default();
    {
        let ctx = ctx.clone();
        ticker.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
            let Some(app) = ctx.app.upgrade() else { return };
            let s = ctx.lock();
            let elapsed = s
                .selected
                .as_ref()
                .and_then(|(_, id)| s.runtime.get(id))
                .and_then(|r| r.started_at)
                .map(fmt_elapsed)
                .unwrap_or_default();
            drop(s);
            app.set_selected_elapsed(elapsed.into());
        });
    }

    render(&ctx);
    app.show().unwrap();
    {
        let weak = app.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || apply_window_chrome(weak, 0));
    }

    // SPORA_UI_DEMO_FLYOUT=1 shows the tray flyout at the top-left for
    // screenshots ("only" hides the main window so window captures find it).
    if let Ok(v) = std::env::var("SPORA_UI_DEMO_FLYOUT") {
        // Deferred until the event loop runs: the winit window only exists
        // then (skip-taskbar and focus need it), and hiding the sole window
        // before the loop starts ends the process. The long delay lets the
        // test harness finish raising the main window first.
        let only = v == "only";
        let flyout_weak = flyout.as_weak();
        let app_weak = app.as_weak();
        slint::Timer::single_shot(Duration::from_millis(2500), move || {
            if let (Some(flyout), Some(app)) = (flyout_weak.upgrade(), app_weak.upgrade()) {
                if only {
                    app.window().hide().ok();
                    show_flyout(&flyout, 20.0, 20.0, (20.0, 20.0, 0.0, 0.0));
                } else {
                    show_flyout(&flyout, 900.0, 20.0, (900.0, 20.0, 0.0, 0.0));
                }
            }
        });
    }

    slint::run_event_loop_until_quit().unwrap();

    // UI closed. On Windows the service keeps running; exiting drops our
    // gRPC streams, which makes wincore stop the sessions this UI started.
    // On Linux dev builds we spawned wincore ourselves, so stop it too.
    #[cfg(not(windows))]
    rt.block_on(async {
        let grpc = ctx.client();
        if let Some(grpc) = grpc
            && let Err(e) = grpc.shutdown().await
        {
            log::warn!("Shutdown RPC failed: {e}");
        }
    });
    drop(ticker);
}

fn on_tray_event(ctx: &Ctx, ev: TrayEvent) {
    log::debug!("tray: handling {ev:?}");
    let Some(app) = ctx.app.upgrade() else { return };
    let flyout = ctx.flyout.upgrade();
    match ev {
        TrayEvent::LeftClick { x, y, icon } => {
            let just_hidden = FLYOUT_HIDDEN_AT
                .with(|t| t.get())
                .is_some_and(|t| t.elapsed() < Duration::from_millis(400));
            if let Some(flyout) = flyout {
                if flyout.window().is_visible() {
                    flyout.window().hide().ok();
                } else if !just_hidden {
                    render(ctx);
                    show_flyout(&flyout, x, y, icon);
                }
            }
        }
        TrayEvent::OpenWindow => {
            if let Some(f) = flyout {
                f.window().hide().ok();
            }
            show_main(&app);
        }
        TrayEvent::NewShare => {
            if let Some(f) = flyout {
                f.window().hide().ok();
            }
            show_main(&app);
            share_new(ctx, &app);
        }
        TrayEvent::NewConnect => {
            if let Some(f) = flyout {
                f.window().hide().ok();
            }
            show_main(&app);
            open_connect_dialog(&app, "");
        }
        TrayEvent::Quit => {
            slint::quit_event_loop().ok();
        }
    }
}

fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => {
            if let Err(e) = clipboard.set_text(text) {
                log::error!("Clipboard set_text failed: {e}");
            }
        }
        Err(e) => log::error!("Failed to access clipboard: {e}"),
    }
}

// ── Platform bits ────────────────────────────────────────────────────
//
// On Windows wincore runs as the "Spora" Windows service; its logs go to
// %ProgramData%\Spora\spora-wincore.log.

fn log_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        // %LOCALAPPDATA%\Spora
        std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("Spora")
    }
    #[cfg(not(windows))]
    {
        std::env::current_exe().unwrap().parent().unwrap().to_path_buf()
    }
}

#[cfg(not(windows))]
fn launch_wincore() {
    let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let log_path = log_dir().join("spora-wincore.log");
    let exe = exe_dir.join("spora-wincore");
    if exe.exists() {
        let _ = std::process::Command::new(&exe)
            .args(["--log-file", &log_path.to_string_lossy()])
            .spawn();
    } else {
        log::warn!("spora-wincore not found at {}, skipping launch", exe.display());
    }
    log::info!("Wincore log file: {}", log_path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_forms_normalise() {
        assert_eq!(convert_spora_url("spora:s/abc?r=h:443").as_deref(), Some("https://spora.to/s/abc?r=h:443"));
        assert_eq!(convert_spora_url(" spora.to/s/abc ").as_deref(), Some("https://spora.to/s/abc"));
        assert_eq!(convert_spora_url("https://spora.to/s/abc").as_deref(), Some("https://spora.to/s/abc"));
        assert_eq!(convert_spora_url("https://example.com/s/abc"), None);
        // The design's own malformed example must be rejected (canvas 1g).
        assert_eq!(convert_spora_url("spora.to/абы-што"), None);
        assert_eq!(convert_spora_url("spora.to/"), None);
        assert_eq!(convert_spora_url("spora.to/s/"), None);
        assert_eq!(convert_spora_url("not a spora link"), None);
    }

    #[test]
    fn barcode_is_deterministic_and_quantized() {
        let a = barcode(47, 132);
        let b = barcode(47, 132);
        assert_eq!(a, b);
        assert_eq!(a.len(), 132);
        assert!(a.iter().all(|h| [0.3f32, 0.6, 1.0].contains(h)));
        assert_ne!(barcode(23, 132), a);
    }

    #[test]
    fn elapsed_formats_hhmmss() {
        let since = Instant::now() - Duration::from_secs(3792);
        assert_eq!(fmt_elapsed(since), "01:03:12");
    }
}
