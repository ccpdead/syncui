//! Filesystem-independent inventories and operations shared by desktop and agent.
use crate::engine::{
    self, Action, CompareOptions, CompareResult, DiffEntry, FileTextSide, SyncOp, CANCELLED,
};
use crate::snapshot::{SnapEntry, Snapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, UNIX_EPOCH},
};

pub type Inventory = BTreeMap<String, Stamp>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Stamp {
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: u32,
    pub ctime: i64,
    pub ctime_ns: i64,
    pub mode: u32,
}
impl Stamp {
    pub fn from_meta(m: &fs::Metadata) -> Result<Self, String> {
        let t = m
            .modified()
            .map_err(|e| e.to_string())?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "不支持 1970 年前的修改时间")?;
        #[cfg(unix)]
        let (ctime, ctime_ns, mode) = {
            use std::os::unix::fs::MetadataExt;
            (m.ctime(), m.ctime_nsec(), m.mode() & 0o777)
        };
        #[cfg(not(unix))]
        let (ctime, ctime_ns, mode) = (0, 0, 0);
        Ok(Self {
            size: m.len(),
            mtime: t.as_secs() as i64,
            mtime_ns: t.subsec_nanos(),
            ctime,
            ctime_ns,
            mode,
        })
    }
    pub fn equivalent(&self, other: &Self) -> bool {
        self.size == other.size && engine::mtime_close(self.mtime, other.mtime)
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub root: String,
    pub files: Inventory,
    pub excluded: Vec<String>,
    pub skipped: usize,
    pub elapsed_ms: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextFile {
    pub side: FileTextSide,
    pub stamp: Option<Stamp>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Directory {
    pub path: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}
pub fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    let p = fs::canonicalize(root).map_err(|e| format!("目录无法访问: {e}"))?;
    if !p.is_dir() {
        return Err("根路径必须是目录".into());
    }
    Ok(p)
}
pub fn validate_rel(rel: &str) -> Result<(), String> {
    if rel.is_empty()
        || rel.contains('\0')
        || rel
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        || !Path::new(rel)
            .components()
            .all(|p| matches!(p, Component::Normal(_)))
    {
        return Err("无效相对路径".into());
    }
    Ok(())
}
pub fn safe_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    validate_rel(rel)?;
    let mut p = canonical_root(root)?;
    let target = p.join(rel);
    for part in Path::new(rel).components() {
        p.push(part);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("拒绝符号链接路径: {rel:?}"))
            }
            Ok(m) if p != target && !m.is_dir() => return Err("父路径不是目录".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(p)
}
pub fn stat(root: &Path, rel: &str) -> Result<Option<Stamp>, String> {
    let p = safe_path(root, rel)?;
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_file() => Stamp::from_meta(&m).map(Some),
        Ok(_) => Err(format!("不是普通文件: {rel:?}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
pub fn scan(
    root: &Path,
    ignore: &[String],
    cancel: &AtomicBool,
    batch: &mut dyn FnMut(&[(String, Stamp)]),
) -> Result<Manifest, String> {
    let started = Instant::now();
    let root = canonical_root(root)?;
    let root_text = root.to_str().ok_or("根路径不是 UTF-8")?.to_owned();
    let mut result = Manifest {
        root: root_text,
        ..Default::default()
    };
    let mut entries = Vec::new();
    let walk = walkdir::WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.path()
                .strip_prefix(&root)
                .ok()
                .and_then(|r| r.to_str())
                .map(|r| !engine::is_ignored(r, ignore) && !r.ends_with(".synctmp"))
                .unwrap_or(true)
        });
    for e in walk {
        check_cancel(cancel)?;
        let e = e.map_err(|e| format!("扫描不完整: {e}"))?;
        let rel = e
            .path()
            .strip_prefix(&root)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("发现非 UTF-8 路径，扫描已停止")?
            .to_owned();
        if rel.is_empty() {
            continue;
        }
        let ty = e.file_type();
        if ty.is_dir() {
            continue;
        }
        if !ty.is_file() {
            result.skipped += 1;
            result.excluded.push(rel);
            continue;
        }
        let m = e.metadata().map_err(|e| format!("扫描不完整: {e}"))?;
        let stamp = Stamp::from_meta(&m)?;
        result.files.insert(rel.clone(), stamp.clone());
        entries.push((rel, stamp));
        if entries.len() >= 256 {
            batch(&entries);
            entries.clear()
        }
    }
    if !entries.is_empty() {
        batch(&entries)
    }
    result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.;
    Ok(result)
}
pub fn hash(
    root: &Path,
    rel: &str,
    expected: &Stamp,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if stat(root, rel)?.as_ref() != Some(expected) {
        return Err(format!("哈希前文件已变化: {rel:?}"));
    }
    let mut f = fs::File::open(safe_path(root, rel)?).map_err(|e| e.to_string())?;
    let mut h = blake3::Hasher::new();
    let mut b = [0u8; 65536];
    loop {
        check_cancel(cancel)?;
        let n = f.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    if stat(root, rel)?.as_ref() != Some(expected) {
        return Err(format!("哈希过程中文件已变化: {rel:?}"));
    }
    Ok(h.finalize().to_hex().to_string())
}
pub fn read_text(root: &Path, rel: &str) -> Result<TextFile, String> {
    let stamp = stat(root, rel)?;
    let mut side = FileTextSide {
        exists: stamp.is_some(),
        size: stamp.as_ref().map(|s| s.size).unwrap_or(0),
        content: None,
        binary: false,
        too_large: false,
        error: None,
    };
    if let Some(s) = &stamp {
        if s.size > engine::MAX_DIFF_BYTES {
            side.too_large = true
        } else {
            let f = fs::File::open(safe_path(root, rel)?).map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            f.take(engine::MAX_DIFF_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() as u64 > engine::MAX_DIFF_BYTES {
                return Err("文件读取期间超过大小限制".into());
            }
            side.binary = bytes.contains(&0);
            if !side.binary {
                match String::from_utf8(bytes) {
                    Ok(t) => side.content = Some(t),
                    Err(_) => side.binary = true,
                }
            }
            if stat(root, rel)? != stamp {
                return Err("读取期间文件已变化，请重新打开".into());
            }
        }
    }
    Ok(TextFile { side, stamp })
}
pub fn write_text(
    root: &Path,
    rel: &str,
    content: &str,
    expected: &Option<Stamp>,
) -> Result<Stamp, String> {
    if content.len() as u64 > engine::MAX_DIFF_BYTES {
        return Err("文本超过 5 MiB".into());
    }
    if &stat(root, rel)? != expected {
        return Err("文件已变化，请重新读取再保存".into());
    }
    let dst = safe_path(root, rel)?;
    let parent = dst.parent().ok_or("没有父目录")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(
        ".syncui-{}-{}.synctmp",
        std::process::id(),
        UNIX_EPOCH.elapsed().unwrap_or_default().as_nanos()
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(
                expected.as_ref().map(|s| s.mode).unwrap_or(0o600),
            ))
            .map_err(|e| e.to_string())?;
        }
        f.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        if &stat(root, rel)? != expected {
            return Err("保存前文件已变化".into());
        }
        fs::rename(&tmp, &dst).map_err(|e| e.to_string())?;
        stat(root, rel)?.ok_or("保存后文件不存在".into())
    })();
    let _ = fs::remove_file(tmp);
    result
}
pub fn delete(root: &Path, rel: &str, expected: &Stamp) -> Result<(), String> {
    if stat(root, rel)?.as_ref() != Some(expected) {
        return Err("删除前文件已变化".into());
    }
    fs::remove_file(safe_path(root, rel)?).map_err(|e| e.to_string())
}
pub fn browse(root: &Path) -> Result<Directory, String> {
    let p = canonical_root(root)?;
    let mut children = Vec::new();
    for e in fs::read_dir(&p).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        if e.file_type().map_err(|e| e.to_string())?.is_dir() {
            children.push(
                e.file_name()
                    .into_string()
                    .map_err(|_| "发现非 UTF-8 目录")?,
            )
        }
    }
    children.sort();
    Ok(Directory {
        path: p.to_str().ok_or("非 UTF-8 路径")?.into(),
        parent: p.parent().and_then(|p| p.to_str()).map(str::to_owned),
        children,
    })
}

pub trait Endpoint: Sync + Send {
    fn scan(
        &self,
        ignore: &[String],
        cancel: &AtomicBool,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Manifest, String>;
    fn stats(
        &self,
        paths: &[String],
        cancel: &AtomicBool,
    ) -> Result<BTreeMap<String, Option<Stamp>>, String>;
    fn hashes(
        &self,
        files: &Inventory,
        cancel: &AtomicBool,
    ) -> Result<BTreeMap<String, String>, String>;
    fn read(&self, path: &str) -> Result<TextFile, String>;
    fn write(&self, path: &str, content: &str, expected: &Option<Stamp>) -> Result<Stamp, String>;
    fn delete(&self, path: &str, expected: &Stamp) -> Result<(), String>;
    fn delete_cancelled(
        &self,
        path: &str,
        expected: &Stamp,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        check_cancel(cancel)?;
        self.delete(path, expected)
    }
}
pub struct LocalEndpoint(pub PathBuf);
impl Endpoint for LocalEndpoint {
    fn scan(
        &self,
        ignore: &[String],
        c: &AtomicBool,
        p: &(dyn Fn(usize) + Sync),
    ) -> Result<Manifest, String> {
        let mut n = 0;
        scan(&self.0, ignore, c, &mut |b| {
            n += b.len();
            p(n)
        })
    }
    fn stats(
        &self,
        paths: &[String],
        c: &AtomicBool,
    ) -> Result<BTreeMap<String, Option<Stamp>>, String> {
        paths
            .iter()
            .map(|p| {
                check_cancel(c)?;
                Ok((p.clone(), stat(&self.0, p)?))
            })
            .collect()
    }
    fn hashes(
        &self,
        files: &Inventory,
        c: &AtomicBool,
    ) -> Result<BTreeMap<String, String>, String> {
        files
            .iter()
            .map(|(p, s)| Ok((p.clone(), hash(&self.0, p, s, c)?)))
            .collect()
    }
    fn read(&self, p: &str) -> Result<TextFile, String> {
        read_text(&self.0, p)
    }
    fn write(&self, p: &str, t: &str, e: &Option<Stamp>) -> Result<Stamp, String> {
        write_text(&self.0, p, t, e)
    }
    fn delete(&self, p: &str, e: &Stamp) -> Result<(), String> {
        delete(&self.0, p, e)
    }
}

pub fn hash_candidates(l: &Manifest, r: &Manifest, enabled: bool) -> Vec<String> {
    if !enabled {
        return vec![];
    }
    l.files
        .iter()
        .filter_map(|(p, a)| {
            r.files
                .get(p)
                .filter(|b| a.size == b.size && !engine::mtime_close(a.mtime, b.mtime))
                .map(|_| p.clone())
        })
        .collect()
}
pub fn equal(l: &Stamp, r: &Stamp, lh: Option<&String>, rh: Option<&String>) -> bool {
    l.equivalent(r) || (l.size == r.size && lh.is_some() && lh == rh)
}
fn changed(m: &Stamp, b: Option<&SnapEntry>) -> bool {
    b.map(|b| b.size != m.size || !engine::mtime_close(b.mtime, m.mtime))
        .unwrap_or(true)
}
pub fn decision(
    l: Option<&Stamp>,
    r: Option<&Stamp>,
    b: Option<&SnapEntry>,
    mode: &str,
    same: bool,
) -> Option<Action> {
    if l.is_some() && r.is_some() && same {
        return Some(Action::Same);
    }
    match (l, r) {
        (Some(l), Some(r)) => Some(match mode {
            "mirror" => Action::Upload,
            "mirror_pull" => Action::Download,
            _ => match (changed(l, b), changed(r, b)) {
                (true, false) => Action::Upload,
                (false, true) => Action::Download,
                _ => Action::Conflict,
            },
        }),
        (Some(l), None) => Some(match mode {
            "mirror" => Action::Upload,
            "mirror_pull" => Action::DeleteLocal,
            _ => match b {
                None => Action::Upload,
                Some(_) if changed(l, b) => Action::Conflict,
                _ => Action::DeleteLocal,
            },
        }),
        (None, Some(r)) => Some(match mode {
            "mirror" => Action::DeleteRemote,
            "mirror_pull" => Action::Download,
            _ => match b {
                None => Action::Download,
                Some(_) if changed(r, b) => Action::Conflict,
                _ => Action::DeleteRemote,
            },
        }),
        _ => None,
    }
}
pub fn compare_manifests(
    l: &Manifest,
    r: &Manifest,
    b: &Snapshot,
    o: &CompareOptions,
    lh: &BTreeMap<String, String>,
    rh: &BTreeMap<String, String>,
) -> CompareResult {
    let mut result = CompareResult {
        comparison_id: None,
        complete: true,
        entries: vec![],
        upload_count: 0,
        download_count: 0,
        delete_local_count: 0,
        delete_remote_count: 0,
        conflict_count: 0,
        same_count: 0,
        skipped_count: l.skipped + r.skipped,
    };
    let mut keys: BTreeSet<&String> = l.files.keys().chain(r.files.keys()).collect();
    if o.mode == "twoway" {
        keys.extend(b.files.keys())
    }
    for p in keys {
        if engine::is_ignored(p, &o.ignore)
            || l.excluded
                .iter()
                .chain(&r.excluded)
                .any(|x| p == x || p.starts_with(&format!("{x}/")))
        {
            continue;
        }
        let a = l.files.get(p);
        let z = r.files.get(p);
        let same = a
            .zip(z)
            .map(|(a, z)| equal(a, z, lh.get(p), rh.get(p)))
            .unwrap_or(false);
        let Some(action) = decision(a, z, b.files.get(p), &o.mode, same) else {
            continue;
        };
        match action {
            Action::Upload => result.upload_count += 1,
            Action::Download => result.download_count += 1,
            Action::DeleteLocal => result.delete_local_count += 1,
            Action::DeleteRemote => result.delete_remote_count += 1,
            Action::Conflict => result.conflict_count += 1,
            Action::Same => {
                result.same_count += 1;
                continue;
            }
        }
        result.entries.push(DiffEntry {
            rel_path: p.clone(),
            action,
            local_size: a.map(|s| s.size),
            remote_size: z.map(|s| s.size),
            local_mtime: a.map(|s| s.mtime),
            remote_mtime: z.map(|s| s.mtime),
            newer: a
                .map(|a| {
                    if z.map(|z| a.mtime >= z.mtime).unwrap_or(true) {
                        "local"
                    } else {
                        "remote"
                    }
                })
                .or(z.map(|_| "remote"))
                .map(str::to_owned),
        });
    }
    result.entries.sort_by_key(|e| {
        (
            match e.action {
                Action::Upload => 0,
                Action::Download => 1,
                Action::DeleteRemote => 2,
                Action::DeleteLocal => 3,
                Action::Conflict => 4,
                Action::Same => 5,
            },
            e.rel_path.clone(),
        )
    });
    result
}
pub fn merge_baseline(old: &Snapshot, l: &Manifest, r: &Manifest, done: &[SyncOp]) -> Snapshot {
    let mut b = old.clone();
    for op in done {
        match (l.files.get(&op.rel_path), r.files.get(&op.rel_path)) {
            (Some(a), Some(z)) if a.equivalent(z) => {
                b.files.insert(
                    op.rel_path.clone(),
                    SnapEntry {
                        size: a.size,
                        mtime: a.mtime,
                        hash: None,
                    },
                );
            }
            (None, None) => {
                b.files.remove(&op.rel_path);
            }
            _ => {}
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Tree(PathBuf);
    impl Tree {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "syncui-endpoint-{}-{}",
                std::process::id(),
                UNIX_EPOCH.elapsed().unwrap().as_nanos()
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn put(&self, p: &str, s: &str) {
            let p = self.0.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, s).unwrap();
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn manifest(t: &Tree) -> Manifest {
        scan(&t.0, &[], &AtomicBool::new(false), &mut |_| {}).unwrap()
    }
    #[test]
    fn scan_preserves_special_names_and_prunes_ignored_subtrees() {
        let t = Tree::new();
        for name in ["中文.txt", "space ' quote\nnext", "-flag", "a\\b", "*.txt"] {
            t.put(name, "a");
        }
        t.put("build/no.txt", "ignored");
        let m = scan(
            &t.0,
            &["build".into()],
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(m.files.len(), 5);
        assert!(m.files.contains_key("a\\b"));
    }
    #[test]
    fn missing_root_and_cancel_are_errors() {
        let t = Tree::new();
        assert!(scan(
            &t.0.join("missing"),
            &[],
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err());
        assert!(scan(&t.0, &[], &AtomicBool::new(true), &mut |_| {}).is_err());
    }
    #[test]
    fn traversal_and_symlink_parents_are_rejected() {
        use std::os::unix::fs::symlink;
        let t = Tree::new();
        let outside = Tree::new();
        outside.put("secret", "outside");
        symlink(&outside.0, t.0.join("link")).unwrap();
        for p in [
            "../secret",
            "/etc/passwd",
            "a/../secret",
            "a//b",
            "./a",
            "link/secret",
        ] {
            assert!(safe_path(&t.0, p).is_err(), "{p}");
        }
        assert_eq!(
            fs::read_to_string(outside.0.join("secret")).unwrap(),
            "outside"
        );
    }
    #[test]
    fn non_utf8_is_fatal_not_a_missing_file() {
        use std::os::unix::ffi::OsStringExt;
        let t = Tree::new();
        fs::write(t.0.join(std::ffi::OsString::from_vec(vec![0xff])), b"x").unwrap();
        assert!(scan(&t.0, &[], &AtomicBool::new(false), &mut |_| {}).is_err());
    }
    #[test]
    fn partial_directory_error_cannot_produce_a_manifest() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tree::new();
        t.put("unreadable/file", "x");
        fs::set_permissions(t.0.join("unreadable"), fs::Permissions::from_mode(0)).unwrap();
        let res = scan(&t.0, &[], &AtomicBool::new(false), &mut |_| {});
        fs::set_permissions(t.0.join("unreadable"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(res.is_err());
    }
    #[test]
    fn edits_and_deletes_check_the_read_version() {
        let t = Tree::new();
        t.put("a", "original");
        let v = read_text(&t.0, "a").unwrap().stamp;
        let updated = write_text(&t.0, "a", "first edit", &v).unwrap();
        assert!(write_text(&t.0, "a", "overwrite", &v).is_err());
        assert!(delete(&t.0, "a", v.as_ref().unwrap()).is_err());
        delete(&t.0, "a", &updated).unwrap();
        assert!(!t.0.join("a").exists());
    }
    #[test]
    fn skipped_symlink_is_never_a_deletion() {
        use std::os::unix::fs::symlink;
        let l = Tree::new();
        let r = Tree::new();
        l.put("a", "file");
        symlink("missing", r.0.join("a")).unwrap();
        let mut b = Snapshot::default();
        let stamp = stat(&l.0, "a").unwrap().unwrap();
        b.files.insert(
            "a".into(),
            SnapEntry {
                size: stamp.size,
                mtime: stamp.mtime,
                hash: None,
            },
        );
        let o = CompareOptions {
            mode: "twoway".into(),
            ..Default::default()
        };
        let d = compare_manifests(
            &manifest(&l),
            &manifest(&r),
            &b,
            &o,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(d.entries.is_empty());
        assert_eq!(d.skipped_count, 1);
    }
    #[test]
    fn hash_candidates_are_batched_and_changed_files_rejected() {
        let l = Tree::new();
        let r = Tree::new();
        l.put("a", "same");
        r.put("a", "same");
        filetime::set_file_mtime(r.0.join("a"), filetime::FileTime::from_unix_time(10, 0)).unwrap();
        let a = manifest(&l);
        let b = manifest(&r);
        assert_eq!(hash_candidates(&a, &b, true), vec!["a"]);
        let h = hash(&l.0, "a", &a.files["a"], &AtomicBool::new(false)).unwrap();
        let k = hash(&r.0, "a", &b.files["a"], &AtomicBool::new(false)).unwrap();
        assert_eq!(h, k);
        l.put("a", "changed");
        assert!(hash(&l.0, "a", &a.files["a"], &AtomicBool::new(false)).is_err());
    }
    #[test]
    fn baseline_merge_retains_unselected_conflicts_and_deletion_history() {
        let l = Tree::new();
        let r = Tree::new();
        l.put("done", "x");
        r.put("done", "x");
        let old = Snapshot {
            files: BTreeMap::from([
                (
                    "unselected".into(),
                    SnapEntry {
                        size: 10,
                        mtime: 12,
                        hash: None,
                    },
                ),
                (
                    "deleted".into(),
                    SnapEntry {
                        size: 2,
                        mtime: 12,
                        hash: None,
                    },
                ),
            ]),
        };
        let b = merge_baseline(
            &old,
            &manifest(&l),
            &manifest(&r),
            &[SyncOp {
                rel_path: "done".into(),
                op: "upload".into(),
            }],
        );
        assert_eq!(b.files.len(), 3);
        assert_eq!(b.files["unselected"].size, 10);
    }
    #[test]
    fn old_and_manifest_decisions_match_on_valid_inventories() {
        let l = Tree::new();
        let r = Tree::new();
        l.put("local", "a");
        r.put("remote", "b");
        l.put("both", "local");
        r.put("both", "remote");
        for mode in ["mirror", "mirror_pull", "twoway"] {
            let o = CompareOptions {
                mode: mode.into(),
                ..Default::default()
            };
            let legacy = engine::compare(&l.0, &r.0, &o).unwrap();
            let current = compare_manifests(
                &manifest(&l),
                &manifest(&r),
                &Snapshot::default(),
                &o,
                &BTreeMap::new(),
                &BTreeMap::new(),
            );
            assert_eq!(
                serde_json::to_value(legacy).unwrap(),
                serde_json::to_value(current).unwrap()
            );
        }
    }
}
