import { useCallback, useEffect, useMemo, useState } from "react";
import { diffLines, createTwoFilesPatch } from "diff";
import CodeEditor from "@uiw/react-textarea-code-editor";
import "@uiw/react-textarea-code-editor/dist.css";
import {
  compareOneEntry,
  readFilePair,
  writeFileText,
  CompareOptions,
  DiffEntry,
  FileTextPair,
  WriteSide,
} from "./api";

type ViewMode = "side" | "unified";

const MAX_LABEL = "5MB";

function langFromPath(relPath: string): string {
  const ext = relPath.split(".").pop()?.toLowerCase() ?? "";
  const map: Record<string, string> = {
    js: "javascript",
    jsx: "jsx",
    ts: "typescript",
    tsx: "tsx",
    py: "python",
    rs: "rust",
    go: "go",
    java: "java",
    c: "c",
    h: "c",
    cpp: "cpp",
    cc: "cpp",
    cxx: "cpp",
    hpp: "cpp",
    cs: "csharp",
    rb: "ruby",
    php: "php",
    sh: "shell",
    bash: "shell",
    zsh: "shell",
    yaml: "yaml",
    yml: "yaml",
    json: "json",
    toml: "toml",
    xml: "xml",
    html: "html",
    css: "css",
    scss: "scss",
    md: "markdown",
    markdown: "markdown",
    sql: "sql",
    conf: "ini",
    ini: "ini",
    cfg: "ini",
    launch: "xml",
    rviz: "yaml",
  };
  return map[ext] ?? "text";
}

function sideBlocked(side: FileTextPair["local"]): string | null {
  if (side.error) return side.error;
  if (side.tooLarge) return `文件超过 ${MAX_LABEL}（${fmtBytes(side.size)}），不展示内容`;
  if (side.binary) return "二进制文件，不支持内容对比";
  if (!side.exists) return "文件不存在";
  return null;
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
  return `${(n / (1024 * 1024)).toFixed(1)}MB`;
}

function lineCount(text: string): number {
  if (text.length === 0) return 1;
  let n = 1;
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10) n++;
  return n;
}

export interface DiffDrawerProps {
  entry: DiffEntry;
  localRoot: string;
  remoteRoot: string;
  options: CompareOptions;
  onClose: () => void;
  /** Called after a successful save + recompare. `null` means path vanished. */
  onEntryUpdated: (entry: DiffEntry | null) => void;
}

export default function DiffDrawer({
  entry,
  localRoot,
  remoteRoot,
  options,
  onClose,
  onEntryUpdated,
}: DiffDrawerProps) {
  const [view, setView] = useState<ViewMode>("side");
  const [showLineNumbers, setShowLineNumbers] = useState(true);
  const [loading, setLoading] = useState(true);
  const [pair, setPair] = useState<FileTextPair | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [localText, setLocalText] = useState("");
  const [remoteText, setRemoteText] = useState("");
  const [savedLocal, setSavedLocal] = useState("");
  const [savedRemote, setSavedRemote] = useState("");
  const [saving, setSaving] = useState<WriteSide | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  const language = useMemo(() => langFromPath(entry.relPath), [entry.relPath]);

  const reload = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    setStatus(null);
    try {
      const p = await readFilePair(localRoot, remoteRoot, entry.relPath);
      setPair(p);
      const l = p.local.content ?? "";
      const r = p.remote.content ?? "";
      setLocalText(l);
      setRemoteText(r);
      setSavedLocal(l);
      setSavedRemote(r);
    } catch (e) {
      setLoadError(String(e));
      setPair(null);
    } finally {
      setLoading(false);
    }
  }, [localRoot, remoteRoot, entry.relPath]);

  useEffect(() => {
    void reload();
  }, [reload]);

  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const localBlock = pair ? sideBlocked(pair.local) : null;
  const remoteBlock = pair ? sideBlocked(pair.remote) : null;
  const canEdit =
    !!pair &&
    !localBlock &&
    !remoteBlock &&
    pair.local.content != null &&
    pair.remote.content != null;

  const localDirty = canEdit && localText !== savedLocal;
  const remoteDirty = canEdit && remoteText !== savedRemote;

  const unifiedText = useMemo(() => {
    if (!canEdit) return "";
    return createTwoFilesPatch(
      `a/${entry.relPath}`,
      `b/${entry.relPath}`,
      localText,
      remoteText,
      "本地",
      "远程",
      { context: 3 }
    );
  }, [canEdit, entry.relPath, localText, remoteText]);

  const saveSide = async (side: WriteSide) => {
    if (!canEdit) return;
    setSaving(side);
    setStatus(null);
    try {
      const content = side === "local" ? localText : remoteText;
      await writeFileText(localRoot, remoteRoot, entry.relPath, side, content);
      if (side === "local") setSavedLocal(content);
      else setSavedRemote(content);

      const updated = await compareOneEntry(localRoot, remoteRoot, entry.relPath, options);
      onEntryUpdated(updated);
      setStatus(
        updated == null
          ? `${side === "local" ? "本地" : "远程"}已保存；该路径已不存在`
          : updated.action === "same"
          ? `${side === "local" ? "本地" : "远程"}已保存；两端已一致`
          : `${side === "local" ? "本地" : "远程"}已保存；已刷新对比结果`
      );
    } catch (e) {
      setStatus(`保存失败: ${e}`);
    } finally {
      setSaving(null);
    }
  };

  const warnParts: string[] = [];
  if (localBlock) warnParts.push(`本地: ${localBlock}`);
  if (remoteBlock) warnParts.push(`远程: ${remoteBlock}`);

  return (
    <div className="drawer-root" role="dialog" aria-modal="true">
      <div className="drawer-backdrop" onClick={onClose} />
      <aside className="drawer-panel">
        <header className="drawer-head">
          <div className="drawer-title">
            <span className="drawer-label">Diff</span>
            <span className="drawer-path" title={entry.relPath}>
              {entry.relPath}
            </span>
          </div>
          <div className="drawer-actions">
            <div className="seg">
              <button
                type="button"
                className={view === "side" ? "on" : ""}
                onClick={() => setView("side")}
              >
                对照
              </button>
              <button
                type="button"
                className={view === "unified" ? "on" : ""}
                onClick={() => setView("unified")}
              >
                Unified
              </button>
            </div>
            <label className="opt drawer-opt">
              <input
                type="checkbox"
                checked={showLineNumbers}
                onChange={(e) => setShowLineNumbers(e.target.checked)}
              />
              行号
            </label>
            <button
              type="button"
              className="btn"
              disabled={!canEdit || !localDirty || saving !== null}
              onClick={() => void saveSide("local")}
            >
              {saving === "local" ? "保存中…" : "保存本地"}
            </button>
            <button
              type="button"
              className="btn"
              disabled={!canEdit || !remoteDirty || saving !== null}
              onClick={() => void saveSide("remote")}
            >
              {saving === "remote" ? "保存中…" : "保存远程"}
            </button>
            <button type="button" className="btn" onClick={onClose}>
              关闭
            </button>
          </div>
        </header>

        {status && <div className="drawer-status">{status}</div>}
        {warnParts.length > 0 && (
          <div className="drawer-warn">
            {warnParts.map((w) => (
              <div key={w}>⚠ {w}</div>
            ))}
          </div>
        )}
        {loadError && <div className="error">{loadError}</div>}
        {loading && (
          <div className="drawer-loading">
            <span className="spinner" /> 正在读取文件…
          </div>
        )}

        {!loading && canEdit && view === "side" && (
          <div className="drawer-split">
            <EditorPane
              title="本地"
              dirty={localDirty}
              value={localText}
              language={language}
              showLineNumbers={showLineNumbers}
              onChange={setLocalText}
            />
            <EditorPane
              title="远程"
              dirty={remoteDirty}
              value={remoteText}
              language={language}
              showLineNumbers={showLineNumbers}
              onChange={setRemoteText}
            />
          </div>
        )}

        {!loading && canEdit && view === "unified" && (
          <UnifiedView
            text={unifiedText}
            showLineNumbers={showLineNumbers}
            localText={localText}
            remoteText={remoteText}
          />
        )}

        {!loading && !canEdit && !loadError && warnParts.length === 0 && (
          <div className="drawer-empty">无法展示差异</div>
        )}
      </aside>
    </div>
  );
}

function EditorPane({
  title,
  dirty,
  value,
  language,
  showLineNumbers,
  onChange,
}: {
  title: string;
  dirty: boolean;
  value: string;
  language: string;
  showLineNumbers: boolean;
  onChange: (v: string) => void;
}) {
  const lines = lineCount(value);
  return (
    <div className="drawer-pane">
      <div className="pane-head">
        {title} {dirty ? <em className="dirty">已修改</em> : null}
      </div>
      <div className="pane-body">
        {showLineNumbers && (
          <div className="line-gutter" aria-hidden>
            {Array.from({ length: lines }, (_, i) => (
              <div key={i}>{i + 1}</div>
            ))}
          </div>
        )}
        <div className="pane-editor">
          <CodeEditor
            value={value}
            language={language}
            placeholder={`${title}内容`}
            onChange={(ev) => onChange(ev.target.value)}
            padding={12}
            data-color-mode="dark"
            style={{
              fontSize: 12.5,
              backgroundColor: "#0b0e13",
              fontFamily: 'ui-monospace, "Cascadia Code", monospace',
              minHeight: "100%",
            }}
          />
        </div>
      </div>
    </div>
  );
}

/** Read-only unified diff with +/- coloring (git-style). */
function UnifiedView({
  text,
  showLineNumbers,
  localText,
  remoteText,
}: {
  text: string;
  showLineNumbers: boolean;
  localText: string;
  remoteText: string;
}) {
  const lines = useMemo(() => text.split("\n"), [text]);
  const stats = useMemo(() => {
    const parts = diffLines(localText, remoteText);
    let added = 0;
    let removed = 0;
    for (const p of parts) {
      const n = p.count ?? 0;
      if (p.added) added += n;
      if (p.removed) removed += n;
    }
    return { added, removed };
  }, [localText, remoteText]);

  return (
    <div className="unified-wrap">
      <div className="unified-meta">
        <span className="st-del">−{stats.removed}</span>
        <span className="st-up">+{stats.added}</span>
        <span className="muted">只读 · 请切换到「对照」编辑</span>
      </div>
      <pre className="unified-pre">
        {lines.map((line, i) => {
          let cls = "u-ctx";
          if (line.startsWith("+++") || line.startsWith("---")) cls = "u-file";
          else if (line.startsWith("@@")) cls = "u-hunk";
          else if (line.startsWith("+")) cls = "u-add";
          else if (line.startsWith("-")) cls = "u-del";
          return (
            <div key={i} className={`u-line ${cls}`}>
              {showLineNumbers && <span className="u-ln">{i + 1}</span>}
              <span className="u-text">{line || " "}</span>
            </div>
          );
        })}
      </pre>
    </div>
  );
}
