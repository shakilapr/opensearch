//! Runtime configuration. The data root MUST be outside any synced path.

use crate::limits::Limits;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Root for all runtime state (SQLite, Tantivy, blobs, text). Server-only.
    pub data_dir: PathBuf,
    /// User-Agent sent with every crawl request.
    pub user_agent: String,
    /// Bind address for the HTTP API.
    pub bind: String,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/search"),
            user_agent: "AgentNativeSearchBot/0.1 (+https://github.com/agent-native-search)".into(),
            bind: "127.0.0.1:8080".into(),
            limits: Limits::default(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut c = Config::default();
        if let Ok(d) = std::env::var("SEARCH_DATA_DIR") {
            c.data_dir = PathBuf::from(d);
        }
        if let Ok(b) = std::env::var("SEARCH_BIND") {
            c.bind = b;
        }
        if let Ok(ua) = std::env::var("SEARCH_USER_AGENT") {
            c.user_agent = ua;
        }
        c
    }

    pub fn meta_db(&self) -> PathBuf {
        self.data_dir.join("meta").join("meta.sqlite")
    }
    /// The graph sidecar: the entity→document mapping plus co-occurrence edges. Written by
    /// `agent0 graph build`, read by the query-time graph lane. It is a sidecar because the index
    /// schema is frozen and hash-asserted, so the join cannot live in the meta store.
    pub fn graph_sidecar(&self) -> PathBuf {
        self.data_dir.join("graph").join("entities.bin")
    }
    pub fn tantivy_base(&self) -> PathBuf {
        self.data_dir.join("tantivy").join("base")
    }
    pub fn tantivy_overlay(&self) -> PathBuf {
        self.data_dir.join("tantivy").join("overlay")
    }
    pub fn blobs_dir(&self) -> PathBuf {
        self.data_dir.join("blobs")
    }
    pub fn text_dir(&self) -> PathBuf {
        self.data_dir.join("text")
    }

    /// Create the directory tree. Idempotent.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for p in [
            self.data_dir.clone(),
            self.data_dir.join("meta"),
            self.tantivy_base(),
            self.tantivy_overlay(),
            self.blobs_dir(),
            self.text_dir(),
        ] {
            std::fs::create_dir_all(p)?;
        }
        Ok(())
    }

    /// Refuse to run if the data root looks like it is inside the synced repo.
    pub fn assert_not_synced(&self) -> Result<(), String> {
        let s = self.data_dir.to_string_lossy();
        if s.contains("projects/search") && !s.contains("/var/") {
            return Err(format!(
                "data_dir {s} is inside a Syncthing-synced path; WAL SQLite and Tantivy \
                 MmapDirectory corrupt if copied mid-write. Use /var/lib/search or add to .stignore."
            ));
        }
        Ok(())
    }

    pub fn blob_path(&self, content_hash_hex: &str) -> PathBuf {
        Path::new(&self.blobs_dir())
            .join(&content_hash_hex[0..2])
            .join(content_hash_hex)
    }

    pub fn text_path(&self, content_hash_hex: &str) -> PathBuf {
        Path::new(&self.text_dir())
            .join(&content_hash_hex[0..2])
            .join(format!("{content_hash_hex}.txt"))
    }
}
