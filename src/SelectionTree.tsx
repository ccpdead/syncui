import { useEffect, useMemo, useRef, useState } from "react";
import type { Action, DiffEntry } from "./api";

export type TreeNode = {
  /** Folder path prefix or file relPath. Root uses "". */
  path: string;
  name: string;
  kind: "dir" | "file";
  children: TreeNode[];
  /** Descendant file relPaths (for dirs). */
  filePaths: string[];
};

export type ActionMeta = { label: string; icon: string; cls: string };

type FlatRow = { node: TreeNode; depth: number };

/** Must match `.tree-row` height in styles.css (rows are virtualized). */
const ROW_HEIGHT = 28;
const OVERSCAN = 10;

const byName = (a: { kind: string; name: string }, b: { kind: string; name: string }) => {
  if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
  return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
};

/** Build a directory tree from relative file paths. */
export function buildSelectionTree(relPaths: string[]): TreeNode[] {
  type Mutable = {
    name: string;
    path: string;
    kind: "dir" | "file";
    children: Map<string, Mutable>;
    filePaths: string[];
  };

  const root: Mutable = {
    name: "",
    path: "",
    kind: "dir",
    children: new Map(),
    filePaths: [],
  };

  const seen = new Set<string>();
  for (const rel of relPaths) {
    if (seen.has(rel)) continue;
    seen.add(rel);
    const parts = rel.split("/").filter(Boolean);
    if (parts.length === 0) continue;

    const chain: Mutable[] = [root];
    let node = root;
    let acc = "";
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      const isFile = i === parts.length - 1;
      acc = acc ? `${acc}/${part}` : part;
      if (isFile) {
        if (!node.children.has(part)) {
          node.children.set(part, {
            name: part,
            path: rel,
            kind: "file",
            children: new Map(),
            filePaths: [rel],
          });
        }
      } else {
        let child = node.children.get(part);
        if (!child) {
          child = {
            name: part,
            path: acc,
            kind: "dir",
            children: new Map(),
            filePaths: [],
          };
          node.children.set(part, child);
        }
        node = child;
        chain.push(node);
      }
    }
    for (const n of chain) n.filePaths.push(rel);
  }

  const freeze = (m: Mutable): TreeNode => ({
    path: m.path,
    name: m.name,
    kind: m.kind,
    filePaths: m.filePaths,
    children: [...m.children.values()].sort(byName).map(freeze),
  });

  return [...root.children.values()].sort(byName).map(freeze);
}

function collectDirPaths(nodes: TreeNode[], out: string[] = []): string[] {
  for (const n of nodes) {
    if (n.kind === "dir") {
      out.push(n.path);
      collectDirPaths(n.children, out);
    }
  }
  return out;
}

/** Rows currently visible given the expanded directories, in display order. */
function flattenVisible(nodes: TreeNode[], expanded: Set<string>): FlatRow[] {
  const rows: FlatRow[] = [];
  const visit = (items: TreeNode[], depth: number) => {
    for (const node of items) {
      rows.push({ node, depth });
      if (node.kind === "dir" && expanded.has(node.path)) visit(node.children, depth + 1);
    }
  };
  visit(nodes, 0);
  return rows;
}

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

function CheckBox({
  state,
  onChange,
}: {
  state: "all" | "none" | "part";
  onChange: () => void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = state === "part";
  }, [state]);
  return (
    <input
      ref={ref}
      type="checkbox"
      checked={state === "all"}
      onChange={onChange}
      onClick={(e) => e.stopPropagation()}
    />
  );
}

function TreeRow({
  node,
  depth,
  selected,
  expanded,
  activePath,
  entryByPath,
  selectedCounts,
  actionMeta,
  diffable,
  onToggleExpand,
  onToggleFile,
  onToggleDir,
  onDiff,
}: {
  node: TreeNode;
  depth: number;
  selected: Set<string>;
  expanded: Set<string>;
  activePath: string | null;
  entryByPath: Map<string, DiffEntry>;
  selectedCounts: Map<string, number>;
  actionMeta: Record<Action, ActionMeta>;
  diffable: Set<string>;
  onToggleExpand: (path: string) => void;
  onToggleFile: (rel: string) => void;
  onToggleDir: (filePaths: string[], select: boolean) => void;
  onDiff?: (rel: string) => void;
}) {
  if (node.kind === "file") {
    const checked = selected.has(node.path);
    const canDiff = diffable.has(node.path);
    const entry = entryByPath.get(node.path);
    const meta = entry ? actionMeta[entry.action] : null;
    const active = activePath === node.path;
    return (
      <div
        className={`tree-row file${active ? " active" : ""}`}
        style={{ paddingLeft: 8 + depth * 14 }}
      >
        <span className="tree-twist spacer" />
        <label className="tree-label">
          <CheckBox state={checked ? "all" : "none"} onChange={() => onToggleFile(node.path)} />
          {meta && (
            <span className={`tree-action ${meta.cls}`} title={meta.label}>
              {meta.icon} {meta.label}
            </span>
          )}
          <span className="tree-file" title={node.path}>
            {node.name}
          </span>
        </label>
        <span className="tree-meta tree-size" title="本地大小">
          {fmtSize(entry?.localSize ?? null)}
        </span>
        <span className="tree-meta tree-size" title="远程大小">
          {fmtSize(entry?.remoteSize ?? null)}
        </span>
        <span className="tree-meta tree-newer" title="较新">
          {entry?.newer === "local" ? "本地" : entry?.newer === "remote" ? "远程" : "—"}
        </span>
        {canDiff ? (
          <button
            type="button"
            className="btn diff-btn tree-diff"
            onClick={(e) => {
              e.preventDefault();
              e.stopPropagation();
              onDiff?.(node.path);
            }}
            title="在右侧查看内容差异"
          >
            Diff
          </button>
        ) : (
          <span className="tree-diff-na" />
        )}
      </div>
    );
  }

  const total = node.filePaths.length;
  const sel = selectedCounts.get(node.path) ?? 0;
  const state: "all" | "none" | "part" =
    total === 0 ? "none" : sel === 0 ? "none" : sel === total ? "all" : "part";
  const isOpen = expanded.has(node.path);

  return (
    <div className="tree-row dir" style={{ paddingLeft: 8 + depth * 14 }}>
      <button
        type="button"
        className="tree-twist"
        onClick={() => onToggleExpand(node.path)}
        aria-label={isOpen ? "折叠" : "展开"}
      >
        {isOpen ? "▾" : "▸"}
      </button>
      <label className="tree-label">
        <CheckBox state={state} onChange={() => onToggleDir(node.filePaths, state !== "all")} />
        <span className="tree-dir" title={node.path}>
          {node.name}
          <span className="tree-count">
            {sel}/{total}
          </span>
        </span>
      </label>
    </div>
  );
}

export default function SelectionTree({
  entries,
  selected,
  activePath,
  actionMeta,
  diffable,
  onToggleFile,
  onToggleFiles,
  onDiff,
}: {
  entries: DiffEntry[];
  selected: Set<string>;
  activePath: string | null;
  actionMeta: Record<Action, ActionMeta>;
  /** Relative paths that support content Diff (upload / download / conflict). */
  diffable: Set<string>;
  onToggleFile: (rel: string) => void;
  onToggleFiles: (rels: string[], select: boolean) => void;
  onDiff?: (rel: string) => void;
}) {
  const relPaths = useMemo(() => entries.map((e) => e.relPath), [entries]);
  const entryByPath = useMemo(() => {
    const m = new Map<string, DiffEntry>();
    for (const e of entries) m.set(e.relPath, e);
    return m;
  }, [entries]);

  const roots = useMemo(() => buildSelectionTree(relPaths), [relPaths]);
  const allDirPaths = useMemo(() => collectDirPaths(roots), [roots]);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(400);
  const bodyRef = useRef<HTMLDivElement>(null);

  // Start collapsed whenever the tree is rebuilt (new compare / filter):
  // expanding tens of thousands of rows up front is what froze the WebView.
  useEffect(() => {
    setExpanded(new Set());
    setScrollTop(0);
    if (bodyRef.current) bodyRef.current.scrollTop = 0;
  }, [roots]);

  const isEmpty = relPaths.length === 0;
  useEffect(() => {
    const body = bodyRef.current;
    if (!body) return;
    setViewportHeight(body.clientHeight);
    const observer = new ResizeObserver(() => setViewportHeight(body.clientHeight));
    observer.observe(body);
    return () => observer.disconnect();
  }, [isEmpty]);

  /** Selected-file count per directory path, computed once per selection change. */
  const selectedCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const rel of selected) {
      if (!entryByPath.has(rel)) continue;
      let slash = rel.lastIndexOf("/");
      while (slash > 0) {
        const dir = rel.slice(0, slash);
        counts.set(dir, (counts.get(dir) ?? 0) + 1);
        slash = rel.lastIndexOf("/", slash - 1);
      }
    }
    return counts;
  }, [selected, entryByPath]);

  const rows = useMemo(() => flattenVisible(roots, expanded), [roots, expanded]);
  const start = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const end = Math.min(rows.length, Math.ceil((scrollTop + viewportHeight) / ROW_HEIGHT) + OVERSCAN);
  const visibleRows = rows.slice(start, end);

  const toggleExpand = (path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  if (isEmpty) {
    return <div className="tree-empty">当前筛选无文件</div>;
  }

  return (
    <div className="selection-tree">
      <div className="tree-toolbar">
        <span className="tree-title">按目录选择</span>
        <button type="button" className="link" onClick={() => setExpanded(new Set(allDirPaths))}>
          全展开
        </button>
        <button type="button" className="link" onClick={() => setExpanded(new Set())}>
          全折叠
        </button>
      </div>
      <div
        className="tree-body"
        ref={bodyRef}
        onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
      >
        <div style={{ height: start * ROW_HEIGHT }} />
        {visibleRows.map(({ node, depth }) => (
          <TreeRow
            key={node.path}
            node={node}
            depth={depth}
            selected={selected}
            expanded={expanded}
            activePath={activePath}
            entryByPath={entryByPath}
            selectedCounts={selectedCounts}
            actionMeta={actionMeta}
            diffable={diffable}
            onToggleExpand={toggleExpand}
            onToggleFile={onToggleFile}
            onToggleDir={onToggleFiles}
            onDiff={onDiff}
          />
        ))}
        <div style={{ height: (rows.length - end) * ROW_HEIGHT }} />
      </div>
    </div>
  );
}
