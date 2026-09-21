//! Saved connections, persisted as JSON under the user's config directory.
//! Older files (without the timestamp/url fields) load with defaults.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedConnection {
    pub id: String,
    pub label: String,
    pub secret_key: String,
    /// The share link, cached once the service reported it, so the detail
    /// pane can show and copy it while the share is off.
    #[serde(default)]
    pub url: String,
    /// Unix seconds; 0 = unknown (pre-existing entries).
    #[serde(default)]
    pub created_at: i64,
    /// Unix seconds of the last session start/stop or peer event; 0 = never.
    #[serde(default)]
    pub last_activity: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UseConnection {
    pub id: String,
    pub label: String,
    pub url: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_activity: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub shared: Vec<SharedConnection>,
    pub uses: Vec<UseConnection>,
    /// False for the in-memory demo store (SPORA_UI_DEMO): never written.
    #[serde(skip)]
    persist: bool,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    fn path() -> PathBuf {
        let dir = directories::ProjectDirs::from("", "", "spora")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join("connections.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let mut store = match fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<Store>(&data) {
                Ok(store) => store,
                Err(e) => {
                    // Never let a corrupt file be silently replaced by an
                    // empty one: the share keys in it are irreplaceable.
                    let bak = path.with_extension("json.bak");
                    log::error!("connections file unreadable ({e}); keeping it as {}", bak.display());
                    let _ = fs::rename(&path, &bak);
                    Store::default()
                }
            },
            Err(_) => Self::default(),
        };
        store.persist = true;
        store
    }

    /// An in-memory store that is never written to disk.
    pub fn ephemeral() -> Self {
        Self::default()
    }

    /// Atomic: written to a sibling temp file and renamed over the target,
    /// so an interrupted write never leaves a truncated file behind.
    pub fn save(&self) {
        if !self.persist {
            return;
        }
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let data = match serde_json::to_string_pretty(self) {
            Ok(d) => d,
            Err(e) => {
                log::error!("could not serialise connections: {e}");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = fs::write(&tmp, data).and_then(|_| fs::rename(&tmp, &path)) {
            log::error!("could not save connections to {}: {e}", path.display());
            let _ = fs::remove_file(&tmp);
        }
    }

    pub fn add_shared(&mut self, label: String, secret_key: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.shared.push(SharedConnection {
            id: id.clone(),
            label,
            secret_key,
            url: String::new(),
            created_at: now(),
            last_activity: 0,
        });
        self.save();
        id
    }

    pub fn add_use(&mut self, label: String, url: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.uses.push(UseConnection {
            id: id.clone(),
            label,
            url,
            created_at: now(),
            last_activity: 0,
        });
        self.save();
        id
    }

    pub fn remove_shared(&mut self, id: &str) {
        self.shared.retain(|c| c.id != id);
        self.save();
    }

    pub fn remove_use(&mut self, id: &str) {
        self.uses.retain(|c| c.id != id);
        self.save();
    }

    pub fn rename_shared(&mut self, id: &str, label: String) {
        if let Some(c) = self.shared.iter_mut().find(|c| c.id == id) {
            c.label = label;
        }
        self.save();
    }

    pub fn rename_use(&mut self, id: &str, label: String) {
        if let Some(c) = self.uses.iter_mut().find(|c| c.id == id) {
            c.label = label;
        }
        self.save();
    }

    pub fn set_share_url(&mut self, id: &str, url: &str) {
        if let Some(c) = self.shared.iter_mut().find(|c| c.id == id)
            && c.url != url
        {
            c.url = url.to_string();
            self.save();
        }
    }

    pub fn touch_shared(&mut self, id: &str) {
        if let Some(c) = self.shared.iter_mut().find(|c| c.id == id) {
            c.last_activity = now();
            self.save();
        }
    }

    pub fn touch_use(&mut self, id: &str) {
        if let Some(c) = self.uses.iter_mut().find(|c| c.id == id) {
            c.last_activity = now();
            self.save();
        }
    }

    /// "Connection N" — the default name for a new share or link, N being
    /// the first number not already used by an entry of that role.
    pub fn next_label(&self, share: bool) -> String {
        let taken: Vec<&str> = if share {
            self.shared.iter().map(|c| c.label.as_str()).collect()
        } else {
            self.uses.iter().map(|c| c.label.as_str()).collect()
        };
        let mut n = 1;
        loop {
            let candidate = format!("Connection {n}");
            if !taken.contains(&candidate.as_str()) {
                return candidate;
            }
            n += 1;
        }
    }
}
