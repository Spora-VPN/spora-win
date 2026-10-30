//! spora-service as the Windows service holds it: the device identity every
//! share is served under, the access to Spora's relays (renewed in the
//! background), the telemetry upload switch and its uploader thread, and the
//! relay settings. Opened at start; if that fails (an I/O error, say) every
//! call that needs it tries again, at most every [`RETRY`], rather than
//! leaving shares impossible until the service restarts.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use log::{error, info};
use spora_service::{RelayAccessStatus, ServiceConfig, Telemetry};

use crate::paths;
use crate::relays::RelayStore;

const RETRY: Duration = Duration::from_secs(30);

/// The built-in API is production. A dev build (Linux) never talks to it
/// unless `SPORA_API` says so: this address answers nothing.
#[cfg(not(windows))]
const DEV_API: &str = "http://127.0.0.1:9";

pub struct Backend {
    pub relays: RelayStore,
    last_attempt: Mutex<Option<Instant>>,
    fetching: AtomicBool,
    fetch_failed: AtomicBool,
}

impl Backend {
    pub fn new() -> Backend {
        let dir = paths::service_dir();
        if let Err(e) = paths::ensure_private_dir(&dir) {
            error!("service state directory {}: {e}", dir.display());
        }
        Backend {
            relays: RelayStore::open(dir),
            last_attempt: Mutex::new(None),
            fetching: AtomicBool::new(false),
            fetch_failed: AtomicBool::new(false),
        }
    }

    /// The service layer, opening it if it is not yet (file I/O: call from a
    /// blocking context). `None` while it cannot be opened.
    pub fn telemetry(&self) -> Option<&'static Telemetry> {
        if let Some(t) = Telemetry::global() {
            return Some(t);
        }
        let mut last = self.last_attempt.lock().unwrap();
        if last.is_some_and(|at| at.elapsed() < RETRY) {
            return None;
        }
        *last = Some(Instant::now());
        match open(self.relays.get().use_spora) {
            Ok(t) => Some(t),
            Err(e) => {
                error!("service state unavailable: {e}");
                None
            }
        }
    }

    /// The service layer if it is open (cheap; never opens it).
    pub fn opened(&self) -> Option<&'static Telemetry> {
        Telemetry::global()
    }

    /// Fetch Spora's relays and a relay token now (network, blocking).
    pub fn fetch(&self, t: &Telemetry) -> Result<RelayAccessStatus, String> {
        self.fetching.store(true, Ordering::SeqCst);
        let r = t.refresh_relays().map_err(|e| e.to_string());
        self.fetching.store(false, Ordering::SeqCst);
        self.fetch_failed.store(r.is_err(), Ordering::SeqCst);
        if let Err(e) = &r {
            log::warn!("cannot fetch Spora's relays: {e}");
        }
        r
    }

    pub fn fetching(&self) -> bool {
        self.fetching.load(Ordering::SeqCst)
    }

    pub fn fetch_failed(&self) -> bool {
        self.fetch_failed.load(Ordering::SeqCst)
    }
}

fn open(manage_relays: bool) -> Result<&'static Telemetry, String> {
    let dir = paths::service_dir();
    paths::ensure_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for d in [
        paths::records_dir(),
        paths::diag_dir(),
        paths::reports_dir(),
    ] {
        if let Err(e) = paths::ensure_private_dir(&d) {
            log::warn!("{}: {e}", d.display());
        }
    }
    let key = crate::secret::state_key(&dir)?;
    let cfg = ServiceConfig::new(&dir, platform(), app_version())
        .with_record_dir(paths::records_dir())
        .with_log_dir(paths::diag_dir())
        .with_api_bases(api_bases(std::env::var_os("SPORA_API").as_deref()))
        .with_state_key(key)
        .with_relay_access(manage_relays);
    let api = if cfg.api_bases.is_empty() {
        "built-in".to_string()
    } else {
        cfg.api_bases.join(", ")
    };
    let t = Telemetry::init_global(cfg).map_err(|e| e.to_string())?;
    let st = t.status();
    info!(
        "service state opened: device {}, API {api}, uploads {}{}, relay access {}",
        t.relay_access_status().cert_hash,
        if st.upload_enabled { "on" } else { "off" },
        if st.required {
            " (required by the account)"
        } else {
            ""
        },
        if manage_relays { "kept fresh" } else { "off" },
    );
    Ok(t)
}

/// The record config every share and connect gets: one directory for all
/// of them, the diagnostic log's run stamps, and a nudge to the uploader
/// when a record closes.
pub fn record_config() -> spora_core::record::RecordConfig {
    spora_core::record::RecordConfig::in_dir(paths::records_dir())
        .with_subscriber(spora_service::record_subscriber())
}

/// The API's name for this OS (`windows`; `linux` for dev builds).
pub fn platform() -> &'static str {
    spora_service::diag::default_platform()
}

/// Sent to the API and written into bug reports: the client's release, with
/// `-debug` on debug builds so the server can tell them from installs.
pub fn app_version() -> String {
    format!(
        "{}{}",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) { "-debug" } else { "" }
    )
}

/// `SPORA_API` as the CLI reads it: comma-separated, trimmed, blanks
/// dropped. Set but blank is not the default: passed on as it is, so the
/// service refuses it as not a URL. Unset: the built-in API on Windows, and
/// an address that answers nothing on a dev build.
fn api_bases(env: Option<&std::ffi::OsStr>) -> Vec<String> {
    let Some(v) = env else {
        #[cfg(windows)]
        return Vec::new();
        #[cfg(not(windows))]
        return vec![DEV_API.to_string()];
    };
    let v = v.to_string_lossy();
    let bases: Vec<String> = v
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .collect();
    if bases.is_empty() {
        vec![v.into_owned()]
    } else {
        bases
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spora_api_is_read_like_the_cli() {
        let os = |s: &str| std::ffi::OsString::from(s);
        assert_eq!(
            api_bases(Some(&os(" http://a , http://b,"))),
            ["http://a", "http://b"]
        );
        // Blank is refused downstream, never the default.
        assert_eq!(api_bases(Some(&os(" , "))), [" , "]);
        #[cfg(not(windows))]
        assert_eq!(api_bases(None), [DEV_API]);
    }
}
