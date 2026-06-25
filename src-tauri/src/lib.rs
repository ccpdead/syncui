//! Tauri command layer: bridges the frontend UI and the sync engine.

mod engine;

use engine::{
    compare_with_progress, copy_file_atomic, delete_file, CompareOptions, CompareResult,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// A single item the UI asked to synchronize.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncItem {
    rel_path: String,
    /// "new" | "modified" | "deleted"
    status: String,
}

/// Progress event emitted to the frontend during a sync run.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncProgress {
    index: usize,
    total: usize,
    rel_path: String,
    action: String,
    ok: bool,
    error: Option<String>,
}

/// Final summary returned when a sync run completes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncResult {
    copied: usize,
    deleted: usize,
    failed: usize,
    errors: Vec<String>,
}

/// Progress event emitted to the frontend while scanning directories.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgress {
    /// "local" | "remote"
    phase: String,
    count: usize,
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
    tauri::async_runtime::spawn_blocking(move || {
        let app2 = app.clone();
        let mut last = Instant::now();
        let mut started = false;
        let mut emit = move |phase: &str, count: usize| {
            // Throttle events to ~8/sec to avoid flooding the event channel.
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
        compare_with_progress(Path::new(&local), Path::new(&remote), &opts, &mut emit)
    })
    .await
    .map_err(|e| format!("对比任务失败: {e}"))?
}

/// Apply the selected sync items (local -> remote). Emits `sync-progress`
/// events as it works and returns a summary at the end.
#[tauri::command]
fn sync_entries(
    app: AppHandle,
    local: String,
    remote: String,
    items: Vec<SyncItem>,
    include_deletes: bool,
) -> Result<SyncResult, String> {
    let local_root = Path::new(&local);
    let remote_root = Path::new(&remote);
    let total = items.len();

    let mut copied = 0usize;
    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let (action, result) = match item.status.as_str() {
            "new" | "modified" => {
                let src = local_root.join(&item.rel_path);
                let dst = remote_root.join(&item.rel_path);
                ("copy", copy_file_atomic(&src, &dst))
            }
            "deleted" => {
                if include_deletes {
                    let target = remote_root.join(&item.rel_path);
                    ("delete", delete_file(&target))
                } else {
                    // Skip deletions unless the user explicitly opted in.
                    continue;
                }
            }
            other => ("skip", Err(format!("未知状态: {other}"))),
        };

        let (ok, err) = match &result {
            Ok(_) => {
                match action {
                    "copy" => copied += 1,
                    "delete" => deleted += 1,
                    _ => {}
                }
                (true, None)
            }
            Err(e) => {
                failed += 1;
                errors.push(format!("{}: {}", item.rel_path, e));
                (false, Some(e.clone()))
            }
        };

        let _ = app.emit(
            "sync-progress",
            SyncProgress {
                index: i + 1,
                total,
                rel_path: item.rel_path.clone(),
                action: action.to_string(),
                ok,
                error: err,
            },
        );
    }

    Ok(SyncResult {
        copied,
        deleted,
        failed,
        errors,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![compare_dirs, sync_entries])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
