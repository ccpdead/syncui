import { useEffect, useMemo, useRef, useState, useCallback } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import {
  compareDirs,
  syncEntries,
  CompareResult,
  DiffEntry,
  SyncProgress,
} from "./api";

type Side = "local" | "remote";

const STATUS_META: Record<
  string,
  { label: string; icon: string; cls: string }
> = {
  new: { label: "新增", icon: "✚", cls: "st-new" },
  modified: { label: "已修改", icon: "✎", cls: "st-mod" },
  deleted: { label: "远程独有", icon: "−", cls: "st-del" },
  same: { label: "一致", icon: "=", cls: "st-same" },
};

function fmtSize(n: number | null): string {
  if (n == null) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 10 || i === 0 ? 0 : 1)}${units[i]}`;
}

export default function App() {
  const [localPath, setLocalPath] = useState("");
  const [remotePath, setRemotePath] = useState("");
  const [useHash, setUseHash] = useState(false);
  const [ignoreText, setIgnoreText] = useState(".git, node_modules, .DS_Store");
  const [includeDeletes, setIncludeDeletes] = useState(false);

  const [result, setResult] = useState<CompareResult | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [comparing, setComparing] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [progress, setProgress] = useState<SyncProgress | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState<Side | null>(null);
  const [dbg, setDbg] = useState<string>("(尚无拖拽事件)");

  const localRef = useRef<HTMLDivElement>(null);
  const remoteRef = useRef<HTMLDivElement>(null);

  const pushLog = useCallback((line: string) => {
    setLog((prev) => [...prev.slice(-300), line]);
  }, []);

  // Route a drop position to the nearest drop zone.
  //
  // Tauri's drag-drop position is documented as physical pixels, but on this
  // Linux/WebKitGTK stack it actually arrives in *logical* (CSS) pixels.
  // getBoundingClientRect() is also logical, so we compare directly. As a
  // defensive measure for platforms that genuinely report physical pixels
  // (values exceeding the logical viewport), we scale those down by dpr.
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
    if (!lc || !rc) {
      return cx < window.innerWidth / 2 ? "local" : "remote";
    }
    const dist = (c: { x: number; y: number }) =>
      Math.hypot(cx - c.x, cy - c.y);
    return dist(lc) <= dist(rc) ? "local" : "remote";
  }, []);

  // Native OS file/folder drag-drop handling (Tauri intercepts these at the
  // window level, so we hit-test the cursor position to route to a zone).
  useEffect(() => {
    // Emit static viewport info to the terminal once, for diagnosis.
    invoke("debug_log", {
      msg: `mount dpr=${window.devicePixelRatio} innerW=${window.innerWidth} innerH=${window.innerHeight}`,
    }).catch(() => {});
    let unlisten: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        const p = event.payload;
        if (p.type === "over") {
          setDragOver(sideAtPosition(p.position.x, p.position.y));
        } else if (p.type === "drop") {
          const side = sideAtPosition(p.position.x, p.position.y);
          const lr = localRef.current?.getBoundingClientRect();
          const rr = remoteRef.current?.getBoundingClientRect();
          const info =
            `drop raw=(${Math.round(p.position.x)},${Math.round(
              p.position.y
            )}) dpr=${window.devicePixelRatio} innerW=${window.innerWidth} ` +
            `Lc=${lr ? Math.round((lr.left + lr.right) / 2) : "?"} ` +
            `Rc=${rr ? Math.round((rr.left + rr.right) / 2) : "?"} -> ${side}`;
          setDbg(info);
          invoke("debug_log", { msg: info }).catch(() => {});
          setDragOver(null);
          if (side && p.paths.length > 0) {
            const path = p.paths[0];
            if (side === "local") setLocalPath(path);
            else setRemotePath(path);
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

  // Listen for sync progress events from the backend.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<SyncProgress>("sync-progress", (e) => {
      const p = e.payload;
      setProgress(p);
      const verb = p.action === "delete" ? "删除" : "复制";
      pushLog(
        `${p.ok ? "✓" : "✗"} [${p.index}/${p.total}] ${verb} ${p.relPath}` +
          (p.error ? ` — ${p.error}` : "")
      );
    }).then((fn) => (unlisten = fn));
    return () => {
      if (unlisten) unlisten();
    };
  }, [pushLog]);

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
    try {
      const ignore = ignoreText
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean);
      const res = await compareDirs(localPath, remotePath, { useHash, ignore });
      setResult(res);
      // Pre-select everything that represents a change (not "same").
      const preset = new Set(
        res.entries
          .filter((e) => e.status !== "same")
          .map((e) => e.relPath)
      );
      setSelected(preset);
      pushLog(
        `对比完成：新增 ${res.newCount} · 修改 ${res.modifiedCount} · 远程独有 ${res.deletedCount} · 一致 ${res.sameCount}`
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setComparing(false);
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

  const changedEntries = useMemo(
    () => (result ? result.entries.filter((e) => e.status !== "same") : []),
    [result]
  );

  const setAll = (on: boolean) => {
    if (on) setSelected(new Set(changedEntries.map((e) => e.relPath)));
    else setSelected(new Set());
  };

  const runSync = async () => {
    if (!result) return;
    const items = changedEntries
      .filter((e) => selected.has(e.relPath))
      .map((e) => ({ relPath: e.relPath, status: e.status }));
    if (items.length === 0) {
      setError("没有选中任何待同步项。");
      return;
    }
    setError(null);
    setSyncing(true);
    setProgress(null);
    try {
      const res = await syncEntries(localPath, remotePath, items, includeDeletes);
      pushLog(
        `同步结束：复制 ${res.copied} · 删除 ${res.deleted} · 失败 ${res.failed}`
      );
      // Refresh the diff so the table reflects the new state.
      await runCompare();
    } catch (e) {
      setError(String(e));
    } finally {
      setSyncing(false);
    }
  };

  const selectedCount = useMemo(
    () => changedEntries.filter((e) => selected.has(e.relPath)).length,
    [changedEntries, selected]
  );

  const pct =
    progress && progress.total > 0
      ? Math.round((progress.index / progress.total) * 100)
      : 0;

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="logo">⇄</span> SyncUI
          <span className="subtitle">目录对比与同步（本地 → 远程/挂载）</span>
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
        <div className="arrow">→</div>
        <DropZone
          ref={remoteRef}
          title="远程 / 挂载目录"
          hint="sftp / smb 挂载点也可（拖入或浏览）"
          path={remotePath}
          active={dragOver === "remote"}
          onBrowse={() => browse("remote")}
          onClear={() => setRemotePath("")}
        />
      </section>

      <div className="dbg">
        DEBUG: dpr={window.devicePixelRatio} innerW={window.innerWidth} innerH=
        {window.innerHeight} | {dbg}
      </div>

      <section className="controls">
        <label className="opt">
          <input
            type="checkbox"
            checked={useHash}
            onChange={(e) => setUseHash(e.target.checked)}
          />
          内容哈希校验（更准，更慢）
        </label>
        <label className="opt">
          <input
            type="checkbox"
            checked={includeDeletes}
            onChange={(e) => setIncludeDeletes(e.target.checked)}
          />
          同步删除（移除远程独有文件）
        </label>
        <label className="opt ignore">
          忽略：
          <input
            type="text"
            value={ignoreText}
            onChange={(e) => setIgnoreText(e.target.value)}
            placeholder=".git, node_modules"
          />
        </label>
        <button
          className="btn primary"
          onClick={runCompare}
          disabled={comparing || syncing}
        >
          {comparing ? "对比中…" : "对比 Compare"}
        </button>
      </section>

      {error && <div className="error">{error}</div>}

      {result && (
        <section className="results">
          <div className="summary">
            <Badge cls="st-new" n={result.newCount} label="新增" />
            <Badge cls="st-mod" n={result.modifiedCount} label="修改" />
            <Badge cls="st-del" n={result.deletedCount} label="远程独有" />
            <Badge cls="st-same" n={result.sameCount} label="一致" />
            <div className="spacer" />
            <button className="link" onClick={() => setAll(true)}>
              全选
            </button>
            <button className="link" onClick={() => setAll(false)}>
              清空
            </button>
            <button
              className="btn primary"
              onClick={runSync}
              disabled={syncing || selectedCount === 0}
            >
              {syncing ? "同步中…" : `同步选中 (${selectedCount}) →`}
            </button>
          </div>

          {syncing && (
            <div className="progress">
              <div className="bar" style={{ width: `${pct}%` }} />
              <span className="pct">{pct}%</span>
            </div>
          )}

          <div className="table">
            <div className="row head">
              <span className="c-check" />
              <span className="c-status">状态</span>
              <span className="c-path">相对路径</span>
              <span className="c-size">本地</span>
              <span className="c-size">远程</span>
              <span className="c-time">较新</span>
            </div>
            {changedEntries.length === 0 && (
              <div className="empty">没有差异，两端一致 🎉</div>
            )}
            {changedEntries.map((e) => (
              <DiffRow
                key={e.relPath}
                e={e}
                checked={selected.has(e.relPath)}
                onToggle={() => toggle(e.relPath)}
              />
            ))}
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

import { forwardRef } from "react";

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

function DiffRow({
  e,
  checked,
  onToggle,
}: {
  e: DiffEntry;
  checked: boolean;
  onToggle: () => void;
}) {
  const meta = STATUS_META[e.status];
  return (
    <label className="row">
      <span className="c-check">
        <input type="checkbox" checked={checked} onChange={onToggle} />
      </span>
      <span className={`c-status ${meta.cls}`}>
        {meta.icon} {meta.label}
      </span>
      <span className="c-path" title={e.relPath}>
        {e.relPath}
      </span>
      <span className="c-size">{fmtSize(e.localSize)}</span>
      <span className="c-size">{fmtSize(e.remoteSize)}</span>
      <span className="c-time">
        {e.newer === "local" ? "本地" : e.newer === "remote" ? "远程" : "—"}
      </span>
    </label>
  );
}
