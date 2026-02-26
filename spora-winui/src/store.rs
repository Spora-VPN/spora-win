use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedConnection {
    pub id: String,
    pub label: String,
    pub secret_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UseConnection {
    pub id: String,
    pub label: String,
    pub url: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub shared: Vec<SharedConnection>,
    pub uses: Vec<UseConnection>,
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
        match fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&path, data);
        }
    }

    pub fn add_shared(&mut self, label: String, secret_key: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.shared.push(SharedConnection {
            id: id.clone(),
            label,
            secret_key,
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
}
