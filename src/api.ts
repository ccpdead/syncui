import { invoke } from "@tauri-apps/api/core";

export type Action =
  | "upload"
  | "download"
  | "deleteLocal"
  | "deleteRemote"
  | "conflict"
  | "same";

export type SyncMode = "mirror" | "mirror_pull" | "twoway";

export type ConflictPolicy = "newer" | "local" | "remote" | "skip";

export interface DiffEntry {
  relPath: string;
  action: Action;
  localSize: number | null;
  remoteSize: number | null;
  localMtime: number | null;
  remoteMtime: number | null;
  newer: "local" | "remote" | null;
}

export interface CompareResult {
  entries: DiffEntry[];
  uploadCount: number;
  downloadCount: number;
  deleteLocalCount: number;
  deleteRemoteCount: number;
  conflictCount: number;
  sameCount: number;
  skippedCount: number;
}

export interface CompareOptions {
  useHash: boolean;
  ignore: string[];
  mode: SyncMode;
}

/** Persisted UI preferences (stored under ~/.config/com.syncui.app/). */
export interface AppSettings {
  mode: SyncMode;
  conflictPolicy: ConflictPolicy;
  useHash: boolean;
  concurrency: number;
  ignoreText: string;
  localPath: string;
  remotePath: string;
}

/** A concrete operation sent to the backend. */
export type Op = "upload" | "download" | "delLocal" | "delRemote";

export interface SyncOp {
  relPath: string;
  op: Op;
}

export interface SyncProgress {
  index: number;
  total: number;
  relPath: string;
  op: string;
  ok: boolean;
  skipped: boolean;
  error: string | null;
}

export interface SyncResult {
  uploaded: number;
  downloaded: number;
  deletedLocal: number;
  deletedRemote: number;
  skipped: number;
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
  items: SyncOp[],
  ignore: string[],
  concurrency: number
): Promise<SyncResult> {
  return invoke<SyncResult>("sync_entries", {
    local,
    remote,
    items,
    ignore,
    concurrency,
  });
}

export function loadSettings(): Promise<AppSettings> {
  return invoke<AppSettings>("load_settings");
}

export function saveSettings(settings: AppSettings): Promise<void> {
  return invoke("save_settings", { settings });
}
