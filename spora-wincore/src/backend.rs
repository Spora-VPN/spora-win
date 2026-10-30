//! spora-service for the Windows user served (see `users.rs`): their
//! device identity every share is served under, the access to Spora's
//! relays (renewed in the background), the telemetry upload switch and its
//! uploader thread, and their relay settings. Opened when the user is
//! served; if that fails (an I/O error, say) every call that needs it tries
//! again, at most every [`RETRY`], rather than leaving shares impossible
//! until the user comes back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{error, info};
use spora_service::{RelayAccessStatus, ServiceConfig, Telemetry};

use crate::paths::UserDirs;
use crate::relays::RelayStore;

const RETRY: Duration = Duration::from_secs(30);

/// How long the last upload pass may take when the user is let go (a
/// system shutdown gives services a few seconds).
const FINAL_UPLOAD_WAIT: Duration = Duration::from_secs(3);

/// The built-in API is production. A dev build (Linux) never talks to it
/// unless `SPORA_API` says so: this address answers nothing.
#[cfg(not(windows))]
const DEV_API: &str = "http://127.0.0.1:9";

pub struct Backend {
    pub dirs: UserDirs,
    pub relays: RelayStore,
    state: Mutex<State>,
    fetching: AtomicBool,
    fetch_failed: AtomicBool,
}

#[derive(Default)]
struct State {
    telemetry: Option<Arc<Telemetry>>,
    last_attempt: Option<Instant>,
    /// Let go ([`Backend::close`]): nothing opens again.
    closed: bool,
}

impl Backend {
    pub fn new(dirs: UserDirs) -> Backend {
        Backend {
            relays: RelayStore::open(dirs.service()),
            dirs,
            state: Mutex::new(State::default()),
            fetching: AtomicBool::new(false),
            fetch_failed: AtomicBool::new(false),
        }
    }

    /// The service layer, opening it if it is not yet (file I/O: call from a
    /// blocking context). `None` while it cannot be opened.
    pub fn telemetry(&self) -> Option<Arc<Telemetry>> {
        let mut st = self.state.lock().unwrap();
        if st.telemetry.is_some() || st.closed {
            return st.telemetry.clone();
        }
        if st.last_attempt.is_some_and(|at| at.elapsed() < RETRY) {
            return None;
        }
        st.last_attempt = Some(Instant::now());
        match open(&self.dirs, self.relays.get().use_spora) {
            Ok(t) => {
                let t = Arc::new(t);
                st.telemetry = Some(t.clone());
                Some(t)
            }
            Err(e) => {
                error!("service state unavailable: {e}");
                None
            }
        }
    }

    /// The service layer if it is open (cheap; never opens it).
    pub fn opened(&self) -> Option<Arc<Telemetry>> {
        self.state.lock().unwrap().telemetry.clone()
    }

    /// The user is let go: one last upload pass (only with uploads on), so
    /// the records their sessions just closed go now rather than when they
    /// come back, and the uploader stops. Blocking.
    pub fn close(&self) {
        let t = {
            let mut st = self.state.lock().unwrap();
            st.closed = true;
            st.telemetry.take()
        };
        if let Some(t) = t {
            t.finish(FINAL_UPLOAD_WAIT);
        }
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

    /// The record config every share and connect gets: the user's records
    /// directory, the diagnostic log's run stamps, and a nudge to the
    /// uploader when a record closes.
    pub fn record_config(&self) -> spora_core::record::RecordConfig {
        let subscriber = match self.opened() {
            Some(t) => t.record_subscriber(),
            None => spora_service::diag::run_tracker(),
        };
        spora_core::record::RecordConfig::in_dir(self.dirs.records()).with_subscriber(subscriber)
    }
}

fn open(dirs: &UserDirs, manage_relays: bool) -> Result<Telemetry, String> {
    let dir = dirs.service();
    let key = crate::secret::state_key(&dir)?;
    let cfg = ServiceConfig::new(&dir, platform(), app_version())
        .with_record_dir(dirs.records())
        .with_log_dir(dirs.diag())
        .with_api_bases(api_bases(std::env::var_os("SPORA_API").as_deref()))
        .with_state_key(key)
        .with_relay_access(manage_relays);
    let api = if cfg.api_bases.is_empty() {
        "built-in".to_string()
    } else {
        cfg.api_bases.join(", ")
    };
    let t = Telemetry::open(cfg).map_err(|e| e.to_string())?;
    t.start();
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
