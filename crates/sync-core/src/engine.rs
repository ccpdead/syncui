//! Core directory comparison & sync engine.
//!
//! Works purely on filesystem paths, so it doesn't care whether a side is a
//! local folder or a mounted remote (sftp/smb/nfs) folder — both look like
//! plain paths to the OS.

use crate::diagnostics::Diagnostics;
use crate::snapshot::{SnapEntry, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};
use walkdir::WalkDir;

/// Error returned when a compare is aborted through its cancel flag.
pub const CANCELLED: &str = "已取消";

fn is_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// Modified-time comparison tolerance in seconds. Mounted filesystems
/// (sftp/smb) often report slightly different mtime precision than local
/// disks, so we don't treat sub-tolerance differences as a change.
const MTIME_TOLERANCE_SECS: i64 = 2;

/// Options that control how a comparison is performed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareOptions {
    /// When true, files with equal size but differing mtime are compared by
    /// content hash (with snapshot-backed caching). Slower but accurate.
    #[serde(default)]
    pub use_hash: bool,
    /// Substrings to ignore (simple `contains` match on the relative path),
    /// e.g. ".git", "node_modules", ".venv". Ignored directories are pruned
    /// during the walk, so we never descend into them.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Sync mode:
    /// - `"mirror"` — one-way local → remote (make remote match local)
    /// - `"mirror_pull"` — one-way remote → local (make local match remote)
    /// - `"twoway"` — bidirectional using the three-way (L/R/Baseline) matrix
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "mirror".to_string()
}

impl Default for CompareOptions {
    fn default() -> Self {
        CompareOptions {
            use_hash: false,
            ignore: Vec::new(),
            mode: default_mode(),
        }
    }
}

#[derive(Debug, Clone)]
struct FileMeta {
    size: u64,
    mtime: i64,
}

/// The action proposed for a relative path after comparison.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    /// Copy local -> remote.
    Upload,
    /// Copy remote -> local.
    Download,
    /// Remove the local file (remote deleted it).
    DeleteLocal,
    /// Remove the remote file (local deleted it, or mirror cleanup).
    DeleteRemote,
    /// Both sides changed relative to the baseline; user must resolve.
    Conflict,
    /// Identical on both sides.
    Same,
}

/// One row in the diff result presented to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffEntry {
    pub rel_path: String,
    pub action: Action,
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
    pub local_mtime: Option<i64>,
    pub remote_mtime: Option<i64>,
    /// "local" | "remote" | null — which side is newer.
    pub newer: Option<String>,
}

/// Aggregate result returned to the UI after a compare.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
    #[serde(default)]
    pub comparison_id: Option<String>,
    #[serde(default)]
    pub complete: bool,
    pub entries: Vec<DiffEntry>,
    pub upload_count: usize,
    pub download_count: usize,
    pub delete_local_count: usize,
    pub delete_remote_count: usize,
    pub conflict_count: usize,
    pub same_count: usize,
    /// Entries skipped during scan (symlinks, broken links, unreadable files).
    pub skipped_count: usize,
}

fn mtime_of(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether `rel` (a `/`-separated relative path) matches any ignore pattern.
///
/// - `build` / `**/build` / `build/`: any path component named exactly `build`
///   (so `rebuild.sh` or `src/dialog.cpp` are NOT caught by `build` / `log`).
/// - `*` and `?` glob within one name: `*.bkp`, `.~lock*`.
/// - A pattern containing `/` matches a path anchored at the root
///   (`slam_ws/build`), or at any depth when written as `**/a/b`.
pub fn is_ignored(rel: &str, ignore: &[String]) -> bool {
    ignore.iter().any(|raw| {
        let any_depth = raw.starts_with("**/");
        let pat = raw.trim_start_matches("**/").trim_matches('/');
        if pat.is_empty() {
            return false;
        }
        if pat.contains('/') {
            let pat_parts: Vec<&str> = pat.split('/').collect();
            let rel_parts: Vec<&str> = rel.split('/').collect();
            let matches_at = |start: usize| {
                start + pat_parts.len() <= rel_parts.len()
                    && pat_parts
                        .iter()
                        .zip(&rel_parts[start..])
                        .all(|(p, r)| glob_match(p, r))
            };
            if any_depth {
                (0..rel_parts.len()).any(matches_at)
            } else {
                matches_at(0)
            }
        } else {
            rel.split('/').any(|part| glob_match(pat, part))
        }
    })
}

/// Match one path component against a pattern with `*` (any run) and `?`
/// (one char) wildcards; everything else is literal.
fn glob_match(pat: &str, name: &str) -> bool {
    if !pat.contains(['*', '?']) {
        return pat == name;
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

pub fn mtime_close(a: i64, b: i64) -> bool {
    (a - b).abs() <= MTIME_TOLERANCE_SECS
}

/// Walk a directory and build a map of relative path -> file metadata.
/// Resilient: unreadable entries (broken links on gvfs/NFS, permission
/// errors) are skipped and counted. Symlinks are skipped entirely. Ignored
/// directories are pruned during the walk for speed.
fn scan(
    root: &Path,
    ignore: &[String],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize),
    diagnostics: &Diagnostics,
    label: &str,
) -> Result<(BTreeMap<String, FileMeta>, usize), String> {
    let started = Instant::now();
    let mut metadata_ms = 0.0;
    let mut scan_errors = 0usize;
    let mut symlinks = 0usize;
    let mut slow_metadata: Vec<(f64, String)> = Vec::new();
    let mut last_update = Instant::now();
    diagnostics.log(format!("扫描开始：phase={label} root={root:?}"));
    let mut map = BTreeMap::new();
    let mut skipped: usize = 0;
    if !root.exists() {
        diagnostics.log(format!("扫描失败：phase={label} 路径不存在 root={root:?}"));
        return Err(format!("路径不存在: {}", root.display()));
    }
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| match e.path().strip_prefix(root) {
            Ok(rel) => {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                !is_ignored(&rel_str, ignore)
            }
            Err(_) => true,
        });

    let mut seen = 0usize;
    for entry in walker {
        if is_cancelled(cancel) {
            diagnostics.log(format!(
                "扫描取消：phase={label} files={} elapsed={:.2}ms",
                map.len(),
                started.elapsed().as_secs_f64() * 1000.0
            ));
            return Err(CANCELLED.into());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(error) => {
                skipped += 1;
                scan_errors += 1;
                let message = format!("扫描跳过：phase={label} error={error:?}");
                if scan_errors <= 3 {
                    diagnostics.log(message);
                } else {
                    diagnostics.detail(message);
                }
                continue;
            }
        };
        let ft = entry.file_type();
        if ft.is_symlink() || !ft.is_file() {
            if ft.is_symlink() {
                skipped += 1;
                symlinks += 1;
            }
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        // Skip our own atomic-copy leftovers (e.g. from an interrupted run).
        if rel_str.ends_with(".synctmp") {
            continue;
        }
        let previous_metadata_ms = metadata_ms;
        let meta = match timed(&mut metadata_ms, || entry.metadata()) {
            Ok(m) => m,
            Err(error) => {
                skipped += 1;
                scan_errors += 1;
                let message =
                    format!("metadata跳过：phase={label} path={rel_str:?} error={error:?}");
                if scan_errors <= 3 {
                    diagnostics.log(message);
                } else {
                    diagnostics.detail(message);
                }
                continue;
            }
        };
        let elapsed = metadata_ms - previous_metadata_ms;
        if last_update.elapsed() >= Duration::from_secs(5) {
            diagnostics.log(format!("扫描仍在进行：phase={label} files={} 当前={rel_str:?} elapsed={:.2}ms metadata={metadata_ms:.2}ms", seen + 1, started.elapsed().as_secs_f64() * 1000.0));
            last_update = Instant::now();
        }
        if elapsed >= 100.0 && diagnostics.enabled() {
            slow_metadata.push((elapsed, rel_str.clone()));
            slow_metadata.sort_by(|a, b| b.0.total_cmp(&a.0));
            slow_metadata.truncate(3);
        }
        map.insert(
            rel_str,
            FileMeta {
                size: meta.len(),
                mtime: mtime_of(&meta),
            },
        );
        seen += 1;
        if seen % 256 == 0 {
            progress(seen);
        }
    }
    progress(seen);
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    diagnostics.log(format!("扫描完成：phase={label} files={seen} skipped={skipped}（symlink={symlinks} errors={scan_errors}）total={total_ms:.2}ms metadata={metadata_ms:.2}ms 其余遍历/整理={:.2}ms", (total_ms - metadata_ms).max(0.0)));
    for (elapsed, path) in slow_metadata {
        diagnostics.log(format!(
            "慢metadata：phase={label} path={path:?} elapsed={elapsed:.2}ms"
        ));
    }
    Ok((map, skipped))
}

/// Compute a blake3 hash of a file's contents (chunked, low memory).
fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        if is_cancelled(cancel) {
            return Err(CANCELLED.into());
        }
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("读取文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Hash a file, reusing the baseline's stored hash when size+mtime match
/// (incremental cache — avoids re-reading unchanged files over the network).
fn hash_cached(
    path: &Path,
    meta: &FileMeta,
    base: Option<&SnapEntry>,
    cancel: &AtomicBool,
) -> Option<String> {
    if let Some(b) = base {
        if b.size == meta.size && mtime_close(b.mtime, meta.mtime) {
            if let Some(h) = &b.hash {
                return Some(h.clone());
            }
        }
    }
    hash_file(path, cancel).ok()
}

/// Whether the local and remote files have identical content.
fn same_content(
    lp: &Path,
    l: &FileMeta,
    rp: &Path,
    r: &FileMeta,
    use_hash: bool,
    base: Option<&SnapEntry>,
    cancel: &AtomicBool,
) -> bool {
    if l.size != r.size {
        return false;
    }
    if mtime_close(l.mtime, r.mtime) {
        return true;
    }
    if !use_hash {
        return false;
    }
    match (
        hash_cached(lp, l, base, cancel),
        hash_cached(rp, r, base, cancel),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Whether a file differs from the recorded baseline. No baseline => changed.
fn changed_from_baseline(meta: &FileMeta, base: Option<&SnapEntry>) -> bool {
    match base {
        None => true,
        Some(b) => !(b.size == meta.size && mtime_close(b.mtime, meta.mtime)),
    }
}

fn newer_of(l: Option<&FileMeta>, r: Option<&FileMeta>) -> Option<String> {
    match (l, r) {
        (Some(l), Some(r)) => Some(
            if l.mtime >= r.mtime {
                "local"
            } else {
                "remote"
            }
            .into(),
        ),
        (Some(_), None) => Some("local".into()),
        (None, Some(_)) => Some("remote".into()),
        (None, None) => None,
    }
}

/// Convenience wrapper used by tests and as a stable API (no progress, empty
/// baseline => behaves as a plain two-way / mirror compare).
#[allow(dead_code)]
pub fn compare(
    local: &Path,
    remote: &Path,
    opts: &CompareOptions,
) -> Result<CompareResult, String> {
    let cancel = AtomicBool::new(false);
    compare_with_progress(
        local,
        remote,
        opts,
        &Snapshot::default(),
        &cancel,
        &mut |_, _| {},
    )
}

/// Compare two directory trees against a baseline snapshot and produce a
/// structured diff of the entries that need attention; identical files are
/// only counted (`same_count`). `progress(phase, count)` is called
/// periodically during scanning ("local"/"remote"). Returns `Err(CANCELLED)`
/// as soon as `cancel` is set.
pub fn compare_with_progress(
    local: &Path,
    remote: &Path,
    opts: &CompareOptions,
    baseline: &Snapshot,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str, usize),
) -> Result<CompareResult, String> {
    compare_with_diagnostics(
        local,
        remote,
        opts,
        baseline,
        cancel,
        progress,
        &Diagnostics::silent(),
    )
}

pub fn compare_with_diagnostics(
    local: &Path,
    remote: &Path,
    opts: &CompareOptions,
    baseline: &Snapshot,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str, usize),
    diagnostics: &Diagnostics,
) -> Result<CompareResult, String> {
    let started = Instant::now();
    // Both walks are independent and usually bound by different devices (a
    // local disk vs. a network mount), so run them concurrently.
    let ((local_map, local_skipped), local_ms, (remote_map, remote_skipped), remote_ms) =
        std::thread::scope(|s| -> Result<_, String> {
            let (tx, rx) = mpsc::channel::<(&'static str, usize)>();
            let walk =
                |root: &Path, phase: &'static str, tx: mpsc::Sender<(&'static str, usize)>| {
                    let t = Instant::now();
                    let res = scan(
                        root,
                        &opts.ignore,
                        cancel,
                        &mut |n| {
                            let _ = tx.send((phase, n));
                        },
                        diagnostics,
                        phase,
                    );
                    (res, t.elapsed().as_millis())
                };
            let local_tx = tx.clone();
            let local_worker = s.spawn(move || walk(local, "local", local_tx));
            let remote_worker = s.spawn(move || walk(remote, "remote", tx));
            for (phase, n) in rx {
                progress(phase, n);
            }
            let (l, lms) = local_worker
                .join()
                .map_err(|_| "本地扫描线程异常退出".to_string())?;
            let (r, rms) = remote_worker
                .join()
                .map_err(|_| "远程扫描线程异常退出".to_string())?;
            Ok((l?, lms, r?, rms))
        })?;
    let decide_started = Instant::now();
    diagnostics.log(format!(
        "差异决策开始：hash={}，基线条目={}，已存hash={}（此阶段含必要的内容哈希和排序）",
        opts.use_hash,
        baseline.files.len(),
        baseline
            .files
            .values()
            .filter(|entry| entry.hash.is_some())
            .count()
    ));

    let twoway = opts.mode == "twoway";
    let mirror_pull = opts.mode == "mirror_pull";

    // Union of every relative path seen on either side (plus baseline in
    // two-way mode, so deletions are detected).
    let mut keys: BTreeSet<&String> = BTreeSet::new();
    keys.extend(local_map.keys());
    keys.extend(remote_map.keys());
    if twoway {
        keys.extend(baseline.files.keys());
    }

    let mut entries: Vec<DiffEntry> = Vec::new();
    let mut counts = [0usize; 6]; // upload, download, delLocal, delRemote, conflict, same
    let mut hash_candidates = 0usize;
    let mut hash_decision_ms = 0.0;
    let mut slow_hash: Vec<(f64, String)> = Vec::new();
    let mut last_heartbeat = Instant::now();

    for rel in keys {
        if is_cancelled(cancel) {
            return Err(CANCELLED.into());
        }
        let l = local_map.get(rel);
        let r = remote_map.get(rel);
        let b = baseline.files.get(rel);
        let lp = local.join(rel);
        let rp = remote.join(rel);
        let hash_candidate = opts.use_hash
            && matches!((l, r), (Some(l), Some(r)) if l.size == r.size && !mtime_close(l.mtime, r.mtime));
        let candidate_started = hash_candidate.then(Instant::now);

        let action = if twoway {
            decide_twoway(&lp, l, &rp, r, b, opts.use_hash, cancel)
        } else if mirror_pull {
            decide_mirror_pull(&lp, l, &rp, r, opts.use_hash, b, cancel)
        } else {
            decide_mirror(&lp, l, &rp, r, opts.use_hash, b, cancel)
        };

        if let Some(started) = candidate_started {
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            hash_candidates += 1;
            hash_decision_ms += elapsed;
            diagnostics.detail(format!("哈希候选比较：path={rel:?} elapsed={elapsed:.2}ms"));
            if elapsed >= 100.0 && diagnostics.enabled() {
                slow_hash.push((elapsed, rel.clone()));
                slow_hash.sort_by(|a, b| b.0.total_cmp(&a.0));
                slow_hash.truncate(3);
            }
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
            diagnostics.log(format!("差异决策仍在进行：当前 path={rel:?}，已检查hash候选={hash_candidates}，elapsed={:.2}ms", decide_started.elapsed().as_secs_f64() * 1000.0));
            last_heartbeat = Instant::now();
        }

        let action = match action {
            Some(a) => a,
            None => continue, // nothing to do (e.g. both deleted)
        };

        match action {
            Action::Upload => counts[0] += 1,
            Action::Download => counts[1] += 1,
            Action::DeleteLocal => counts[2] += 1,
            Action::DeleteRemote => counts[3] += 1,
            Action::Conflict => counts[4] += 1,
            Action::Same => {
                counts[5] += 1;
                continue;
            }
        }

        entries.push(DiffEntry {
            rel_path: rel.clone(),
            action,
            local_size: l.map(|m| m.size),
            remote_size: r.map(|m| m.size),
            local_mtime: l.map(|m| m.mtime),
            remote_mtime: r.map(|m| m.mtime),
            newer: newer_of(l, r),
        });
    }

    entries.sort_by(|a, b| {
        fn rank(s: &Action) -> u8 {
            match s {
                Action::Upload => 0,
                Action::Download => 1,
                Action::DeleteRemote => 2,
                Action::DeleteLocal => 3,
                Action::Conflict => 4,
                Action::Same => 5,
            }
        }
        rank(&a.action)
            .cmp(&rank(&b.action))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    if is_cancelled(cancel) {
        return Err(CANCELLED.into());
    }

    eprintln!(
        "[syncui compare] local={} remote={} changed={} same={} local_scan_ms={} remote_scan_ms={} decide_ms={} total_ms={}",
        local_map.len(),
        remote_map.len(),
        entries.len(),
        counts[5],
        local_ms,
        remote_ms,
        decide_started.elapsed().as_millis(),
        started.elapsed().as_millis(),
    );
    diagnostics.log(format!("对比完成：local_scan={local_ms}ms remote_scan={remote_ms}ms（两侧并行，不能相加）decision/hash/sort={:.2}ms total={:.2}ms changed={} same={}", decide_started.elapsed().as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0, entries.len(), counts[5]));
    diagnostics.log(format!("哈希候选比较：count={hash_candidates} 相关决策耗时={hash_decision_ms:.2}ms（含内容读取或缓存命中；不是额外可相加的阶段）"));
    for (elapsed, path) in slow_hash {
        diagnostics.log(format!("慢哈希候选：path={path:?} elapsed={elapsed:.2}ms"));
    }

    Ok(CompareResult {
        comparison_id: None,
        complete: true,
        entries,
        upload_count: counts[0],
        download_count: counts[1],
        delete_local_count: counts[2],
        delete_remote_count: counts[3],
        conflict_count: counts[4],
        same_count: counts[5],
        skipped_count: local_skipped + remote_skipped,
    })
}

/// One-way mirror decision (make remote match local).
fn decide_mirror(
    lp: &Path,
    l: Option<&FileMeta>,
    rp: &Path,
    r: Option<&FileMeta>,
    use_hash: bool,
    base: Option<&SnapEntry>,
    cancel: &AtomicBool,
) -> Option<Action> {
    match (l, r) {
        (Some(l), Some(r)) => {
            if same_content(lp, l, rp, r, use_hash, base, cancel) {
                Some(Action::Same)
            } else {
                Some(Action::Upload)
            }
        }
        (Some(_), None) => Some(Action::Upload),
        (None, Some(_)) => Some(Action::DeleteRemote), // remote extra
        (None, None) => None,
    }
}

/// One-way reverse mirror (make local match remote).
fn decide_mirror_pull(
    lp: &Path,
    l: Option<&FileMeta>,
    rp: &Path,
    r: Option<&FileMeta>,
    use_hash: bool,
    base: Option<&SnapEntry>,
    cancel: &AtomicBool,
) -> Option<Action> {
    match (l, r) {
        (Some(l), Some(r)) => {
            if same_content(lp, l, rp, r, use_hash, base, cancel) {
                Some(Action::Same)
            } else {
                Some(Action::Download)
            }
        }
        (None, Some(_)) => Some(Action::Download),
        (Some(_), None) => Some(Action::DeleteLocal), // local extra
        (None, None) => None,
    }
}

/// Three-way (Local/Remote/Baseline) decision matrix for bidirectional sync.
fn decide_twoway(
    lp: &Path,
    l: Option<&FileMeta>,
    rp: &Path,
    r: Option<&FileMeta>,
    b: Option<&SnapEntry>,
    use_hash: bool,
    cancel: &AtomicBool,
) -> Option<Action> {
    match (l, r) {
        (Some(l), Some(r)) => {
            if same_content(lp, l, rp, r, use_hash, b, cancel) {
                return Some(Action::Same);
            }
            let lc = changed_from_baseline(l, b);
            let rc = changed_from_baseline(r, b);
            match (lc, rc) {
                (true, false) => Some(Action::Upload),
                (false, true) => Some(Action::Download),
                _ => Some(Action::Conflict),
            }
        }
        (Some(l), None) => match b {
            None => Some(Action::Upload), // new local file
            Some(_) => {
                if changed_from_baseline(l, b) {
                    Some(Action::Conflict) // modified locally, deleted remotely
                } else {
                    Some(Action::DeleteLocal) // remote deletion to propagate
                }
            }
        },
        (None, Some(r)) => match b {
            None => Some(Action::Download), // new remote file
            Some(_) => {
                if changed_from_baseline(r, b) {
                    Some(Action::Conflict) // modified remotely, deleted locally
                } else {
                    Some(Action::DeleteRemote) // local deletion to propagate
                }
            }
        },
        (None, None) => None, // both gone; baseline is stale
    }
}

/// Set a destination file's mtime to match the source (best-effort).
#[cfg(test)]
fn mirror_mtime(src: &Path, dst: &Path) {
    let _ = mirror_mtime_result(src, dst);
}

fn mirror_mtime_result(src: &Path, dst: &Path) -> Result<(), String> {
    let mtime = fs::metadata(src)
        .and_then(|meta| meta.modified())
        .map_err(|e| e.to_string())?;
    filetime::set_file_mtime(dst, filetime::FileTime::from_system_time(mtime))
        .map_err(|e| e.to_string())
}

fn timed<T>(milliseconds: &mut f64, operation: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = operation();
    *milliseconds += started.elapsed().as_secs_f64() * 1000.0;
    result
}

#[derive(Debug, Clone, Default)]
struct OpTimings {
    exists_ms: f64,
    mkdir_ms: f64,
    open_ms: f64,
    metadata_ms: f64,
    create_ms: f64,
    read_ms: f64,
    write_ms: f64,
    permissions_ms: f64,
    close_ms: f64,
    mtime_ms: f64,
    rename_ms: f64,
    delete_ms: f64,
    fallback_ms: f64,
    read_calls: u64,
    write_calls: u64,
    rename_error: Option<String>,
    mtime_error: Option<String>,
}

impl OpTimings {
    fn describe(&self) -> String {
        format!("exists={:.2} mkdir={:.2} open={:.2} metadata={:.2} create={:.2} read={:.2}({}次) write={:.2}({}次) chmod={:.2} close={:.2} mtime={:.2} rename={:.2} delete={:.2} fallback={:.2}ms{}{}",
            self.exists_ms, self.mkdir_ms, self.open_ms, self.metadata_ms, self.create_ms,
            self.read_ms, self.read_calls, self.write_ms, self.write_calls, self.permissions_ms,
            self.close_ms, self.mtime_ms, self.rename_ms, self.delete_ms, self.fallback_ms,
            self.rename_error.as_ref().map(|e| format!(" rename_error={e:?}")).unwrap_or_default(),
            self.mtime_error.as_ref().map(|e| format!(" mtime_error={e:?}")).unwrap_or_default())
    }
}

/// Copy a single file to `dst`, preserving the source mtime so subsequent
/// comparisons stay idempotent.
///
/// Prefers an atomic temp-file + rename (avoids half-written files). But some
/// network mounts — notably gvfs (sftp/nfs) FUSE backends — implement `rename`
/// unreliably and return ENOENT even when the temp file exists. In that case
/// we fall back to a direct copy onto the destination, which works wherever
/// writing the temp file worked.
#[allow(dead_code)]
pub fn copy_file_atomic(src: &Path, dst: &Path) -> Result<(), String> {
    copy_file_atomic_with_progress(src, dst, &|_| {})
}

/// Count bytes after writes succeed, including bytes copied during a fallback.
fn copy_file_with_progress(
    src: &Path,
    dst: &Path,
    on_bytes: &(dyn Fn(u64) + Sync),
    timings: &mut OpTimings,
) -> Result<(), String> {
    let mut source = timed(&mut timings.open_ms, || fs::File::open(src))
        .map_err(|e| format!("复制失败: {e}"))?;
    let meta = timed(&mut timings.metadata_ms, || source.metadata())
        .map_err(|e| format!("复制失败: {e}"))?;
    if !meta.is_file() {
        return Err("复制失败: 源路径不是普通文件".into());
    }
    let mut destination = timed(&mut timings.create_ms, || fs::File::create(dst))
        .map_err(|e| format!("复制失败: {e}"))?;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        timings.read_calls += 1;
        let count = match timed(&mut timings.read_ms, || source.read(&mut buffer)) {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("复制失败: {e}")),
        };
        let mut remaining = &buffer[..count];
        while !remaining.is_empty() {
            timings.write_calls += 1;
            match timed(&mut timings.write_ms, || destination.write(remaining)) {
                Ok(0) => return Err("复制失败: 写入未取得进展".into()),
                Ok(written) => {
                    on_bytes(written as u64);
                    remaining = &remaining[written..];
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("复制失败: {e}")),
            }
        }
    }
    let result = timed(&mut timings.permissions_ms, || {
        destination.set_permissions(meta.permissions())
    })
    .map_err(|e| format!("复制失败: {e}"));
    // FUSE/SFTP mounts may defer work until close; keep this separate from write().
    timed(&mut timings.close_ms, || {
        drop(destination);
        drop(source);
    });
    result
}

fn copy_file_atomic_with_progress(
    src: &Path,
    dst: &Path,
    on_bytes: &(dyn Fn(u64) + Sync),
) -> Result<(), String> {
    copy_file_measured(src, dst, on_bytes, &mut OpTimings::default())
}

fn copy_file_measured(
    src: &Path,
    dst: &Path,
    on_bytes: &(dyn Fn(u64) + Sync),
    timings: &mut OpTimings,
) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        timed(&mut timings.mkdir_ms, || fs::create_dir_all(parent))
            .map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let tmp: PathBuf = {
        let mut p = dst.to_path_buf();
        let name = format!(
            ".{}.synctmp",
            dst.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        );
        p.set_file_name(name);
        p
    };

    copy_file_with_progress(src, &tmp, on_bytes, timings)?;
    if let Err(error) = timed(&mut timings.mtime_ms, || mirror_mtime_result(src, &tmp)) {
        timings.mtime_error = Some(error);
    }

    match timed(&mut timings.rename_ms, || fs::rename(&tmp, dst)) {
        Ok(_) => Ok(()),
        Err(error) => {
            timings.rename_error = Some(format!(
                "{error}; kind={:?}; os={:?}",
                error.kind(),
                error.raw_os_error()
            ));
            // Fallback for mounts where rename is unreliable (gvfs/NFS).
            let fallback_started = Instant::now();
            let direct = copy_file_with_progress(src, dst, on_bytes, timings);
            timings.fallback_ms += fallback_started.elapsed().as_secs_f64() * 1000.0;
            let _ = timed(&mut timings.delete_ms, || fs::remove_file(&tmp));
            if direct.is_ok() {
                if let Err(error) = timed(&mut timings.mtime_ms, || mirror_mtime_result(src, dst)) {
                    timings.mtime_error = Some(error);
                }
            }
            direct
        }
    }
}

/// Delete a file (used for delete actions when the user opts in).
pub fn delete_file(path: &Path) -> Result<(), String> {
    fs::remove_file(path).map_err(|e| format!("删除失败: {e}"))
}

// ----------------------------- sync execution -----------------------------

/// A concrete operation the UI asked to perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOp {
    pub rel_path: String,
    /// "upload" | "download" | "delLocal" | "delRemote"
    pub op: String,
}

/// Progress emitted per operation during a sync run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpProgress {
    pub index: usize,
    pub total: usize,
    pub rel_path: String,
    pub op: String,
    pub ok: bool,
    pub skipped: bool,
    pub error: Option<String>,
    pub elapsed_ms: f64,
    pub bytes_copied: u64,
}

/// Aggregate file-copy throughput across all workers (not network wire speed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    #[serde(default)]
    pub logical_bytes: Option<u64>,
    pub bytes_copied: u64,
    pub bytes_per_second: f64,
    pub elapsed_seconds: f64,
    pub finished: bool,
}

fn report_transfer(
    bytes: &AtomicU64,
    started: Instant,
    stop: mpsc::Receiver<()>,
    progress: &(dyn Fn(TransferProgress) + Sync),
) {
    let mut previous_time = started;
    let mut previous_bytes = 0;
    loop {
        let finished = !matches!(
            stop.recv_timeout(Duration::from_millis(500)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let now = Instant::now();
        let bytes_copied = bytes.load(Ordering::Relaxed);
        let elapsed_seconds = now.duration_since(started).as_secs_f64();
        let (rate_bytes, rate_seconds) = if finished {
            (bytes_copied, elapsed_seconds)
        } else {
            (
                bytes_copied - previous_bytes,
                now.duration_since(previous_time).as_secs_f64(),
            )
        };
        progress(TransferProgress {
            logical_bytes: None,
            bytes_copied,
            bytes_per_second: rate_bytes as f64 / rate_seconds.max(f64::EPSILON),
            elapsed_seconds,
            finished,
        });
        if finished {
            break;
        }
        previous_time = now;
        previous_bytes = bytes_copied;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemResult {
    pub rel_path: String,
    pub op: String,
    pub status: String,
    pub message: Option<String>,
}

/// Summary returned when a sync run completes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    #[serde(default)]
    pub items: Vec<ItemResult>,
    #[serde(default)]
    pub refreshed: Option<CompareResult>,
    #[serde(default)]
    pub needs_recompare: bool,
    #[serde(default)]
    pub logical_bytes: u64,
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    /// Items whose file vanished between compare and sync (raced away).
    pub skipped: usize,
    pub failed: usize,
    pub errors: Vec<String>,
    /// True when the run was stopped through its cancel flag before every op ran.
    pub cancelled: bool,
    /// Relative paths whose op succeeded; only filled for cancelled runs so the
    /// UI can update its diff list without a full re-compare.
    pub done_paths: Vec<String>,
    pub bytes_copied: u64,
    pub elapsed_seconds: f64,
}

/// Outcome of a single op: either performed, or skipped because the file
/// disappeared between comparison and execution (a benign race).
enum OpOutcome {
    Done,
    Skipped,
}

fn exec_op(
    local: &Path,
    remote: &Path,
    op: &SyncOp,
    on_bytes: &(dyn Fn(u64) + Sync),
    timings: &mut OpTimings,
) -> Result<OpOutcome, String> {
    let lp = local.join(&op.rel_path);
    let rp = remote.join(&op.rel_path);
    match op.op.as_str() {
        "upload" => {
            if !timed(&mut timings.exists_ms, || lp.exists()) {
                return Ok(OpOutcome::Skipped);
            }
            match copy_file_measured(&lp, &rp, on_bytes, timings) {
                Ok(_) => Ok(OpOutcome::Done),
                // If it failed because the source raced away, skip quietly.
                Err(e) if !timed(&mut timings.exists_ms, || lp.exists()) => {
                    let _ = e;
                    Ok(OpOutcome::Skipped)
                }
                Err(e) => Err(e),
            }
        }
        "download" => {
            if !timed(&mut timings.exists_ms, || rp.exists()) {
                return Ok(OpOutcome::Skipped);
            }
            match copy_file_measured(&rp, &lp, on_bytes, timings) {
                Ok(_) => Ok(OpOutcome::Done),
                Err(e) if !timed(&mut timings.exists_ms, || rp.exists()) => {
                    let _ = e;
                    Ok(OpOutcome::Skipped)
                }
                Err(e) => Err(e),
            }
        }
        "delLocal" => {
            if !timed(&mut timings.exists_ms, || lp.exists()) {
                return Ok(OpOutcome::Skipped); // already gone == success
            }
            timed(&mut timings.delete_ms, || delete_file(&lp)).map(|_| OpOutcome::Done)
        }
        "delRemote" => {
            if !timed(&mut timings.exists_ms, || rp.exists()) {
                return Ok(OpOutcome::Skipped);
            }
            timed(&mut timings.delete_ms, || delete_file(&rp)).map(|_| OpOutcome::Done)
        }
        other => Err(format!("未知操作: {other}")),
    }
}

/// Apply operations in parallel with a bounded worker pool. `progress` is
/// invoked (from worker threads) as each op completes. Once `cancel` is set,
/// workers finish the file they are copying and take no further ops.
#[allow(dead_code)]
pub fn apply_ops(
    local: &Path,
    remote: &Path,
    ops: &[SyncOp],
    concurrency: usize,
    cancel: &AtomicBool,
    progress: &(dyn Fn(OpProgress) + Sync),
) -> SyncResult {
    apply_ops_with_transfer_progress(local, remote, ops, concurrency, cancel, progress, &|_| {})
}

pub fn apply_ops_with_transfer_progress(
    local: &Path,
    remote: &Path,
    ops: &[SyncOp],
    concurrency: usize,
    cancel: &AtomicBool,
    progress: &(dyn Fn(OpProgress) + Sync),
    transfer: &(dyn Fn(TransferProgress) + Sync),
) -> SyncResult {
    apply_ops_with_diagnostics(
        local,
        remote,
        ops,
        concurrency,
        cancel,
        progress,
        transfer,
        &Diagnostics::silent(),
    )
}

pub fn apply_ops_with_diagnostics(
    local: &Path,
    remote: &Path,
    ops: &[SyncOp],
    concurrency: usize,
    cancel: &AtomicBool,
    progress: &(dyn Fn(OpProgress) + Sync),
    transfer: &(dyn Fn(TransferProgress) + Sync),
    diagnostics: &Diagnostics,
) -> SyncResult {
    let started = Instant::now();
    let bytes_copied = AtomicU64::new(0);
    let on_bytes = |count| {
        bytes_copied.fetch_add(count, Ordering::Relaxed);
    };
    let total = ops.len();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let res = Mutex::new(SyncResult::default());
    let workers = concurrency.clamp(1, 32);
    let samples = Mutex::new(Vec::new());
    diagnostics.log(format!(
        "文件操作开始：任务={}，worker={}，不含后续快照/重新对比",
        total, workers
    ));

    std::thread::scope(|reporter| {
        let (stop, stopped) = mpsc::channel();
        let bytes = &bytes_copied;
        reporter.spawn(move || report_transfer(bytes, started, stopped, transfer));
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    if is_cancelled(cancel) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total {
                        break;
                    }
                    let op = &ops[i];
                    diagnostics.detail(format!("文件开始：op={} path={:?}", op.op, op.rel_path));
                    let operation_started = Instant::now();
                    let operation_bytes = AtomicU64::new(0);
                    let count_bytes = |count| {
                        operation_bytes.fetch_add(count, Ordering::Relaxed);
                        on_bytes(count);
                    };
                    let mut timings = OpTimings::default();
                    let result = exec_op(local, remote, op, &count_bytes, &mut timings);
                    let elapsed_ms = operation_started.elapsed().as_secs_f64() * 1000.0;
                    let copied = operation_bytes.load(Ordering::Relaxed);
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let (ok, skipped, err) = match &result {
                        Ok(OpOutcome::Done) => {
                            let mut g = res.lock().unwrap();
                            match op.op.as_str() {
                                "upload" => g.uploaded += 1,
                                "download" => g.downloaded += 1,
                                "delLocal" => g.deleted_local += 1,
                                "delRemote" => g.deleted_remote += 1,
                                _ => {}
                            }
                            g.done_paths.push(op.rel_path.clone());
                            (true, false, None)
                        }
                        Ok(OpOutcome::Skipped) => {
                            let mut g = res.lock().unwrap();
                            g.skipped += 1;
                            (true, true, None)
                        }
                        Err(e) => {
                            let mut g = res.lock().unwrap();
                            g.failed += 1;
                            g.errors.push(format!("{}: {}", op.rel_path, e));
                            (false, false, Some(e.clone()))
                        }
                    };
                    if diagnostics.enabled() {
                        let sample = OpSample {
                            path: op.rel_path.clone(),
                            op: op.op.clone(),
                            elapsed_ms,
                            bytes: copied,
                            timings,
                        };
                        diagnostics.detail(format!(
                            "文件结束：{} ok={} skipped={} error={err:?}",
                            sample.describe(),
                            ok,
                            skipped
                        ));
                        samples.lock().unwrap().push(sample);
                    }
                    progress(OpProgress {
                        index: d,
                        total,
                        rel_path: op.rel_path.clone(),
                        op: op.op.clone(),
                        ok,
                        skipped,
                        error: err,
                        elapsed_ms,
                        bytes_copied: copied,
                    });
                });
            }
        });
        let _ = stop.send(());
    });

    let mut res = res.into_inner().unwrap();
    res.bytes_copied = bytes_copied.load(Ordering::Relaxed);
    res.elapsed_seconds = started.elapsed().as_secs_f64();
    res.cancelled = done.load(Ordering::Relaxed) < total;
    if !res.cancelled {
        res.done_paths = Vec::new();
    }
    diagnostics.log(format!(
        "文件操作完成：wall={:.2}ms，已处理={}/{}，bytes={}，失败={}，跳过={}，cancelled={}",
        res.elapsed_seconds * 1000.0,
        done.load(Ordering::Relaxed),
        total,
        res.bytes_copied,
        res.failed,
        res.skipped,
        res.cancelled
    ));
    summarize_operations(samples.into_inner().unwrap(), diagnostics);
    res
}

struct OpSample {
    path: String,
    op: String,
    elapsed_ms: f64,
    bytes: u64,
    timings: OpTimings,
}

impl OpSample {
    fn describe(&self) -> String {
        format!(
            "op={} path={:?} bytes={} total={:.2}ms；分段(ms)：{}",
            self.op,
            self.path,
            self.bytes,
            self.elapsed_ms,
            self.timings.describe()
        )
    }
}

fn summarize_operations(mut samples: Vec<OpSample>, diagnostics: &Diagnostics) {
    if samples.is_empty() {
        return;
    }
    samples.sort_by(|a, b| a.elapsed_ms.total_cmp(&b.elapsed_ms));
    let count = samples.len();
    let percentile =
        |p: f64| samples[((count as f64 * p).ceil() as usize).saturating_sub(1)].elapsed_ms;
    let sum: f64 = samples.iter().map(|s| s.elapsed_ms).sum();
    let read_ms: f64 = samples.iter().map(|s| s.timings.read_ms).sum();
    let write_ms: f64 = samples.iter().map(|s| s.timings.write_ms).sum();
    let close_ms: f64 = samples.iter().map(|s| s.timings.close_ms).sum();
    let metadata_ms: f64 = samples
        .iter()
        .map(|s| {
            s.timings.exists_ms
                + s.timings.mkdir_ms
                + s.timings.open_ms
                + s.timings.metadata_ms
                + s.timings.create_ms
                + s.timings.permissions_ms
                + s.timings.mtime_ms
                + s.timings.rename_ms
                + s.timings.delete_ms
        })
        .sum();
    diagnostics.log(format!("文件耗时分布：n={count} avg={:.2}ms p50={:.2}ms p95={:.2}ms max={:.2}ms；rename回退={}次 mtime失败={}次", sum / count as f64, percentile(0.5), percentile(0.95), samples.last().unwrap().elapsed_ms, samples.iter().filter(|s| s.timings.rename_error.is_some()).count(), samples.iter().filter(|s| s.timings.mtime_error.is_some()).count()));
    diagnostics.log(format!("累计线程耗时：read={read_ms:.2}ms write={write_ms:.2}ms close={close_ms:.2}ms 元数据/目录/重命名={metadata_ms:.2}ms（并发项会重叠，不能当作实际总等待时间）"));
    for (index, sample) in samples.iter().rev().take(5).enumerate() {
        diagnostics.log(format!("慢文件TOP{}：{}", index + 1, sample.describe()));
    }
}

/// Rebuild a baseline snapshot from the *current* state of both sides: record
/// every file that is now equal on both sides. Used after a sync completes so
/// the next comparison has an accurate baseline.
#[allow(dead_code)]
pub fn build_snapshot(local: &Path, remote: &Path, ignore: &[String]) -> Snapshot {
    build_snapshot_with_diagnostics(local, remote, ignore, &Diagnostics::silent())
}

pub fn build_snapshot_with_diagnostics(
    local: &Path,
    remote: &Path,
    ignore: &[String],
    diagnostics: &Diagnostics,
) -> Snapshot {
    let started = Instant::now();
    diagnostics.log("快照重建开始：重新全量扫描两端（本地、远程依次扫描）");
    let mut noop = |_: usize| {};
    let never = AtomicBool::new(false);
    let local_map = scan(
        local,
        ignore,
        &never,
        &mut noop,
        diagnostics,
        "snapshot-local",
    )
    .map(|(m, _)| m)
    .unwrap_or_default();
    let remote_map = scan(
        remote,
        ignore,
        &never,
        &mut noop,
        diagnostics,
        "snapshot-remote",
    )
    .map(|(m, _)| m)
    .unwrap_or_default();

    let merge_started = Instant::now();
    let mut snap = Snapshot::default();
    for (rel, lm) in &local_map {
        if let Some(rm) = remote_map.get(rel) {
            if lm.size == rm.size && mtime_close(lm.mtime, rm.mtime) {
                snap.files.insert(
                    rel.clone(),
                    SnapEntry {
                        size: lm.size,
                        mtime: lm.mtime,
                        hash: None,
                    },
                );
            }
        }
    }
    diagnostics.log(format!("快照重建完成：local_files={} remote_files={} baseline_files={} merge={:.2}ms total={:.2}ms", local_map.len(), remote_map.len(), snap.files.len(), merge_started.elapsed().as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0));
    snap
}

// ----------------------------- content diff I/O -----------------------------

/// Max file size (bytes) allowed for content diff / edit in the UI.
pub const MAX_DIFF_BYTES: u64 = 5 * 1024 * 1024;

/// One side of a text-pair load result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTextSide {
    pub exists: bool,
    pub size: u64,
    /// UTF-8 text when readable; absent when missing / binary / too large / error.
    pub content: Option<String>,
    pub binary: bool,
    pub too_large: bool,
    pub error: Option<String>,
}

/// Local + remote text payload for a single relative path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTextPair {
    pub rel_path: String,
    pub local: FileTextSide,
    pub remote: FileTextSide,
}

/// Resolve `rel` under `root`, rejecting `..` and absolute paths.
fn resolve_under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() || rel.contains('\0') {
        return Err("无效相对路径".into());
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("相对路径不允许包含 .. 或绝对路径".into());
    }
    Ok(root.join(rel_path))
}

fn meta_of_file(path: &Path) -> Option<FileMeta> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(FileMeta {
        size: meta.len(),
        mtime: mtime_of(&meta),
    })
}

/// Heuristic: NUL in the first chunk => binary; otherwise require valid UTF-8.
fn looks_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        return true;
    }
    false
}

fn read_text_side(path: &Path) -> FileTextSide {
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileTextSide {
            exists: false,
            size: 0,
            content: None,
            binary: false,
            too_large: false,
            error: None,
        },
        Err(e) => FileTextSide {
            exists: false,
            size: 0,
            content: None,
            binary: false,
            too_large: false,
            error: Some(format!("无法访问: {e}")),
        },
        Ok(meta) => {
            if !meta.is_file() {
                return FileTextSide {
                    exists: true,
                    size: meta.len(),
                    content: None,
                    binary: false,
                    too_large: false,
                    error: Some("不是普通文件".into()),
                };
            }
            let size = meta.len();
            if size > MAX_DIFF_BYTES {
                return FileTextSide {
                    exists: true,
                    size,
                    content: None,
                    binary: false,
                    too_large: true,
                    error: None,
                };
            }
            match fs::read(path) {
                Err(e) => FileTextSide {
                    exists: true,
                    size,
                    content: None,
                    binary: false,
                    too_large: false,
                    error: Some(format!("读取失败: {e}")),
                },
                Ok(bytes) => {
                    if looks_binary(&bytes) {
                        return FileTextSide {
                            exists: true,
                            size,
                            content: None,
                            binary: true,
                            too_large: false,
                            error: None,
                        };
                    }
                    match String::from_utf8(bytes) {
                        Ok(content) => FileTextSide {
                            exists: true,
                            size,
                            content: Some(content),
                            binary: false,
                            too_large: false,
                            error: None,
                        },
                        Err(_) => FileTextSide {
                            exists: true,
                            size,
                            content: None,
                            binary: true,
                            too_large: false,
                            error: None,
                        },
                    }
                }
            }
        }
    }
}

/// Load local + remote text for content diff (size / binary gated).
pub fn read_text_pair(local: &Path, remote: &Path, rel: &str) -> Result<FileTextPair, String> {
    let lp = resolve_under(local, rel)?;
    let rp = resolve_under(remote, rel)?;
    Ok(FileTextPair {
        rel_path: rel.replace('\\', "/"),
        local: read_text_side(&lp),
        remote: read_text_side(&rp),
    })
}

/// Write UTF-8 text to one side (`"local"` | `"remote"`), creating parents.
pub fn write_text_file(root: &Path, rel: &str, content: &str) -> Result<(), String> {
    let path = resolve_under(root, rel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    // Atomic-ish write via temp + rename, with direct-write fallback for gvfs.
    let tmp: PathBuf = {
        let mut p = path.to_path_buf();
        let name = format!(
            ".{}.synctmp",
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        );
        p.set_file_name(name);
        p
    };
    fs::write(&tmp, content.as_bytes()).map_err(|e| format!("写入失败: {e}"))?;
    match fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let direct = fs::write(&path, content.as_bytes()).map_err(|e| format!("写入失败: {e}"));
            let _ = fs::remove_file(&tmp);
            direct
        }
    }
}

/// Re-compare a single relative path and return its DiffEntry (or None if gone).
pub fn compare_one(
    local: &Path,
    remote: &Path,
    rel: &str,
    opts: &CompareOptions,
    baseline: &Snapshot,
) -> Result<Option<DiffEntry>, String> {
    let rel = rel.replace('\\', "/");
    if is_ignored(&rel, &opts.ignore) {
        return Ok(None);
    }
    let lp = resolve_under(local, &rel)?;
    let rp = resolve_under(remote, &rel)?;
    let l = meta_of_file(&lp);
    let r = meta_of_file(&rp);
    let b = baseline.files.get(&rel);

    if l.is_none() && r.is_none() && !(opts.mode == "twoway" && b.is_some()) {
        return Ok(None);
    }

    let twoway = opts.mode == "twoway";
    let mirror_pull = opts.mode == "mirror_pull";
    let never = AtomicBool::new(false);
    let action = if twoway {
        decide_twoway(&lp, l.as_ref(), &rp, r.as_ref(), b, opts.use_hash, &never)
    } else if mirror_pull {
        decide_mirror_pull(&lp, l.as_ref(), &rp, r.as_ref(), opts.use_hash, b, &never)
    } else {
        decide_mirror(&lp, l.as_ref(), &rp, r.as_ref(), opts.use_hash, b, &never)
    };

    let action = match action {
        Some(a) => a,
        None => return Ok(None),
    };

    Ok(Some(DiffEntry {
        rel_path: rel,
        action,
        local_size: l.as_ref().map(|m| m.size),
        remote_size: r.as_ref().map(|m| m.size),
        local_mtime: l.as_ref().map(|m| m.mtime),
        remote_mtime: r.as_ref().map(|m| m.mtime),
        newer: newer_of(l.as_ref(), r.as_ref()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "syncui_test_{}_{}",
            tag,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(uniq);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    fn opts(mode: &str) -> CompareOptions {
        CompareOptions {
            use_hash: false,
            ignore: vec![],
            mode: mode.to_string(),
        }
    }

    fn find<'a>(res: &'a CompareResult, rel: &str) -> &'a DiffEntry {
        res.entries.iter().find(|e| e.rel_path == rel).unwrap()
    }

    #[test]
    fn mirror_detects_upload_and_delete_remote() {
        let local = tmp_dir("m_local");
        let remote = tmp_dir("m_remote");
        write(&local, "same.txt", "hello");
        write(&remote, "same.txt", "hello");
        write(&local, "new.txt", "fresh"); // upload
        write(&local, "mod.txt", "longer content");
        write(&remote, "mod.txt", "short"); // upload (overwrite)
        write(&remote, "extra.txt", "old"); // remote-only -> delete remote

        let res = compare(&local, &remote, &opts("mirror")).unwrap();
        assert_eq!(res.upload_count, 2);
        assert_eq!(res.delete_remote_count, 1);
        assert_eq!(res.same_count, 1);
        assert_eq!(find(&res, "new.txt").action, Action::Upload);
        assert_eq!(find(&res, "extra.txt").action, Action::DeleteRemote);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn mirror_pull_detects_download_and_delete_local() {
        let local = tmp_dir("mp_local");
        let remote = tmp_dir("mp_remote");
        write(&local, "same.txt", "hello");
        write(&remote, "same.txt", "hello");
        write(&remote, "new.txt", "fresh"); // download
        write(&local, "mod.txt", "short");
        write(&remote, "mod.txt", "longer content"); // download (overwrite)
        write(&local, "extra.txt", "old"); // local-only -> delete local

        let res = compare(&local, &remote, &opts("mirror_pull")).unwrap();
        assert_eq!(res.download_count, 2);
        assert_eq!(res.delete_local_count, 1);
        assert_eq!(res.same_count, 1);
        assert_eq!(find(&res, "new.txt").action, Action::Download);
        assert_eq!(find(&res, "extra.txt").action, Action::DeleteLocal);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn twoway_distinguishes_delete_from_new() {
        let local = tmp_dir("t_local");
        let remote = tmp_dir("t_remote");

        // Baseline says both had "shared.txt" and "gone.txt".
        write(&local, "shared.txt", "v1");
        write(&remote, "shared.txt", "v1");
        // "gone.txt" existed at baseline on both, but local deleted it.
        write(&remote, "gone.txt", "old");
        // A brand new remote file with no baseline -> should download.
        write(&remote, "fromremote.txt", "remote new");

        let mut base = Snapshot::default();
        for (rel, content) in [("shared.txt", "v1"), ("gone.txt", "old")] {
            let m = fs::metadata(remote.join(rel)).unwrap();
            base.files.insert(
                rel.to_string(),
                SnapEntry {
                    size: content.len() as u64,
                    mtime: mtime_of(&m),
                    hash: None,
                },
            );
        }

        let res = compare_with_progress(
            &local,
            &remote,
            &opts("twoway"),
            &base,
            &AtomicBool::new(false),
            &mut |_, _| {},
        )
        .unwrap();

        // local-deleted + remote-unchanged baseline file -> delete remote
        assert_eq!(find(&res, "gone.txt").action, Action::DeleteRemote);
        // remote-only with no baseline -> download (a new file, not a deletion)
        assert_eq!(find(&res, "fromremote.txt").action, Action::Download);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn twoway_flags_conflict_when_both_changed() {
        let local = tmp_dir("c_local");
        let remote = tmp_dir("c_remote");
        write(&local, "f.txt", "local edit longer");
        write(&remote, "f.txt", "remote edit");

        // Baseline differs from both current versions.
        let mut base = Snapshot::default();
        base.files.insert(
            "f.txt".to_string(),
            SnapEntry {
                size: 4,
                mtime: 0,
                hash: None,
            },
        );

        let res = compare_with_progress(
            &local,
            &remote,
            &opts("twoway"),
            &base,
            &AtomicBool::new(false),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(find(&res, "f.txt").action, Action::Conflict);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn apply_ops_uploads_downloads_and_builds_snapshot() {
        let local = tmp_dir("a_local");
        let remote = tmp_dir("a_remote");
        write(&local, "up/a.txt", "to upload");
        write(&remote, "down/b.txt", "to download");

        let ops = vec![
            SyncOp {
                rel_path: "up/a.txt".into(),
                op: "upload".into(),
            },
            SyncOp {
                rel_path: "down/b.txt".into(),
                op: "download".into(),
            },
        ];
        let res = apply_ops(&local, &remote, &ops, 4, &AtomicBool::new(false), &|_p| {});
        assert_eq!(res.uploaded, 1);
        assert_eq!(res.downloaded, 1);
        assert_eq!(res.failed, 0);
        assert_eq!(
            res.bytes_copied,
            ("to upload".len() + "to download".len()) as u64
        );
        assert!(res.elapsed_seconds > 0.0);
        assert_eq!(
            fs::read_to_string(remote.join("up/a.txt")).unwrap(),
            "to upload"
        );
        assert_eq!(
            fs::read_to_string(local.join("down/b.txt")).unwrap(),
            "to download"
        );

        // After sync both sides match; snapshot should record both files.
        let snap = build_snapshot(&local, &remote, &[]);
        assert!(snap.files.contains_key("up/a.txt"));
        assert!(snap.files.contains_key("down/b.txt"));

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    #[cfg(unix)]
    fn broken_symlink_is_skipped_not_fatal() {
        use std::os::unix::fs::symlink;
        let local = tmp_dir("s_local");
        let remote = tmp_dir("s_remote");
        write(&local, "real.txt", "data");
        symlink(local.join("nonexistent-target"), local.join("broken.link")).unwrap();

        let res = compare(&local, &remote, &opts("mirror")).unwrap();
        assert_eq!(res.upload_count, 1, "only the real file uploads");
        assert!(res.skipped_count >= 1, "broken symlink counted as skipped");

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn read_write_text_pair_and_compare_one() {
        let local = tmp_dir("d_local");
        let remote = tmp_dir("d_remote");
        write(&local, "cfg/a.yaml", "a: 1\n");
        write(&remote, "cfg/a.yaml", "a: 2\n");

        let pair = read_text_pair(&local, &remote, "cfg/a.yaml").unwrap();
        assert_eq!(pair.local.content.as_deref(), Some("a: 1\n"));
        assert_eq!(pair.remote.content.as_deref(), Some("a: 2\n"));
        assert!(!pair.local.binary && !pair.remote.too_large);

        write_text_file(&remote, "cfg/a.yaml", "a: 1\n").unwrap();
        let entry = compare_one(
            &local,
            &remote,
            "cfg/a.yaml",
            &opts("mirror"),
            &Snapshot::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(entry.action, Action::Same);

        // Binary / oversized rejection
        let mut big = vec![0u8; (MAX_DIFF_BYTES as usize) + 1];
        big[0] = b'x';
        fs::write(local.join("big.bin"), &big).unwrap();
        fs::write(remote.join("big.bin"), &big).unwrap();
        let big_pair = read_text_pair(&local, &remote, "big.bin").unwrap();
        assert!(big_pair.local.too_large);

        fs::write(local.join("nul.bin"), b"ok\0no").unwrap();
        fs::write(remote.join("nul.bin"), b"ok\0no").unwrap();
        let bin_pair = read_text_pair(&local, &remote, "nul.bin").unwrap();
        assert!(bin_pair.local.binary);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn atomic_copy_preserves_mtime_idempotent() {
        let local = tmp_dir("i_local");
        let remote = tmp_dir("i_remote");
        write(&local, "a/b/file.txt", "payload-123");

        let past = SystemTime::now() - Duration::from_secs(10_000);
        let ft = filetime::FileTime::from_system_time(past);
        filetime::set_file_mtime(local.join("a/b/file.txt"), ft).unwrap();

        copy_file_atomic(&local.join("a/b/file.txt"), &remote.join("a/b/file.txt")).unwrap();
        assert_eq!(
            fs::read_to_string(remote.join("a/b/file.txt")).unwrap(),
            "payload-123"
        );

        // Re-compare in mirror mode: should be Same (idempotent).
        let res = compare(&local, &remote, &opts("mirror")).unwrap();
        assert_eq!(res.same_count, 1);
        assert_eq!(res.upload_count, 0);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn apply_ops_skips_vanished_source() {
        let local = tmp_dir("v_local");
        let remote = tmp_dir("v_remote");
        // op references a file that does not exist (raced away before sync).
        let ops = vec![SyncOp {
            rel_path: "ghost.txt".into(),
            op: "upload".into(),
        }];
        let res = apply_ops(&local, &remote, &ops, 2, &AtomicBool::new(false), &|_p| {});
        assert_eq!(res.uploaded, 0);
        assert_eq!(res.failed, 0);
        assert_eq!(res.skipped, 1);
        assert!(!res.cancelled);
        assert_eq!(res.bytes_copied, 0);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn ignore_matches_whole_names_and_globs() {
        let ig = |pats: &[&str]| pats.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let user = ig(&["__pycache__", ".trash", "**/build", "**/log", "**/install"]);
        assert!(is_ignored("slam_ws/build", &user));
        assert!(is_ignored("slam_ws/build/pkg/CMakeCache.txt", &user));
        assert!(is_ignored("slam_ws/log/latest", &user));
        assert!(is_ignored("slam_ws/install", &user));
        assert!(is_ignored("a/__pycache__/x.pyc", &user));
        // Substrings of a name must not match.
        assert!(!is_ignored("slam_ws/src/dialog.cpp", &user));
        assert!(!is_ignored("slam_ws/rebuild.sh", &user));
        assert!(!is_ignored("slam_ws/src/install_deps.md", &user));

        assert!(is_ignored("build", &ig(&["build/"])));
        assert!(is_ignored("x/.$a.drawio.bkp", &ig(&["*.bkp"])));
        assert!(!is_ignored("x/a.bkp.txt", &ig(&["*.bkp"])));
        assert!(is_ignored("f1.txt", &ig(&["f?.txt"])));

        let anchored = ig(&["slam_ws/build"]);
        assert!(is_ignored("slam_ws/build/a", &anchored));
        assert!(!is_ignored("other/slam_ws/build/a", &anchored));
        assert!(is_ignored(
            "other/slam_ws/build/a",
            &ig(&["**/slam_ws/build"])
        ));
    }

    #[test]
    fn compare_omits_same_entries_but_counts_them() {
        let local = tmp_dir("same_local");
        let remote = tmp_dir("same_remote");
        write(&local, "same.txt", "hello");
        fs::copy(local.join("same.txt"), remote.join("same.txt")).unwrap();
        mirror_mtime(&local.join("same.txt"), &remote.join("same.txt"));
        write(&local, "new.txt", "fresh");

        let res = compare(&local, &remote, &opts("mirror")).unwrap();
        assert_eq!(res.same_count, 1);
        assert_eq!(res.upload_count, 1);
        assert!(res.entries.iter().all(|e| e.action != Action::Same));
        assert_eq!(res.entries.len(), 1);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn cancelled_compare_returns_error() {
        let local = tmp_dir("cc_local");
        let remote = tmp_dir("cc_remote");
        write(&local, "a.txt", "a");

        let cancel = AtomicBool::new(true);
        let res = compare_with_progress(
            &local,
            &remote,
            &opts("mirror"),
            &Snapshot::default(),
            &cancel,
            &mut |_, _| {},
        );
        assert_eq!(res.unwrap_err(), CANCELLED);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn cancelled_apply_ops_runs_nothing() {
        let local = tmp_dir("ca_local");
        let remote = tmp_dir("ca_remote");
        write(&local, "a.txt", "a");
        write(&local, "b.txt", "b");
        let ops: Vec<SyncOp> = ["a.txt", "b.txt"]
            .iter()
            .map(|r| SyncOp {
                rel_path: r.to_string(),
                op: "upload".into(),
            })
            .collect();

        let res = apply_ops(&local, &remote, &ops, 2, &AtomicBool::new(true), &|_p| {});
        assert!(res.cancelled);
        assert_eq!(res.uploaded, 0);
        assert_eq!(res.bytes_copied, 0);
        assert!(!remote.join("a.txt").exists());

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn tracked_copy_counts_chunks_and_keeps_destination_until_complete() {
        let local = tmp_dir("bytes_local");
        let remote = tmp_dir("bytes_remote");
        let source = local.join("payload.bin");
        let destination = remote.join("payload.bin");
        let payload: Vec<u8> = (0..(700 * 1024 + 17)).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &payload).unwrap();
        fs::write(&destination, b"original").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
        }
        let copied = AtomicU64::new(0);
        let chunks = AtomicUsize::new(0);
        copy_file_atomic_with_progress(&source, &destination, &|bytes| {
            copied.fetch_add(bytes, Ordering::Relaxed);
            chunks.fetch_add(1, Ordering::Relaxed);
            assert_eq!(fs::read(&destination).unwrap(), b"original");
        })
        .unwrap();
        assert_eq!(copied.load(Ordering::Relaxed), payload.len() as u64);
        assert!(chunks.load(Ordering::Relaxed) > 1);
        assert_eq!(fs::read(&destination).unwrap(), payload);
        assert!(!remote.join(".payload.bin.synctmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o750
            );
        }
        fs::remove_dir_all(&local).unwrap();
        fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn transfer_reports_live_rate_and_final_total_without_file_completion() {
        let bytes = AtomicU64::new(4096);
        let (stop, stopped) = mpsc::channel();
        let (events, received) = mpsc::channel();
        std::thread::scope(|s| {
            let bytes_ref = &bytes;
            s.spawn(move || {
                report_transfer(bytes_ref, Instant::now(), stopped, &|p| {
                    events.send(p).unwrap();
                })
            });
            let live = received.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!live.finished);
            assert_eq!(live.bytes_copied, 4096);
            assert!(live.bytes_per_second.is_finite() && live.bytes_per_second > 0.0);
            bytes.store(8192, Ordering::Relaxed);
            stop.send(()).unwrap();
            loop {
                let final_event = received.recv_timeout(Duration::from_secs(5)).unwrap();
                if final_event.finished {
                    assert_eq!(final_event.bytes_copied, 8192);
                    assert!(final_event.elapsed_seconds > 0.0);
                    assert_eq!(
                        final_event.bytes_per_second,
                        8192.0 / final_event.elapsed_seconds
                    );
                    break;
                }
            }
        });
    }

    #[test]
    fn transfer_total_aggregates_workers_and_excludes_deletes_and_skips() {
        let local = tmp_dir("total_local");
        let remote = tmp_dir("total_remote");
        write(&local, "up.bin", "upload payload");
        write(&remote, "down.bin", "download payload");
        write(&remote, "delete.bin", "not transferred");
        let ops: Vec<SyncOp> = [
            ("up.bin", "upload"),
            ("down.bin", "download"),
            ("delete.bin", "delRemote"),
            ("missing.bin", "upload"),
        ]
        .iter()
        .map(|(rel, op)| SyncOp {
            rel_path: (*rel).into(),
            op: (*op).into(),
        })
        .collect();
        let events = Mutex::new(Vec::new());
        let result = apply_ops_with_transfer_progress(
            &local,
            &remote,
            &ops,
            4,
            &AtomicBool::new(false),
            &|_| {},
            &|p| events.lock().unwrap().push(p),
        );
        assert_eq!(
            result.bytes_copied,
            ("upload payload".len() + "download payload".len()) as u64
        );
        assert_eq!(result.uploaded, 1);
        assert_eq!(result.downloaded, 1);
        assert_eq!(result.deleted_remote, 1);
        assert_eq!(result.skipped, 1);
        assert_eq!(result.failed, 0);
        let events = events.into_inner().unwrap();
        let last = events.last().unwrap();
        assert!(last.finished);
        assert_eq!(last.bytes_copied, result.bytes_copied);
        assert!(last.bytes_per_second.is_finite());
        fs::remove_dir_all(&local).unwrap();
        fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn diagnostics_cover_real_transfer_snapshot_and_compare_phases() {
        let local = tmp_dir("diagnostic_local");
        let remote = tmp_dir("diagnostic_remote");
        let logs = tmp_dir("diagnostic_logs");
        write(&local, "upload.txt", "from local");
        write(&remote, "download.txt", "from remote");
        let lines = std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured = lines.clone();
        let diagnostics = Diagnostics::new("integration", Some(&logs), move |line| {
            captured.lock().unwrap().push(line.to_string())
        });
        let ops = vec![
            SyncOp {
                rel_path: "upload.txt".into(),
                op: "upload".into(),
            },
            SyncOp {
                rel_path: "download.txt".into(),
                op: "download".into(),
            },
        ];
        let progress = Mutex::new(Vec::new());
        let result = apply_ops_with_diagnostics(
            &local,
            &remote,
            &ops,
            2,
            &AtomicBool::new(false),
            &|p| progress.lock().unwrap().push(p),
            &|_| {},
            &diagnostics,
        );
        assert_eq!(result.failed, 0);
        assert_eq!(result.bytes_copied, 21);
        let progress = progress.into_inner().unwrap();
        assert_eq!(progress.len(), 2);
        assert!(progress
            .iter()
            .all(|p| p.elapsed_ms.is_finite() && p.elapsed_ms >= 0.0 && p.bytes_copied > 0));
        let snapshot = build_snapshot_with_diagnostics(&local, &remote, &[], &diagnostics);
        assert_eq!(snapshot.files.len(), 2);
        let compared = compare_with_diagnostics(
            &local,
            &remote,
            &opts("twoway"),
            &snapshot,
            &AtomicBool::new(false),
            &mut |_, _| {},
            &diagnostics,
        )
        .unwrap();
        assert_eq!(compared.same_count, 2);
        assert!(compared.entries.is_empty());
        diagnostics.flush();
        let visible = lines.lock().unwrap().join("\n");
        assert!(visible.contains("文件耗时分布：n=2"));
        assert!(visible.contains("慢文件TOP1"));
        assert!(visible.contains("phase=snapshot-local"));
        assert!(visible.contains("phase=snapshot-remote"));
        assert!(visible.contains("快照重建完成"));
        assert!(visible.contains("decision/hash/sort="));
        let path = fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
        let full = fs::read_to_string(path).unwrap();
        assert_eq!(full.matches("文件开始：").count(), 2);
        assert_eq!(full.matches("文件结束：").count(), 2);
        assert!(full.contains("close="));
        assert!(full.contains("read="));
        assert!(full.contains("rename="));
        drop(diagnostics);
        fs::remove_dir_all(local).unwrap();
        fs::remove_dir_all(remote).unwrap();
        fs::remove_dir_all(logs).unwrap();
    }

    #[test]
    fn failed_rename_records_fallback_cause_without_destroying_destination() {
        let root = tmp_dir("diagnostic_rename");
        let source = root.join("source.txt");
        let destination = root.join("occupied");
        fs::write(&source, "payload").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep.txt"), "keep").unwrap();
        let mut timings = OpTimings::default();
        let bytes = AtomicU64::new(0);
        let result = copy_file_measured(
            &source,
            &destination,
            &|count| {
                bytes.fetch_add(count, Ordering::Relaxed);
            },
            &mut timings,
        );
        assert!(result.is_err());
        assert!(timings.rename_error.is_some());
        assert!(timings.fallback_ms.is_finite() && timings.fallback_ms >= 0.0);
        assert_eq!(bytes.load(Ordering::Relaxed), 7);
        assert_eq!(
            fs::read_to_string(destination.join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(!root.join(".occupied.synctmp").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
