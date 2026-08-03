import { useEffect, useMemo, useRef, useState, useCallback, forwardRef } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  compareDirs,
  syncEntries,
  loadSettings,
  saveSettings,
  CompareResult,
  DiffEntry,
  SyncProgress,
  Action,
  Op,
  SyncMode,
  ConflictPolicy,
} from "./api";
import DiffDrawer from "./DiffDrawer";
import SelectionTree from "./SelectionTree";

const DIFFABLE: ReadonlySet<Action> = new Set(["upload", "download", "conflict"]);

function recount(entries: DiffEntry[], skippedCount: number): CompareResult {
  const counts = {
    uploadCount: 0,
    downloadCount: 0,
    deleteLocalCount: 0,
    deleteRemoteCount: 0,
    conflictCount: 0,
    sameCount: 0,
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
      if (policy === "local") return "upload";
      if (policy === "remote") return "download";
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
  const [mode, setMode] = useState<SyncMode>("mirror");
  const [conflictPolicy, setConflictPolicy] = useState<ConflictPolicy>("newer");
  const [useHash, setUseHash] = useState(false);
  const [concurrency, setConcurrency] = useState(4);
  const [ignoreText, setIgnoreText] = useState(DEFAULT_IGNORE);
  const [settingsReady, setSettingsReady] = useState(false);

  const [result, setResult] = useState<CompareResult | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [comparing, setComparing] = useState(false);
  const [scanProgress, setScanProgress] = useState<{ phase: string; count: number } | null>(
    null
  );
  const [syncing, setSyncing] = useState(false);
  const [progress, setProgress] = useState<SyncProgress | null>(null);
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

  const pushLog = useCallback((line: string) => {
    setLog((prev) => [...prev.slice(-300), line]);
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
        if (typeof s.ignoreText === "string" && s.ignoreText.length > 0) {
          setIgnoreText(s.ignoreText);
        }
        if (s.localPath) setLocalPath(s.localPath);
        if (s.remotePath) setRemotePath(s.remotePath);
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
  ]);

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
    getCurrentWebview()
      .onDragDropEvent((event) => {
        const p = event.payload;
        if (p.type === "over") {
          setDragOver(sideAtPosition(p.position.x, p.position.y));
        } else if (p.type === "drop") {
          const side = sideAtPosition(p.position.x, p.position.y);
          setDragOver(null);
          if (side && p.paths.length > 0) {
            if (side === "local") setLocalPath(p.paths[0]);
            else setRemotePath(p.paths[0]);
          }
        } else {
          setDragOver(null);
        }
      })
      .then((fn) => (unlisten = fn));
    return () => {
      if (unlisten) unlisten();
    };
  }, [sideAtPosition]);

  useEffect(() => {
    let unlistenSync: (() => void) | undefined;
    let unlistenScan: (() => void) | undefined;
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
      pushLog(`${mark} [${p.index}/${p.total}] ${verb} ${p.relPath}${tail}`);
    }).then((fn) => (unlistenSync = fn));
    listen<{ phase: string; count: number }>("scan-progress", (e) => {
      setScanProgress(e.payload);
    }).then((fn) => (unlistenScan = fn));
    return () => {
      if (unlistenSync) unlistenSync();
      if (unlistenScan) unlistenScan();
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

  const runCompare = async () => {
    setError(null);
    if (!localPath || !remotePath) {
      setError("请先指定本地目录和远程/挂载目录。");
      return;
    }
    setComparing(true);
    setResult(null);
    setSelected(new Set());
    setActionFilter("all");
    setScanProgress({ phase: "local", count: 0 });
    try {
      const res = await compareDirs(localPath, remotePath, {
        useHash,
        ignore: ignoreList,
        mode,
      });
      setResult(res);
      // Right pane only lists tree selection — start empty so user picks via the tree.
      setSelected(new Set());
      pushLog(
        `对比完成(${modeLabel})：↑${res.uploadCount} ↓${res.downloadCount} ` +
          `删远程${res.deleteRemoteCount} 删本地${res.deleteLocalCount} 冲突${res.conflictCount} 一致${res.sameCount}` +
          (res.skippedCount ? ` 跳过${res.skippedCount}` : "")
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setComparing(false);
      setScanProgress(null);
    }
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

  const changedEntries = useMemo(
    () => (result ? result.entries.filter((e) => e.action !== "same") : []),
    [result]
  );

  const visibleEntries = useMemo(
    () =>
      actionFilter === "all"
        ? changedEntries
        : changedEntries.filter((e) => e.action === actionFilter),
    [changedEntries, actionFilter]
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
    if (!result) return;
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
    setProgress(null);
    try {
      const res = await syncEntries(localPath, remotePath, items, ignoreList, concurrency);
      pushLog(
        `同步结束：↑${res.uploaded} ↓${res.downloaded} 删远程${res.deletedRemote} ` +
          `删本地${res.deletedLocal} 跳过${res.skipped} 失败${res.failed}`
      );
      await runCompare();
    } catch (e) {
      setError(String(e));
    } finally {
      setSyncing(false);
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
        if (updated == null) {
          next.splice(idx, 1);
        } else if (updated.action === "same") {
          next[idx] = updated;
        } else {
          next[idx] = updated;
        }
        return recount(next, prev.skippedCount);
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

      <section className="zones">
        <DropZone
          ref={localRef}
          title="本地目录"
          hint="拖入文件夹，或点击浏览"
          path={localPath}
          active={dragOver === "local"}
          onBrowse={() => browse("local")}
          onClear={() => setLocalPath("")}
        />
        <div className="arrow">{arrowGlyph}</div>
        <DropZone
          ref={remoteRef}
          title="远程 / 挂载目录"
          hint="sftp / smb / nfs 挂载点也可"
          path={remotePath}
          active={dragOver === "remote"}
          onBrowse={() => browse("remote")}
          onClear={() => setRemotePath("")}
        />
      </section>

      <section className="controls">
        <label className="opt">
          模式
          <select value={mode} onChange={(e) => setMode(e.target.value as SyncMode)}>
            <option value="mirror">镜像（本地→远程）</option>
            <option value="mirror_pull">镜像（远程→本地）</option>
            <option value="twoway">双向（三方对比）</option>
          </select>
        </label>
        <label className="opt">
          冲突
          <select
            value={conflictPolicy}
            disabled={!isTwoway}
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
            min={1}
            max={32}
            value={concurrency}
            onChange={(e) => setConcurrency(Math.max(1, Math.min(32, +e.target.value || 1)))}
          />
        </label>
        <label className="opt">
          <input type="checkbox" checked={useHash} onChange={(e) => setUseHash(e.target.checked)} />
          哈希校验
        </label>
        <label className="opt ignore">
          忽略
          <input
            type="text"
            value={ignoreText}
            onChange={(e) => setIgnoreText(e.target.value)}
            placeholder=".git, node_modules"
          />
        </label>
        <button className="btn primary" onClick={runCompare} disabled={comparing || syncing}>
          {comparing ? "对比中…" : "对比 Compare"}
        </button>
      </section>

      {error && <div className="error">{error}</div>}

      {comparing && (
        <div className="scanning">
          <span className="spinner" />
          正在扫描{scanProgress?.phase === "remote" ? "远程" : "本地"}目录… 已发现{" "}
          <b>{scanProgress?.count ?? 0}</b> 个文件
          <span className="scan-hint">（大目录请用"忽略"过滤以加速）</span>
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
              disabled={syncing || selectedCount === 0}
            >
              {syncing ? "同步中…" : `同步选中 (${selectedCount})`}
            </button>
          </div>

          {syncing && (
            <div className="progress">
              <div className="bar" style={{ width: `${pct}%` }} />
              <span className="pct">{pct}%</span>
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
                diffable={new Set(
                  visibleEntries.filter((e) => DIFFABLE.has(e.action)).map((e) => e.relPath)
                )}
                onToggleFile={toggle}
                onToggleFiles={toggleFiles}
                onDiff={(rel) => {
                  const e =
                    visibleEntries.find((x) => x.relPath === rel) ??
                    changedEntries.find((x) => x.relPath === rel);
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
                  remoteRoot={remotePath}
                  options={{ useHash, ignore: ignoreList, mode }}
                  onClose={() => setDiffEntry(null)}
                  onEntryUpdated={(updated) => applyEntryUpdate(diffEntry.relPath, updated)}
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

      {log.length > 0 && (
        <section className="logbox">
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
    onBrowse: () => void;
    onClear: () => void;
  }
>(({ title, hint, path, active, onBrowse, onClear }, ref) => {
  return (
    <div ref={ref} className={`zone ${active ? "active" : ""} ${path ? "filled" : ""}`}>
      <div className="zone-title">{title}</div>
      {path ? (
        <>
          <div className="zone-path" title={path}>
            {path}
          </div>
          <div className="zone-actions">
            <button className="link" onClick={onBrowse}>
              更换
            </button>
            <button className="link" onClick={onClear}>
              清除
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="zone-hint">{hint}</div>
          <button className="btn" onClick={onBrowse}>
            浏览…
          </button>
        </>
      )}
    </div>
  );
});
