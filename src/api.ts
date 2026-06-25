import { invoke } from "@tauri-apps/api/core";

export type DiffStatus = "new" | "modified" | "deleted" | "same";

export interface DiffEntry {
  relPath: string;
  status: DiffStatus;
  localSize: number | null;
  remoteSize: number | null;
  localMtime: number | null;
  remoteMtime: number | null;
  newer: "local" | "remote" | null;
}

export interface CompareResult {
  entries: DiffEntry[];
  newCount: number;
  modifiedCount: number;
  deletedCount: number;
  sameCount: number;
  skippedCount: number;
}

export interface CompareOptions {
  useHash: boolean;
  ignore: string[];
}

export interface SyncItem {
  relPath: string;
  status: DiffStatus;
}

export interface SyncProgress {
  index: number;
  total: number;
  relPath: string;
  action: string;
  ok: boolean;
  error: string | null;
}

export interface SyncResult {
  copied: number;
  deleted: number;
  failed: number;
  errors: string[];
}

export function compareDirs(
  local: string,
  remote: string,
  options: CompareOptions
): Promise<CompareResult> {
  return invoke<CompareResult>("compare_dirs", { local, remote, options });
}

export function syncEntries(
  local: string,
  remote: string,
  items: SyncItem[],
  includeDeletes: boolean
): Promise<SyncResult> {
  return invoke<SyncResult>("sync_entries", {
    local,
    remote,
    items,
    includeDeletes,
  });
}
