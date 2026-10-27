//! Tauri command layer: bridges the frontend UI and the sync engine.

mod diagnostics;
mod engine;
mod settings;
mod snapshot;

use diagnostics::{mount_description, Diagnostics};
use engine::{
    apply_ops_with_diagnostics, build_snapshot_with_diagnostics, compare_one,
    compare_with_diagnostics, read_text_pair, write_text_file, CompareOptions, CompareResult,
    DiffEntry, FileTextPair, OpProgress, SyncOp, SyncResult,
};
use serde::Serialize;
use settings::AppSettings;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

/// Cancel flags of the compare / sync currently running (at most one each).
#[derive(Default)]
struct TaskState {
    compare: Mutex<Option<Arc<AtomicBool>>>,
    sync: Mutex<Option<Arc<AtomicBool>>>,
}

/// Install a fresh cancel flag for a new task, replacing the previous one.
fn begin_task(slot: &Mutex<Option<Arc<AtomicBool>>>) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    *slot.lock().unwrap() = Some(flag.clone());
    flag
}

fn cancel_task(slot: &Mutex<Option<Arc<AtomicBool>>>) {
    if let Some(flag) = slot.lock().unwrap().as_ref() {
        flag.store(true, Ordering::Relaxed);
    }
}

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

fn task_diagnostics(app: &AppHandle, kind: &str) -> Diagnostics {
    let directory = app
        .path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join("diagnostics"));
    let handle = app.clone();
    Diagnostics::new(kind, directory.as_deref(), move |line| {
        let _ = handle.emit("diagnostic-log", line);
    })
}

/// Compare two directories and return a structured diff for the UI.
///
/// Async + spawn_blocking so the heavy directory walk runs off the main
/// thread and never freezes the WebView. Emits `scan-progress` events.
#[tauri::command]
async fn compare_dirs(
    app: AppHandle,
    tasks: State<'_, TaskState>,
    local: String,
    remote: String,
    options: Option<CompareOptions>,
) -> Result<CompareResult, String> {
    let command_started = Instant::now();
    let opts = options.unwrap_or_default();
    let snap_path = snapshot_path(&app, &local, &remote)?;
    let cancel = begin_task(&tasks.compare);
    tauri::async_runtime::spawn_blocking(move || {
        let queued_ms = command_started.elapsed().as_secs_f64() * 1000.0;
        let diagnostics = task_diagnostics(&app, "compare");
        diagnostics.log(format!(
            "对比参数：mode={} hash={} ignore={:?}，后台排队={queued_ms:.2}ms",
            opts.mode, opts.use_hash, opts.ignore
        ));
        diagnostics.log(format!(
            "本地={local:?}；{}",
            mount_description(Path::new(&local))
        ));
        diagnostics.log(format!(
            "远程={remote:?}；{}",
            mount_description(Path::new(&remote))
        ));
        let baseline_started = Instant::now();
        let baseline = snapshot::load(&snap_path);
        diagnostics.log(format!(
            "基线加载：files={} elapsed={:.2}ms",
            baseline.files.len(),
            baseline_started.elapsed().as_secs_f64() * 1000.0
        ));
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
        let result = compare_with_diagnostics(
            Path::new(&local),
            Path::new(&remote),
            &opts,
            &baseline,
            &cancel,
            &mut emit,
            &diagnostics,
        );
        diagnostics.log(format!(
            "后台对比命令结束：wall={:.2}ms result={}",
            command_started.elapsed().as_secs_f64() * 1000.0,
            if result.is_ok() {
                "ok"
            } else {
                "error/cancelled"
            }
        ));
        result
    })
    .await
    .map_err(|e| format!("对比任务失败: {e}"))?
}

#[tauri::command]
fn cancel_compare(tasks: State<'_, TaskState>) {
    cancel_task(&tasks.compare);
}

#[tauri::command]
fn cancel_sync(tasks: State<'_, TaskState>) {
    cancel_task(&tasks.sync);
}

/// Apply the selected operations (parallel), emit `sync-progress` events, then
/// (two-way mode only) rebuild the baseline snapshot from the resulting state.
#[tauri::command]
async fn sync_entries(
    app: AppHandle,
    tasks: State<'_, TaskState>,
    local: String,
    remote: String,
    items: Vec<SyncOp>,
    ignore: Vec<String>,
    concurrency: Option<usize>,
    mode: Option<String>,
) -> Result<SyncResult, String> {
    let command_started = Instant::now();
    let snap_path = snapshot_path(&app, &local, &remote)?;
    let workers = concurrency.unwrap_or(4);
    let twoway = mode.as_deref() == Some("twoway");
    let cancel = begin_task(&tasks.sync);
    tauri::async_runtime::spawn_blocking(move || {
        let queued_ms = command_started.elapsed().as_secs_f64() * 1000.0;
        let diagnostics = task_diagnostics(&app, "sync");
        let upload = items.iter().filter(|item| item.op == "upload").count();
        let download = items.iter().filter(|item| item.op == "download").count();
        diagnostics.log(format!("同步参数：mode={:?} tasks={} upload={upload} download={download} delete/other={} workers={} ignore={:?} 后台排队={queued_ms:.2}ms", mode, items.len(), items.len() - upload - download, workers.clamp(1, 32), ignore));
        diagnostics.log(format!("本地={local:?}；{}", mount_description(Path::new(&local))));
        diagnostics.log(format!("远程={remote:?}；{}", mount_description(Path::new(&remote))));
        let app2 = app.clone();
        let last = Mutex::new(Instant::now());
        let progress = move |p: OpProgress| {
            let mut guard = last.lock().unwrap();
            // Keep all small-batch items, errors, and slow items; throttle bulk progress.
            if p.total <= 20 || p.index <= 10 || !p.ok || p.skipped || p.elapsed_ms >= 1000.0 || p.index == p.total || guard.elapsed() >= Duration::from_millis(100) {
                *guard = Instant::now();
                let _ = app2.emit("sync-progress", p);
            }
        };

        let transfer_app = app.clone();
        let transfer_diagnostics = diagnostics.clone();
        let heartbeat = Mutex::new(Instant::now());
        let transfer = move |p: engine::TransferProgress| {
            transfer_diagnostics.flush();
            let mut last = heartbeat.lock().unwrap();
            if !p.finished && last.elapsed() >= Duration::from_secs(5) {
                *last = Instant::now();
                transfer_diagnostics.log(format!("同步心跳：文件操作仍在进行 elapsed={:.1}s bytes={} current={:.1}KB/s", p.elapsed_seconds, p.bytes_copied, p.bytes_per_second / 1024.0));
            }
            let _ = transfer_app.emit("sync-transfer-progress", p);
        };

        let res = apply_ops_with_diagnostics(
            Path::new(&local),
            Path::new(&remote),
            &items,
            workers,
            &cancel,
            &progress,
            &transfer,
            &diagnostics,
        );

        if twoway {
            // A cancelled run keeps the previous baseline: it is still a valid
            // ancestor of both sides, and rebuilding means two more full walks.
            if !res.cancelled {
                let snap = build_snapshot_with_diagnostics(Path::new(&local), Path::new(&remote), &ignore, &diagnostics);
                let save_started = Instant::now();
                match snapshot::save(&snap_path, &snap) {
                    Ok(()) => diagnostics.log(format!("快照保存完成：files={} elapsed={:.2}ms", snap.files.len(), save_started.elapsed().as_secs_f64() * 1000.0)),
                    Err(error) => diagnostics.log(format!("快照保存失败：{error} elapsed={:.2}ms", save_started.elapsed().as_secs_f64() * 1000.0)),
                }
            } else {
                diagnostics.log("同步已取消，保留旧基线，不重建快照");
            }
        } else {
            // One-way mirrors never read the baseline; drop it so a later
            // switch to two-way can't trust a stale one.
            let _ = std::fs::remove_file(&snap_path);
            diagnostics.log("单向模式：跳过快照重建");
        }

        let wall_ms = command_started.elapsed().as_secs_f64() * 1000.0;
        diagnostics.log(format!("后台同步命令结束：wall={wall_ms:.2}ms 文件操作={:.2}ms 其他阶段={:.2}ms（含排队/诊断/快照；不含前端随后重新对比）", res.elapsed_seconds * 1000.0, (wall_ms - res.elapsed_seconds * 1000.0).max(0.0)));

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
        .manage(TaskState::default())
        .invoke_handler(tauri::generate_handler![
            compare_dirs,
            cancel_compare,
            sync_entries,
            cancel_sync,
            load_settings,
            save_settings,
            read_file_pair,
            write_file_text,
            compare_one_entry
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
