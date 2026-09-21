//! System tray icon for the Spora client (design canvas 1h/2e): the mono spore
//! mark with a state shape beside it (● active · ○ pending · ◆ error, nothing
//! when offline), theme-aware (ink on a light taskbar, light on a dark one),
//! plus the native right-click menu (canvas 1e). A left click reports its
//! position so the caller can open the flyout window (canvas 1d) above it.
//!
//! How it works
//! ------------
//! * The bitmap is rendered at runtime: the mono mark SVG
//!   (`ui/assets/logo-mono-{ink,light}.svg`) goes through resvg into a
//!   tiny-skia pixmap of the system small-icon size, the state shape is drawn
//!   next to it with tiny-skia paths in the same ink, and the result becomes
//!   a [`tray_icon::Icon`]. Everything is one ink colour: the *shape* carries
//!   the state, so it reads the same under high contrast and for colour-blind
//!   users, and it never clashes with whatever accent colour the taskbar has.
//! * The ink follows the taskbar theme (`SystemUsesLightTheme` under
//!   `HKCU\...\Themes\Personalize`), re-read on every [`Tray::set_state`]:
//!   Windows does not notify tray icons of theme changes, so the next state
//!   change is the moment the icon catches up.
//! * Events are pushed through the closure given to [`Tray::new`] using
//!   `TrayIconEvent::set_event_handler` / `MenuEvent::set_event_handler`.
//!   On Windows both fire on the thread that created the icon, from inside
//!   its message loop (winit's, once the Slint event loop runs); the closure
//!   is still required to be `Send + Sync` and must hop to the UI thread
//!   itself (`slint::invoke_from_event_loop`), because tray-icon makes no
//!   promise about the dispatching thread.
//! * On non-Windows builds the whole thing is inert so the crate keeps
//!   building and running on the Linux dev box.

/// What the icon says about the client. The state is drawn as a shape next
/// to the mark, never as a colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayState {
    /// Mark only.
    Offline,
    /// Mark + filled circle (●): a session is up.
    Active,
    /// Mark + ring (○): a session is being set up.
    Pending,
    /// Mark + diamond (◆): the last session failed or was lost.
    Error,
}

/// What the user did with the icon or its menu.
#[derive(Clone, Debug)]
// Only the Windows side ever constructs these; the Linux dev build would
// otherwise warn about every variant.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum TrayEvent {
    /// Left click on the icon. Physical screen pixels of the click and the
    /// icon's rectangle (x, y, w, h), for positioning the flyout.
    ///
    /// A Windows double-click arrives as one `LeftClick` followed by
    /// [`TrayEvent::OpenWindow`]; the second button-up the OS sends after the
    /// double-click is swallowed here so the flyout is not toggled twice.
    LeftClick { x: f64, y: f64, icon: (f64, f64, f64, f64) },
    /// Double-click on the icon, or "Open Spora" in the menu.
    OpenWindow,
    /// "Create a link to share…" in the menu.
    NewShare,
    /// "Connect via a link…" in the menu.
    NewConnect,
    /// "Quit Spora" in the menu.
    Quit,
}

/// The tray icon and its context menu. Dropping it removes the icon from
/// the notification area.
///
/// Thread-bound: tray-icon's handles are `Rc`-based, so a `Tray` must live
/// on the thread that created it (the UI thread). The non-Windows stand-in
/// is made `!Send` too, so a Linux dev build fails the same way a Windows
/// build would if the value is moved to another thread.
pub struct Tray {
    #[cfg(windows)]
    icon: tray_icon::TrayIcon,
    /// tray-icon keeps a clone of the menu inside the `TrayIcon` already;
    /// holding ours as well makes the lifetime explicit for the reader.
    #[cfg(windows)]
    _menu: tray_icon::menu::Menu,
    #[cfg(not(windows))]
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(windows)]
impl Tray {
    /// Create the tray icon. Must run on the UI thread before the Slint event
    /// loop starts; `on_event` is called from the tray's own thread context,
    /// so it must hop to the UI thread itself (slint::invoke_from_event_loop).
    ///
    /// The icon only receives clicks while the creating thread pumps window
    /// messages; winit does that once the Slint event loop runs, so create
    /// the tray right before `run_event_loop` and nothing is missed. The
    /// event handlers are process-global and can be installed only once
    /// (tray-icon keeps them in a `OnceCell`), so create a single `Tray` per
    /// process; a second one would show an icon that never reports events.
    ///
    /// The icon starts as [`TrayState::Offline`] with the tooltip "Spora";
    /// call [`Tray::set_state`] to change either.
    pub fn new(on_event: impl Fn(TrayEvent) + Send + Sync + 'static) -> Result<Self, String> {
        imp::install_handlers(on_event);
        let menu = imp::build_menu()?;
        let icon = imp::make_icon(TrayState::Offline)?;
        let tray = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            // Left click is ours (it opens the flyout); only the right button
            // pops the context menu, as native Windows tray icons do.
            .with_menu_on_left_click(false)
            .with_icon(icon)
            .with_tooltip("Spora")
            .build()
            .map_err(|e| format!("tray icon: {e}"))?;
        Ok(Tray {
            icon: tray,
            _menu: menu,
        })
    }

    /// Update the icon's state shape and hover tooltip.
    ///
    /// Re-reads the taskbar theme and the small-icon size every time, so a
    /// theme switch or a DPI change made since the last call is picked up.
    /// Rendering a ~16px bitmap is far cheaper than caching would be worth.
    /// Failures are logged and leave the previous icon/tooltip in place.
    pub fn set_state(&self, state: TrayState, tooltip: &str) {
        match imp::make_icon(state) {
            Ok(icon) => {
                if let Err(e) = self.icon.set_icon(Some(icon)) {
                    log::warn!("tray: could not update the icon: {e}");
                }
            }
            Err(e) => log::warn!("tray: {e}"),
        }
        if let Err(e) = self.icon.set_tooltip(Some(imp::clip_tooltip(tooltip))) {
            log::warn!("tray: could not update the tooltip: {e}");
        }
    }
}

#[cfg(not(windows))]
impl Tray {
    /// Create the tray icon. Must run on the UI thread before the Slint event
    /// loop starts; `on_event` is called from the tray's own thread context,
    /// so it must hop to the UI thread itself (slint::invoke_from_event_loop).
    ///
    /// There is no system tray on this platform: the returned `Tray` is
    /// inert and `on_event` is never called.
    pub fn new(_on_event: impl Fn(TrayEvent) + Send + Sync + 'static) -> Result<Self, String> {
        log::debug!("tray: no system tray on this platform; icon and menu are inert");
        Ok(Tray {
            _not_send: std::marker::PhantomData,
        })
    }

    /// Update the icon's state shape and hover tooltip (no-op here).
    pub fn set_state(&self, _state: TrayState, _tooltip: &str) {}
}

/// Pure bitmap composition, kept free of Win32 so it can be unit-tested on
/// the Linux dev box.
#[cfg(any(windows, test))]
mod render {
    use super::TrayState;
    use resvg::tiny_skia::{BlendMode, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
    use resvg::usvg;

    /// Ink on a light taskbar (design token `ink`).
    const INK_LIGHT_THEME: [u8; 3] = [0x1D, 0x21, 0x1D];
    /// Ink on a dark taskbar (design token `light`).
    const INK_DARK_THEME: [u8; 3] = [0xEF, 0xF2, 0xEE];

    /// The mono mark, one file per ink so the SVG's own fill/stroke colour is
    /// already the one the shape is drawn in. Both share the 73×102 viewBox.
    const MARK_SVG_INK: &str = include_str!("../ui/assets/logo-mono-ink.svg");
    const MARK_SVG_LIGHT: &str = include_str!("../ui/assets/logo-mono-light.svg");

    /// Transparent border so the icon never touches its neighbours.
    const PADDING: f32 = 1.0;
    /// State shape side, as a fraction of the icon size (6px at 16).
    const SHAPE_FRACTION: f32 = 0.38;
    /// Ring stroke, as a fraction of the icon size (1.6px at 16).
    const RING_STROKE_FRACTION: f32 = 0.1;

    /// Render one tray icon: `size`×`size` pixels of straight (not
    /// premultiplied) RGBA8, row-major, as `tray_icon::Icon::from_rgba`
    /// expects.
    ///
    /// Layout: the mark fills the inner height flush left (its 73:102 aspect
    /// makes that roughly the left 70% of the width); the state shape sits in
    /// the bottom-right corner. Where the two would touch, a thin transparent
    /// halo is punched around the shape so it stays legible against the
    /// mark's arc rather than merging with it (same ink, so nothing else
    /// would separate them).
    pub fn render_icon(size: u32, light_theme: bool, state: TrayState) -> Result<Vec<u8>, String> {
        let (svg, ink) = if light_theme {
            (MARK_SVG_INK, INK_LIGHT_THEME)
        } else {
            (MARK_SVG_LIGHT, INK_DARK_THEME)
        };
        let mut pixmap =
            Pixmap::new(size, size).ok_or_else(|| format!("tray icon: cannot allocate a {size}px pixmap"))?;
        let s = size as f32;
        let inner = (s - 2.0 * PADDING).max(1.0);

        // The mark, scaled by height and centred vertically in the inner box.
        let tree = usvg::Tree::from_str(svg, &usvg::Options::default())
            .map_err(|e| format!("tray icon: the mark svg does not parse: {e}"))?;
        let mark = tree.size();
        let scale = inner / mark.height();
        let mark_y = PADDING + (inner - mark.height() * scale) / 2.0;
        // post_translate: scale the SVG first, then move it into place.
        let place = Transform::from_scale(scale, scale).post_translate(PADDING, mark_y);
        resvg::render(&tree, place, &mut pixmap.as_mut());

        if state == TrayState::Offline {
            return Ok(demultiply(&pixmap));
        }

        // The state shape: a box of SHAPE_FRACTION in the bottom-right corner.
        let side = (s * SHAPE_FRACTION).round().max(3.0);
        let r = side / 2.0;
        let (cx, cy) = (s - PADDING - r, s - PADDING - r);
        let identity = Transform::identity();

        // Knock out a halo one "device pixel" wide (scaled with the icon) so
        // the shape does not fuse with the mark's arc underneath it.
        let halo = (s / 16.0).max(1.0);
        if let Some(disc) = PathBuilder::from_circle(cx, cy, r + halo) {
            let clear = Paint {
                blend_mode: BlendMode::Clear,
                ..Paint::default()
            };
            pixmap.fill_path(&disc, &clear, FillRule::Winding, identity, None);
        }

        let mut paint = Paint::default(); // anti-aliased by default
        paint.set_color_rgba8(ink[0], ink[1], ink[2], 0xFF);
        match state {
            TrayState::Offline => {}
            TrayState::Active => {
                if let Some(disc) = PathBuilder::from_circle(cx, cy, r) {
                    pixmap.fill_path(&disc, &paint, FillRule::Winding, identity, None);
                }
            }
            TrayState::Pending => {
                // Stroke centred on the path: pull the radius in by half the
                // width so the ring's outer edge stays inside the shape box.
                let width = (s * RING_STROKE_FRACTION).max(1.0);
                if let Some(ring) = PathBuilder::from_circle(cx, cy, r - width / 2.0) {
                    let stroke = Stroke { width, ..Stroke::default() };
                    pixmap.stroke_path(&ring, &paint, &stroke, identity, None);
                }
            }
            TrayState::Error => {
                // A square rotated 45°, inscribed in the shape box.
                let mut pb = PathBuilder::new();
                pb.move_to(cx, cy - r);
                pb.line_to(cx + r, cy);
                pb.line_to(cx, cy + r);
                pb.line_to(cx - r, cy);
                pb.close();
                if let Some(diamond) = pb.finish() {
                    pixmap.fill_path(&diamond, &paint, FillRule::Winding, identity, None);
                }
            }
        }

        Ok(demultiply(&pixmap))
    }

    /// tiny-skia stores premultiplied RGBA; `Icon::from_rgba` (and the HICON
    /// it builds) wants straight alpha, otherwise anti-aliased edges go dark.
    fn demultiply(pixmap: &Pixmap) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(pixmap.data().len());
        for px in pixmap.pixels() {
            let c = px.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        rgba
    }
}

/// The Win32 side: sizing, theme, menu, event mapping.
#[cfg(windows)]
mod imp {
    use super::render::render_icon;
    use super::{TrayEvent, TrayState};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconEvent};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON, SM_CYSMICON};

    /// Menu item ids; muda hands them back in `MenuEvent::id`.
    const ID_OPEN: &str = "spora.open";
    const ID_NEW_SHARE: &str = "spora.new-share";
    const ID_NEW_CONNECT: &str = "spora.new-connect";
    const ID_QUIT: &str = "spora.quit";

    /// How long after a double-click its trailing button-up is swallowed.
    /// Windows' default double-click time; the real one needs a Win32
    /// feature (`Win32_UI_Input_KeyboardAndMouse`) the crate does not pull.
    const DOUBLE_CLICK_TAIL: Duration = Duration::from_millis(500);

    /// `NOTIFYICONDATAW.szTip` holds 128 UTF-16 units including the
    /// terminator; tray-icon copies up to 128 units of a NUL-terminated
    /// buffer, so anything longer would lose its terminator.
    const TOOLTIP_MAX_UTF16: usize = 127;

    /// Route tray-icon's process-global event handlers into `on_event`.
    pub fn install_handlers(on_event: impl Fn(TrayEvent) + Send + Sync + 'static) {
        let on_event: Arc<dyn Fn(TrayEvent) + Send + Sync> = Arc::new(on_event);

        let f = on_event.clone();
        let last_double_click = Mutex::new(None::<Instant>);
        TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
            if let Some(e) = map_icon_event(ev, &last_double_click) {
                f(e);
            }
        }));

        let f = on_event;
        MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
            if let Some(e) = map_menu_event(&ev.id) {
                f(e);
            }
        }));
    }

    /// Clicks on the icon itself. tray-icon reports every button in both
    /// states plus hover; we want the left button-up (a completed click) and
    /// the left double-click. Right button-up pops the menu inside tray-icon.
    ///
    /// Windows delivers a double-click as DOWN, UP, DBLCLK, UP: the first UP
    /// becomes a `LeftClick`, DBLCLK becomes `OpenWindow`, and the second UP
    /// is dropped when it follows the DBLCLK closely enough, so the flyout
    /// the caller opened on the first UP is not toggled again.
    fn map_icon_event(ev: TrayIconEvent, last_double_click: &Mutex<Option<Instant>>) -> Option<TrayEvent> {
        match ev {
            TrayIconEvent::Click {
                position,
                rect,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => {
                let after_double_click = last_double_click
                    .lock()
                    .ok()
                    .and_then(|mut g| g.take())
                    .is_some_and(|t| t.elapsed() < DOUBLE_CLICK_TAIL);
                if after_double_click {
                    return None;
                }
                Some(TrayEvent::LeftClick {
                    x: position.x,
                    y: position.y,
                    icon: (
                        rect.position.x,
                        rect.position.y,
                        f64::from(rect.size.width),
                        f64::from(rect.size.height),
                    ),
                })
            }
            TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => {
                if let Ok(mut g) = last_double_click.lock() {
                    *g = Some(Instant::now());
                }
                Some(TrayEvent::OpenWindow)
            }
            // Button-down halves, other buttons, hover enter/move/leave, and
            // whatever future variants tray-icon adds (the enum is
            // non-exhaustive).
            _ => None,
        }
    }

    fn map_menu_event(id: &MenuId) -> Option<TrayEvent> {
        match id.as_ref() {
            ID_OPEN => Some(TrayEvent::OpenWindow),
            ID_NEW_SHARE => Some(TrayEvent::NewShare),
            ID_NEW_CONNECT => Some(TrayEvent::NewConnect),
            ID_QUIT => Some(TrayEvent::Quit),
            other => {
                log::debug!("tray: ignoring menu event for unknown item {other:?}");
                None
            }
        }
    }

    /// The right-click menu (canvas 1e), top to bottom.
    pub fn build_menu() -> Result<Menu, String> {
        let open = MenuItem::with_id(ID_OPEN, "Open Spora", true, None);
        let share = MenuItem::with_id(ID_NEW_SHARE, "Create a link to share…", true, None);
        let connect = MenuItem::with_id(ID_NEW_CONNECT, "Connect via a link…", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit Spora", true, None);
        Menu::with_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &share,
            &connect,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| format!("tray menu: {e}"))
    }

    /// Render the icon for `state` at the current size and taskbar theme.
    pub fn make_icon(state: TrayState) -> Result<Icon, String> {
        let size = small_icon_size();
        let rgba = render_icon(size, taskbar_is_light(), state)?;
        Icon::from_rgba(rgba, size, size).map_err(|e| format!("tray icon: {e}"))
    }

    /// The notification area shows icons at the system small-icon size
    /// (16px at 100% scaling, 20 at 125%, 24 at 150%, ...). Rendering at
    /// exactly that size avoids the shell's blurry rescale.
    fn small_icon_size() -> u32 {
        // SAFETY: GetSystemMetrics reads a system table; no pointers, no
        // preconditions.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
        // Icons are square; on the odd chance the metrics disagree, take the
        // larger so nothing is clipped.
        match w.max(h) {
            px if px > 0 => px as u32,
            px => {
                log::warn!("tray: GetSystemMetrics(SM_CXSMICON) returned {px}; assuming 16px");
                16
            }
        }
    }

    /// Whether the taskbar is light, which decides the ink. This is
    /// `SystemUsesLightTheme` (taskbar, Start, notification area), not
    /// `AppsUseLightTheme` (app windows): the two can differ. A missing value
    /// means the setting was never touched, which Windows treats as light.
    fn taskbar_is_light() -> bool {
        let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        let value = wide("SystemUsesLightTheme");
        let mut data: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        // SAFETY: every pointer refers to a local that outlives the call, the
        // strings are NUL-terminated, and `len` tells the API how big `data`
        // is. RRF_RT_REG_DWORD rejects any other value type.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut len,
            )
        };
        match rc {
            ERROR_SUCCESS => data != 0,
            ERROR_FILE_NOT_FOUND => true,
            other => {
                log::debug!("tray: RegGetValueW(SystemUsesLightTheme) failed with {other}; assuming light");
                true
            }
        }
    }

    /// Cut a tooltip down to what `szTip` can hold, on a char boundary.
    pub fn clip_tooltip(s: &str) -> String {
        let mut units = 0;
        s.chars()
            .take_while(|c| {
                units += c.len_utf16();
                units <= TOOLTIP_MAX_UTF16
            })
            .collect()
    }

    /// NUL-terminated UTF-16, as the W-suffixed Win32 APIs take.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::TrayState;
    use super::render::render_icon;

    const STATES: [TrayState; 4] = [
        TrayState::Offline,
        TrayState::Active,
        TrayState::Pending,
        TrayState::Error,
    ];

    fn ink_pixels(rgba: &[u8]) -> usize {
        rgba.chunks_exact(4).filter(|px| px[3] > 0).count()
    }

    #[test]
    fn every_state_renders_and_looks_different() {
        for size in [16u32, 20, 24, 32] {
            let icons: Vec<Vec<u8>> = STATES
                .iter()
                .map(|&state| {
                    let rgba = render_icon(size, true, state).expect("render");
                    assert_eq!(rgba.len(), (size * size * 4) as usize, "{size}px {state:?}");
                    assert!(ink_pixels(&rgba) > 0, "{size}px {state:?} rendered nothing");
                    rgba
                })
                .collect();
            for a in 0..STATES.len() {
                for b in (a + 1)..STATES.len() {
                    assert_ne!(
                        ink_pixels(&icons[a]),
                        ink_pixels(&icons[b]),
                        "{size}px: {:?} and {:?} have the same ink coverage",
                        STATES[a],
                        STATES[b]
                    );
                }
            }
        }
    }

    #[test]
    fn ink_follows_the_theme_and_is_demultiplied() {
        for (light, expect) in [(true, [0x1D, 0x21, 0x1D]), (false, [0xEF, 0xF2, 0xEE])] {
            let rgba = render_icon(16, light, TrayState::Active).expect("render");
            let opaque = rgba
                .chunks_exact(4)
                .find(|px| px[3] == 0xFF)
                .expect("the filled circle has at least one fully opaque pixel");
            assert_eq!(&opaque[..3], &expect, "light_theme={light}");
            // Anti-aliased edge pixels must carry the full ink colour with a
            // partial alpha, not a darkened premultiplied one. 8-bit
            // premultiplied storage quantises, so allow a little slack and
            // look at a pixel with at least half coverage.
            let edge = rgba
                .chunks_exact(4)
                .find(|px| (0x80..0xFF).contains(&px[3]))
                .expect("anti-aliasing leaves partially covered pixels");
            for (got, want) in edge[..3].iter().zip(expect) {
                assert!(
                    got.abs_diff(want) <= 2,
                    "light_theme={light}: edge pixel {edge:?} is not straight-alpha {expect:?}"
                );
            }
        }
    }

    #[test]
    fn keeps_a_transparent_border() {
        for size in [16u32, 24] {
            for &state in &STATES {
                let rgba = render_icon(size, true, state).expect("render");
                let alpha = |x: u32, y: u32| rgba[((y * size + x) * 4 + 3) as usize];
                for i in 0..size {
                    for (x, y) in [(i, 0), (i, size - 1), (0, i), (size - 1, i)] {
                        assert_eq!(alpha(x, y), 0, "{size}px {state:?}: ink at ({x},{y})");
                    }
                }
            }
        }
    }
}
