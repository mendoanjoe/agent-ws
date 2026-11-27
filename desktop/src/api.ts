// Typed calls into the Rust side (src-tauri/src/commands.rs), which runs the
// agent-ws library. The shapes match the library's JSON output.
import { invoke } from "@tauri-apps/api/core";

export type Repo = { root: string; dir: string; workspaces: number };

export type Run = {
  command: string[];
  pid: number;
  started: number;
  ended: number | null;
  exit_code: number | null;
  log: boolean;
  state: "running" | "exited" | "interrupted";
};

export type Workspace = {
  name: string;
  path: string;
  base: string;
  created: number;
  branch?: string;
  origin?: string;
  age_secs: number;
  port: number | null;
  dirty: boolean | null;
  ahead: number | null;
  processes: number[];
  run: Run | null;
  private_bytes: number | null;
};

export type Conflict = { workspaces: string[]; paths: string[] };

export type Status = {
  workspaces: { name: string; changed: string[] }[];
  overlaps: Record<string, string[]>;
  conflicts: Conflict[] | null;
};

export type FileChange = {
  path: string;
  status: string;
  added: number | null;
  deleted: number | null;
};

export type Diff = { files: FileChange[]; patch: string; truncated: boolean };

export type Usage = {
  files: number;
  logical: number;
  allocated: number;
  private: number;
};

export type DirUsage = {
  path: string;
  usage: Usage;
};

export type WsUsage = Usage & {
  name: string;
  path: string;
  kind: "workspace" | "pool" | "trash";
  breakdown?: DirUsage[];
};

export type Landed = {
  name: string;
  branch: string;
  commit: string;
  files: number;
  dropped: boolean;
  overlaps: Record<string, string[]>;
  pushed: boolean;
  pr_url?: string;
};

export type Pruned = {
  name: string;
  path: string;
  reason: string;
  private_bytes: number;
  branch?: string;
};

export type Prune = {
  base: string;
  base_commit: string;
  dry_run: boolean;
  pruned: Pruned[];
  kept: { name: string; why: string }[];
  freed_bytes: number;
};

export const api = {
  repos: () => invoke<Repo[]>("repos"),
  workspaces: (root: string, disk: boolean) => invoke<Workspace[]>("workspaces", { root, disk }),
  diskUsage: (root: string) => invoke<WsUsage[]>("disk_usage", { root }),
  status: (root: string) => invoke<Status>("status", { root }),
  diff: (root: string, name: string) => invoke<Diff>("diff", { root, name }),
  runLog: (root: string, name: string) => invoke<string | null>("run_log", { root, name }),
  land: (root: string, name: string, message: string, squash: boolean, pr: boolean) =>
    invoke<Landed>("land", { root, name, message, squash, pr }),
  drop: (root: string, name: string) => invoke<void>("drop_workspace", { root, name }),
  prune: (root: string, dryRun: boolean) => invoke<Prune>("prune", { root, dryRun }),
  open: (target: string, how: "files" | "editor" | "terminal" | "url") => invoke<void>("open", { target, how }),
};
