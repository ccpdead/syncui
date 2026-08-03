//! Tauri command layer: bridges the frontend UI and the sync engine.

mod engine;
mod settings;
mod snapshot;

use engine::{
    apply_ops, build_snapshot, compare_one, compare_with_progress, read_text_pair, write_text_file,
    CompareOptions, CompareResult, DiffEntry, FileTextPair, OpProgress, SyncOp, SyncResult,
};
use serde::Serialize;
use settings::AppSettings;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// Progress event emitted to the frontend while scanning directories.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgress {
    /// "local" | "remote"
    phase: String,
    count: usize,
}

/// Compute the on-disk path of the baseline snapshot for a (local, remote)
/// pair. Stored in the app config dir, keyed by a hash of both paths, so it
/// never pollutes the user's synced directories.
fn snapshot_path(app: &AppHandle, local: &str, remote: &str) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录: {e}"))?;
    let key = blake3::hash(format!("{local}\u{0}{remote}").as_bytes())
        .to_hex()
        .to_string();
    Ok(dir.join("snapshots").join(format!("{key}.json")))
}

/// Compare two directories and return a structured diff for the UI.
///
/// Async + spawn_blocking so the heavy directory walk runs off the main
/// thread and never freezes the WebView. Emits `scan-progress` events.
#[tauri::command]
async fn compare_dirs(
    app: AppHandle,
    local: String,
    remote: String,
    options: Option<CompareOptions>,
) -> Result<CompareResult, String> {
    let opts = options.unwrap_or_default();
    let snap_path = snapshot_path(&app, &local, &remote)?;
    tauri::async_runtime::spawn_blocking(move || {
        let baseline = snapshot::load(&snap_path);
        let app2 = app.clone();
        let mut last = Instant::now();
        let mut started = false;
        let mut emit = move |phase: &str, count: usize| {
            if !started || last.elapsed() >= Duration::from_millis(120) {
                started = true;
                last = Instant::now();
                let _ = app2.emit(
                    "scan-progress",
                    ScanProgress {
                        phase: phase.to_string(),
                        count,
                    },
                );
            }
        };
        compare_with_progress(
            Path::new(&local),
            Path::new(&remote),
            &opts,
            &baseline,
            &mut emit,
        )
    })
    .await
    .map_err(|e| format!("对比任务失败: {e}"))?
}

/// Apply the selected operations (parallel), emit `sync-progress` events, then
/// rebuild the baseline snapshot from the resulting state.
#[tauri::command]
async fn sync_entries(
    app: AppHandle,
    local: String,
    remote: String,
    items: Vec<SyncOp>,
    ignore: Vec<String>,
    concurrency: Option<usize>,
) -> Result<SyncResult, String> {
    let snap_path = snapshot_path(&app, &local, &remote)?;
    let workers = concurrency.unwrap_or(4);
    tauri::async_runtime::spawn_blocking(move || {
        let app2 = app.clone();
        let last = Mutex::new(Instant::now());
        let progress = move |p: OpProgress| {
            let mut guard = last.lock().unwrap();
            // Always emit the final event; throttle the rest to ~10/sec.
            if p.index == p.total || guard.elapsed() >= Duration::from_millis(100) {
                *guard = Instant::now();
                let _ = app2.emit("sync-progress", p);
            }
        };

        let res = apply_ops(
            Path::new(&local),
            Path::new(&remote),
            &items,
            workers,
            &progress,
        );

        // Refresh the baseline so the next comparison is accurate.
        let snap = build_snapshot(Path::new(&local), Path::new(&remote), &ignore);
        let _ = snapshot::save(&snap_path, &snap);

        Ok::<SyncResult, String>(res)
    })
    .await
    .map_err(|e| format!("同步任务失败: {e}"))?
}

#[tauri::command]
fn load_settings(app: AppHandle) -> AppSettings {
    settings::load(&app)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: AppSettings) -> Result<(), String> {
    settings::save(&app, &settings)
}

/// Read local + remote text for content diff (binary / >5MB gated).
#[tauri::command]
async fn read_file_pair(
    local: String,
    remote: String,
    rel_path: String,
) -> Result<FileTextPair, String> {
    tauri::async_runtime::spawn_blocking(move || {
        read_text_pair(Path::new(&local), Path::new(&remote), &rel_path)
    })
    .await
    .map_err(|e| format!("读取任务失败: {e}"))?
}

/// Write UTF-8 content to one side: side = "local" | "remote".
#[tauri::command]
async fn write_file_text(
    local: String,
    remote: String,
    rel_path: String,
    side: String,
    content: String,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = match side.as_str() {
            "local" => Path::new(&local),
            "remote" => Path::new(&remote),
            _ => return Err(format!("未知写入侧: {side}")),
        };
        write_text_file(root, &rel_path, &content)
    })
    .await
    .map_err(|e| format!("写入任务失败: {e}"))?
}

/// Re-compare a single relative path after an in-diff save.
#[tauri::command]
async fn compare_one_entry(
    app: AppHandle,
    local: String,
    remote: String,
    rel_path: String,
    options: Option<CompareOptions>,
) -> Result<Option<DiffEntry>, String> {
    let opts = options.unwrap_or_default();
    let snap_path = snapshot_path(&app, &local, &remote)?;
    tauri::async_runtime::spawn_blocking(move || {
        let baseline = snapshot::load(&snap_path);
        compare_one(
            Path::new(&local),
            Path::new(&remote),
            &rel_path,
            &opts,
            &baseline,
        )
    })
    .await
    .map_err(|e| format!("单文件对比失败: {e}"))?
}

/// On Linux, WebKitGTK's DMABUF / GPU compositing path fails with certain
/// drivers and renders a blank (white) window. The dev script exports these
/// vars, but a packaged binary (.deb / .AppImage) launched from a desktop
/// icon inherits no such environment, so we set them here before the WebView
/// is created. Done as early as possible in `run()` to take effect.
#[cfg(target_os = "linux")]
fn apply_webkit_workarounds() {
    // Set before any other thread spawns and before the WebView initializes.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    apply_webkit_workarounds();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            compare_dirs,
            sync_entries,
            load_settings,
            save_settings,
            read_file_pair,
            write_file_text,
            compare_one_entry
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
