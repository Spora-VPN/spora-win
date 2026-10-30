//! The relays new shares are made with: Spora's (on by default) and the
//! user's own, each with an optional token. Kept by the service per Windows
//! user (like the device identity they serve), in the user's
//! `service\relays.json`; the own
//! relays' tokens are sealed there ([`crate::secret`]) and never go back to
//! the UI. A share keeps the relays it was made with (they are part of its
//! URL), so these settings apply to new shares only.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use spora_core::identity::{Credential, RelayEndpoint, RelayEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnRelay {
    /// Canonical `?r=` form (`RelayEndpoint::to_url_param`).
    pub endpoint: String,
    /// What the relay wants, as the user pasted it (trimmed); `None` for an
    /// open relay.
    pub token: Option<String>,
}

impl OwnRelay {
    pub fn entry(&self) -> Option<RelayEntry> {
        let endpoint = RelayEndpoint::from_url_param(&self.endpoint).ok()?;
        Some(RelayEntry {
            endpoint,
            credential: match &self.token {
                Some(t) => Credential::Static(t.as_bytes().to_vec()),
                None => Credential::None,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySettings {
    pub use_spora: bool,
    pub own: Vec<OwnRelay>,
}

impl Default for RelaySettings {
    fn default() -> Self {
        Self {
            use_spora: true,
            own: Vec::new(),
        }
    }
}

impl RelaySettings {
    /// A share needs at least one relay.
    pub fn has_relays(&self) -> bool {
        self.use_spora || !self.own.is_empty()
    }
}

/// `relays.json` as written: tokens sealed.
#[derive(Serialize, Deserialize)]
struct Stored {
    use_spora: bool,
    own: Vec<StoredRelay>,
}

#[derive(Serialize, Deserialize)]
struct StoredRelay {
    endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

pub struct RelayStore {
    path: PathBuf,
    settings: Mutex<RelaySettings>,
}

impl RelayStore {
    /// Read `relays.json` in `dir`. Missing = the defaults; unreadable =
    /// the defaults too, with the file kept aside (`relays.json.corrupt`).
    pub fn open(dir: PathBuf) -> RelayStore {
        let path = dir.join("relays.json");
        let settings = match std::fs::read(&path) {
            Ok(data) => match serde_json::from_slice::<Stored>(&data) {
                Ok(s) => RelaySettings {
                    use_spora: s.use_spora,
                    own: s
                        .own
                        .into_iter()
                        .map(|r| OwnRelay {
                            token: r.token.and_then(|t| {
                                let plain = crate::secret::unseal(&t);
                                if plain.is_none() {
                                    log::warn!(
                                        "relays: the token of {} does not open on this machine; dropped",
                                        r.endpoint
                                    );
                                }
                                plain
                            }),
                            endpoint: r.endpoint,
                        })
                        .collect(),
                },
                Err(e) => {
                    // The position only: serde's message may quote a token.
                    log::error!(
                        "relays: {} unreadable (line {}, column {}); keeping it aside and starting from the defaults",
                        path.display(),
                        e.line(),
                        e.column()
                    );
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    RelaySettings::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RelaySettings::default(),
            Err(e) => {
                log::error!("relays: reading {}: {e}; using the defaults", path.display());
                RelaySettings::default()
            }
        };
        RelayStore {
            path,
            settings: Mutex::new(settings),
        }
    }

    pub fn get(&self) -> RelaySettings {
        self.settings.lock().unwrap().clone()
    }

    /// Change the settings and write them; the change stands in memory even
    /// if the write fails (logged).
    pub fn update<R>(&self, f: impl FnOnce(&mut RelaySettings) -> R) -> R {
        let mut s = self.settings.lock().unwrap();
        let r = f(&mut s);
        let stored = Stored {
            use_spora: s.use_spora,
            own: s
                .own
                .iter()
                .map(|r| StoredRelay {
                    endpoint: r.endpoint.clone(),
                    token: r.token.as_deref().map(crate::secret::seal),
                })
                .collect(),
        };
        match serde_json::to_vec_pretty(&stored) {
            Ok(data) => {
                if let Err(e) = crate::paths::write_atomic(&self.path, &data) {
                    log::error!("relays: writing {}: {e}", self.path.display());
                }
            }
            Err(e) => log::error!("relays: cannot serialise the settings: {e}"),
        }
        r
    }
}

/// Why a relay address cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressError {
    Format,
    Protocol,
    Ipv6Brackets,
    Port,
}

/// A relay address as typed: trimmed, a pasted `?r=` value's
/// percent-encoding undone.
pub fn normalize(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains('%') {
        percent_encoding::percent_decode_str(trimmed)
            .decode_utf8()
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| trimmed.to_string())
    } else {
        trimmed.to_string()
    }
}

const PROTOCOL_TAGS: &[&str] = &["quic", "udpquic", "direct", "tcptls", "nz"];

/// Check a normalized address the way spora-core's
/// `RelayEndpoint::from_url_param` reads it, more strictly (no empty host,
/// no port 0), and with a reason a person can act on. Returns the
/// canonical `?r=` form.
pub fn parse(endpoint: &str) -> Result<String, AddressError> {
    if endpoint.is_empty() || endpoint.chars().any(char::is_whitespace) {
        return Err(AddressError::Format);
    }
    let address = match endpoint.split_once('/') {
        Some((tag, rest)) => {
            if !PROTOCOL_TAGS.contains(&tag) {
                return Err(AddressError::Protocol);
            }
            rest
        }
        None => endpoint,
    };
    let port = if let Some(rest) = address.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or(AddressError::Format)?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(AddressError::Format);
        }
        port
    } else {
        let (host, port) = address.rsplit_once(':').ok_or(AddressError::Format)?;
        if host.contains(':') {
            return Err(AddressError::Ipv6Brackets);
        }
        if host.is_empty() || host.contains('/') || host.contains('[') || host.contains(']') {
            return Err(AddressError::Format);
        }
        port
    };
    // ASCII digits only: `parse` would take a leading '+'.
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AddressError::Port);
    }
    match port.parse::<u32>() {
        Ok(1..=65535) => {}
        _ => return Err(AddressError::Port),
    }
    RelayEndpoint::from_url_param(endpoint)
        .map(|e| e.to_url_param())
        .map_err(|_| AddressError::Format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_as_the_core_reads_them() {
        assert_eq!(
            parse("relay.example.org:443").as_deref(),
            Ok("relay.example.org:443")
        );
        // `quic/` is the default protocol, written without a tag.
        assert_eq!(parse("quic/1.2.3.4:443").as_deref(), Ok("1.2.3.4:443"));
        assert_eq!(parse("nz/1.2.3.4:443").as_deref(), Ok("nz/1.2.3.4:443"));
        assert_eq!(
            parse("tcptls/[2001:db8::1]:443").as_deref(),
            Ok("tcptls/[2001:db8::1]:443")
        );
        assert_eq!(
            parse("[::ffff:1.2.3.4]:80").as_deref(),
            Ok("[::ffff:1.2.3.4]:80")
        );
    }

    #[test]
    fn bad_addresses_say_why() {
        assert_eq!(parse(""), Err(AddressError::Format));
        assert_eq!(parse("relay.example.org"), Err(AddressError::Format));
        assert_eq!(parse("relay example:443"), Err(AddressError::Format));
        assert_eq!(parse(":443"), Err(AddressError::Format));
        assert_eq!(parse("warp/1.2.3.4:443"), Err(AddressError::Protocol));
        assert_eq!(parse("2001:db8::1:443"), Err(AddressError::Ipv6Brackets));
        assert_eq!(parse("[2001:db8::zz]:443"), Err(AddressError::Format));
        assert_eq!(parse("relay:0"), Err(AddressError::Port));
        assert_eq!(parse("relay:65536"), Err(AddressError::Port));
        assert_eq!(parse("relay:+443"), Err(AddressError::Port));
        assert_eq!(parse("relay:"), Err(AddressError::Port));
    }

    #[test]
    fn pasted_values_are_decoded() {
        assert_eq!(normalize("  nz%2F1.2.3.4%3A443 "), "nz/1.2.3.4:443");
        assert_eq!(normalize("relay:443"), "relay:443");
    }

    #[test]
    fn settings_round_trip_with_tokens() {
        let dir = std::env::temp_dir().join(format!("spora-wincore-relays-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = RelayStore::open(dir.clone());
        assert_eq!(store.get(), RelaySettings::default());
        store.update(|s| {
            s.use_spora = false;
            s.own.push(OwnRelay {
                endpoint: "relay:443".into(),
                token: Some("secret".into()),
            });
        });
        let again = RelayStore::open(dir.clone());
        assert_eq!(again.get(), store.get());
        assert!(!again.get().use_spora);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
