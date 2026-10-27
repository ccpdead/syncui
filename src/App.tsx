import { isTauri } from "@tauri-apps/api/core";
import { useEffect, useMemo, useRef, useState, useCallback, forwardRef } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  compareDirs,
  syncEntries,
  cancelCompare,
  cancelSync,
  CANCELLED,
  loadSettings,
  saveSettings,
  CompareResult,
  DiffEntry,
  SyncProgress,
  TransferProgress,
  Action,
  Op,
  SyncMode,
  ConflictPolicy,
  RemoteEndpoint,
} from "./api";
import SshConnection, { SshAuthDialog } from "./SshConnection";
import DiffDrawer from "./DiffDrawer";
import SelectionTree from "./SelectionTree";
import IgnoreRules from "./IgnoreRules";

const DIFFABLE: ReadonlySet<Action> = new Set(["upload", "download", "conflict"]);

function formatTransferAmount(bytes: number): string {
  const amount = Number.isFinite(bytes) ? Math.max(0, bytes) : 0;
  return amount >= 1024 * 1024
    ? `${(amount / (1024 * 1024)).toFixed(2)} MB`
    : `${(amount / 1024).toFixed(1)} KB`;
}

/** Rebuild the per-action counts; `sameCount` is carried over because
 * identical files are not part of `entries`. */
function recount(entries: DiffEntry[], skippedCount: number, sameCount: number): CompareResult {
  const counts = {
    uploadCount: 0,
    downloadCount: 0,
    deleteLocalCount: 0,
    deleteRemoteCount: 0,
    conflictCount: 0,
    sameCount,
  };
  for (const e of entries) {
    switch (e.action) {
      case "upload":
        counts.uploadCount++;
        break;
      case "download":
        counts.downloadCount++;
        break;
      case "deleteLocal":
        counts.deleteLocalCount++;
        break;
      case "deleteRemote":
        counts.deleteRemoteCount++;
        break;
      case "conflict":
        counts.conflictCount++;
        break;
      case "same":
        counts.sameCount++;
        break;
    }
  }
  return { entries, skippedCount, ...counts };
}

type Side = "local" | "remote";

const DEFAULT_IGNORE =
  ".git, node_modules, .venv, __pycache__, target, dist, .DS_Store";

const ACTION_META: Record<Action, { label: string; icon: string; cls: string }> = {
  upload: { label: "上传", icon: "↑", cls: "st-up" },
  download: { label: "下载", icon: "↓", cls: "st-down" },
  deleteRemote: { label: "删远程", icon: "✗", cls: "st-del" },
  deleteLocal: { label: "删本地", icon: "✗", cls: "st-del" },
  conflict: { label: "冲突", icon: "⚠", cls: "st-conf" },
  same: { label: "一致", icon: "=", cls: "st-same" },
};

/** Resolve a diff entry + conflict policy into a concrete backend op. */
function entryToOp(e: DiffEntry, policy: ConflictPolicy): Op | null {
  switch (e.action) {
    case "upload":
      return "upload";
    case "download":
      return "download";
    case "deleteLocal":
      return "delLocal";
    case "deleteRemote":
      return "delRemote";
    case "conflict":
      if (policy === "local") return e.localSize == null ? "delRemote" : "upload";
      if (policy === "remote") return e.remoteSize == null ? "delLocal" : "download";
      if (policy === "skip") return null;
      // "newer"
      return (e.localMtime ?? 0) >= (e.remoteMtime ?? 0) ? "upload" : "download";
    default:
      return null;
  }
}

export default function App() {
  const [localPath, setLocalPath] = useState("");
  const [remotePath, setRemotePath] = useState("");
  const [sshEndpoint, setSshEndpoint] = useState<Extract<RemoteEndpoint,{kind:"ssh"}>>({kind:"ssh",profileId:"",path:""});
  const [remoteKind, setRemoteKind] = useState<"filesystem"|"ssh">("filesystem");
  const [sshReady, setSshReady] = useState(false);
  const [sshAuthReady, setSshAuthReady] = useState(false);
  const remoteEndpoint = useMemo<RemoteEndpoint>(() => remoteKind === "ssh" ? sshEndpoint : {kind:"filesystem",path:remotePath}, [remoteKind, sshEndpoint, remotePath]);
  const effectiveRemotePath = remoteEndpoint.path;
  const [mode, setMode] = useState<SyncMode>("mirror");
  const [conflictPolicy, setConflictPolicy] = useState<ConflictPolicy>("newer");
  const [useHash, setUseHash] = useState(false);
  const [concurrency, setConcurrency] = useState(4);
  const [ignoreText, setIgnoreText] = useState(DEFAULT_IGNORE);
  const [settingsReady, setSettingsReady] = useState(false);

  const [result, setResult] = useState<CompareResult | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [comparing, setComparing] = useState(false);
  const [scanProgress, setScanProgress] = useState<{ local: number; remote: number } | null>(
    null
  );
  const [syncing, setSyncing] = useState(false);
  /** A cancel request was sent for the running compare / sync. */
  const [cancelling, setCancelling] = useState(false);
  const [progress, setProgress] = useState<SyncProgress | null>(null);
  const [transfer, setTransfer] = useState<TransferProgress | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState<Side | null>(null);
  const [diffEntry, setDiffEntry] = useState<DiffEntry | null>(null);
  /** Action filter for tree + table view. "all" shows every changed entry. */
  const [actionFilter, setActionFilter] = useState<Action | "all">("all");
  /** Left pane width as % of results-body (draggable splitter). */
  const [leftPanePct, setLeftPanePct] = useState(46);
  const [splitting, setSplitting] = useState(false);
  const resultsBodyRef = useRef<HTMLDivElement>(null);

  const localRef = useRef<HTMLDivElement>(null);
  const remoteRef = useRef<HTMLDivElement>(null);
  /** performance.now() when a compare result was handed to React. */
  const resultAtRef = useRef<number | null>(null);

  const pushLog = useCallback((line: string) => {
    const now = new Date();
    const stamp = `${now.toLocaleTimeString("zh-CN", { hour12: false })}.${String(now.getMilliseconds()).padStart(3, "0")}`;
    setLog((prev) => [...prev.slice(-300), `[${stamp}] ${line}`]);
  }, []);

  const ignoreList = useMemo(
    () => ignoreText.split(",").map((s) => s.trim()).filter(Boolean),
    [ignoreText]
  );

  // Restore prefs from ~/.config/com.syncui.app/settings.json on startup.
  useEffect(() => {
    let cancelled = false;
    loadSettings()
      .then((s) => {
        if (cancelled) return;
        if (s.mode === "mirror" || s.mode === "mirror_pull" || s.mode === "twoway") {
          setMode(s.mode);
        }
        if (
          s.conflictPolicy === "newer" ||
          s.conflictPolicy === "local" ||
          s.conflictPolicy === "remote" ||
          s.conflictPolicy === "skip"
        ) {
          setConflictPolicy(s.conflictPolicy);
        }
        setUseHash(!!s.useHash);
        if (typeof s.concurrency === "number" && s.concurrency >= 1) {
          setConcurrency(Math.max(1, Math.min(32, s.concurrency)));
        }
        if (typeof s.ignoreText === "string") {
          setIgnoreText(s.ignoreText);
        }
        if (s.localPath) setLocalPath(s.localPath);
        if (s.remotePath) setRemotePath(s.remotePath);
        if (s.remoteEndpoint?.kind === "ssh") { setSshEndpoint(s.remoteEndpoint); setRemoteKind("ssh"); }
      })
      .catch(() => {
        /* keep defaults */
      })
      .finally(() => {
        if (!cancelled) setSettingsReady(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Debounced persist whenever prefs change (after initial load).
  useEffect(() => {
    if (!settingsReady) return;
    const timer = window.setTimeout(() => {
      saveSettings({
        mode,
        conflictPolicy,
        useHash,
        concurrency,
        ignoreText,
        localPath,
        remotePath,
        remoteEndpoint,
      }).catch(() => {
        /* ignore write errors in UI */
      });
    }, 300);
    return () => window.clearTimeout(timer);
  }, [
    settingsReady,
    mode,
    conflictPolicy,
    useHash,
    concurrency,
    ignoreText,
    localPath,
    remotePath,
    remoteEndpoint,
  ]);

  useEffect(() => { setResult(null); setSelected(new Set()); setDiffEntry(null); }, [localPath, remoteEndpoint, mode, useHash, ignoreText, sshReady]);

  const isTwoway = mode === "twoway";
  const isMirrorPull = mode === "mirror_pull";
  const showUpload = mode === "mirror" || isTwoway;
  const showDownload = isMirrorPull || isTwoway;
  const showDelRemote = mode === "mirror" || isTwoway;
  const showDelLocal = isMirrorPull || isTwoway;
  const modeLabel =
    mode === "twoway" ? "双向" : mode === "mirror_pull" ? "镜像←" : "镜像→";
  const arrowGlyph = isTwoway ? "⇄" : isMirrorPull ? "←" : "→";

  // Route a drop position to the nearest drop zone (Tauri reports logical px
  // on this stack; defensively scale down if a platform reports physical px).
  const sideAtPosition = useCallback((x: number, y: number): Side | null => {
    const dpr = window.devicePixelRatio || 1;
    let cx = x;
    let cy = y;
    if (x > window.innerWidth + 4 || y > window.innerHeight + 4) {
      cx = x / dpr;
      cy = y / dpr;
    }
    const center = (el: HTMLDivElement | null) => {
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
    };
    const lc = center(localRef.current);
    const rc = center(remoteRef.current);
    if (!lc || !rc) return cx < window.innerWidth / 2 ? "local" : "remote";
    const dist = (c: { x: number; y: number }) => Math.hypot(cx - c.x, cy - c.y);
    return dist(lc) <= dist(rc) ? "local" : "remote";
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    if (!isTauri()) return;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        if (comparing || syncing) return;
        const p = event.payload;
        if (p.type === "over") {
          setDragOver(sideAtPosition(p.position.x, p.position.y));
        } else if (p.type === "drop") {
          const side = sideAtPosition(p.position.x, p.position.y);
          setDragOver(null);
          if (side && p.paths.length > 0) {
            if (side === "local") setLocalPath(p.paths[0]);
            else if (remoteKind === "filesystem") setRemotePath(p.paths[0]);
          }
        } else {
          setDragOver(null);
        }
      })
      .then((fn) => (unlisten = fn));
    return () => {
      if (unlisten) unlisten();
    };
  }, [sideAtPosition, comparing, syncing, remoteKind]);

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let unlistenSync: (() => void) | undefined;
    let unlistenScan: (() => void) | undefined;
    let unlistenTransfer: (() => void) | undefined;
    let unlistenDiagnostics: (() => void) | undefined;
    listen<string>("diagnostic-log", (e) => pushLog(e.payload)).then((fn) => {
      if (disposed) fn();
      else unlistenDiagnostics = fn;
    });
    listen<TransferProgress>("sync-transfer-progress", (e) => {
      setTransfer(e.payload);
    }).then((fn) => {
      if (disposed) fn();
      else unlistenTransfer = fn;
    });
    listen<SyncProgress>("sync-progress", (e) => {
      const p = e.payload;
      setProgress(p);
      const verb =
        p.op === "delLocal" || p.op === "delRemote"
          ? "删除"
          : p.op === "download"
          ? "下载"
          : "上传";
      const mark = p.skipped ? "⊘" : p.ok ? "✓" : "✗";
      const tail = p.skipped ? " — 已跳过(文件已变动)" : p.error ? ` — ${p.error}` : "";
      const timing = Number.isFinite(p.elapsedMs)
        ? ` — ${p.elapsedMs.toFixed(2)}ms，复制 ${p.bytesCopied} B`
        : "";
      pushLog(`${mark} [${p.index}/${p.total}] ${verb} ${p.relPath}${timing}${tail}`);
    }).then((fn) => {
      if (disposed) fn();
      else unlistenSync = fn;
    });
    listen<{ phase: string; count: number }>("scan-progress", (e) => {
      const { phase, count } = e.payload;
      // Local and remote are scanned concurrently; keep both counters.
      setScanProgress((prev) =>
        prev ? { ...prev, [phase === "remote" ? "remote" : "local"]: count } : prev
      );
    }).then((fn) => {
      if (disposed) fn();
      else unlistenScan = fn;
    });
    return () => {
      disposed = true;
      if (unlistenSync) unlistenSync();
      if (unlistenScan) unlistenScan();
      if (unlistenTransfer) unlistenTransfer();
      if (unlistenDiagnostics) unlistenDiagnostics();
    };
  }, [pushLog]);

  useEffect(() => {
    if (!splitting) return;
    const onMove = (e: MouseEvent) => {
      const el = resultsBodyRef.current;
      if (!el) return;
      const rect = el.getBoundingClientRect();
      if (rect.width <= 0) return;
      const pct = ((e.clientX - rect.left) / rect.width) * 100;
      setLeftPanePct(Math.min(72, Math.max(26, pct)));
    };
    const onUp = () => setSplitting(false);
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
  }, [splitting]);

  const browse = async (side: Side) => {
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked === "string") {
      if (side === "local") setLocalPath(picked);
      else setRemotePath(picked);
    }
  };

  // Log how long the diff UI took to render, to spot regressions on big trees.
  useEffect(() => {
    const t0 = resultAtRef.current;
    if (!result || t0 == null) return;
    resultAtRef.current = null;
    const raf = requestAnimationFrame(() => {
      const ms = Math.round(performance.now() - t0);
      pushLog(`差异界面渲染：${ms}ms，${result.entries.length} 个变更项`);
    });
    return () => cancelAnimationFrame(raf);
  }, [result, pushLog]);

  const runCompare = async (reason: "manual" | "after-sync" = "manual") => {
    setError(null);
    if (!localPath || !effectiveRemotePath || (remoteKind === "ssh" && !sshReady)) {
      setError("请先指定本地目录和远程/挂载目录。");
      return;
    }
    setComparing(true);
    setCancelling(false);
    setResult(null);
    setSelected(new Set());
    setActionFilter("all");
    setDiffEntry(null);
    setScanProgress({ local: 0, remote: 0 });
    const compareStarted = performance.now();
    pushLog(`[诊断/前端] 开始${reason === "after-sync" ? "同步后自动" : "手动"}对比：mode=${mode} hash=${useHash}，忽略规则=${ignoreList.length}项`);
    try {
      const res = await compareDirs(localPath, effectiveRemotePath, {
        useHash,
        ignore: ignoreList,
        mode,
      }, remoteEndpoint);
      pushLog(`[诊断/前端] 对比命令往返：${(performance.now() - compareStarted).toFixed(2)}ms（含后台排队/扫描/哈希/结果传输）`);
      resultAtRef.current = performance.now();
      setResult(res);
      // Right pane only lists tree selection — start empty so user picks via the tree.
      setSelected(new Set());
      pushLog(
        `对比完成(${modeLabel})：↑${res.uploadCount} ↓${res.downloadCount} ` +
          `删远程${res.deleteRemoteCount} 删本地${res.deleteLocalCount} 冲突${res.conflictCount} 一致${res.sameCount}` +
          (res.skippedCount ? ` 跳过${res.skippedCount}` : "")
      );
    } catch (e) {
      if (String(e) === CANCELLED) pushLog("对比已取消");
      else setError(String(e));
    } finally {
      setComparing(false);
      setCancelling(false);
      setScanProgress(null);
      pushLog(`[诊断/前端] ${reason === "after-sync" ? "同步后自动" : "手动"}对比结束：${(performance.now() - compareStarted).toFixed(2)}ms`);
    }
  };

  const requestCancel = (kind: "compare" | "sync") => {
    setCancelling(true);
    (kind === "compare" ? cancelCompare() : cancelSync()).catch((e) => setError(String(e)));
  };

  const toggle = (rel: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(rel)) next.delete(rel);
      else next.add(rel);
      return next;
    });
  };

  const toggleFiles = (rels: string[], select: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev);
      for (const r of rels) {
        if (select) next.add(r);
        else next.delete(r);
      }
      return next;
    });
  };

  // The backend only returns entries that need attention (no "same").
  const changedEntries = useMemo(() => result?.entries ?? [], [result]);
  const changedByPath = useMemo(() => {
    const m = new Map<string, DiffEntry>();
    for (const e of changedEntries) m.set(e.relPath, e);
    return m;
  }, [changedEntries]);

  const visibleEntries = useMemo(
    () =>
      actionFilter === "all"
        ? changedEntries
        : changedEntries.filter((e) => e.action === actionFilter),
    [changedEntries, actionFilter]
  );
  const diffablePaths = useMemo(
    () => new Set(visibleEntries.filter((e) => DIFFABLE.has(e.action)).map((e) => e.relPath)),
    [visibleEntries]
  );

  const setAll = (on: boolean) => {
    const rels = visibleEntries.map((e) => e.relPath);
    if (on) {
      setSelected((prev) => {
        const next = new Set(prev);
        for (const r of rels) next.add(r);
        return next;
      });
    } else {
      setSelected((prev) => {
        const next = new Set(prev);
        for (const r of rels) next.delete(r);
        return next;
      });
    }
  };

  /** Per-action selection stats, used by the clickable summary badges. */
  const actionStats = useMemo(() => {
    const stats = {} as Record<Action, { total: number; selected: number }>;
    for (const e of changedEntries) {
      const s = (stats[e.action] ??= { total: 0, selected: 0 });
      s.total++;
      if (selected.has(e.relPath)) s.selected++;
    }
    return stats;
  }, [changedEntries, selected]);

  /** Toggle view filter for an action type (click again → show all). */
  const setFilter = (action: Action) => {
    setActionFilter((prev) => (prev === action ? "all" : action));
  };

  const selectedCount = useMemo(
    () => changedEntries.filter((e) => selected.has(e.relPath)).length,
    [changedEntries, selected]
  );

  const runSync = async () => {
    if (!result?.comparisonId) { setError("比较结果已失效，请重新对比后同步。"); return; }
    const items = changedEntries
      .filter((e) => selected.has(e.relPath))
      .map((e) => ({ relPath: e.relPath, op: entryToOp(e, conflictPolicy) }))
      .filter((x): x is { relPath: string; op: Op } => x.op !== null);
    if (items.length === 0) {
      setError("没有可执行的同步项（冲突可能被策略跳过）。");
      return;
    }
    setError(null);
    setSyncing(true);
    setCancelling(false);
    setProgress(null);
    setTransfer({ bytesCopied: 0, bytesPerSecond: 0, elapsedSeconds: 0, finished: false });
    const syncStarted = performance.now();
    const copyItems = items.filter((item) => item.op === "upload" || item.op === "download");
    const plannedBytes = copyItems.reduce((sum, item) => {
      const entry = changedByPath.get(item.relPath);
      return sum + (item.op === "upload" ? entry?.localSize ?? 0 : entry?.remoteSize ?? 0);
    }, 0);
    pushLog(`开始同步：${items.length} 项，正在统计传输速度…`);
    pushLog(`[诊断/前端] 任务预估：复制=${copyItems.length}项，预计=${plannedBytes} B，平均文件=${Math.round(plannedBytes / Math.max(1, copyItems.length))} B，workers=${concurrency}，hash=${useHash}`);
    try {
      const res = await syncEntries(result.comparisonId, [...selected], conflictPolicy, concurrency);
      const commandMs = performance.now() - syncStarted;
      pushLog(`[诊断/前端] 同步命令往返=${commandMs.toFixed(2)}ms；文件操作=${(res.elapsedSeconds * 1000).toFixed(2)}ms；其余=${Math.max(0, commandMs - res.elapsedSeconds * 1000).toFixed(2)}ms（含结果刷新/排队/通信/诊断等）`);
      const averageSpeed = res.elapsedSeconds > 0 ? res.bytesCopied / res.elapsedSeconds : 0;
      setTransfer({
        bytesCopied: res.bytesCopied,
        bytesPerSecond: averageSpeed,
        elapsedSeconds: res.elapsedSeconds,
        finished: true,
        logicalBytes: res.logicalBytes,
      });
      const summary =
        `↑${res.uploaded} ↓${res.downloaded} 删远程${res.deletedRemote} ` +
        `删本地${res.deletedLocal} 跳过${res.skipped} 失败${res.failed}，` +
        `已传输 ${formatTransferAmount(res.bytesCopied)}，平均速度 ${formatTransferAmount(averageSpeed)}/s`;
      pushLog(`同步结束：${summary}，文件完成量 ${formatTransferAmount(res.logicalBytes)}`);
      if (res.refreshed) { resultAtRef.current = performance.now(); setResult(res.refreshed); setSelected(new Set()); setDiffEntry(null); }
      else { setResult(null); setSelected(new Set()); setDiffEntry(null); }
      if (res.needsRecompare || res.cancelled) pushLog("任务部分完成或结果变化，请重新对比。");
      for (const message of res.errors) pushLog(message);
    } catch (e) {
      setResult(null); setSelected(new Set()); setDiffEntry(null);
      setError(String(e));
    } finally {
      setSyncing(false);
      setCancelling(false);
      setTransfer((prev) => prev ? {
        ...prev,
        finished: true,
        bytesPerSecond: prev.elapsedSeconds > 0 ? prev.bytesCopied / prev.elapsedSeconds : 0,
      } : prev);
      pushLog(`[诊断/前端] 同步流程整体耗时：${(performance.now() - syncStarted).toFixed(2)}ms（文件操作 + 单次扫描刷新 + 基线保存）`);
    }
  };

  const pct =
    progress && progress.total > 0 ? Math.round((progress.index / progress.total) * 100) : 0;

  const applyEntryUpdate = useCallback(
    (relPath: string, updated: DiffEntry | null) => {
      setResult((prev) => {
        if (!prev) return prev;
        const idx = prev.entries.findIndex((e) => e.relPath === relPath);
        if (idx < 0) return prev;
        const next = [...prev.entries];
        let sameCount = prev.sameCount;
        if (updated == null) {
          next.splice(idx, 1);
        } else if (updated.action === "same") {
          next.splice(idx, 1);
          sameCount++;
        } else {
          next[idx] = updated;
        }
        return recount(next, prev.skippedCount, sameCount);
      });
      if (updated == null || updated.action === "same") {
        setSelected((prev) => {
          const next = new Set(prev);
          next.delete(relPath);
          return next;
        });
        setDiffEntry(null);
      } else {
        setDiffEntry(updated);
      }
    },
    []
  );

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="logo">⇄</span> SyncUI
          <span className="subtitle">目录对比与同步</span>
        </div>
      </header>

      <SshAuthDialog onReady={setSshAuthReady} />
      <div className="endpoint-switch"><label>远端连接方式 <select disabled={comparing||syncing} value={remoteKind} onChange={e=>{setRemoteKind(e.target.value as "filesystem"|"ssh");setSshReady(false);}}><option value="filesystem">本地 / 挂载目录</option><option value="ssh">SSH 服务器</option></select></label>{remoteKind==="ssh"&&<span>远端直接扫描 · rsync 批量传输</span>}</div>
      <section className="zones">
        <DropZone
          ref={localRef}
          disabled={comparing || syncing}
          title="本地目录"
          hint="拖入文件夹，或点击浏览"
          path={localPath}
          active={dragOver === "local"}
          onBrowse={() => browse("local")}
          onClear={() => setLocalPath("")}
        />
        <div className="arrow">{arrowGlyph}</div>
        {remoteKind === "ssh" ? <SshConnection authReady={sshAuthReady} endpoint={sshEndpoint} onChange={setSshEndpoint} onReady={setSshReady} disabled={comparing||syncing}/> : <DropZone
          ref={remoteRef}
          disabled={comparing || syncing}
          title="远程 / 挂载目录"
          hint="sftp / smb / nfs 挂载点也可"
          path={remotePath}
          active={dragOver === "remote"}
          onBrowse={() => browse("remote")}
          onClear={() => setRemotePath("")}
        />}
      </section>

      <section className="controls">
        <label className="opt">
          模式
          <select disabled={comparing||syncing} value={mode} onChange={(e) => setMode(e.target.value as SyncMode)}>
            <option value="mirror">镜像（本地→远程）</option>
            <option value="mirror_pull">镜像（远程→本地）</option>
            <option value="twoway">双向（三方对比）</option>
          </select>
        </label>
        <label className="opt">
          冲突
          <select
            value={conflictPolicy}
            disabled={!isTwoway||comparing||syncing}
            onChange={(e) => setConflictPolicy(e.target.value as ConflictPolicy)}
          >
            <option value="newer">较新优先</option>
            <option value="local">用本地</option>
            <option value="remote">用远程</option>
            <option value="skip">跳过</option>
          </select>
        </label>
        <label className="opt">
          并发
          <input
            type="number"
            disabled={remoteKind==="ssh" || comparing || syncing}
            title={remoteKind==="ssh" ? "SSH 模式使用单批次流水传输" : "同步线程数"}
            min={1}
            max={32}
            value={concurrency}
            onChange={(e) => setConcurrency(Math.max(1, Math.min(32, +e.target.value || 1)))}
          />
        </label>
        <label className="opt">
          <input type="checkbox" disabled={comparing||syncing} checked={useHash} onChange={(e) => setUseHash(e.target.checked)} />
          哈希校验
        </label>
        <IgnoreRules
          value={ignoreList}
          disabled={!settingsReady || comparing || syncing}
          onChange={(rules) => {
            setIgnoreText(rules.join(", "));
            setResult(null);
            setSelected(new Set());
            setDiffEntry(null);
          }}
        />
        <button className="btn primary" onClick={() => void runCompare()} disabled={comparing || syncing || (remoteKind==="ssh" && !sshReady)}>
          {comparing ? "对比中…" : "对比 Compare"}
        </button>
      </section>

      {error && <div className="error">{error}</div>}

      {comparing && (
        <div className="scanning">
          <span className="spinner" />
          正在扫描… 本地 <b>{scanProgress?.local ?? 0}</b> / 远程{" "}
          <b>{scanProgress?.remote ?? 0}</b> 个文件
          <span className="scan-hint">（大目录请用"忽略"过滤以加速）</span>
          <button
            className="btn cancel-btn"
            onClick={() => requestCancel("compare")}
            disabled={cancelling}
          >
            {cancelling ? "正在停止…" : "取消对比"}
          </button>
        </div>
      )}

      {result && (
        <section className="results">
          <div className="summary">
            {showUpload && (
              <ActionBadge
                cls="st-up"
                label="上传"
                stat={actionStats.upload}
                active={actionFilter === "upload"}
                onToggle={() => setFilter("upload")}
              />
            )}
            {showDownload && (
              <ActionBadge
                cls="st-down"
                label="下载"
                stat={actionStats.download}
                active={actionFilter === "download"}
                onToggle={() => setFilter("download")}
              />
            )}
            {showDelRemote && (
              <ActionBadge
                cls="st-del"
                label="删远程"
                stat={actionStats.deleteRemote}
                active={actionFilter === "deleteRemote"}
                onToggle={() => setFilter("deleteRemote")}
              />
            )}
            {showDelLocal && (
              <ActionBadge
                cls="st-del"
                label="删本地"
                stat={actionStats.deleteLocal}
                active={actionFilter === "deleteLocal"}
                onToggle={() => setFilter("deleteLocal")}
              />
            )}
            {isTwoway && (
              <ActionBadge
                cls="st-conf"
                label="冲突"
                stat={actionStats.conflict}
                active={actionFilter === "conflict"}
                onToggle={() => setFilter("conflict")}
              />
            )}
            <Badge cls="st-same" n={result.sameCount} label="一致" />
            {result.skippedCount > 0 && (
              <span className="badge" title="符号链接 / 无法访问 / 已失效的项，已安全跳过">
                跳过 <b>{result.skippedCount}</b>
              </span>
            )}
            {actionFilter !== "all" && (
              <button className="link" onClick={() => setActionFilter("all")} title="显示全部差异">
                清除筛选
              </button>
            )}
            <div className="spacer" />
            <button
              className="link"
              onClick={() => setAll(true)}
              title={actionFilter === "all" ? "选中全部差异" : "选中当前筛选全部"}
            >
              全选{actionFilter !== "all" ? ` (${visibleEntries.length})` : ""}
            </button>
            <button
              className="link"
              onClick={() => setAll(false)}
              title={actionFilter === "all" ? "清空全部选中" : "取消当前筛选的选中"}
            >
              清空
            </button>
            <button
              className="btn primary"
              onClick={runSync}
              disabled={syncing || comparing || selectedCount === 0 || !result.comparisonId}
            >
              {syncing ? "同步中…" : `同步选中 (${selectedCount})`}
            </button>
          </div>

          {syncing && (
            <div className="progress-row">
              <div className="progress">
                <div className="bar" style={{ width: `${pct}%` }} />
                <span className="pct">{pct}%</span>
              </div>
              <button
                className="btn cancel-btn"
                onClick={() => requestCancel("sync")}
                disabled={cancelling}
              >
                {cancelling ? "正在停止…" : "取消同步"}
              </button>
            </div>
          )}

          <div
            ref={resultsBodyRef}
            className={`results-body${splitting ? " splitting" : ""}`}
          >
            <div className="pane-left" style={{ width: `${leftPanePct}%` }}>
              <SelectionTree
                entries={visibleEntries}
                selected={selected}
                activePath={diffEntry?.relPath ?? null}
                actionMeta={ACTION_META}
                diffable={diffablePaths}
                onToggleFile={toggle}
                onToggleFiles={toggleFiles}
                onDiff={(rel) => {
                  const e = changedByPath.get(rel);
                  if (e) setDiffEntry(e);
                }}
              />
            </div>
            <div
              className="pane-splitter"
              role="separator"
              aria-orientation="vertical"
              aria-label="拖动调整左右宽度"
              title="拖动调整左右宽度"
              onMouseDown={(e) => {
                e.preventDefault();
                setSplitting(true);
              }}
            />
            <div className="diff-host">
              {diffEntry ? (
                <DiffDrawer
                  key={diffEntry.relPath}
                  embedded
                  entry={diffEntry}
                  localRoot={localPath}
                  remoteRoot={effectiveRemotePath}
                  remoteEndpoint={remoteEndpoint}
                  disabled={syncing||comparing}
                  options={{ useHash, ignore: ignoreList, mode }}
                  onClose={() => setDiffEntry(null)}
                  onEntryUpdated={(updated) => { applyEntryUpdate(diffEntry.relPath, updated); pushLog("文件已保存，请重新对比后再同步。"); }}
                />
              ) : (
                <div className="diff-placeholder">
                  <div className="diff-placeholder-title">文件差异</div>
                  <p>在左侧点击文件行的 Diff，在此查看内容差异后再决定是否勾选同步。</p>
                </div>
              )}
            </div>
          </div>
        </section>
      )}

      {(log.length > 0 || syncing) && (
        <section className="logbox" aria-label="同步日志">
          {transfer && (
            <div className="log-transfer-status">
              <span>
                {transfer.finished ? "平均同步速度" : "当前同步速度"}
                <strong>{formatTransferAmount(transfer.bytesPerSecond)}/s</strong>
              </span>
              <span title={remoteKind==="ssh" ? "统计 SSH 加密前的双向协议字节，不含 SSH 封装" : "实际复制的文件字节"}>已传输 {formatTransferAmount(transfer.bytesCopied)}</span>
              {transfer.logicalBytes != null && <span>文件处理量 {formatTransferAmount(transfer.logicalBytes)}</span>}
            </div>
          )}
          {log.map((l, i) => (
            <div key={i} className="logline">
              {l}
            </div>
          ))}
        </section>
      )}
    </div>
  );
}

function Badge({ cls, n, label }: { cls: string; n: number; label: string }) {
  return (
    <span className={`badge ${cls}`}>
      {label} <b>{n}</b>
    </span>
  );
}

/**
 * Summary badge: click filters the tree + table to this action type
 * (click again to clear). Shows selection stats for that type.
 */
function ActionBadge({
  cls,
  label,
  stat,
  active,
  onToggle,
}: {
  cls: string;
  label: string;
  stat: { total: number; selected: number } | undefined;
  active: boolean;
  onToggle: () => void;
}) {
  const total = stat?.total ?? 0;
  const sel = stat?.selected ?? 0;
  const state = total === 0 ? "empty" : sel === 0 ? "none" : sel === total ? "all" : "part";
  return (
    <button
      type="button"
      className={`badge action ${cls} sel-${state}${active ? " filter-on" : ""}`}
      disabled={total === 0}
      onClick={onToggle}
      title={
        total === 0
          ? "无此类差异"
          : active
          ? `取消筛选「${label}」`
          : `筛选「${label}」（树与列表仅显示此类）`
      }
    >
      <span className="badge-check">{state === "all" ? "☑" : state === "part" ? "◪" : "☐"}</span>
      {label} <b>{state === "part" ? `${sel}/${total}` : total}</b>
    </button>
  );
}

const DropZone = forwardRef<
  HTMLDivElement,
  {
    title: string;
    hint: string;
    path: string;
    active: boolean;
    disabled: boolean;
    onBrowse: () => void;
    onClear: () => void;
  }
>(({ title, hint, path, active, disabled, onBrowse, onClear }, ref) => {
  return (
    <div ref={ref} className={`zone ${active ? "active" : ""} ${path ? "filled" : ""}`}>
      <div className="zone-title">{title}</div>
      {path ? (
        <>
          <div className="zone-path" title={path}>
            {path}
          </div>
          <div className="zone-actions">
            <button className="link" disabled={disabled} onClick={onBrowse}>
              更换
            </button>
            <button className="link" disabled={disabled} onClick={onClear}>
              清除
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="zone-hint">{hint}</div>
          <button className="btn" disabled={disabled} onClick={onBrowse}>
            浏览…
          </button>
        </>
      )}
    </div>
  );
});
