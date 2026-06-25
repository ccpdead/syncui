//! Last-sync snapshot persistence.
//!
//! A snapshot records the agreed state of files that were equal on both sides
//! after the previous successful sync. It is the "baseline" (B) in the
//! three-way comparison (Local vs Remote vs Baseline), which is what lets the
//! engine tell a *deletion* apart from a *new file* — the core capability of a
//! real bidirectional syncer.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapEntry {
    pub size: u64,
    pub mtime: i64,
    #[serde(default)]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub files: BTreeMap<String, SnapEntry>,
}

/// Load a snapshot from disk. Missing or corrupt files yield an empty
/// snapshot (treated as "no baseline / first sync").
pub fn load(path: &Path) -> Snapshot {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist a snapshot to disk, creating parent directories as needed.
pub fn save(path: &Path, snap: &Snapshot) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建快照目录失败: {e}"))?;
    }
    let data = serde_json::to_string(snap).map_err(|e| format!("序列化快照失败: {e}"))?;
    fs::write(path, data).map_err(|e| format!("写入快照失败: {e}"))
}
