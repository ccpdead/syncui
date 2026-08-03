//! Core directory comparison & sync engine.
//!
//! Works purely on filesystem paths, so it doesn't care whether a side is a
//! local folder or a mounted remote (sftp/smb/nfs) folder — both look like
//! plain paths to the OS.

use crate::snapshot::{SnapEntry, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

/// Modified-time comparison tolerance in seconds. Mounted filesystems
/// (sftp/smb) often report slightly different mtime precision than local
/// disks, so we don't treat sub-tolerance differences as a change.
const MTIME_TOLERANCE_SECS: i64 = 2;

/// Options that control how a comparison is performed.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Serialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
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

fn is_ignored(rel: &str, ignore: &[String]) -> bool {
    ignore
        .iter()
        .any(|pat| !pat.is_empty() && rel.contains(pat.as_str()))
}

fn mtime_close(a: i64, b: i64) -> bool {
    (a - b).abs() <= MTIME_TOLERANCE_SECS
}

/// Walk a directory and build a map of relative path -> file metadata.
/// Resilient: unreadable entries (broken links on gvfs/NFS, permission
/// errors) are skipped and counted. Symlinks are skipped entirely. Ignored
/// directories are pruned during the walk for speed.
fn scan(
    root: &Path,
    ignore: &[String],
    progress: &mut dyn FnMut(usize),
) -> Result<(BTreeMap<String, FileMeta>, usize), String> {
    let mut map = BTreeMap::new();
    let mut skipped: usize = 0;
    if !root.exists() {
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
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let ft = entry.file_type();
        if ft.is_symlink() || !ft.is_file() {
            if ft.is_symlink() {
                skipped += 1;
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
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
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
    Ok((map, skipped))
}

/// Compute a blake3 hash of a file's contents (chunked, low memory).
fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Hash a file, reusing the baseline's stored hash when size+mtime match
/// (incremental cache — avoids re-reading unchanged files over the network).
fn hash_cached(path: &Path, meta: &FileMeta, base: Option<&SnapEntry>) -> Option<String> {
    if let Some(b) = base {
        if b.size == meta.size && mtime_close(b.mtime, meta.mtime) {
            if let Some(h) = &b.hash {
                return Some(h.clone());
            }
        }
    }
    hash_file(path).ok()
}

/// Whether the local and remote files have identical content.
fn same_content(
    lp: &Path,
    l: &FileMeta,
    rp: &Path,
    r: &FileMeta,
    use_hash: bool,
    base: Option<&SnapEntry>,
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
    match (hash_cached(lp, l, base), hash_cached(rp, r, base)) {
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
        (Some(l), Some(r)) => Some(if l.mtime >= r.mtime { "local" } else { "remote" }.into()),
        (Some(_), None) => Some("local".into()),
        (None, Some(_)) => Some("remote".into()),
        (None, None) => None,
    }
}

/// Convenience wrapper used by tests and as a stable API (no progress, empty
/// baseline => behaves as a plain two-way / mirror compare).
#[allow(dead_code)]
pub fn compare(local: &Path, remote: &Path, opts: &CompareOptions) -> Result<CompareResult, String> {
    compare_with_progress(local, remote, opts, &Snapshot::default(), &mut |_, _| {})
}

/// Compare two directory trees against a baseline snapshot and produce a
/// structured diff. `progress(phase, count)` is called periodically during
/// scanning ("local"/"remote").
pub fn compare_with_progress(
    local: &Path,
    remote: &Path,
    opts: &CompareOptions,
    baseline: &Snapshot,
    progress: &mut dyn FnMut(&str, usize),
) -> Result<CompareResult, String> {
    let (local_map, local_skipped) = scan(local, &opts.ignore, &mut |n| progress("local", n))?;
    let (remote_map, remote_skipped) = scan(remote, &opts.ignore, &mut |n| progress("remote", n))?;

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

    for rel in keys {
        let l = local_map.get(rel);
        let r = remote_map.get(rel);
        let b = baseline.files.get(rel);
        let lp = local.join(rel);
        let rp = remote.join(rel);

        let action = if twoway {
            decide_twoway(&lp, l, &rp, r, b, opts.use_hash)
        } else if mirror_pull {
            decide_mirror_pull(&lp, l, &rp, r, opts.use_hash, b)
        } else {
            decide_mirror(&lp, l, &rp, r, opts.use_hash, b)
        };

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
            Action::Same => counts[5] += 1,
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

    Ok(CompareResult {
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
) -> Option<Action> {
    match (l, r) {
        (Some(l), Some(r)) => {
            if same_content(lp, l, rp, r, use_hash, base) {
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
) -> Option<Action> {
    match (l, r) {
        (Some(l), Some(r)) => {
            if same_content(lp, l, rp, r, use_hash, base) {
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
) -> Option<Action> {
    match (l, r) {
        (Some(l), Some(r)) => {
            if same_content(lp, l, rp, r, use_hash, b) {
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
fn mirror_mtime(src: &Path, dst: &Path) {
    if let Ok(meta) = fs::metadata(src) {
        if let Ok(mtime) = meta.modified() {
            let ft = filetime::FileTime::from_system_time(mtime);
            let _ = filetime::set_file_mtime(dst, ft);
        }
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
pub fn copy_file_atomic(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
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

    fs::copy(src, &tmp).map_err(|e| format!("复制失败: {e}"))?;
    mirror_mtime(src, &tmp);

    match fs::rename(&tmp, dst) {
        Ok(_) => Ok(()),
        Err(_) => {
            // Fallback for mounts where rename is unreliable (gvfs/NFS).
            let direct = fs::copy(src, dst)
                .map(|_| ())
                .map_err(|e| format!("复制失败: {e}"));
            let _ = fs::remove_file(&tmp);
            if direct.is_ok() {
                mirror_mtime(src, dst);
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
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOp {
    pub rel_path: String,
    /// "upload" | "download" | "delLocal" | "delRemote"
    pub op: String,
}

/// Progress emitted per operation during a sync run.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpProgress {
    pub index: usize,
    pub total: usize,
    pub rel_path: String,
    pub op: String,
    pub ok: bool,
    pub skipped: bool,
    pub error: Option<String>,
}

/// Summary returned when a sync run completes.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    /// Items whose file vanished between compare and sync (raced away).
    pub skipped: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

/// Outcome of a single op: either performed, or skipped because the file
/// disappeared between comparison and execution (a benign race).
enum OpOutcome {
    Done,
    Skipped,
}

fn exec_op(local: &Path, remote: &Path, op: &SyncOp) -> Result<OpOutcome, String> {
    let lp = local.join(&op.rel_path);
    let rp = remote.join(&op.rel_path);
    match op.op.as_str() {
        "upload" => {
            if !lp.exists() {
                return Ok(OpOutcome::Skipped);
            }
            match copy_file_atomic(&lp, &rp) {
                Ok(_) => Ok(OpOutcome::Done),
                // If it failed because the source raced away, skip quietly.
                Err(e) if !lp.exists() => {
                    let _ = e;
                    Ok(OpOutcome::Skipped)
                }
                Err(e) => Err(e),
            }
        }
        "download" => {
            if !rp.exists() {
                return Ok(OpOutcome::Skipped);
            }
            match copy_file_atomic(&rp, &lp) {
                Ok(_) => Ok(OpOutcome::Done),
                Err(e) if !rp.exists() => {
                    let _ = e;
                    Ok(OpOutcome::Skipped)
                }
                Err(e) => Err(e),
            }
        }
        "delLocal" => {
            if !lp.exists() {
                return Ok(OpOutcome::Skipped); // already gone == success
            }
            delete_file(&lp).map(|_| OpOutcome::Done)
        }
        "delRemote" => {
            if !rp.exists() {
                return Ok(OpOutcome::Skipped);
            }
            delete_file(&rp).map(|_| OpOutcome::Done)
        }
        other => Err(format!("未知操作: {other}")),
    }
}

/// Apply operations in parallel with a bounded worker pool. `progress` is
/// invoked (from worker threads) as each op completes.
pub fn apply_ops(
    local: &Path,
    remote: &Path,
    ops: &[SyncOp],
    concurrency: usize,
    progress: &(dyn Fn(OpProgress) + Sync),
) -> SyncResult {
    let total = ops.len();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let res = Mutex::new(SyncResult::default());
    let workers = concurrency.clamp(1, 32);

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= total {
                    break;
                }
                let op = &ops[i];
                let result = exec_op(local, remote, op);
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
                progress(OpProgress {
                    index: d,
                    total,
                    rel_path: op.rel_path.clone(),
                    op: op.op.clone(),
                    ok,
                    skipped,
                    error: err,
                });
            });
        }
    });

    res.into_inner().unwrap()
}

/// Rebuild a baseline snapshot from the *current* state of both sides: record
/// every file that is now equal on both sides. Used after a sync completes so
/// the next comparison has an accurate baseline.
pub fn build_snapshot(local: &Path, remote: &Path, ignore: &[String]) -> Snapshot {
    let mut noop = |_: usize| {};
    let local_map = scan(local, ignore, &mut noop)
        .map(|(m, _)| m)
        .unwrap_or_default();
    let remote_map = scan(remote, ignore, &mut noop)
        .map(|(m, _)| m)
        .unwrap_or_default();

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
    snap
}

// ----------------------------- content diff I/O -----------------------------

/// Max file size (bytes) allowed for content diff / edit in the UI.
pub const MAX_DIFF_BYTES: u64 = 5 * 1024 * 1024;

/// One side of a text-pair load result.
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
        || rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
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
    let action = if twoway {
        decide_twoway(&lp, l.as_ref(), &rp, r.as_ref(), b, opts.use_hash)
    } else if mirror_pull {
        decide_mirror_pull(&lp, l.as_ref(), &rp, r.as_ref(), opts.use_hash, b)
    } else {
        decide_mirror(&lp, l.as_ref(), &rp, r.as_ref(), opts.use_hash, b)
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

        let res =
            compare_with_progress(&local, &remote, &opts("twoway"), &base, &mut |_, _| {}).unwrap();

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

        let res =
            compare_with_progress(&local, &remote, &opts("twoway"), &base, &mut |_, _| {}).unwrap();
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
        let res = apply_ops(&local, &remote, &ops, 4, &|_p| {});
        assert_eq!(res.uploaded, 1);
        assert_eq!(res.downloaded, 1);
        assert_eq!(res.failed, 0);
        assert_eq!(fs::read_to_string(remote.join("up/a.txt")).unwrap(), "to upload");
        assert_eq!(fs::read_to_string(local.join("down/b.txt")).unwrap(), "to download");

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
        let res = apply_ops(&local, &remote, &ops, 2, &|_p| {});
        assert_eq!(res.uploaded, 0);
        assert_eq!(res.failed, 0);
        assert_eq!(res.skipped, 1);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }
}
