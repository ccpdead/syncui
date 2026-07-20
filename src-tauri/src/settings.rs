//! Persist UI preferences under the app config directory
//! (Linux: `~/.config/com.syncui.app/settings.json`).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_conflict")]
    pub conflict_policy: String,
    #[serde(default)]
    pub use_hash: bool,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_ignore")]
    pub ignore_text: String,
    #[serde(default)]
    pub local_path: String,
    #[serde(default)]
    pub remote_path: String,
}

fn default_mode() -> String {
    "mirror".to_string()
}

fn default_conflict() -> String {
    "newer".to_string()
}

fn default_concurrency() -> usize {
    4
}

fn default_ignore() -> String {
    ".git, node_modules, .venv, __pycache__, target, dist, .DS_Store".to_string()
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            mode: default_mode(),
            conflict_policy: default_conflict(),
            use_hash: false,
            concurrency: default_concurrency(),
            ignore_text: default_ignore(),
            local_path: String::new(),
            remote_path: String::new(),
        }
    }
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录: {e}"))?;
    Ok(dir.join(SETTINGS_FILE))
}

/// Load settings; missing or corrupt file → defaults.
pub fn load(app: &AppHandle) -> AppSettings {
    let Ok(path) = settings_path(app) else {
        return AppSettings::default();
    };
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist settings to disk (creates parent dirs as needed).
pub fn save(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let data = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("序列化配置失败: {e}"))?;
    fs::write(&path, data).map_err(|e| format!("写入配置失败: {e}"))
}
