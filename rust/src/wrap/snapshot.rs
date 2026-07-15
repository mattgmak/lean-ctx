//! Pre-wrap config snapshots for byte-for-byte restore via `unwrap`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;

const SNAPSHOTS_DIR: &str = "snapshots";

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct WrapSnapshot {
    pub agent: String,
    pub timestamp: String,
    pub lean_ctx_version: String,
    pub files: BTreeMap<String, FileRecord>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct FileRecord {
    pub backup_path: String,
    pub existed: bool,
}

impl WrapSnapshot {
    pub(super) fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_string(),
            timestamp: Utc::now().to_rfc3339(),
            lean_ctx_version: env!("CARGO_PKG_VERSION").to_string(),
            files: BTreeMap::new(),
        }
    }

    pub(super) fn record_file(&mut self, path: &Path) {
        let existed = path.exists();
        let backup_path = if existed {
            match self.backup_copy(path) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(e) => {
                    tracing::warn!("snapshot backup failed for {}: {e}", path.display());
                    String::new()
                }
            }
        } else {
            String::new()
        };

        self.files.insert(
            path.to_string_lossy().to_string(),
            FileRecord {
                backup_path,
                existed,
            },
        );
    }

    fn backup_copy(&self, path: &Path) -> Result<PathBuf, String> {
        let snap_dir = snapshot_dir_for(&self.agent)?;
        std::fs::create_dir_all(&snap_dir).map_err(|e| e.to_string())?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let backup = snap_dir.join(format!("{file_name}.pre-wrap"));

        std::fs::copy(path, &backup).map_err(|e| e.to_string())?;
        Ok(backup)
    }

    pub(super) fn save(&self) -> Result<PathBuf, String> {
        let snap_dir = snapshot_dir_for(&self.agent)?;
        std::fs::create_dir_all(&snap_dir).map_err(|e| e.to_string())?;

        let manifest = snap_dir.join("wrap-manifest.json");
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize snapshot: {e}"))?;
        crate::config_io::write_atomic(&manifest, &json)?;
        Ok(manifest)
    }

    pub(super) fn load(agent: &str) -> Result<Self, String> {
        let snap_dir = snapshot_dir_for(agent)?;
        let manifest = snap_dir.join("wrap-manifest.json");
        if !manifest.exists() {
            return Err(format!(
                "No wrap snapshot found for '{agent}'. Was it wrapped with `lean-ctx wrap`?"
            ));
        }
        let content = std::fs::read_to_string(&manifest).map_err(|e| e.to_string())?;
        serde_json::from_str(&content).map_err(|e| format!("parse snapshot: {e}"))
    }
}

fn snapshot_dir_for(agent: &str) -> Result<PathBuf, String> {
    let state = crate::core::paths::state_dir()?;
    Ok(state.join(SNAPSHOTS_DIR).join(agent))
}
