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

  const sorted = [...relPaths].sort((a, b) => a.localeCompare(b));
  for (const rel of sorted) {
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
    for (const n of chain) {
      if (!n.filePaths.includes(rel)) n.filePaths.push(rel);
    }
  }

  const freeze = (m: Mutable): TreeNode => ({
    path: m.path,
    name: m.name,
    kind: m.kind,
    filePaths: m.filePaths,
    children: [...m.children.values()]
      .sort((a, b) => {
        if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
        return a.name.localeCompare(b.name);
      })
      .map(freeze),
  });

  return [...root.children.values()]
    .sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
      return a.name.localeCompare(b.name);
    })
    .map(freeze);
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
  const sel = node.filePaths.reduce((n, p) => n + (selected.has(p) ? 1 : 0), 0);
  const state: "all" | "none" | "part" =
    total === 0 ? "none" : sel === 0 ? "none" : sel === total ? "all" : "part";
  const isOpen = expanded.has(node.path);

  return (
    <div className="tree-branch">
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
          <CheckBox
            state={state}
            onChange={() => onToggleDir(node.filePaths, state !== "all")}
          />
          <span className="tree-dir" title={node.path}>
            {node.name}
            <span className="tree-count">
              {sel}/{total}
            </span>
          </span>
        </label>
      </div>
      {isOpen &&
        node.children.map((c) => (
          <TreeRow
            key={c.path}
            node={c}
            depth={depth + 1}
            selected={selected}
            expanded={expanded}
            activePath={activePath}
            entryByPath={entryByPath}
            actionMeta={actionMeta}
            diffable={diffable}
            onToggleExpand={onToggleExpand}
            onToggleFile={onToggleFile}
            onToggleDir={onToggleDir}
            onDiff={onDiff}
          />
        ))}
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
  const allDirsKey = useMemo(() => collectDirPaths(roots).join("\0"), [roots]);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set(collectDirPaths(roots)));

  // Default: expand all when the tree structure changes (filter / compare).
  useEffect(() => {
    setExpanded(new Set(collectDirPaths(roots)));
  }, [allDirsKey, roots]);

  const toggleExpand = (path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  if (relPaths.length === 0) {
    return <div className="tree-empty">当前筛选无文件</div>;
  }

  return (
    <div className="selection-tree">
      <div className="tree-toolbar">
        <span className="tree-title">按目录选择</span>
        <button
          type="button"
          className="link"
          onClick={() => setExpanded(new Set(collectDirPaths(roots)))}
        >
          全展开
        </button>
        <button type="button" className="link" onClick={() => setExpanded(new Set())}>
          全折叠
        </button>
      </div>
      <div className="tree-body">
        {roots.map((n) => (
          <TreeRow
            key={n.path}
            node={n}
            depth={0}
            selected={selected}
            expanded={expanded}
            activePath={activePath}
            entryByPath={entryByPath}
            actionMeta={actionMeta}
            diffable={diffable}
            onToggleExpand={toggleExpand}
            onToggleFile={onToggleFile}
            onToggleDir={onToggleFiles}
            onDiff={onDiff}
          />
        ))}
      </div>
    </div>
  );
}
