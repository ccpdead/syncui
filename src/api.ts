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
  comparisonId?: string | null;
  complete?: boolean;
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
  remoteEndpoint?: RemoteEndpoint | null;
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
  elapsedMs: number;
  bytesCopied: number;
}

/** Aggregate file-copy throughput, sampled about twice per second. */
export interface TransferProgress {
  logicalBytes?: number | null;
  bytesCopied: number;
  bytesPerSecond: number;
  elapsedSeconds: number;
  finished: boolean;
}

export interface SyncResult {
  items: {relPath:string;op:string;status:"success"|"skipped"|"uncertain"|"notStarted";message:string|null}[];
  refreshed?: CompareResult | null;
  needsRecompare: boolean;
  logicalBytes: number;
  uploaded: number;
  downloaded: number;
  deletedLocal: number;
  deletedRemote: number;
  skipped: number;
  failed: number;
  errors: string[];
  /** True when the run was stopped by cancelSync() before every item ran. */
  cancelled: boolean;
  /** Paths verified after their operation, including partial runs. */
  donePaths: string[];
  bytesCopied: number;
  elapsedSeconds: number;
}

/** Error message the backend returns for a cancelled compare. */
export const CANCELLED = "已取消";

export interface FileTextSide {
  exists: boolean;
  size: number;
  content: string | null;
  binary: boolean;
  tooLarge: boolean;
  error: string | null;
}

export interface FileTextPair {
  localVersion?: FileStamp | null;
  remoteVersion?: FileStamp | null;
  relPath: string;
  local: FileTextSide;
  remote: FileTextSide;
}

export type WriteSide = "local" | "remote";

export function compareDirs(
  local: string,
  remote: string,
  options: CompareOptions,
  remoteEndpoint?: RemoteEndpoint
): Promise<CompareResult> {
  return invoke<CompareResult>("compare_dirs", { local, remote, options, remoteEndpoint });
}

export function syncEntries(comparisonId: string, selectedPaths: string[], conflictPolicy: ConflictPolicy, concurrency: number): Promise<SyncResult> {
  return invoke("sync_entries", { comparisonId, selectedPaths, conflictPolicy, concurrency });
}

export function cancelCompare(): Promise<void> {
  return invoke("cancel_compare");
}

export function cancelSync(): Promise<void> {
  return invoke("cancel_sync");
}

export function loadSettings(): Promise<AppSettings> {
  return invoke<AppSettings>("load_settings");
}

export function saveSettings(settings: AppSettings): Promise<void> {
  return invoke("save_settings", { settings });
}

export function readFilePair(
  local: string,
  remote: string,
  relPath: string,
  remoteEndpoint?: RemoteEndpoint
): Promise<FileTextPair> {
  return invoke<FileTextPair>("read_file_pair", { local, remote, relPath, remoteEndpoint });
}

export function writeFileText(
  local: string,
  remote: string,
  relPath: string,
  side: WriteSide,
  content: string,
  expected?: FileStamp | null,
  remoteEndpoint?: RemoteEndpoint
): Promise<FileStamp> {
  return invoke("write_file_text", { local, remote, relPath, side, content, expected: expected ?? null, remoteEndpoint });
}

export function compareOneEntry(
  local: string,
  remote: string,
  relPath: string,
  options: CompareOptions,
  remoteEndpoint?: RemoteEndpoint
): Promise<DiffEntry | null> {
  return invoke<DiffEntry | null>("compare_one_entry", {
    local,
    remote,
    relPath,
    options,
    remoteEndpoint,
  });
}

export type RemoteEndpoint = { kind: "filesystem"; path: string } | { kind: "ssh"; profileId: string; path: string };
export interface FileStamp { size: number; mtime: number; mtimeNs: number; ctime: number; ctimeNs: number; mode: number }
export interface SshProfile { id: string; name: string; host: string; user: string; port?: number | null; identityFile: string; auth: "auto" | "password" | "key"; rememberPassword: boolean }
export interface ConnectionInfo { id: string; user: string; home: string; arch: string; hostKey: string; rsyncReady: boolean; agentReady: boolean; message: string }
export interface RemoteDirectory { path: string; parent: string | null; children: string[] }
export const loadSshProfiles = () => invoke<SshProfile[]>("load_ssh_profiles");
export const saveSshProfiles = (profiles: SshProfile[]) => invoke<void>("save_ssh_profiles", { profiles });
export const connectSsh = (profile: SshProfile, password?: string) => invoke<ConnectionInfo>("connect_ssh", { profile, password });
export const deploySshAgent = (id: string) => invoke<ConnectionInfo>("deploy_ssh_agent", { id });
export const disconnectSsh = (id: string) => invoke<void>("disconnect_ssh", { id });
export const forgetSshPassword = (profile: SshProfile) => invoke<void>("forget_ssh_password", { profile });
export const sshAuthReply = (id: string, answer: string | null) => invoke<void>("ssh_auth_reply", { id, answer });
export const browseSsh = (id: string, path: string) => invoke<RemoteDirectory>("browse_ssh", { id, path });

export interface TaskEvent { taskId:string; phase:string; status:"running"|"complete"|"error"; error?:{code:string;message:string} }
