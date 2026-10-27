//! Compare-bound operation plans, endpoint routing and a single post-sync refresh.
use crate::{
    diagnostics::Diagnostics,
    engine::{
        Action, CompareOptions, CompareResult, OpProgress, SyncOp, SyncResult, TransferProgress,
    },
    snapshot::Snapshot,
    ssh::{self, SshEndpoint},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use sync_core::endpoint::{self, Endpoint, Inventory, LocalEndpoint, Manifest, Stamp, TextFile};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RemoteEndpoint {
    Filesystem {
        path: String,
    },
    Ssh {
        #[serde(rename = "profileId")]
        profile_id: String,
        path: String,
    },
}
#[derive(Clone)]
pub struct Session {
    pub local: Manifest,
    pub remote: Manifest,
    pub endpoint: RemoteEndpoint,
    pub options: CompareOptions,
    pub baseline: Snapshot,
    pub baseline_path: PathBuf,
    pub result: CompareResult,
    pub created: Instant,
}
#[derive(Default)]
pub struct Coordinator {
    pub sessions: Arc<Mutex<BTreeMap<String, Session>>>,
    pub lock: Arc<Mutex<()>>,
}
fn endpoint(app: &AppHandle, remote: &RemoteEndpoint) -> Result<Box<dyn Endpoint>, String> {
    match remote {
        RemoteEndpoint::Filesystem { path } => Ok(Box::new(LocalEndpoint(PathBuf::from(path)))),
        RemoteEndpoint::Ssh { profile_id, path } => {
            let s = app.state::<ssh::SshState>();
            let c = s
                .connections
                .lock()
                .unwrap()
                .get(profile_id)
                .cloned()
                .ok_or("SSH 尚未连接，请先连接服务器")?;
            if !c.info.lock().unwrap().agent_ready {
                return Err("请先部署远端工具".into());
            }
            Ok(Box::new(SshEndpoint {
                connection: c,
                root: path.clone(),
            }))
        }
    }
}
fn diagnostics(app: &AppHandle, kind: &str) -> Diagnostics {
    let dir = app
        .path()
        .app_config_dir()
        .ok()
        .map(|p| p.join("diagnostics"));
    let a = app.clone();
    Diagnostics::new(kind, dir.as_deref(), move |s| {
        let _ = a.emit("diagnostic-log", s);
    })
}
fn phase(app: &AppHandle, log: &Diagnostics, name: &str) {
    let _ = app.emit(
        "task-event",
        serde_json::json!({"taskId":log.id(),"phase":name,"status":"running"}),
    );
}
fn baseline_path(
    app: &AppHandle,
    local: &str,
    remote: &RemoteEndpoint,
    canonical_remote: &str,
) -> Result<PathBuf, String> {
    let identity = match remote {
        RemoteEndpoint::Filesystem { path } => format!("{local}\0{path}"),
        RemoteEndpoint::Ssh { profile_id, .. } => {
            let s = app.state::<ssh::SshState>();
            let c = s
                .connections
                .lock()
                .unwrap()
                .get(profile_id)
                .cloned()
                .ok_or("SSH 已断开")?;
            let i = c.info.lock().unwrap();
            format!(
                "ssh-v1\0{local}\0{}\0{}\0{canonical_remote}",
                i.host_key, i.user
            )
        }
    };
    Ok(app
        .path()
        .app_config_dir()
        .map_err(|e| e.to_string())?
        .join("snapshots")
        .join(format!(
            "{}.json",
            blake3::hash(identity.as_bytes()).to_hex()
        )))
}
fn inventory_pair(
    app: &AppHandle,
    local: &str,
    remote: &RemoteEndpoint,
    options: &CompareOptions,
    cancel: &AtomicBool,
    log: &Diagnostics,
) -> Result<(Manifest, Manifest), String> {
    phase(app, log, "scan");
    let wall = Instant::now();
    let l = LocalEndpoint(PathBuf::from(local));
    let r = endpoint(app, remote)?;
    let result = std::thread::scope(|scope| {
        let lp = |n| {
            let _ = app.emit(
                "scan-progress",
                serde_json::json!({"phase":"local","count":n,"taskId":log.id()}),
            );
        };
        let rp = |n| {
            let _ = app.emit(
                "scan-progress",
                serde_json::json!({"phase":"remote","count":n,"taskId":log.id()}),
            );
        };
        let a = scope.spawn(move || l.scan(&options.ignore, cancel, &lp));
        let b = scope.spawn(move || r.scan(&options.ignore, cancel, &rp));
        Ok::<_, String>((
            a.join().map_err(|_| "本机扫描线程失败")??,
            b.join().map_err(|_| "远端扫描线程失败")??,
        ))
    })?;
    log.log(format!(
        "清单接收/解析完成：wall={:.2}ms（含远端启动与传输），SSH 操作均访问远端真实目录",
        wall.elapsed().as_secs_f64() * 1000.
    ));
    log.log(format!(
        "扫描完成：local={} files {:.2}ms；remote={} files {:.2}ms；两侧并行",
        result.0.files.len(),
        result.0.elapsed_ms,
        result.1.files.len(),
        result.1.elapsed_ms
    ));
    Ok(result)
}
fn decisions(
    app: &AppHandle,
    local: &Manifest,
    remote: &Manifest,
    e: &RemoteEndpoint,
    options: &CompareOptions,
    baseline: &Snapshot,
    cancel: &AtomicBool,
    log: &Diagnostics,
) -> Result<CompareResult, String> {
    let started = Instant::now();
    phase(app, log, "hash-and-decide");
    let candidates = endpoint::hash_candidates(local, remote, options.use_hash);
    let mut lh = BTreeMap::new();
    let mut rh = BTreeMap::new();
    if !candidates.is_empty() {
        let l = LocalEndpoint(local.root.clone().into());
        let r = endpoint(app, e)?;
        let lf: Inventory = candidates
            .iter()
            .map(|p| (p.clone(), local.files[p].clone()))
            .collect();
        let rf: Inventory = candidates
            .iter()
            .map(|p| (p.clone(), remote.files[p].clone()))
            .collect();
        (lh, rh) = std::thread::scope(|s| {
            let a = s.spawn(|| l.hashes(&lf, cancel));
            let b = s.spawn(|| r.hashes(&rf, cancel));
            Ok::<_, String>((
                a.join().map_err(|_| "哈希线程失败")??,
                b.join().map_err(|_| "哈希线程失败")??,
            ))
        })?;
        if lh.len() != candidates.len() || rh.len() != candidates.len() {
            return Err("哈希清单不完整".into());
        }
    }
    endpoint::check_cancel(cancel)?;
    let result = endpoint::compare_manifests(local, remote, baseline, options, &lh, &rh);
    log.log(format!(
        "哈希候选={}；哈希/差异决策={:.2}ms",
        candidates.len(),
        started.elapsed().as_secs_f64() * 1000.
    ));
    Ok(result)
}
fn finish_event<T>(app: &AppHandle, log: &Diagnostics, phase: &str, result: &Result<T, String>) {
    let error=result.as_ref().err().map(|e|serde_json::json!({"code":if e==crate::engine::CANCELLED {"cancelled"}else{"taskFailed"},"message":e}));
    if let Err(e) = result {
        log.log(format!("任务失败或取消：{e}"));
    }
    let _=app.emit("task-event",serde_json::json!({"taskId":log.id(),"phase":phase,"status":if error.is_some(){"error"}else{"complete"},"error":error}));
}
pub fn compare(
    app: &AppHandle,
    state: &Coordinator,
    local: String,
    remote: RemoteEndpoint,
    options: CompareOptions,
    cancel: &AtomicBool,
) -> Result<CompareResult, String> {
    let log = diagnostics(app, "compare");
    let result = compare_impl(app, state, local, remote, options, cancel, &log);
    finish_event(app, &log, "compare", &result);
    result
}
pub fn sync(
    app: &AppHandle,
    state: &Coordinator,
    id: &str,
    paths: Vec<String>,
    policy: String,
    concurrency: usize,
    cancel: &AtomicBool,
) -> Result<SyncResult, String> {
    let log = diagnostics(app, "sync");
    let result = sync_impl(app, state, id, paths, policy, concurrency, cancel, &log);
    finish_event(app, &log, "sync", &result);
    result
}
fn compare_impl(
    app: &AppHandle,
    state: &Coordinator,
    local: String,
    remote: RemoteEndpoint,
    options: CompareOptions,
    cancel: &AtomicBool,
    log: &Diagnostics,
) -> Result<CompareResult, String> {
    let _guard = state
        .lock
        .try_lock()
        .map_err(|_| "另一个目录任务正在执行")?;
    if !["mirror", "mirror_pull", "twoway"].contains(&options.mode.as_str()) {
        return Err("无效同步模式".into());
    }
    let started = Instant::now();
    let (l, r) = inventory_pair(app, &local, &remote, &options, cancel, &log)?;
    let baseline_local = if matches!(remote, RemoteEndpoint::Filesystem { .. }) {
        &local
    } else {
        &l.root
    };
    let path = baseline_path(app, baseline_local, &remote, &r.root)?;
    let remote = match remote {
        RemoteEndpoint::Filesystem { .. } => RemoteEndpoint::Filesystem {
            path: r.root.clone(),
        },
        RemoteEndpoint::Ssh { profile_id, .. } => RemoteEndpoint::Ssh {
            profile_id,
            path: r.root.clone(),
        },
    };
    let baseline = load_baseline(&path)?;
    let mut result = decisions(app, &l, &r, &remote, &options, &baseline, cancel, &log)?;
    let id = ssh::random_id();
    result.comparison_id = Some(id.clone());
    let mut sessions = state.sessions.lock().unwrap();
    sessions.clear();
    sessions.insert(
        id,
        Session {
            local: l,
            remote: r,
            endpoint: remote,
            options,
            baseline,
            baseline_path: path,
            result: result.clone(),
            created: Instant::now(),
        },
    );
    log.log(format!(
        "对比完成：wall={:.2}ms complete=true",
        started.elapsed().as_secs_f64() * 1000.
    ));
    Ok(result)
}
fn load_baseline(p: &Path) -> Result<Snapshot, String> {
    match std::fs::read(p) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("基线损坏，已停止任务: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot::default()),
        Err(e) => Err(format!("基线读取失败: {e}")),
    }
}
fn atomic_baseline(p: &Path, b: &Snapshot) -> Result<(), String> {
    let parent = p.parent().ok_or("基线路径无效")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(".{}.tmp", ssh::random_id()));
    let result = (|| {
        std::fs::write(&tmp, serde_json::to_vec(b).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, p).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_file(tmp);
    result
}
fn chosen_op(entry: &crate::engine::DiffEntry, policy: &str) -> Result<Option<String>, String> {
    Ok(Some(
        match entry.action {
            Action::Upload => "upload",
            Action::Download => "download",
            Action::DeleteLocal => "delLocal",
            Action::DeleteRemote => "delRemote",
            Action::Same => return Ok(None),
            Action::Conflict => match policy {
                "skip" => return Ok(None),
                "local" => {
                    if entry.local_size.is_some() {
                        "upload"
                    } else {
                        "delRemote"
                    }
                }
                "remote" => {
                    if entry.remote_size.is_some() {
                        "download"
                    } else {
                        "delLocal"
                    }
                }
                "newer" => {
                    if entry.newer.as_deref() == Some("local") {
                        "upload"
                    } else {
                        "download"
                    }
                }
                _ => return Err("无效冲突策略".into()),
            },
        }
        .into(),
    ))
}
fn selected_plan(s: &Session, paths: &[String], policy: &str) -> Result<Vec<SyncOp>, String> {
    let mut unique = BTreeSet::new();
    let map: BTreeMap<_, _> = s.result.entries.iter().map(|e| (&e.rel_path, e)).collect();
    let mut ops = vec![];
    for p in paths {
        if !unique.insert(p) {
            continue;
        }
        let e = map.get(p).ok_or("选择包含对比结果以外的路径")?;
        if let Some(op) = chosen_op(e, policy)? {
            ops.push(SyncOp {
                rel_path: p.clone(),
                op,
            })
        }
    }
    Ok(ops)
}
fn stats_pair(
    l: &dyn Endpoint,
    r: &dyn Endpoint,
    paths: &[String],
    c: &AtomicBool,
) -> Result<
    (
        BTreeMap<String, Option<Stamp>>,
        BTreeMap<String, Option<Stamp>>,
    ),
    String,
> {
    std::thread::scope(|s| {
        let a = s.spawn(|| l.stats(paths, c));
        let b = s.spawn(|| r.stats(paths, c));
        Ok((
            a.join().map_err(|_| "属性查询线程失败")??,
            b.join().map_err(|_| "属性查询线程失败")??,
        ))
    })
}
fn op_success(
    op: &SyncOp,
    old_l: Option<&Stamp>,
    old_r: Option<&Stamp>,
    l: Option<&Stamp>,
    r: Option<&Stamp>,
) -> bool {
    match op.op.as_str() {
        "upload" => old_l == l && l.zip(r).is_some_and(|(a, b)| a.equivalent(b)),
        "download" => old_r == r && l.zip(r).is_some_and(|(a, b)| a.equivalent(b)),
        "delLocal" | "delRemote" => l.is_none() && r.is_none(),
        _ => false,
    }
}
fn rsync_batch(
    app: &AppHandle,
    c: &Arc<ssh::Connection>,
    local: &str,
    remote: &str,
    paths: &[String],
    upload: bool,
    cancel: &AtomicBool,
    log: &Diagnostics,
    base_bytes: u64,
    base_logical: u64,
    start: Instant,
) -> Result<u64, String> {
    let probe = Command::new("rsync")
        .arg("--version")
        .output()
        .map_err(|_| "本机缺少 rsync，请安装 rsync >= 3.2.0")?;
    if !ssh::rsync_version_ok(&String::from_utf8_lossy(&probe.stdout))
        || !c.info.lock().unwrap().rsync_ready
    {
        return Err("两端需要 rsync >= 3.2.0；Ubuntu/Debian: sudo apt install rsync".into());
    }
    let mut files = paths.to_vec();
    files.sort();
    let mut input = vec![];
    for p in files {
        endpoint::validate_rel(&p)?;
        input.extend_from_slice(p.as_bytes());
        input.push(0)
    }
    let source = format!("{}/", local.trim_end_matches('/'));
    let destination = format!("syncui-peer:{}/", remote.trim_end_matches('/'));
    let mut cmd = Command::new("rsync");
    cmd.args([
        "--times",
        "--perms",
        "--relative",
        "--dirs",
        "--no-recursive",
        "--omit-dir-times",
        "--no-implied-dirs",
        "--from0",
        "--files-from=-",
        "--ignore-times",
        "--protect-args",
        "--stats",
        "--info=progress2",
        "--no-human-readable",
    ])
    .arg("-e")
    .arg(format!(
        "{} rsync-rsh",
        ssh::quote(&c.bridge.to_string_lossy())
    ))
    .arg("--");
    if upload {
        cmd.args([&source, &destination]);
    } else {
        cmd.args([&destination, &source]);
    }
    cmd.env("LC_ALL", "C")
        .env("SYNCUI_BRIDGE_CONFIG", &c.config);
    let before = c.bytes.load(Ordering::Relaxed);
    let mut last = Instant::now();
    let mut last_bytes = base_bytes;
    let logical = std::cell::Cell::new(0u64);
    let mut progress_buffer = Vec::new();
    let batch_started = Instant::now();
    let result = ssh::process_live(
        cmd,
        input,
        cancel,
        &mut || {
            if last.elapsed().as_millis() >= 500 {
                let now = base_bytes + c.bytes.load(Ordering::Relaxed).saturating_sub(before);
                let _ = app.emit(
                    "sync-transfer-progress",
                    TransferProgress {
                        logical_bytes: Some(base_logical + logical.get()),
                        bytes_copied: now,
                        bytes_per_second: (now - last_bytes) as f64 / last.elapsed().as_secs_f64(),
                        elapsed_seconds: start.elapsed().as_secs_f64(),
                        finished: false,
                    },
                );
                last = Instant::now();
                last_bytes = now;
            }
        },
        &mut |chunk| {
            progress_buffer.extend_from_slice(chunk);
            while let Some(end) = progress_buffer
                .iter()
                .position(|b| *b == b'\r' || *b == b'\n')
            {
                let line = String::from_utf8_lossy(&progress_buffer[..end]);
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() >= 2 && fields[1].ends_with('%') {
                    if let Ok(n) = fields[0].replace(',', "").parse::<u64>() {
                        logical.set(n);
                    }
                }
                progress_buffer.drain(..=end);
            }
            if progress_buffer.len() > 65536 {
                progress_buffer.clear();
            }
        },
    );
    let bytes = c.bytes.load(Ordering::Relaxed).saturating_sub(before);
    log.log(format!(
        "rsync {}：files={} protocol_bytes={bytes}（SSH 加密前双向协议数据）",
        if upload { "upload" } else { "download" },
        paths.len()
    ));
    match result {
        Ok(out) => {
            log.log(format!(
                "rsync exit={:?} elapsed={:.3}s logical_bytes={}",
                out.code,
                batch_started.elapsed().as_secs_f64(),
                logical.get()
            ));
            log.detail(String::from_utf8_lossy(&out.stdout));
            if !out.success {
                return Err(format!(
                    "rsync 部分失败，结果需要重新对比: {}",
                    out.stderr.trim()
                ));
            }
            if !out.stderr.trim().is_empty() {
                log.detail(out.stderr)
            }
            Ok(bytes)
        }
        Err(e) => Err(e),
    }
}
fn sync_impl(
    app: &AppHandle,
    state: &Coordinator,
    id: &str,
    paths: Vec<String>,
    policy: String,
    concurrency: usize,
    cancel: &AtomicBool,
    log: &Diagnostics,
) -> Result<SyncResult, String> {
    let _guard = state
        .lock
        .try_lock()
        .map_err(|_| "另一个目录任务正在执行")?;
    let s = state
        .sessions
        .lock()
        .unwrap()
        .remove(id)
        .ok_or("比较结果已失效，请重新对比")?;
    if s.created.elapsed().as_secs() > 1800 {
        return Err("比较结果已超过 30 分钟，请重新对比".into());
    }
    if matches!(s.endpoint, RemoteEndpoint::Ssh { .. })
        && baseline_path(app, &s.local.root, &s.endpoint, &s.remote.root)? != s.baseline_path
    {
        return Err("SSH 服务器身份发生变化，请重新对比".into());
    }
    let started = Instant::now();
    let l = LocalEndpoint(s.local.root.clone().into());
    let r = endpoint(app, &s.endpoint)?;
    phase(app, &log, "validate-plan");
    log.log(format!(
        "comparison_id={id} selected={} endpoint={:?}",
        paths.len(),
        s.endpoint
    ));
    let ops = selected_plan(&s, &paths, &policy)?;
    let all_paths: Vec<_> = ops.iter().map(|o| o.rel_path.clone()).collect();
    let (mut before_l, mut before_r) = stats_pair(&l, r.as_ref(), &all_paths, cancel)?;
    let mut result = SyncResult::default();
    let mut ready = vec![];
    let mut attempted = BTreeSet::new();
    for op in ops {
        result.items.push(crate::engine::ItemResult {
            rel_path: op.rel_path.clone(),
            op: op.op.clone(),
            status: "pending".into(),
            message: None,
        });
        let p = &op.rel_path;
        if before_l.get(p) != Some(&s.local.files.get(p).cloned())
            || before_r.get(p) != Some(&s.remote.files.get(p).cloned())
        {
            result.skipped += 1;
            let item = result.items.last_mut().unwrap();
            item.status = "skipped".into();
            item.message = Some("文件自比较后已变化".into());
            result.errors.push(format!("文件已变化，跳过 {p:?}"));
        } else {
            ready.push(op)
        }
    }
    let connection = if let RemoteEndpoint::Ssh { profile_id, .. } = &s.endpoint {
        Some(
            app.state::<ssh::SshState>()
                .connections
                .lock()
                .unwrap()
                .get(profile_id)
                .cloned()
                .ok_or("SSH 已断开")?,
        )
    } else {
        None
    };
    let initial_bytes = connection
        .as_ref()
        .map(|c| c.bytes.load(Ordering::Relaxed))
        .unwrap_or(0);
    let mut done = vec![];
    let mut interrupted = false;
    for direction in ["upload", "download", "delete"] {
        let batch: Vec<_> = ready
            .iter()
            .filter(|o| {
                if direction == "delete" {
                    o.op.starts_with("del")
                } else {
                    o.op == direction
                }
            })
            .cloned()
            .collect();
        if batch.is_empty() {
            continue;
        }
        if let Err(e) = endpoint::check_cancel(cancel) {
            result.errors.push(e);
            interrupted = true;
            break;
        }
        // Revalidate immediately before each phase; all metadata requests are batched.
        let names: Vec<_> = batch.iter().map(|o| o.rel_path.clone()).collect();
        let fresh = stats_pair(&l, r.as_ref(), &names, cancel);
        let (a, b) = match fresh {
            Ok(v) => v,
            Err(e) => {
                result.errors.push(e);
                interrupted = true;
                break;
            }
        };
        if names
            .iter()
            .any(|p| a.get(p) != before_l.get(p) || b.get(p) != before_r.get(p))
        {
            result
                .errors
                .push("阶段执行前文件已变化，请重新对比".into());
            interrupted = true;
            break;
        }
        phase(app, &log, direction);
        attempted.extend(names.iter().cloned());
        let stage_started = Instant::now();
        let operation = if direction == "delete" {
            let mut outcome = Ok(());
            for op in &batch {
                if let Err(e) = endpoint::check_cancel(cancel) {
                    outcome = Err(e);
                    break;
                }
                let expected = if op.op == "delLocal" {
                    before_l.get(&op.rel_path).and_then(Option::as_ref)
                } else {
                    before_r.get(&op.rel_path).and_then(Option::as_ref)
                };
                let deleted = match expected {
                    Some(stamp) => {
                        if op.op == "delLocal" {
                            l.delete_cancelled(&op.rel_path, stamp, cancel)
                        } else {
                            r.delete_cancelled(&op.rel_path, stamp, cancel)
                        }
                    }
                    None => Err("删除计划的目标不存在".into()),
                };
                if let Err(e) = deleted {
                    outcome = Err(e);
                    break;
                }
            }
            outcome
        } else if let Some(c) = &connection {
            rsync_batch(
                app,
                c,
                &s.local.root,
                &s.remote.root,
                &names,
                direction == "upload",
                cancel,
                &log,
                result.bytes_copied,
                result.logical_bytes,
                started,
            )
            .map(|b| result.bytes_copied += b)
        } else {
            let a = app.clone();
            let t = app.clone();
            let partial = crate::engine::apply_ops_with_diagnostics(
                Path::new(&s.local.root),
                Path::new(&s.remote.root),
                &batch,
                concurrency,
                cancel,
                &|p| {
                    let _ = a.emit("sync-progress", p);
                },
                &|p| {
                    let _ = t.emit("sync-transfer-progress", p);
                },
                &log,
            );
            result.bytes_copied += partial.bytes_copied;
            if partial.cancelled {
                Err(crate::engine::CANCELLED.into())
            } else if partial.failed > 0 || partial.skipped > 0 {
                Err(partial.errors.join("; "))
            } else {
                Ok(())
            }
        };
        log.log(format!(
            "阶段 {direction}：items={} elapsed={:.2}ms",
            batch.len(),
            stage_started.elapsed().as_secs_f64() * 1000.
        ));
        if let Err(e) = operation {
            result.errors.push(e);
            interrupted = true;
            break;
        }
        let (a, b) = match stats_pair(&l, r.as_ref(), &names, cancel) {
            Ok(v) => v,
            Err(e) => {
                result.errors.push(e);
                interrupted = true;
                break;
            }
        };
        for op in batch {
            let p = &op.rel_path;
            let valid = op_success(
                &op,
                before_l.get(p).and_then(Option::as_ref),
                before_r.get(p).and_then(Option::as_ref),
                a.get(p).and_then(Option::as_ref),
                b.get(p).and_then(Option::as_ref),
            );
            if !valid {
                result.errors.push(format!("同步后复核失败: {p:?}"));
                interrupted = true;
                break;
            }
            match op.op.as_str() {
                "upload" => result.uploaded += 1,
                "download" => result.downloaded += 1,
                "delLocal" => result.deleted_local += 1,
                _ => result.deleted_remote += 1,
            }
            result.logical_bytes += a
                .get(p)
                .and_then(Option::as_ref)
                .map(|s| s.size)
                .unwrap_or(0);
            result.done_paths.push(p.clone());
            let _ = app.emit(
                "sync-progress",
                OpProgress {
                    index: done.len() + 1,
                    total: ready.len(),
                    rel_path: p.clone(),
                    op: op.op.clone(),
                    ok: true,
                    skipped: false,
                    error: None,
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.,
                    bytes_copied: 0,
                },
            );
            done.push(op);
        }
        before_l.extend(a);
        before_r.extend(b);
        if interrupted {
            break;
        }
    }
    result.elapsed_seconds = started.elapsed().as_secs_f64();
    result.cancelled = cancel.load(Ordering::Relaxed);
    result.needs_recompare = interrupted || result.skipped > 0;
    if interrupted {
        result.failed = if result.cancelled {
            0
        } else {
            ready.len().saturating_sub(done.len())
        };
        log.log("任务部分完成或取消：保留旧基线，剩余结果需要重新对比");
    } else {
        let refreshed = (|| {
            log.log("同步后刷新：两端各扫描一次，共用清单进行验证、基线更新和界面刷新");
            let (nl, nr) =
                inventory_pair(app, &s.local.root, &s.endpoint, &s.options, cancel, &log)?;
            for op in &done {
                if !op_success(
                    op,
                    before_l.get(&op.rel_path).and_then(Option::as_ref),
                    before_r.get(&op.rel_path).and_then(Option::as_ref),
                    nl.files.get(&op.rel_path),
                    nr.files.get(&op.rel_path),
                ) {
                    return Err(format!("刷新时发现外部修改: {:?}", op.rel_path));
                }
            }
            let baseline = endpoint::merge_baseline(&s.baseline, &nl, &nr, &done);
            let mut diff = decisions(
                app,
                &nl,
                &nr,
                &s.endpoint,
                &s.options,
                &baseline,
                cancel,
                &log,
            )?;
            endpoint::check_cancel(cancel)?;
            phase(app, &log, "save-baseline");
            let saving = Instant::now();
            atomic_baseline(&s.baseline_path, &baseline)?;
            log.log(format!(
                "基线合并保存：files={} elapsed={:.2}ms",
                baseline.files.len(),
                saving.elapsed().as_secs_f64() * 1000.
            ));
            let new_id = ssh::random_id();
            diff.comparison_id = Some(new_id.clone());
            state.sessions.lock().unwrap().insert(
                new_id,
                Session {
                    local: nl,
                    remote: nr,
                    endpoint: s.endpoint.clone(),
                    options: s.options.clone(),
                    baseline,
                    baseline_path: s.baseline_path.clone(),
                    result: diff.clone(),
                    created: Instant::now(),
                },
            );
            Ok::<_, String>(diff)
        })();
        match refreshed {
            Ok(d) => result.refreshed = Some(d),
            Err(e) => {
                result.errors.push(format!("刷新失败，保留旧基线: {e}"));
                result.needs_recompare = true;
            }
        }
    }
    if let Some(c) = connection {
        result.bytes_copied = c
            .bytes
            .load(Ordering::Relaxed)
            .saturating_sub(initial_bytes)
    }
    result.cancelled |= cancel.load(Ordering::Relaxed);
    let done_set: BTreeSet<_> = result.done_paths.iter().collect();
    for item in &mut result.items {
        if item.status == "skipped" {
            continue;
        }
        if done_set.contains(&item.rel_path) && (result.refreshed.is_some() || interrupted) {
            item.status = "success".into();
        } else if attempted.contains(&item.rel_path) {
            item.status = "uncertain".into();
            item.message = Some("操作可能已部分完成，需重新对比".into());
        } else {
            item.status = "notStarted".into();
            item.message = Some("前序阶段失败或任务已取消".into());
        }
    }
    let _ = app.emit(
        "sync-transfer-progress",
        TransferProgress {
            logical_bytes: Some(result.logical_bytes),
            bytes_copied: result.bytes_copied,
            bytes_per_second: result.bytes_copied as f64 / result.elapsed_seconds.max(0.001),
            elapsed_seconds: result.elapsed_seconds,
            finished: true,
        },
    );
    log.log(format!("同步整体结束：wall={:.2}s transfer_phase={:.2}s uploaded={} downloaded={} failed={} needs_recompare={} bytes={} logical_bytes={}",started.elapsed().as_secs_f64(),result.elapsed_seconds,result.uploaded,result.downloaded,result.failed,result.needs_recompare,result.bytes_copied,result.logical_bytes));
    Ok(result)
}
pub fn compare_one(
    app: &AppHandle,
    local: &str,
    remote: &RemoteEndpoint,
    path: &str,
    options: &CompareOptions,
) -> Result<Option<crate::engine::DiffEntry>, String> {
    endpoint::validate_rel(path)?;
    let cancel = AtomicBool::new(false);
    let local_root = endpoint::canonical_root(Path::new(local))?
        .to_str()
        .ok_or("非 UTF-8 路径")?
        .to_owned();
    let remote_root = match remote {
        RemoteEndpoint::Filesystem { path } => endpoint::canonical_root(Path::new(path))?
            .to_str()
            .ok_or("非 UTF-8 路径")?
            .to_owned(),
        RemoteEndpoint::Ssh { profile_id, path } => {
            let c = app
                .state::<ssh::SshState>()
                .connections
                .lock()
                .unwrap()
                .get(profile_id)
                .cloned()
                .ok_or("SSH 已断开")?;
            c.request(
                path,
                sync_core::protocol::Operation::Hello,
                &cancel,
                &mut |_| {},
            )?["root"]
                .as_str()
                .ok_or("无效远端目录")?
                .to_owned()
        }
    };
    let l = LocalEndpoint(local_root.clone().into());
    let r = endpoint(app, remote)?;
    let (ls, rs) = stats_pair(&l, r.as_ref(), &[path.to_owned()], &cancel)?;
    let make = |root, stats: BTreeMap<String, Option<Stamp>>| Manifest {
        root,
        files: stats
            .into_iter()
            .filter_map(|(p, s)| s.map(|s| (p, s)))
            .collect(),
        ..Manifest::default()
    };
    let lm = make(local_root.clone(), ls);
    let rm = make(remote_root.clone(), rs);
    let baseline_local = if matches!(remote, RemoteEndpoint::Filesystem { .. }) {
        local
    } else {
        &local_root
    };
    let mut baseline = load_baseline(&baseline_path(app, baseline_local, remote, &remote_root)?)?;
    baseline.files.retain(|p, _| p == path);
    let diff = decisions(
        app,
        &lm,
        &rm,
        remote,
        options,
        &baseline,
        &cancel,
        &Diagnostics::silent(),
    )?;
    if diff.same_count > 0 {
        let a = lm.files.get(path);
        let b = rm.files.get(path);
        return Ok(Some(crate::engine::DiffEntry {
            rel_path: path.into(),
            action: Action::Same,
            local_size: a.map(|s| s.size),
            remote_size: b.map(|s| s.size),
            local_mtime: a.map(|s| s.mtime),
            remote_mtime: b.map(|s| s.mtime),
            newer: None,
        }));
    }
    Ok(diff.entries.into_iter().next())
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextPair {
    pub rel_path: String,
    pub local: crate::engine::FileTextSide,
    pub remote: crate::engine::FileTextSide,
    pub local_version: Option<Stamp>,
    pub remote_version: Option<Stamp>,
}
pub fn read_pair(
    app: &AppHandle,
    local: &str,
    remote: &RemoteEndpoint,
    p: &str,
) -> Result<TextPair, String> {
    let l = LocalEndpoint(local.into());
    let r = endpoint(app, remote)?;
    let (a, b) = std::thread::scope(|s| {
        let x = s.spawn(|| l.read(p));
        let y = s.spawn(|| r.read(p));
        Ok::<(TextFile, TextFile), String>((
            x.join().map_err(|_| "读取失败")??,
            y.join().map_err(|_| "读取失败")??,
        ))
    })?;
    Ok(TextPair {
        rel_path: p.into(),
        local: a.side,
        remote: b.side,
        local_version: a.stamp,
        remote_version: b.stamp,
    })
}
pub fn write(
    app: &AppHandle,
    state: &Coordinator,
    local: &str,
    remote: &RemoteEndpoint,
    p: &str,
    side: &str,
    content: &str,
    expected: &Option<Stamp>,
) -> Result<Stamp, String> {
    let _guard = state.lock.try_lock().map_err(|_| "任务执行期间不能编辑")?;
    let result = match side {
        "local" => LocalEndpoint(local.into()).write(p, content, expected),
        "remote" => endpoint(app, remote)?.write(p, content, expected),
        _ => Err("无效编辑侧".into()),
    }?;
    state.sessions.lock().unwrap().clear();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflict_resolution_honors_deleted_side() {
        let e = crate::engine::DiffEntry {
            rel_path: "a".into(),
            action: Action::Conflict,
            local_size: None,
            remote_size: Some(1),
            local_mtime: None,
            remote_mtime: Some(1),
            newer: Some("remote".into()),
        };
        assert_eq!(chosen_op(&e, "local").unwrap(), Some("delRemote".into()));
        assert_eq!(chosen_op(&e, "remote").unwrap(), Some("download".into()));
        assert_eq!(chosen_op(&e, "skip").unwrap(), None);
    }
    #[test]
    fn a_failed_source_change_is_not_reported_as_success() {
        let s = Stamp {
            size: 1,
            mtime: 12,
            mtime_ns: 0,
            ctime: 1,
            ctime_ns: 0,
            mode: 0o600,
        };
        let mut changed = s.clone();
        changed.ctime = 2;
        let op = SyncOp {
            rel_path: "a".into(),
            op: "upload".into(),
        };
        assert!(!op_success(&op, Some(&s), None, Some(&changed), Some(&s)));
    }
}
