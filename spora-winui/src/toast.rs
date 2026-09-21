//! Windows toast notifications (design canvas 1f): "Sharing started",
//! "Connected", "Connection lost". System template, attributed to Spora.
//!
//! Windows shows a toast under an app's own name and icon only when the
//! notifying AppUserModelID (AUMID) resolves to a display name and icon.
//! Packaged (MSIX) apps get that for free. For an unpackaged desktop app
//! like ours the documented routes are a Start Menu shortcut carrying the
//! AUMID property, or a registration under
//! `HKCU\Software\Classes\AppUserModelId\<AUMID>` — we use the registry:
//! it needs no COM property-store dance, survives the user deleting the
//! Start Menu shortcut, and is plain Win32 through `windows-sys`.
//!
//! Off Windows both functions are no-ops that log the toast text, so Linux
//! dev builds still show something in the log.

/// The AppUserModelID toasts are attributed to. Must match between
/// [`init`] (the registry key) and [`show`] (the toast notifier).
const AUMID: &str = "Spora.Client";

/// Register what Windows needs to attribute toasts to "Spora" (an
/// AppUserModelID with display name + icon). Idempotent; no-op off Windows.
///
/// Writes `DisplayName = "Spora"` and, when `spora.ico` sits next to the
/// running executable (the installer ships it there), `IconUri = <that
/// path>` under `HKCU\Software\Classes\AppUserModelId\Spora.Client`.
/// Values that already match are left alone, so calling this on every
/// start is cheap. Failures are logged at warn and otherwise ignored:
/// toasts still appear, just under a generic header.
#[cfg(windows)]
pub fn init() {
    match win::register_aumid() {
        Ok(true) => log::info!("toast: registered AppUserModelId {AUMID} for toast attribution"),
        Ok(false) => log::debug!("toast: AppUserModelId {AUMID} already registered"),
        Err(e) => log::warn!("toast: could not register AppUserModelId {AUMID}: {e}"),
    }
}

/// Show a toast with a title and one line of body text. Fire-and-forget:
/// failures are logged, never surfaced.
///
/// The WinRT call runs on its own short-lived thread so the UI thread never
/// blocks: `ToastNotifier::Show` is synchronous, and the crate deliberately
/// sleeps 10 ms after it so the notification is not dropped when the
/// calling thread exits. WinRT is initialised per thread by the `windows`
/// crate on first use, so a fresh `std::thread` is fine. Default system
/// sound, short (7 s) duration.
#[cfg(windows)]
pub fn show(title: &str, body: &str) {
    use tauri_winrt_notification::{Duration, Toast};

    let (title, body) = (title.to_owned(), body.to_owned());
    let spawned = std::thread::Builder::new()
        .name("spora-toast".into())
        .spawn(move || {
            let toast = Toast::new(AUMID)
                .title(&title)
                .text1(&body)
                .duration(Duration::Short);
            if let Err(e) = toast.show() {
                log::warn!("toast: could not show {title:?}: {e}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("toast: could not spawn toast thread: {e}");
    }
}

/// No-op off Windows; logs at debug so the call is visible in dev logs.
#[cfg(not(windows))]
pub fn init() {
    log::debug!("toast: init is a no-op off Windows (AppUserModelId {AUMID})");
}

/// No-op off Windows; logs the toast text at debug instead.
#[cfg(not(windows))]
pub fn show(title: &str, body: &str) {
    log::debug!("toast: {title} — {body}");
}

/// The registry side of AUMID registration.
#[cfg(windows)]
mod win {
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
        REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW,
    };

    use super::AUMID;

    /// Display name Windows puts in the toast header and in Settings >
    /// Notifications.
    const DISPLAY_NAME: &str = "Spora";

    /// Icon file the installer places beside `spora-winui.exe`.
    const ICON_FILE: &str = "spora.ico";

    /// Create/refresh `HKCU\Software\Classes\AppUserModelId\<AUMID>`.
    /// Returns `Ok(true)` when something was written, `Ok(false)` when every
    /// value already matched.
    pub(super) fn register_aumid() -> Result<bool, String> {
        let key = Key::create(&format!(r"Software\Classes\AppUserModelId\{AUMID}"))?;
        let mut changed = key.set_sz_if_changed("DisplayName", DISPLAY_NAME)?;
        match icon_path() {
            Some(icon) => changed |= key.set_sz_if_changed("IconUri", &icon)?,
            None => log::debug!("toast: no {ICON_FILE} beside the executable, skipping IconUri"),
        }
        Ok(changed)
    }

    /// Absolute path of `spora.ico` next to the running executable, if the
    /// file exists (dev builds run straight out of `target/` without it).
    fn icon_path() -> Option<String> {
        let exe = std::env::current_exe()
            .map_err(|e| log::warn!("toast: current_exe: {e}"))
            .ok()?;
        let icon: PathBuf = exe.parent()?.join(ICON_FILE);
        if !icon.is_file() {
            return None;
        }
        // The toast platform wants a plain absolute path (or file:// URI);
        // a verbatim `\\?\` prefix, which std may produce for long paths,
        // would not resolve. `current_exe` normally returns a plain path.
        let s = icon.to_string_lossy();
        Some(s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned())
    }

    /// UTF-16, nul-terminated, as every `*W` registry call expects.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// An open registry key, closed on drop.
    struct Key(HKEY);

    impl Key {
        /// Open or create `HKCU\<path>` with query + set-value access.
        /// HKCU needs no elevation, which is why the registration lives
        /// there and not under HKLM.
        fn create(path: &str) -> Result<Self, String> {
            let path = wide(path);
            let mut hkey: HKEY = 0;
            let mut disposition = 0;
            // SAFETY: `path` outlives the call and is nul-terminated; the
            // out-pointers point at live locals; the class and security
            // pointers are documented to accept null.
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    path.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut hkey,
                    &mut disposition,
                )
            };
            check(status).map_err(|e| format!("RegCreateKeyExW: {e}"))?;
            Ok(Key(hkey))
        }

        /// Set a REG_SZ value unless it already holds `value`. Returns
        /// whether a write happened.
        fn set_sz_if_changed(&self, name: &str, value: &str) -> Result<bool, String> {
            let data = wide(value);
            // Registry strings may or may not be stored with their
            // terminator, so compare without it.
            if self.get_sz(name).as_deref() == Some(&data[..data.len() - 1]) {
                return Ok(false);
            }
            let name = wide(name);
            // cbData for REG_SZ must include the terminating nul, in bytes.
            let bytes = u32::try_from(data.len() * 2).map_err(|_| "value too long".to_owned())?;
            // SAFETY: `name` and `data` are nul-terminated and outlive the
            // call; `bytes` is exactly the length of `data` in bytes.
            let status = unsafe {
                RegSetValueExW(self.0, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), bytes)
            };
            check(status).map_err(|e| format!("RegSetValueExW({value:?}): {e}"))?;
            Ok(true)
        }

        /// Read a REG_SZ value as UTF-16 with trailing nuls trimmed. `None`
        /// when the value is missing, not a string, or unreadable — every
        /// one of those just means "write it".
        fn get_sz(&self, name: &str) -> Option<Vec<u16>> {
            let name = wide(name);
            let mut kind: REG_VALUE_TYPE = 0;
            let mut bytes: u32 = 0;
            // First call sizes the buffer (null data pointer).
            // SAFETY: `name` is nul-terminated; out-pointers point at locals.
            let status = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    std::ptr::null_mut(),
                    &mut bytes,
                )
            };
            if status != ERROR_SUCCESS || kind != REG_SZ {
                return None;
            }
            let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
            // SAFETY: `buf` holds at least `bytes` bytes and `bytes` tells
            // the API its size; a concurrent grow of the value makes the
            // call fail with ERROR_MORE_DATA, which we treat as "rewrite".
            let status = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buf.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if status != ERROR_SUCCESS || kind != REG_SZ {
                return None;
            }
            buf.truncate((bytes as usize) / 2);
            while buf.last() == Some(&0) {
                buf.pop();
            }
            Some(buf)
        }
    }

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a successful RegCreateKeyExW and
            // is closed exactly once, here.
            let status = unsafe { RegCloseKey(self.0) };
            if status != ERROR_SUCCESS {
                log::warn!("toast: RegCloseKey: {status}");
            }
        }
    }

    /// Map a `WIN32_ERROR` return to a `Result`. Registry calls return the
    /// error code directly rather than setting `GetLastError`.
    fn check(status: WIN32_ERROR) -> Result<(), String> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("Win32 error {status}"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wide_is_nul_terminated_utf16() {
            let w = wide("Spora");
            assert_eq!(w, [b'S' as u16, b'p' as u16, b'o' as u16, b'r' as u16, b'a' as u16, 0]);
            // Byte length handed to RegSetValueExW includes the terminator.
            assert_eq!(w.len() * 2, 12);
        }

        #[test]
        fn register_is_idempotent() {
            // First call may or may not write (depends on the machine);
            // the second must find everything in place.
            register_aumid().expect("register");
            assert_eq!(register_aumid(), Ok(false));
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn show_does_not_panic_off_windows() {
        super::init();
        super::show("Connected", "Alice's network is reachable.");
    }
}
