//! Core directory comparison & sync engine.
//!
//! Works purely on filesystem paths, so it doesn't care whether a side is a
//! local folder or a mounted remote (sftp/smb) folder — both look like plain
//! paths to the OS.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
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
    /// When true, files with equal size are additionally compared by content
    /// hash. Slower but accurate (also reads the remote over the network).
    #[serde(default)]
    pub use_hash: bool,
    /// Glob-ish substrings to ignore (simple `contains` match on the relative
    /// path). e.g. ".git", "node_modules", ".tmp".
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl Default for CompareOptions {
    fn default() -> Self {
        CompareOptions {
            use_hash: false,
            ignore: Vec::new(),
        }
    }
}

/// A single scanned file (directories are tracked separately during sync).
#[derive(Debug, Clone)]
struct FileMeta {
    size: u64,
    mtime: i64,
}

/// The status of a single relative path when comparing local vs remote.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    /// Exists locally but not remotely.
    New,
    /// Exists on both sides but differs.
    Modified,
    /// Exists remotely but not locally.
    Deleted,
    /// Identical on both sides.
    Same,
}

/// One row in the diff result presented to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffEntry {
    pub rel_path: String,
    pub status: DiffStatus,
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
    pub local_mtime: Option<i64>,
    pub remote_mtime: Option<i64>,
    /// "local" | "remote" | null — which side is newer (for Modified rows).
    pub newer: Option<String>,
}

/// Aggregate result returned to the UI after a compare.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
    pub entries: Vec<DiffEntry>,
    pub new_count: usize,
    pub modified_count: usize,
    pub deleted_count: usize,
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
    ignore.iter().any(|pat| !pat.is_empty() && rel.contains(pat.as_str()))
}

/// Walk a directory and build a map of relative path -> file metadata.
/// Only regular files are recorded; directories are recreated implicitly
/// during sync from the file paths.
///
/// Resilient by design: individual entries that can't be read (broken
/// symlinks on NFS/gvfs, permission errors, files that vanished mid-scan)
/// are skipped and counted rather than aborting the whole scan. Symlinks
/// are skipped entirely — a file sync tool copies real files, and following
/// links on network mounts is a common source of errors.
fn scan(
    root: &Path,
    ignore: &[String],
) -> Result<(BTreeMap<String, FileMeta>, usize), String> {
    let mut map = BTreeMap::new();
    let mut skipped: usize = 0;
    if !root.exists() {
        return Err(format!("路径不存在: {}", root.display()));
    }
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                // Unreadable directory entry (e.g. broken link on gvfs/NFS).
                skipped += 1;
                continue;
            }
        };
        let ft = entry.file_type();
        // Skip symlinks (including broken ones) and anything not a regular file.
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
        // Normalize separators to forward slash for cross-platform stability.
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_ignored(&rel_str, ignore) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                // Couldn't stat this file; skip rather than fail the scan.
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
    }
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

/// Compare two directory trees and produce a structured diff.
pub fn compare(local: &Path, remote: &Path, opts: &CompareOptions) -> Result<CompareResult, String> {
    let (local_map, local_skipped) = scan(local, &opts.ignore)?;
    let (remote_map, remote_skipped) = scan(remote, &opts.ignore)?;

    let mut entries: Vec<DiffEntry> = Vec::new();
    let (mut new_count, mut modified_count, mut deleted_count, mut same_count) = (0, 0, 0, 0);

    for (rel, lmeta) in &local_map {
        match remote_map.get(rel) {
            None => {
                new_count += 1;
                entries.push(DiffEntry {
                    rel_path: rel.clone(),
                    status: DiffStatus::New,
                    local_size: Some(lmeta.size),
                    remote_size: None,
                    local_mtime: Some(lmeta.mtime),
                    remote_mtime: None,
                    newer: Some("local".into()),
                });
            }
            Some(rmeta) => {
                let size_diff = lmeta.size != rmeta.size;
                let mtime_diff = (lmeta.mtime - rmeta.mtime).abs() > MTIME_TOLERANCE_SECS;

                let mut changed = size_diff || mtime_diff;
                // If sizes match but mtime differs, an optional hash check can
                // confirm whether contents actually changed.
                if changed && !size_diff && opts.use_hash {
                    let lh = hash_file(&local.join(rel))?;
                    let rh = hash_file(&remote.join(rel))?;
                    changed = lh != rh;
                }

                if changed {
                    modified_count += 1;
                    let newer = if lmeta.mtime >= rmeta.mtime {
                        "local"
                    } else {
                        "remote"
                    };
                    entries.push(DiffEntry {
                        rel_path: rel.clone(),
                        status: DiffStatus::Modified,
                        local_size: Some(lmeta.size),
                        remote_size: Some(rmeta.size),
                        local_mtime: Some(lmeta.mtime),
                        remote_mtime: Some(rmeta.mtime),
                        newer: Some(newer.into()),
                    });
                } else {
                    same_count += 1;
                    entries.push(DiffEntry {
                        rel_path: rel.clone(),
                        status: DiffStatus::Same,
                        local_size: Some(lmeta.size),
                        remote_size: Some(rmeta.size),
                        local_mtime: Some(lmeta.mtime),
                        remote_mtime: Some(rmeta.mtime),
                        newer: None,
                    });
                }
            }
        }
    }

    for (rel, rmeta) in &remote_map {
        if !local_map.contains_key(rel) {
            deleted_count += 1;
            entries.push(DiffEntry {
                rel_path: rel.clone(),
                status: DiffStatus::Deleted,
                local_size: None,
                remote_size: Some(rmeta.size),
                local_mtime: None,
                remote_mtime: Some(rmeta.mtime),
                newer: Some("remote".into()),
            });
        }
    }

    // Stable, human-friendly ordering: by status group, then path.
    entries.sort_by(|a, b| {
        fn rank(s: &DiffStatus) -> u8 {
            match s {
                DiffStatus::New => 0,
                DiffStatus::Modified => 1,
                DiffStatus::Deleted => 2,
                DiffStatus::Same => 3,
            }
        }
        rank(&a.status)
            .cmp(&rank(&b.status))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });

    Ok(CompareResult {
        entries,
        new_count,
        modified_count,
        deleted_count,
        same_count,
    })
}

/// Copy a single file local->remote atomically: write to a temp file in the
/// destination directory, fsync-rename into place, then mirror the source
/// mtime so subsequent comparisons stay idempotent.
pub fn copy_file_atomic(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let tmp: PathBuf = {
        let mut p = dst.to_path_buf();
        let name = format!(
            ".{}.synctmp",
            dst.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
        );
        p.set_file_name(name);
        p
    };

    fs::copy(src, &tmp).map_err(|e| format!("复制失败: {e}"))?;

    // Preserve source mtime on the temp file before renaming.
    if let Ok(meta) = fs::metadata(src) {
        if let Ok(mtime) = meta.modified() {
            let ft = filetime::FileTime::from_system_time(mtime);
            let _ = filetime::set_file_mtime(&tmp, ft);
        }
    }

    fs::rename(&tmp, dst).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("重命名失败: {e}")
    })?;
    Ok(())
}

/// Delete a file (used for "deleted" entries when the user opts in).
pub fn delete_file(path: &Path) -> Result<(), String> {
    fs::remove_file(path).map_err(|e| format!("删除失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, Duration};

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

    #[test]
    fn detects_new_modified_deleted_same() {
        let local = tmp_dir("local");
        let remote = tmp_dir("remote");

        // same on both
        write(&local, "same.txt", "hello");
        write(&remote, "same.txt", "hello");
        // new (local only)
        write(&local, "sub/new.txt", "fresh");
        // modified (different size)
        write(&local, "mod.txt", "longer content here");
        write(&remote, "mod.txt", "short");
        // deleted (remote only)
        write(&remote, "gone.txt", "old");

        let opts = CompareOptions::default();
        let res = compare(&local, &remote, &opts).unwrap();

        assert_eq!(res.new_count, 1, "new");
        assert_eq!(res.modified_count, 1, "modified");
        assert_eq!(res.deleted_count, 1, "deleted");
        assert_eq!(res.same_count, 1, "same");

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn ignore_filter_excludes_paths() {
        let local = tmp_dir("local_ig");
        let remote = tmp_dir("remote_ig");
        write(&local, ".git/config", "x");
        write(&local, "keep.txt", "y");

        let opts = CompareOptions {
            use_hash: false,
            ignore: vec![".git".to_string()],
        };
        let res = compare(&local, &remote, &opts).unwrap();
        // only keep.txt should be seen as new; .git/config ignored
        assert_eq!(res.new_count, 1);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn atomic_copy_preserves_content_and_mtime() {
        let local = tmp_dir("local_cp");
        let remote = tmp_dir("remote_cp");
        write(&local, "a/b/file.txt", "payload-123");

        // set a known mtime in the past on the source
        let past = SystemTime::now() - Duration::from_secs(10_000);
        let ft = filetime::FileTime::from_system_time(past);
        filetime::set_file_mtime(local.join("a/b/file.txt"), ft).unwrap();

        let src = local.join("a/b/file.txt");
        let dst = remote.join("a/b/file.txt");
        copy_file_atomic(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "payload-123");

        let s_m = mtime_of(&fs::metadata(&src).unwrap());
        let d_m = mtime_of(&fs::metadata(&dst).unwrap());
        assert!((s_m - d_m).abs() <= MTIME_TOLERANCE_SECS, "mtime preserved");

        // After copy, a compare should report this file as Same (idempotent).
        let res = compare(&local, &remote, &CompareOptions::default()).unwrap();
        assert_eq!(res.same_count, 1);
        assert_eq!(res.new_count, 0);
        assert_eq!(res.modified_count, 0);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }

    #[test]
    fn hash_mode_treats_touched_but_identical_as_same() {
        let local = tmp_dir("local_h");
        let remote = tmp_dir("remote_h");
        write(&local, "f.txt", "identical");
        write(&remote, "f.txt", "identical");

        // Make mtimes differ beyond tolerance but contents identical.
        let ft_old = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(50_000),
        );
        filetime::set_file_mtime(remote.join("f.txt"), ft_old).unwrap();

        // Without hash: counts as modified due to mtime diff.
        let plain = compare(&local, &remote, &CompareOptions::default()).unwrap();
        assert_eq!(plain.modified_count, 1);

        // With hash: identical content -> Same.
        let hashed = compare(
            &local,
            &remote,
            &CompareOptions { use_hash: true, ignore: vec![] },
        )
        .unwrap();
        assert_eq!(hashed.same_count, 1);
        assert_eq!(hashed.modified_count, 0);

        fs::remove_dir_all(&local).ok();
        fs::remove_dir_all(&remote).ok();
    }
}
