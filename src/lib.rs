//! agent-ws: instant, isolated workspaces for coding agents.
//!
//! A workspace is a Git worktree whose files are copy-on-write clones of the
//! source checkout (APFS clones on macOS, reflinks on Linux Btrfs/XFS),
//! ignored files included (`node_modules`, build caches). Spawning costs
//! metadata only, so agents skip `install` and start with warm caches.
//! A pool of ready workspaces makes spawning a rename-sized operation.
//! The library holds all logic so other tools can embed it.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!(
    "agent-ws supports macOS (APFS clones) and Linux (Btrfs or XFS reflinks): it needs copy-on-write file clones."
);

pub mod claude;
pub mod config;
pub mod fsutil;
pub mod git;

use anyhow::{Context, Result, bail};
use config::Config;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Paths never cloned. `.claude/worktrees` holds other agents' checkouts.
pub const DEFAULT_EXCLUDES: &[&str] = &[".git", ".claude/worktrees"];

/// Pooled workspace ids start with `@`, which workspace names cannot contain.
const POOL_PREFIX: &str = "@pool-";

const GB: u64 = 1_000_000_000;

/// Largest pool. Every pooled workspace turns into real disk use once the
/// source checkout rewrites the files it shares (a reinstall, a rebuild).
pub const MAX_POOL: usize = 8;

/// `spawn` warns from this many workspaces on (see `warn_workspaces`).
const WARN_WORKSPACES: usize = 20;

/// `spawn` warns below this much free disk.
const WARN_FREE: u64 = 10 * GB;

/// Trash entries older than this are leftovers of a killed background `rm`.
const TRASH_GRACE_NS: u128 = 600 * 1_000_000_000;

/// `prune` leaves workspaces younger than this alone by default.
pub const PRUNE_MIN_AGE_SECS: u64 = 3600;

/// The background prune after `spawn` runs at most this often.
const AUTO_PRUNE_EVERY_SECS: u64 = 600;

/// The background prune drops an unused Claude Code worktree this old.
const CLAUDE_UNUSED_SECS: u64 = 24 * 3600;

/// `PORT` of slot 0 and the gap between slots, unless configured. Clear of
/// the usual dev server ports (3000, 4321, 5173, 8787).
const PORT_BASE: u16 = 4100;
const PORT_STEP: u16 = 10;

/// Machine-readable error codes for structured `--json` errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    AlreadyExists,
    NotFound,
    Conflict,
    Dirty,
    UntrustedHooks,
    LowDisk,
    InvalidInput,
    SelfActionBlocked,
    UnsupportedFs,
    GitError,
    Internal,
}

/// An error carrying an explicit [`ErrorCode`].
#[derive(Debug)]
pub struct CodedError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

/// Construct an `anyhow::Error` tagged with an [`ErrorCode`].
pub fn coded_err(code: ErrorCode, msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CodedError {
        code,
        message: msg.into(),
    })
}

/// Extract the [`ErrorCode`] from a [`CodedError`] in the error chain, or infer
/// one sensibly from the error message.
pub fn error_code(err: &anyhow::Error) -> ErrorCode {
    for cause in err.chain() {
        if let Some(ce) = cause.downcast_ref::<CodedError>() {
            return ce.code;
        }
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    if msg.contains("already exists") {
        ErrorCode::AlreadyExists
    } else if msg.contains("no workspace named")
        || msg.contains("not found")
        || msg.contains("no log for")
        || msg.contains("no snapshot")
    {
        ErrorCode::NotFound
    } else if msg.contains("conflict") {
        ErrorCode::Conflict
    } else if msg.contains("dirty") || msg.contains("uncommitted") {
        ErrorCode::Dirty
    } else if msg.contains("agent-ws trust") || msg.contains("hooks that run") {
        ErrorCode::UntrustedHooks
    } else if msg.contains("free on disk") || msg.contains("max_private_gb") {
        ErrorCode::LowDisk
    } else if msg.contains("currently inside") || msg.contains("allow-self") {
        ErrorCode::SelfActionBlocked
    } else if msg.contains("different volumes")
        || msg.contains("reflink")
        || msg.contains("bare repositories")
    {
        ErrorCode::UnsupportedFs
    } else if msg.contains("invalid")
        || msg.contains("must be")
        || msg.contains("not a valid")
        || msg.contains("above the maximum")
        || msg.contains("no changes")
        || msg.contains("no command given")
    {
        ErrorCode::InvalidInput
    } else if msg.contains("git ") || msg.contains("not inside a git repository") {
        ErrorCode::GitError
    } else {
        ErrorCode::Internal
    }
}

pub struct Repo {
    /// Main checkout of the repository.
    pub root: PathBuf,
    /// State and workspaces for this repository.
    pub dir: PathBuf,
    pub config: Config,
}

/// Records live in `meta/<name>.json` (named workspaces) and
/// `pool/<id>.json` (pooled ones). States: `creating`, `ready`, `pooled`.
#[derive(Serialize, Deserialize, Clone)]
pub struct Meta {
    pub name: String,
    pub path: PathBuf,
    /// Empty while the workspace is being created.
    pub base: String,
    pub created: u64,
    #[serde(default = "ready")]
    pub state: String,
    /// Process creating or claiming the workspace (0 when idle).
    #[serde(default)]
    pub pid: u32,
    /// Start time of `pid` in microseconds, so a reused pid is not mistaken
    /// for the spawner (0 in records written before this field existed).
    #[serde(default)]
    pub pid_start: u64,
    /// Freshness key of a pooled workspace (see `Repo::pool_key`).
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub notes: Notes,
    /// Port slot, unique among workspaces: `exec` and hooks get
    /// `PORT = port_base + slot * port_step`. Absent in older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    /// Branch checked out in the workspace (`spawn --branch`, Claude Code
    /// worktrees); empty when HEAD is detached.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
    /// What created it: empty for the command line, `claude` for Claude
    /// Code's WorktreeCreate hook.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
}

fn ready() -> String {
    "ready".into()
}

/// The last `exec` in a workspace, kept in `runs/<name>.json`.
#[derive(Serialize, Deserialize, Clone)]
pub struct Run {
    pub command: Vec<String>,
    pub pid: u32,
    pub pid_start: u64,
    pub started: u64,
    /// Unset while the command runs, or when agent-ws was killed first.
    pub ended: Option<u64>,
    pub exit_code: Option<i32>,
    /// Its output is in `runs/<name>.log` (see `Repo::run_log`).
    #[serde(default)]
    pub log: bool,
}

/// One changed path in `diff --json`.
#[derive(Serialize)]
pub struct FileChange {
    pub path: String,
    /// Git's status letter: `A`dded, `M`odified, `D`eleted, `T`ype changed.
    pub status: String,
    /// Lines added and deleted; absent for binary files.
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// A workspace as it is right now, as a tree object (see `Repo::snapshot`).
pub struct Snapshot {
    pub path: PathBuf,
    pub base: String,
    pub tree: String,
}

/// A single diff payload combining changed files and the patch from one snapshot.
#[derive(Serialize)]
pub struct DiffBundle {
    pub files: Vec<FileChange>,
    pub patch: String,
    pub truncated: bool,
}

/// Two workspaces whose changes conflict when both are merged.
#[derive(Serialize)]
pub struct Conflict {
    pub workspaces: Vec<String>,
    pub paths: Vec<String>,
}

/// What spawning found and fixed in a workspace.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Notes {
    pub symlinks_retargeted: Vec<PathBuf>,
    /// Symlinks resolving outside the workspace. Writes through them leave it.
    pub symlinks_external: Vec<ExternalLink>,
    pub secrets_removed: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ExternalLink {
    pub path: PathBuf,
    pub target: PathBuf,
}

#[derive(Serialize)]
pub struct Spawned {
    pub name: String,
    pub path: PathBuf,
    pub base: String,
    pub from_pool: bool,
    pub clone_ms: u128,
    pub total_ms: u128,
    pub clone_calls: usize,
    #[serde(flatten)]
    pub notes: Notes,
    /// Many workspaces or little free disk: time to `prune`.
    pub warnings: Vec<String>,
    /// `PORT` for commands in this workspace (see `Repo::env`).
    pub port: Option<u32>,
    /// Branch checked out in the workspace (`--branch`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub branch: String,
    /// Paths `--carry` brought along from the source checkout.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub carried: Vec<String>,
    /// True when `--ensure` reused an existing ready workspace.
    #[serde(default)]
    pub reused: bool,
}

/// What `Repo::fill` did to a new workspace.
struct Filled {
    clone_ms: u128,
    calls: usize,
    notes: Notes,
    carried: Vec<String>,
}

/// Per-directory disk usage breakdown entry.
#[derive(Serialize)]
pub struct DirUsage {
    pub path: PathBuf,
    pub usage: fsutil::Usage,
}

/// Disk use of one workspace, pooled workspace or trash entry.
#[derive(Serialize)]
pub struct WsUsage {
    pub name: String,
    pub path: PathBuf,
    /// `workspace`, `pool` or `trash`.
    pub kind: String,
    #[serde(flatten)]
    pub usage: fsutil::Usage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<DirUsage>,
}

#[derive(Serialize)]
pub struct DiskUsage {
    pub entries: Vec<WsUsage>,
    pub total: fsutil::Usage,
    pub free_bytes: u64,
    pub cohort_shared_bytes: u64,
    pub reclaimable_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_bytes: Option<u64>,
}

#[derive(Default)]
pub struct PruneOpts {
    /// Branch or commit the work must be in (default: see `Repo::prune_base`).
    pub base: Option<String>,
    pub dry_run: bool,
    /// Also ask GitHub (`gh`) whether a PR from a branch at the workspace
    /// HEAD was merged.
    pub gh: bool,
    /// Leave workspaces created less than this many seconds ago.
    pub min_age_secs: u64,
    /// Keep `unused` workspaces too (the automatic prune after `spawn`):
    /// a coordinator may have spawned one for later.
    pub merged_only: bool,
    /// Only prune enough (largest first, clearing pool if needed) until free
    /// disk space reaches this threshold.
    pub target_free_bytes: Option<u64>,
}

#[derive(Serialize)]
pub struct Pruned {
    pub name: String,
    pub path: PathBuf,
    /// `merged` (HEAD is in the base), `squash-merged` (merging HEAD into the
    /// base changes nothing), `pr-merged` (GitHub merged a PR at HEAD) or
    /// `unused` (no commits and no changes since spawn).
    pub reason: String,
    /// Bytes only this workspace holds; deleting it frees at least this.
    pub private_bytes: u64,
    /// The workspace's own branch (`spawn --branch`), deleted with it
    /// because the base has all of it (`merged`) or it never moved (`unused`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub branch: String,
}

#[derive(Serialize)]
pub struct Kept {
    pub name: String,
    pub why: String,
}

#[derive(Serialize)]
pub struct Prune {
    /// The base as given and the commit it resolved to.
    pub base: String,
    pub base_commit: String,
    pub dry_run: bool,
    pub pruned: Vec<Pruned>,
    pub kept: Vec<Kept>,
    pub freed_bytes: u64,
}

#[derive(Serialize, Default)]
pub struct Gc {
    /// Records whose path is not a workspace folder of this repository.
    /// Only the record is removed; the path is never touched.
    pub corrupt: Vec<String>,
    /// Spawns or pool fills whose process died midway.
    pub interrupted: Vec<String>,
    /// Records whose directory was deleted by hand.
    pub missing: Vec<String>,
    /// Directories under `ws/` with no record and no work in them, removed.
    pub orphans: Vec<String>,
    /// Orphan directories with uncommitted changes or commits no branch
    /// holds: registered again (base: fork point with the prune base), so
    /// `list`, `land` and `drop` see them.
    pub adopted: Vec<String>,
    /// Orphan directories whose state Git cannot read: left alone.
    pub unreadable: Vec<String>,
    /// Pool claims whose process died.
    pub claims: usize,
    /// Leftovers in `.trash` from interrupted background deletes.
    pub trash: usize,
}

#[derive(Serialize)]
pub struct WsStatus {
    pub name: String,
    pub path: PathBuf,
    pub base: String,
    pub changed: Vec<String>,
}

#[derive(Serialize)]
pub struct Status {
    pub workspaces: Vec<WsStatus>,
    /// Paths changed in more than one workspace, with the workspaces involved.
    pub overlaps: BTreeMap<String, Vec<String>>,
    /// Overlapping workspaces whose changes do not merge cleanly. `null`
    /// when Git is older than 2.38 and cannot tell.
    pub conflicts: Option<Vec<Conflict>>,
}

#[derive(Serialize)]
pub struct Landed {
    pub name: String,
    pub branch: String,
    pub commit: String,
    pub files: usize,
    pub dropped: bool,
    /// Landed paths that other workspaces also changed.
    pub overlaps: BTreeMap<String, Vec<String>>,
    /// The branch was pushed to `origin` (`--pr`).
    pub pushed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

#[derive(Serialize, Default)]
pub struct PoolFill {
    /// Another fill holds the pool lock; nothing was done.
    pub busy: bool,
    pub created: usize,
    pub refreshed: usize,
    pub removed_stale: usize,
    pub removed_extra: usize,
    pub ready: usize,
}

#[derive(Serialize)]
pub struct PoolStatus {
    pub target: usize,
    pub fresh: usize,
    pub refreshable: usize,
    pub stale: usize,
    pub creating: usize,
}

#[derive(Clone, Default)]
pub struct SpawnOpts {
    pub name: String,
    pub base: Option<String>,
    pub exclude: Vec<String>,
    pub keep_secrets: bool,
    /// Defaults to the config value, else 2 GB.
    pub min_free_bytes: Option<u64>,
    /// Take a ready workspace from the pool when one matches.
    pub use_pool: bool,
    /// Bring the source checkout's uncommitted edits and untracked files
    /// along. The base must be HEAD, which they were made on.
    pub carry: bool,
    /// Create this branch at the base and check it out (default: detached).
    pub branch: Option<String>,
    /// Kept in `Meta::origin`.
    pub origin: String,
    /// Skip the `post_spawn` hooks.
    pub no_hooks: bool,
    /// Clone from an existing workspace instead of the main checkout.
    pub from: Option<String>,
    /// Return an existing ready workspace instead of failing with AlreadyExists.
    pub ensure: bool,
    /// Pick first unused name in `name`, `name-2`, ..., `name-100`.
    pub auto_name: bool,
}

pub struct LandOpts {
    pub name: String,
    pub message: String,
    pub branch: Option<String>,
    pub force_branch: bool,
    pub keep: bool,
    /// Skip Git's commit hooks and the `pre_land` hooks.
    pub no_verify: bool,
    /// One commit on top of the base instead of the agent's commits plus one.
    pub squash: bool,
    /// Push the branch to `origin` and open a GitHub pull request (`gh`).
    pub pr: bool,
    /// Branch the pull request targets (default: the repository's default).
    pub pr_base: Option<String>,
    pub draft: bool,
    /// Allow landing even if the current directory is inside the workspace.
    pub allow_self: bool,
}

/// Result of refreshing ignored dependency directories in a workspace.
#[derive(Serialize)]
pub struct ResyncResult {
    pub name: String,
    pub resynced: Vec<String>,
    pub skipped: Vec<String>,
    pub freed_private_bytes: u64,
}

#[derive(Clone, Default)]
pub struct SyncOpts {
    pub name: String,
    pub base: Option<String>,
    pub merge: bool,
    pub resync_deps: bool,
}

#[derive(Serialize)]
pub struct Synced {
    pub name: String,
    pub old_base: String,
    pub new_base: String,
    pub head: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resync: Option<ResyncResult>,
}

/// Metadata for a workspace CoW checkpoint.
#[derive(Serialize, Deserialize, Clone)]
pub struct SnapInfo {
    pub name: String,
    pub workspace: String,
    pub head: String,
    pub base: String,
    pub created: u64,
    pub path: PathBuf,
}

/// Identity and optional overlap/conflict context for the current working directory.
#[derive(Serialize)]
pub struct WhoAmI {
    pub inside_workspace: bool,
    pub repo_root: PathBuf,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub base: Option<String>,
    pub branch: Option<String>,
    pub port: Option<u16>,
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overlaps: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<Conflict>>,
}

/// One workspace in `list --long`.
#[derive(Serialize)]
pub struct WsInfo {
    #[serde(flatten)]
    pub meta: Meta,
    pub age_secs: u64,
    pub port: Option<u32>,
    /// Uncommitted changes; `null` when Git cannot read the workspace.
    pub dirty: Option<bool>,
    /// Commits since the base.
    pub ahead: Option<u64>,
    /// Processes whose current directory is inside (agents, shells).
    pub processes: Vec<u32>,
    pub run: Option<RunInfo>,
    /// Bytes only this workspace holds; `null` unless asked for.
    pub private_bytes: Option<u64>,
}

/// The last `exec` and whether it still runs.
#[derive(Serialize)]
pub struct RunInfo {
    #[serde(flatten)]
    pub run: Run,
    /// `running`, `exited`, or `interrupted` (agent-ws was killed first).
    pub state: String,
}

/// A repository agent-ws keeps state for.
#[derive(Serialize)]
pub struct KnownRepo {
    /// The main checkout.
    pub root: PathBuf,
    /// Its state folder under `~/.agent-ws`.
    pub dir: PathBuf,
    /// Workspaces that finished spawning.
    pub workspaces: usize,
}

/// Every repository with state under `~/.agent-ws` (or `AGENT_WS_HOME`)
/// whose checkout still exists, by name. State folders written before
/// `repo.json` existed are recognised by asking Git about one of their
/// workspaces.
pub fn known_repos() -> Result<Vec<KnownRepo>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(home()?).into_iter().flatten().flatten() {
        let dir = e.path();
        if !dir.join("meta").is_dir() {
            continue;
        }
        let noted = std::fs::read(dir.join("repo.json"))
            .ok()
            .and_then(|r| serde_json::from_slice::<serde_json::Value>(&r).ok())
            .and_then(|v| v["root"].as_str().map(PathBuf::from));
        let mut cached_records = None;
        let root = match noted {
            Some(r) => Some(r),
            None => {
                let recs = read_records(&dir.join("meta"), ".json");
                let found = recs
                    .iter()
                    .find(|(_, m)| m.path.exists())
                    .and_then(|(_, m)| {
                        git::line(
                            &m.path,
                            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                        )
                        .ok()
                    })
                    .and_then(|common| Path::new(&common).parent().map(Path::to_path_buf));
                cached_records = Some(recs);
                found
            }
        };
        let Some(root) = root.filter(|r| r.join(".git").exists()) else {
            continue;
        };
        let records = cached_records.unwrap_or_else(|| read_records(&dir.join("meta"), ".json"));
        let workspaces = records.iter().filter(|(_, m)| m.state == "ready").count();
        out.push(KnownRepo {
            root,
            dir,
            workspaces,
        });
    }
    out.sort_by(|a, b| a.root.file_name().cmp(&b.root.file_name()));
    Ok(out)
}

fn home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("AGENT_WS_HOME") {
        return Ok(std::path::absolute(h)?);
    }
    let h = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(h).join(".agent-ws"))
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok {
        return Err(coded_err(
            ErrorCode::InvalidInput,
            format!("invalid workspace name {name:?}: use letters, digits, '.', '_' or '-'"),
        ));
    }
    Ok(())
}

/// Rules for a new workspace: `check_name`, and the name must work in the
/// branch `land` creates. Kept apart from `check_name`, which also decides
/// what `owns` accepts: workspaces made before this rule stay manageable.
fn check_new_name(name: &str) -> Result<()> {
    check_name(name)?;
    if name.contains("..") || name.ends_with('.') || name.ends_with(".lock") {
        return Err(coded_err(
            ErrorCode::InvalidInput,
            format!(
                "invalid workspace name {name:?}: it becomes the branch agent-ws/{name}, and Git branch names cannot contain '..' or end with '.' or '.lock'"
            ),
        ));
    }
    Ok(())
}

/// Fail unless Git accepts `branch` as a new branch name, before `land`
/// commits anything. The output must equal the input: `--branch` mode
/// expands `@{-1}` to the previous branch.
fn check_branch(root: &Path, branch: &str) -> Result<()> {
    if branch.starts_with('-') {
        return Err(coded_err(
            ErrorCode::InvalidInput,
            format!("{branch:?} is not a valid branch name; pick another with --branch"),
        ));
    }
    match git::line(root, &["check-ref-format", "--branch", branch]) {
        Ok(out) if out == branch => Ok(()),
        _ => Err(coded_err(
            ErrorCode::InvalidInput,
            format!("{branch:?} is not a valid branch name; pick another with --branch"),
        )),
    }
}

fn read_meta(p: &Path) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// Records in a directory whose file name ends with `suffix`. Records can
/// vanish mid-scan when another process drops or claims them.
fn read_records(dir: &Path, suffix: &str) -> Vec<(PathBuf, Meta)> {
    let mut out: Vec<(PathBuf, Meta)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
        })
        .filter_map(|p| read_meta(&p).map(|m| (p, m)))
        .collect();
    out.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    out
}

fn read_claim_records(dir: &Path) -> Vec<(PathBuf, Meta)> {
    let mut out: Vec<(PathBuf, Meta)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".claim-"))
        })
        .filter_map(|p| read_meta(&p).map(|m| (p, m)))
        .collect();
    out.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    out
}

fn flock_file(path: &Path, blocking: bool) -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    let flags = if blocking {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    while unsafe { libc::flock(f.as_raw_fd(), flags) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if !blocking && e.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(e).with_context(|| format!("lock {}", path.display()));
    }
    Ok(Some(f))
}

/// Hold an exclusive `flock` on `path` until the returned file is dropped.
/// The lock goes away with the process, so a crash cannot leave it held.
fn lock_file(path: &Path) -> Result<std::fs::File> {
    flock_file(path, true).map(|f| f.expect("blocking flock always returns a file"))
}

fn try_lock_file(path: &Path) -> Result<Option<std::fs::File>> {
    flock_file(path, false)
}

/// Write `value` as JSON to `p` by rename, so readers never see half of it.
fn write_json<T: Serialize>(p: &Path, value: &T) -> Result<()> {
    let tmp = p.with_extension(format!("tmp.{}.{}", std::process::id(), nanos()));
    let raw = serde_json::to_vec_pretty(value)?;
    if let Err(e) = std::fs::write(&tmp, raw).and_then(|()| std::fs::rename(&tmp, p)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn write_record(p: &Path, m: &Meta, create_new: bool) -> Result<()> {
    if create_new {
        let raw = serde_json::to_vec_pretty(m)?;
        let tmp = p.with_extension(format!("tmp.{}.{}", std::process::id(), nanos()));
        if let Err(e) = std::fs::write(&tmp, &raw) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        let res = std::fs::hard_link(&tmp, p);
        let _ = std::fs::remove_file(&tmp);
        res.map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => coded_err(
                ErrorCode::AlreadyExists,
                format!("workspace {:?} already exists", m.name),
            ),
            _ => e.into(),
        })?;
        Ok(())
    } else {
        write_json(p, m)
    }
}

fn is_pool_name(name: &str) -> bool {
    name.strip_prefix(POOL_PREFIX)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn is_refreshable(meta_key: &str, deps_key: &str) -> bool {
    meta_key.split_once(':').is_some_and(|(_, d)| d == deps_key)
}

fn copy_file_cow_or_plain(src: &Path, dst: &Path) -> Result<()> {
    let tmp = dst.with_extension(format!("tmp.{}.{}", std::process::id(), nanos()));
    let cloned = fsutil::clone_path(src, &tmp).is_ok();
    if !cloned && let Err(e) = std::fs::copy(src, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn normalize_rel_subpath(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(coded_err(
            ErrorCode::InvalidInput,
            format!("path {raw:?} must be a non-empty relative path"),
        ));
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return Err(coded_err(
            ErrorCode::InvalidInput,
            format!("path {raw:?} must be relative"),
        ));
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(coded_err(
                    ErrorCode::InvalidInput,
                    format!("path {raw:?} must not contain '..'"),
                ));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(coded_err(
                    ErrorCode::InvalidInput,
                    format!("path {raw:?} must be relative"),
                ));
            }
            std::path::Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() || out == Path::new(".git") || out.starts_with(".git") {
        return Err(coded_err(
            ErrorCode::InvalidInput,
            format!("invalid path {raw:?}"),
        ));
    }
    Ok(out)
}

impl Repo {
    /// Find the repository containing `cwd`. Works from inside a workspace too.
    pub fn discover(cwd: &Path) -> Result<Repo> {
        let common = git::line(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .map_err(|_| coded_err(ErrorCode::GitError, "not inside a Git repository"))?;
        let common = PathBuf::from(common);
        if common.file_name().and_then(|n| n.to_str()) != Some(".git") {
            return Err(coded_err(
                ErrorCode::UnsupportedFs,
                format!("bare repositories are not supported ({})", common.display()),
            ));
        }
        let root = common.parent().context("no repository root")?.to_path_buf();
        let base = root.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
        let dir = home()?.join(format!(
            "{base}-{:08x}",
            fnv(&root.to_string_lossy()) as u32
        ));
        let config = Config::load(&root)
            .map_err(|e| coded_err(ErrorCode::InvalidInput, format!("{e:#}")))?;
        Ok(Repo { root, dir, config })
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.dir.join("meta").join(format!("{name}.json"))
    }

    /// Whether a workspace record with this name exists (in any state).
    pub fn exists(&self, name: &str) -> bool {
        check_name(name).is_ok() && self.meta_path(name).exists()
    }

    /// Whether `path` is a workspace folder this tool created: a direct child
    /// of `<dir>/ws` named like a workspace or a pool entry, and not a symlink.
    /// Records are plain files anyone can edit, so every destructive or git
    /// operation on a record's path checks this first.
    pub fn owns(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let named = check_name(name).is_ok() || is_pool_name(name);
        // Compare the exact text: `Path` equality ignores a trailing `/` and
        // repeated separators, which would let a record name `ws/x/` pass.
        if !named || path.as_os_str() != self.dir.join("ws").join(name).as_os_str() {
            return false;
        }
        !path
            .symlink_metadata()
            .is_ok_and(|md| md.file_type().is_symlink())
    }

    fn valid_named_record(&self, stem: &str, m: &Meta) -> bool {
        if check_name(stem).is_err() || m.name != stem || !self.owns(&m.path) {
            return false;
        }
        let Some(fname) = m.path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        fname == stem || is_pool_name(fname)
    }

    fn valid_pool_record(&self, stem: &str, m: &Meta) -> bool {
        is_pool_name(stem)
            && m.name == stem
            && self.owns(&m.path)
            && m.path.as_os_str() == self.dir.join("ws").join(stem).as_os_str()
    }

    pub fn meta(&self, name: &str) -> Result<Meta> {
        check_name(name)?;
        let p = self.meta_path(name);
        let raw = std::fs::read(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                coded_err(ErrorCode::NotFound, format!("no workspace named {name:?}"))
            } else {
                anyhow::Error::new(e).context(format!("no workspace named {name:?}"))
            }
        })?;
        let m: Meta = serde_json::from_slice(&raw)?;
        if !self.valid_named_record(name, &m) {
            return Err(coded_err(
                ErrorCode::Internal,
                format!(
                    "corrupt record for {name:?}: path {} is not a valid workspace folder for {name:?} (run `agent-ws gc`)",
                    m.path.display()
                ),
            ));
        }
        if m.state != "ready" {
            return Err(coded_err(
                ErrorCode::Conflict,
                format!(
                    "workspace {name:?} is still being created (run `agent-ws gc` if its spawn died)"
                ),
            ));
        }
        Ok(m)
    }

    /// Workspaces that finished spawning.
    pub fn list(&self) -> Result<Vec<Meta>> {
        Ok(read_records(&self.dir.join("meta"), ".json")
            .into_iter()
            .filter_map(|(p, m)| {
                let stem = p.file_name()?.to_str()?.strip_suffix(".json")?;
                (m.state == "ready" && self.valid_named_record(stem, &m)).then_some(m)
            })
            .collect())
    }

    fn prepare_home(&self) -> Result<()> {
        for d in ["meta", "pool", "ws", "snaps", ".trash"] {
            std::fs::create_dir_all(self.dir.join(d))?;
        }
        // The folder name holds only the repository's name and a hash: note
        // where it lives, so `known_repos` (and the desktop app) find it.
        let note = self.dir.join("repo.json");
        let root = serde_json::json!({ "root": self.root });
        if std::fs::read(&note)
            .ok()
            .and_then(|r| serde_json::from_slice(&r).ok())
            != Some(root.clone())
        {
            write_json(&note, &root)?;
        }
        // Keep Spotlight from indexing every new tree, and Time Machine from
        // backing up clones at their full logical size. `tmutil` can take
        // ~10 s, so it runs detached instead of blocking the first spawn.
        #[cfg(target_os = "macos")]
        {
            let top = self.dir.parent().context("no home")?;
            let marker = top.join(".metadata_never_index");
            if !marker.exists() {
                std::fs::write(&marker, b"")?;
                let _ = std::process::Command::new("tmutil")
                    .arg("addexclusion")
                    .arg(top)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        }
        Ok(())
    }

    fn resolve(&self, rev: Option<&str>) -> Result<String> {
        if let Some(r) = rev
            && r.starts_with('-')
        {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!("invalid revision {r:?}"),
            ));
        }
        let rev = format!("{}^{{commit}}", rev.unwrap_or("HEAD"));
        git::line(
            &self.root,
            &["rev-parse", "--verify", "--end-of-options", &rev],
        )
        .map_err(|e| coded_err(ErrorCode::GitError, format!("{e:#}")))
    }

    /// Key for everything that shapes a pooled workspace except `HEAD`:
    /// excludes, secret rules, symlink policy, scan skips, and dependency stamps.
    fn deps_key(&self) -> String {
        use std::os::unix::fs::MetadataExt;
        let c = &self.config;
        let mut s = format!(
            "{:?}|{:?}|{:?}|{}|{:?}",
            c.exclude, c.secrets, c.secrets_keep, c.strict_symlinks, c.scan_skip
        );
        for f in c.stamp_files() {
            if let Ok(m) = std::fs::metadata(self.root.join(&f)) {
                s.push_str(&format!(
                    "|{f}:{}:{}:{}",
                    m.mtime(),
                    m.mtime_nsec(),
                    m.size()
                ));
            }
        }
        format!("{:016x}", fnv(&s))
    }

    /// A pooled workspace is reusable while HEAD, the settings that shape a
    /// workspace and the dependency stamp files are unchanged.
    fn pool_key(&self, head: &str) -> String {
        format!("{head}:{}", self.deps_key())
    }

    pub fn pool_target(&self) -> usize {
        let p = self.dir.join("pool.json");
        std::fs::read(p)
            .ok()
            .and_then(|r| serde_json::from_slice::<serde_json::Value>(&r).ok())
            .and_then(|v| v["size"].as_u64())
            .map(|n| n as usize)
            .or(self.config.pool)
            .unwrap_or(0)
            .min(MAX_POOL)
    }

    pub fn pool_set(&self, size: usize) -> Result<()> {
        if size > MAX_POOL {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!("pool size {size} is above the maximum of {MAX_POOL}"),
            ));
        }
        self.prepare_home()?;
        std::fs::write(
            self.dir.join("pool.json"),
            serde_json::to_vec(&serde_json::json!({ "size": size }))?,
        )?;
        Ok(())
    }

    /// Refresh an existing pooled workspace in-place to a new `head` commit
    /// when only `HEAD` moved and `deps_key` still matches.
    fn refresh_pooled(&self, m: &mut Meta, head: &str) -> Result<()> {
        if head.starts_with('-') {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!("invalid revision {head:?}"),
            ));
        }
        git::run(
            &m.path,
            &[
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "checkout",
                "-q",
                "-f",
                "--detach",
                head,
            ],
        )?;
        git::run(&m.path, &["-c", "core.fsmonitor=false", "clean", "-fdq"])?;
        if m.path.join(".gitmodules").exists() {
            isolate_submodules(&m.path, &self.root, &self.root, true)?;
        }
        m.base = head.to_string();
        m.key = self.pool_key(head);
        Ok(())
    }

    pub fn spawn(&self, o: &SpawnOpts) -> Result<Spawned> {
        let t0 = Instant::now();
        if o.ensure && o.auto_name {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                "--ensure cannot be used with --auto",
            ));
        }
        if o.from.is_some() && (o.carry || o.base.is_some()) {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                "--from cannot be used with --base or --carry",
            ));
        }
        check_new_name(&o.name)?;

        if o.ensure && self.meta_path(&o.name).exists() {
            let m = self.meta(&o.name).map_err(|e| {
                if error_code(&e) == ErrorCode::Conflict {
                    coded_err(ErrorCode::AlreadyExists, format!("{e:#}"))
                } else {
                    e
                }
            })?;
            if let Some(b) = &o.branch
                && m.branch != *b
            {
                return Err(coded_err(
                    ErrorCode::InvalidInput,
                    format!(
                        "workspace {:?} already exists on branch {:?}, not {:?}",
                        o.name, m.branch, b
                    ),
                ));
            }
            if o.from.is_some() {
                return Err(coded_err(
                    ErrorCode::InvalidInput,
                    format!(
                        "workspace {:?} already exists; cannot use --from with --ensure",
                        o.name
                    ),
                ));
            }
            if let Some(base_ref) = &o.base {
                let resolved = self.resolve(Some(base_ref))?;
                if resolved != m.base {
                    return Err(coded_err(
                        ErrorCode::InvalidInput,
                        format!(
                            "workspace {:?} already exists at base {}, not {}",
                            o.name, m.base, resolved
                        ),
                    ));
                }
            }
            return Ok(Spawned {
                name: m.name.clone(),
                path: m.path.clone(),
                base: m.base.clone(),
                from_pool: false,
                clone_ms: 0,
                total_ms: t0.elapsed().as_millis(),
                clone_calls: 0,
                notes: m.notes.clone(),
                warnings: vec![],
                port: self.port(&m),
                branch: m.branch.clone(),
                carried: vec![],
                reused: true,
            });
        }

        let effective_name = if o.auto_name {
            (1..=100)
                .map(|i| {
                    if i == 1 {
                        o.name.clone()
                    } else {
                        format!("{}-{i}", o.name)
                    }
                })
                .find(|cand| {
                    !self.meta_path(cand).exists() && !self.dir.join("ws").join(cand).exists()
                })
                .ok_or_else(|| {
                    coded_err(
                        ErrorCode::AlreadyExists,
                        format!("no available workspace name starting with {:?}", o.name),
                    )
                })?
        } else {
            o.name.clone()
        };

        let mut opts = o.clone();
        opts.name = effective_name;

        if let Some(b) = &opts.branch {
            check_branch(&self.root, b)?;
            if self.branch_tip(b).is_some() {
                return Err(coded_err(
                    ErrorCode::AlreadyExists,
                    format!("branch {b} already exists"),
                ));
            }
        }
        let hooks = match &self.config.hooks {
            Some(h) if !opts.no_hooks && !h.post_spawn.is_empty() => {
                self.trusted_hooks()?.post_spawn
            }
            _ => vec![],
        };
        self.prepare_home()?;
        let mut pooled = None;
        if opts.from.is_none()
            && opts.use_pool
            && opts.exclude.is_empty()
            && !opts.keep_secrets
            && !opts.carry
        {
            let head = self.resolve(None)?;
            if self.resolve(opts.base.as_deref())? == head {
                pooled = self.claim(&opts, &head)?;
            }
        }
        let (mut s, mut meta) = match pooled {
            Some(pair) => pair,
            None => match &opts.from {
                Some(src_name) => self.spawn_from(&opts, src_name, t0)?,
                None => self.spawn_fresh(&opts, t0)?,
            },
        };
        // The workspace exists from here on; a failure removes it again so
        // that a spawn either fully happens or leaves nothing. Keep state as
        // `creating` until branch checkout and post_spawn hooks finish.
        let mut finish = || -> Result<()> {
            if let Some(b) = &opts.branch {
                git::run(&meta.path, &["switch", "-q", "-c", b])?;
            }
            self.run_hooks("post_spawn", &hooks, &meta)?;
            meta.state = ready();
            meta.pid = 0;
            meta.pid_start = 0;
            write_record(&self.meta_path(&opts.name), &meta, false)?;
            Ok(())
        };
        if let Err(e) = finish() {
            let _ = self.drop_meta(&meta);
            if let Some(b) = &opts.branch {
                let _ = git::run(&self.root, &["branch", "-q", "-D", b]);
            }
            return Err(e.context(format!("spawn of {:?} undone", opts.name)));
        }
        s.total_ms = t0.elapsed().as_millis();
        s.warnings = self.spawn_warnings();
        Ok(s)
    }

    /// Clone a new workspace for `o` and register it.
    fn spawn_fresh(&self, o: &SpawnOpts, t0: Instant) -> Result<(Spawned, Meta)> {
        let path = self.dir.join("ws").join(&o.name);
        // Claim the name first: the record marks the directory as in use, so
        // `gc` leaves it alone while the owning process is alive.
        let rec = self.meta_path(&o.name);
        let mut meta = Meta {
            name: o.name.clone(),
            path: path.clone(),
            base: String::new(),
            created: now(),
            state: "creating".into(),
            pid: std::process::id(),
            pid_start: own_start(),
            key: String::new(),
            notes: Notes::default(),
            slot: None,
            branch: o.branch.clone().unwrap_or_default(),
            origin: o.origin.clone(),
        };
        self.with_slot(|slot| {
            meta.slot = Some(slot);
            write_record(&rec, &meta, true)
        })?;
        match self.create(o, &path, t0) {
            Ok(mut s) => {
                meta.base = s.base.clone();
                meta.notes = s.notes.clone();
                write_record(&rec, &meta, false)?;
                s.port = self.port(&meta);
                s.branch = meta.branch.clone();
                Ok((s, meta))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&rec);
                Err(e)
            }
        }
    }

    /// Clone a new workspace from an existing workspace `src_name`, preserving
    /// its uncommitted edits, staged index state, untracked files, and original base.
    fn spawn_from(&self, o: &SpawnOpts, src_name: &str, t0: Instant) -> Result<(Spawned, Meta)> {
        let src_meta = self.meta(src_name)?;
        if !self.owns(&src_meta.path) {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!("source workspace {src_name:?} is not owned by this repository"),
            ));
        }
        let path = self.dir.join("ws").join(&o.name);
        let rec = self.meta_path(&o.name);
        let mut meta = Meta {
            name: o.name.clone(),
            path: path.clone(),
            base: String::new(),
            created: now(),
            state: "creating".into(),
            pid: std::process::id(),
            pid_start: own_start(),
            key: String::new(),
            notes: Notes::default(),
            slot: None,
            branch: o.branch.clone().unwrap_or_default(),
            origin: o.origin.clone(),
        };
        self.with_slot(|slot| {
            meta.slot = Some(slot);
            write_record(&rec, &meta, true)
        })?;

        let populate = || -> Result<Spawned> {
            self.check_spawn_env(o, &path)?;
            let src_head = git::line(&src_meta.path, &["rev-parse", "HEAD"])?;
            let p = path.to_string_lossy().to_string();
            self.worktree(&["add", "--no-checkout", "--detach", &p, &src_head])?;
            let run_fill = || -> Result<Spawned> {
                let excludes = self.build_excludes(&o.exclude, &src_meta.path);
                let t = Instant::now();
                let calls = fsutil::clone_tree(&src_meta.path, &path, Path::new(""), &excludes)?;
                let clone_ms = t.elapsed().as_millis();

                let src_idx_s = git::line(
                    &src_meta.path,
                    &[
                        "--no-optional-locks",
                        "rev-parse",
                        "--path-format=absolute",
                        "--git-path",
                        "index",
                    ],
                )?;
                let dst_idx_s = git::line(
                    &path,
                    &[
                        "--no-optional-locks",
                        "rev-parse",
                        "--path-format=absolute",
                        "--git-path",
                        "index",
                    ],
                )?;
                let src_idx = Path::new(&src_idx_s);
                let dst_idx = Path::new(&dst_idx_s);
                if src_idx.exists() {
                    copy_file_cow_or_plain(src_idx, dst_idx)?;
                }

                if path.join(".gitmodules").exists() {
                    isolate_submodules(&path, &src_meta.path, &self.root, false)?;
                }

                let scan = self.scan_and_check(&src_meta.path, &path, o.keep_secrets)?;
                let notes = Notes {
                    symlinks_retargeted: scan.retargeted,
                    symlinks_external: scan
                        .external
                        .into_iter()
                        .map(|(path, target)| ExternalLink { path, target })
                        .collect(),
                    secrets_removed: scan.secrets,
                };
                Ok(Spawned {
                    name: o.name.clone(),
                    path: path.clone(),
                    base: src_meta.base.clone(),
                    from_pool: false,
                    clone_ms,
                    total_ms: t0.elapsed().as_millis(),
                    clone_calls: calls,
                    notes,
                    warnings: vec![],
                    port: self.port(&meta),
                    branch: meta.branch.clone(),
                    carried: vec![],
                    reused: false,
                })
            };
            match run_fill() {
                Ok(s) => Ok(s),
                Err(e) => {
                    let _ = self.discard(&path);
                    Err(e)
                }
            }
        };

        match populate() {
            Ok(s) => {
                meta.base = s.base.clone();
                meta.notes = s.notes.clone();
                write_record(&rec, &meta, false)?;
                Ok((s, meta))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&rec);
                Err(e)
            }
        }
    }

    /// Tip of a local branch, if it exists.
    fn branch_tip(&self, branch: &str) -> Option<String> {
        git::line(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}^{{commit}}"),
            ],
        )
        .ok()
    }

    fn try_claim_entry(
        &self,
        o: &SpawnOpts,
        rec: &Path,
        mut m: Meta,
        head: Option<&str>,
    ) -> Result<Option<(Spawned, Meta)>> {
        // rename() is atomic: of several spawns racing for this entry,
        // exactly one moves the record; the others get NotFound.
        let claim = rec.with_extension(claim_ext());
        if std::fs::rename(rec, &claim).is_err() {
            return Ok(None);
        }
        if !m.path.exists() {
            let _ = std::fs::remove_file(&claim);
            return Ok(None);
        }
        if let Some(target_head) = head
            && let Err(_) = self.refresh_pooled(&mut m, target_head)
        {
            let _ = self.discard(&m.path);
            let _ = std::fs::remove_file(&claim);
            return Ok(None);
        }
        let mut named = Meta {
            name: o.name.clone(),
            created: now(),
            state: "creating".into(),
            pid: std::process::id(),
            pid_start: own_start(),
            key: String::new(),
            branch: o.branch.clone().unwrap_or_default(),
            origin: o.origin.clone(),
            ..m.clone()
        };
        let written = self.with_slot(|slot| {
            named.slot = Some(slot);
            write_record(&self.meta_path(&o.name), &named, true)
        });
        if let Err(e) = written {
            if head.is_some() && write_json(rec, &m).is_ok() {
                let _ = std::fs::remove_file(&claim);
            } else {
                let _ = std::fs::rename(&claim, rec);
            }
            return Err(e);
        }
        std::fs::remove_file(&claim)?;
        let spawned = Spawned {
            name: o.name.clone(),
            path: m.path,
            base: m.base,
            from_pool: true,
            clone_ms: 0,
            total_ms: 0,
            clone_calls: 0,
            notes: m.notes,
            warnings: vec![],
            port: self.port(&named),
            branch: named.branch.clone(),
            carried: vec![],
            reused: false,
        };
        Ok(Some((spawned, named)))
    }

    /// Take a fresh (or incrementally refreshable) pooled workspace and register it under `o.name`.
    fn claim(&self, o: &SpawnOpts, head: &str) -> Result<Option<(Spawned, Meta)>> {
        let deps = self.deps_key();
        let full_key = format!("{head}:{deps}");
        let recs = read_records(&self.dir.join("pool"), ".json");
        // First pass: exact fresh match.
        for (rec, m) in &recs {
            if m.state == "pooled"
                && m.key == full_key
                && self.owns(&m.path)
                && let Some(pair) = self.try_claim_entry(o, rec, m.clone(), None)?
            {
                return Ok(Some(pair));
            }
        }
        // Second pass: refreshable match (only HEAD changed; deps_key matches).
        for (rec, m) in recs {
            if m.state == "pooled"
                && m.key != full_key
                && is_refreshable(&m.key, &deps)
                && self.owns(&m.path)
                && let Some(pair) = self.try_claim_entry(o, &rec, m, Some(head))?
            {
                return Ok(Some(pair));
            }
        }
        Ok(None)
    }

    fn check_spawn_env(&self, o: &SpawnOpts, path: &Path) -> Result<()> {
        if path.exists() {
            return Err(coded_err(
                ErrorCode::AlreadyExists,
                format!("{} already exists; run `agent-ws gc`", path.display()),
            ));
        }
        if fsutil::device(&self.root)? != fsutil::device(&self.dir)? {
            return Err(coded_err(
                ErrorCode::UnsupportedFs,
                format!(
                    "{} and {} are on different volumes; clones need one volume (set AGENT_WS_HOME)",
                    self.root.display(),
                    self.dir.display()
                ),
            ));
        }
        let min_free = o
            .min_free_bytes
            .or(self.config.min_free_gb.map(|g| g * GB))
            .unwrap_or(2 * GB);
        let free = fsutil::free_bytes(&self.dir)?;
        if free < min_free {
            return Err(coded_err(
                ErrorCode::LowDisk,
                format!(
                    "only {} MB free on disk (minimum {} MB)",
                    free / 1_000_000,
                    min_free / 1_000_000
                ),
            ));
        }
        Ok(())
    }

    fn build_excludes(&self, extra: &[String], src_root: &Path) -> Vec<PathBuf> {
        let mut excludes: Vec<PathBuf> = DEFAULT_EXCLUDES.iter().map(PathBuf::from).collect();
        excludes.extend(self.config.exclude.iter().chain(extra).map(PathBuf::from));
        // Never clone the agent-ws home if it lives inside the repository.
        if let Ok(rel) = self.dir.strip_prefix(src_root)
            && !rel.as_os_str().is_empty()
        {
            excludes.push(rel.to_path_buf());
        }
        excludes
    }

    fn scan_and_check(
        &self,
        src_root: &Path,
        path: &Path,
        keep_secrets: bool,
    ) -> Result<fsutil::Scan> {
        let tracked: HashSet<String> = git::run(path, &["ls-files", "-z", "--recurse-submodules"])?
            .split('\0')
            .map(String::from)
            .collect();
        let never = |_: &str| false;
        let secret = |n: &str| self.config.is_secret(n);
        let is_secret: &(dyn Fn(&str) -> bool + Sync) = if keep_secrets { &never } else { &secret };
        let scan =
            fsutil::scan_with_skip(src_root, path, &tracked, is_secret, &self.config.scan_skip)?;
        if self.config.strict_symlinks && !scan.external.is_empty() {
            let list: Vec<String> = scan
                .external
                .iter()
                .map(|(p, t)| format!("{} -> {}", p.display(), t.display()))
                .collect();
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!(
                    "symlinks point outside the workspace (strict_symlinks): {}",
                    list.join(", ")
                ),
            ));
        }
        Ok(scan)
    }

    fn create(&self, o: &SpawnOpts, path: &Path, t0: Instant) -> Result<Spawned> {
        self.check_spawn_env(o, path)?;
        let base = self.resolve(o.base.as_deref())?;
        if o.carry && base != self.resolve(None)? {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                "--carry brings changes made on top of HEAD, so the base must be HEAD",
            ));
        }
        let p = path.to_string_lossy().to_string();
        self.worktree(&["add", "--no-checkout", "--detach", &p, &base])?;
        match self.fill(o, path) {
            Ok(f) => Ok(Spawned {
                name: o.name.clone(),
                path: path.to_path_buf(),
                base,
                from_pool: false,
                clone_ms: f.clone_ms,
                total_ms: t0.elapsed().as_millis(),
                clone_calls: f.calls,
                notes: f.notes,
                warnings: vec![],
                port: None,
                branch: String::new(),
                carried: f.carried,
                reused: false,
            }),
            Err(e) => {
                let _ = self.discard(path);
                Err(e)
            }
        }
    }

    /// Clone the checkout into `path`, then make its tracked files equal the
    /// worktree's HEAD (the base commit), unless `carry` keeps the source's
    /// uncommitted edits and untracked files.
    fn fill(&self, o: &SpawnOpts, path: &Path) -> Result<Filled> {
        let excludes = self.build_excludes(&o.exclude, &self.root);
        let t = Instant::now();
        let calls = fsutil::clone_tree(&self.root, path, Path::new(""), &excludes)?;
        let clone_ms = t.elapsed().as_millis();

        // Index from the base commit, then drop the source's uncommitted
        // edits and untracked files. Ignored files (deps, caches) stay.
        // With `carry` the edits stay too, as unstaged changes on HEAD.
        git::run(path, &["reset", "-q"])?;
        if !o.carry {
            git::run(path, &["checkout", "-q", "-f", "--", "."])?;
        }
        // `clean` removes untracked, not-ignored files. Secrets among them are
        // reported as removed, or set aside and put back with --keep-secrets.
        let untracked = git_bytes(path, &["ls-files", "-o", "--exclude-standard", "-z"])?;
        let mut cleaned_secrets: Vec<PathBuf> = untracked
            .split(|&b| b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| PathBuf::from(OsStr::from_bytes(r)))
            .filter(|rel| {
                rel.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| self.config.is_secret(n))
                    && !fsutil::only_certificates(&path.join(rel))
            })
            .collect();
        if o.carry {
            // Untracked files come along; secrets among them only with
            // --keep-secrets.
            if o.keep_secrets {
                cleaned_secrets.clear();
            }
            for rel in &cleaned_secrets {
                std::fs::remove_file(path.join(rel))?;
            }
        } else {
            let hold = self.dir.join(".trash").join(format!("hold-{}", nanos()));
            if o.keep_secrets {
                for rel in &cleaned_secrets {
                    let held = hold.join(rel);
                    std::fs::create_dir_all(held.parent().context("no parent")?)?;
                    std::fs::rename(path.join(rel), held)?;
                }
            }
            git::run(path, &["clean", "-fdq"])?;
            if o.keep_secrets {
                for rel in cleaned_secrets.drain(..) {
                    let back = path.join(&rel);
                    std::fs::create_dir_all(back.parent().context("no parent")?)?;
                    std::fs::rename(hold.join(&rel), back)?;
                }
                let _ = std::fs::remove_dir_all(&hold);
            }
        }
        if path.join(".gitmodules").exists() {
            isolate_submodules(path, &self.root, &self.root, !o.carry)?;
        }

        let scan = self.scan_and_check(&self.root, path, o.keep_secrets)?;
        let notes = Notes {
            symlinks_retargeted: scan.retargeted,
            symlinks_external: scan
                .external
                .into_iter()
                .map(|(path, target)| ExternalLink { path, target })
                .collect(),
            secrets_removed: {
                let mut all = scan.secrets;
                all.extend(
                    cleaned_secrets
                        .into_iter()
                        .map(|p| p.to_string_lossy().into_owned()),
                );
                all.sort();
                all
            },
        };
        let carried = if o.carry {
            git::changed_paths(path, "HEAD")?
        } else {
            vec![]
        };
        Ok(Filled {
            clone_ms,
            calls,
            notes,
            carried,
        })
    }

    /// Bring the pool to `size` (default: the configured target) fresh
    /// workspaces, refreshing commit-stale entries in-place and removing stale ones.
    /// Only one fill runs at a time.
    pub fn pool_fill(&self, size: Option<usize>) -> Result<PoolFill> {
        static POOL_SEQ: AtomicU64 = AtomicU64::new(0);
        self.prepare_home()?;
        let Some(_lock) = try_lock_file(&self.dir.join("pool.lock"))? else {
            return Ok(PoolFill {
                busy: true,
                ..Default::default()
            });
        };
        let target = size.unwrap_or_else(|| self.pool_target());
        let head = self.resolve(None)?;
        let deps = self.deps_key();
        let key = format!("{head}:{deps}");
        let mut report = PoolFill::default();
        let mut fresh = 0;
        let mut fresh_recs = Vec::new();
        let mut refreshable_recs = Vec::new();
        let mut stale_recs = Vec::new();
        for (rec, m) in read_records(&self.dir.join("pool"), ".json") {
            // Corrupt records are left for `gc`, which forgets them safely.
            if m.state != "pooled" || !self.owns(&m.path) {
                continue;
            }
            if m.key == key {
                fresh_recs.push((rec, m));
            } else if is_refreshable(&m.key, &deps) {
                refreshable_recs.push((rec, m));
            } else {
                stale_recs.push((rec, m));
            }
        }
        let discard_pooled = |rec: &Path, m: &Meta| -> Result<bool> {
            let claim = rec.with_extension(claim_ext());
            if std::fs::rename(rec, &claim).is_ok() {
                let res = self.discard(&m.path);
                let _ = std::fs::remove_file(&claim);
                res?;
                return Ok(true);
            }
            Ok(false)
        };
        for (rec, m) in fresh_recs {
            if fresh < target {
                fresh += 1;
            } else if discard_pooled(&rec, &m)? {
                report.removed_extra += 1;
            }
        }
        for (rec, mut m) in refreshable_recs {
            if fresh < target {
                let claim = rec.with_extension(claim_ext());
                if std::fs::rename(&rec, &claim).is_ok() {
                    if m.path.exists() && self.refresh_pooled(&mut m, &head).is_ok() {
                        write_json(&rec, &m)?;
                        let _ = std::fs::remove_file(&claim);
                        fresh += 1;
                        report.refreshed += 1;
                    } else {
                        let res = self.discard(&m.path);
                        let _ = std::fs::remove_file(&claim);
                        res?;
                        report.removed_stale += 1;
                    }
                }
            } else if discard_pooled(&rec, &m)? {
                report.removed_stale += 1;
            }
        }
        for (rec, m) in stale_recs {
            if discard_pooled(&rec, &m)? {
                report.removed_stale += 1;
            }
        }
        let opts = SpawnOpts::default();
        while fresh < target {
            let seq = POOL_SEQ.fetch_add(1, Ordering::Relaxed);
            let id = format!("{POOL_PREFIX}{}{seq:04}", nanos());
            let rec = self.dir.join("pool").join(format!("{id}.json"));
            let path = self.dir.join("ws").join(&id);
            let mut meta = Meta {
                name: id.clone(),
                path: path.clone(),
                base: String::new(),
                created: now(),
                state: "creating".into(),
                pid: std::process::id(),
                pid_start: own_start(),
                key: key.clone(),
                notes: Notes::default(),
                slot: None,
                branch: String::new(),
                origin: String::new(),
            };
            write_record(&rec, &meta, true)?;
            match self.create(
                &SpawnOpts {
                    name: id,
                    ..opts.clone()
                },
                &path,
                Instant::now(),
            ) {
                Ok(s) => {
                    meta.base = s.base;
                    meta.notes = s.notes;
                    meta.state = "pooled".into();
                    meta.pid = 0;
                    meta.pid_start = 0;
                    write_record(&rec, &meta, false)?;
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&rec);
                    return Err(e);
                }
            }
            fresh += 1;
            report.created += 1;
        }
        report.ready = fresh;
        Ok(report)
    }

    pub fn pool_status(&self) -> Result<PoolStatus> {
        let head = self.resolve(None)?;
        let deps = self.deps_key();
        let key = format!("{head}:{deps}");
        let recs = read_records(&self.dir.join("pool"), ".json");
        let mut fresh = 0;
        let mut refreshable = 0;
        let mut stale = 0;
        let mut creating = 0;
        for (_, m) in &recs {
            if m.state == "creating" {
                creating += 1;
            } else if m.state == "pooled" {
                if m.key == key {
                    fresh += 1;
                } else if is_refreshable(&m.key, &deps) {
                    refreshable += 1;
                } else {
                    stale += 1;
                }
            }
        }
        Ok(PoolStatus {
            target: self.pool_target(),
            fresh,
            refreshable,
            stale,
            creating,
        })
    }

    /// Move `path` into `.trash/<prefix>-<nanos>` and delete it in the background.
    fn trash_path(&self, path: &Path, prefix: &str) -> Result<()> {
        if path.symlink_metadata().is_ok() {
            let trash_name = format!("{prefix}-{}", nanos());
            let trash = self.dir.join(".trash").join(&trash_name);
            std::fs::create_dir_all(self.dir.join(".trash"))?;
            std::fs::rename(path, &trash)?;
            let pid = delete_in_background_pid(&trash)?;
            self.record_trash_pid(&trash_name, pid);
        }
        Ok(())
    }

    /// Move a workspace tree into `.trash`, delete it in the background and
    /// forget its worktree registration. Refuses any path this tool does not
    /// own, whatever a record says.
    fn discard(&self, path: &Path) -> Result<()> {
        if !self.owns(path) {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!(
                    "refusing to remove {}: not a workspace folder of this repository",
                    path.display()
                ),
            ));
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("ws");
        self.trash_path(path, name)?;
        self.worktree(&["prune"])?;
        Ok(())
    }

    fn trash_pid_path(&self, trash_name: &str) -> PathBuf {
        self.scratch().join(format!("rm-{trash_name}.pid"))
    }

    fn record_trash_pid(&self, trash_name: &str, pid: u32) {
        let start = process_start(pid).unwrap_or(0);
        let _ = std::fs::create_dir_all(self.scratch());
        let _ = std::fs::write(self.trash_pid_path(trash_name), format!("{pid}:{start}"));
    }

    fn trash_delete_running(&self, trash_name: &str) -> bool {
        let Ok(raw) = std::fs::read_to_string(self.trash_pid_path(trash_name)) else {
            return false;
        };
        let Some((pid_s, start_s)) = raw.trim().split_once(':') else {
            return false;
        };
        let Ok(pid) = pid_s.parse::<u32>() else {
            return false;
        };
        let start = start_s.parse::<u64>().unwrap_or(0);
        pid_alive(pid, start)
    }

    /// Run `git worktree <args>` in the source repository, one agent-ws
    /// process at a time. `prune` deletes `.git/worktrees` once it is empty,
    /// and Git 2.56 dies with "Invalid path" when a parallel prune or `add`
    /// is still using it, so parallel lands and drops failed after doing
    /// their work. The `flock` goes away with the process, so a crash cannot
    /// leave it held.
    fn worktree(&self, args: &[&str]) -> Result<String> {
        let _lock = lock_file(&self.dir.join("worktrees.lock"))?;
        let args: Vec<&str> = std::iter::once("worktree")
            .chain(args.iter().copied())
            .collect();
        git::run(&self.root, &args)
    }

    /// Run `write` with the lowest port slot no workspace record holds,
    /// under `slots.lock`, so parallel spawns get different slots.
    fn with_slot<T>(&self, write: impl FnOnce(u32) -> Result<T>) -> Result<T> {
        let _lock = lock_file(&self.dir.join("slots.lock"))?;
        let used: HashSet<u32> = read_records(&self.dir.join("meta"), ".json")
            .into_iter()
            .filter_map(|(_, m)| m.slot)
            .collect();
        write((0..).find(|s| !used.contains(s)).unwrap_or(0))
    }

    /// `PORT` for a workspace: `port_base + slot * port_step`.
    pub fn port(&self, m: &Meta) -> Option<u32> {
        let base = u32::from(self.config.port_base.unwrap_or(PORT_BASE));
        let step = u32::from(self.config.port_step.unwrap_or(PORT_STEP));
        let port = base + m.slot? * step;
        (port <= u32::from(u16::MAX)).then_some(port)
    }

    /// Environment for commands run in a workspace (`exec`, hooks).
    pub fn env(&self, m: &Meta) -> Vec<(String, String)> {
        let mut env = vec![
            ("AGENT_WS_NAME".to_string(), m.name.clone()),
            (
                "AGENT_WS_PATH".into(),
                m.path.to_string_lossy().into_owned(),
            ),
            ("AGENT_WS_BASE".into(), m.base.clone()),
            (
                "AGENT_WS_REPO".into(),
                self.root.to_string_lossy().into_owned(),
            ),
        ];
        if let Some(slot) = m.slot {
            env.push(("AGENT_WS_SLOT".into(), slot.to_string()));
        }
        if let Some(port) = self.port(m) {
            env.push(("AGENT_WS_PORT".into(), port.to_string()));
            env.push(("PORT".into(), port.to_string()));
        }
        env
    }

    /// The hooks from `.agent-ws.toml`, provided the user trusts this exact
    /// set (`agent-ws trust`). The file comes with the repository, so
    /// without this a clone could run any command on `spawn` or `land`.
    fn trusted_hooks(&self) -> Result<config::Hooks> {
        let hooks = self.config.hooks.clone().unwrap_or_default();
        if hooks.is_empty() {
            return Ok(hooks);
        }
        let trusted: Option<config::Hooks> = std::fs::read(self.dir.join("trusted-hooks.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok());
        if trusted.as_ref() == Some(&hooks) {
            return Ok(hooks);
        }
        let mut list = String::new();
        for (event, cmds) in [
            ("post_spawn", &hooks.post_spawn),
            ("pre_land", &hooks.pre_land),
        ] {
            for c in cmds {
                list.push_str(&format!("\n  {event}: {c}"));
            }
        }
        Err(coded_err(
            ErrorCode::UntrustedHooks,
            format!(
                "{} has hooks that run commands in your workspaces:{list}\nReview them, then run `agent-ws trust` (or skip them with --no-hooks / --no-verify)",
                config::FILE
            ),
        ))
    }

    /// Trust the hooks `.agent-ws.toml` has now. Changing them later needs
    /// another `trust`.
    pub fn trust(&self) -> Result<config::Hooks> {
        let hooks = self.config.hooks.clone().unwrap_or_default();
        self.prepare_home()?;
        write_json(&self.dir.join("trusted-hooks.json"), &hooks)?;
        Ok(hooks)
    }

    /// Run hook commands in a workspace; the first failure stops. Their
    /// output goes to stderr, so `--json` output and the path printed for
    /// Claude Code stay clean.
    fn run_hooks(&self, event: &str, cmds: &[String], m: &Meta) -> Result<()> {
        use std::os::fd::AsFd;
        for c in cmds {
            let out = std::io::stderr().as_fd().try_clone_to_owned()?;
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(c)
                .current_dir(&m.path)
                .envs(self.env(m))
                .stdin(std::process::Stdio::null())
                .stdout(out)
                .status()
                .with_context(|| format!("{event} hook `{c}`"))?;
            if !status.success() {
                bail!("{event} hook `{c}` failed ({status})");
            }
        }
        Ok(())
    }

    /// Workspace directories some record points at, including pool claims.
    fn referenced(&self) -> HashSet<PathBuf> {
        let mut out = HashSet::new();
        for (_, m) in read_records(&self.dir.join("meta"), ".json") {
            out.insert(m.path);
        }
        for (_, m) in read_records(&self.dir.join("pool"), ".json")
            .into_iter()
            .chain(read_claim_records(&self.dir.join("pool")))
        {
            out.insert(m.path);
        }
        out
    }

    /// Clean up after crashes and manual deletes. Never touches a workspace
    /// whose creating or claiming process is still alive.
    pub fn gc(&self, dry_run: bool) -> Result<Gc> {
        let mut report = Gc::default();
        for (dir, is_pool) in [
            (self.dir.join("meta"), false),
            (self.dir.join("pool"), true),
        ] {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let rec = e.path();
                let Some(fname) = rec.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(stem) = fname.strip_suffix(".json") else {
                    continue;
                };
                let Some(m) = read_meta(&rec) else {
                    report.corrupt.push(stem.to_string());
                    if !dry_run {
                        let _ = std::fs::remove_file(&rec);
                    }
                    continue;
                };
                let valid = if is_pool {
                    self.valid_pool_record(stem, &m)
                } else {
                    self.valid_named_record(stem, &m)
                };
                if !valid {
                    report.corrupt.push(m.name.clone());
                    if !dry_run {
                        let _ = std::fs::remove_file(&rec);
                    }
                    continue;
                }
                let dead = m.state == "creating" && !pid_alive(m.pid, m.pid_start);
                let missing = m.state != "creating" && m.path.symlink_metadata().is_err();
                if !(dead || missing) {
                    continue;
                }
                if dead {
                    report.interrupted.push(m.name.clone())
                } else {
                    report.missing.push(m.name.clone())
                }
                if !dry_run {
                    self.discard(&m.path)?;
                    let _ = std::fs::remove_file(&rec);
                }
            }
        }
        // A claim file whose process died: if a named record already points
        // at the workspace the claim had succeeded; otherwise it is lost.
        for (rec, m) in read_claim_records(&self.dir.join("pool")) {
            // `claim-<pid>-<start>`; older claims carry only the pid.
            let ext = rec
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|e| e.strip_prefix("claim-"))
                .unwrap_or("");
            let mut parts = ext.splitn(2, '-');
            let pid = parts.next().and_then(|p| p.parse::<u32>().ok());
            let start = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if pid.is_some_and(|p| pid_alive(p, start)) {
                continue;
            }
            report.claims += 1;
            if !dry_run {
                let owned = read_records(&self.dir.join("meta"), ".json")
                    .into_iter()
                    .any(|(p, n)| {
                        p.file_name()
                            .and_then(|f| f.to_str())
                            .and_then(|f| f.strip_suffix(".json"))
                            .is_some_and(|stem| self.valid_named_record(stem, &n))
                            && n.path == m.path
                    });
                if !owned && self.owns(&m.path) {
                    self.discard(&m.path)?;
                }
                let _ = std::fs::remove_file(rec);
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.dir.join("ws")) {
            let mut refs = self.referenced();
            for e in rd {
                let e = e?;
                let path = e.path();
                // Folders whose name this tool never creates are left alone.
                // Re-read on a miss: a spawn may have registered it meanwhile.
                if !self.owns(&path) || refs.contains(&path) || {
                    refs = self.referenced();
                    refs.contains(&path)
                } {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                // A named folder can hold an agent's work (its record was
                // lost or deleted by hand): register it again, never delete.
                // Pool folders never had an agent in them.
                if !name.starts_with(POOL_PREFIX) {
                    match self.orphan_work(&path) {
                        Err(_) => {
                            report.unreadable.push(name);
                            continue;
                        }
                        Ok(Some(base)) => {
                            if !dry_run {
                                self.adopt(&name, &path, &base)?;
                            }
                            report.adopted.push(name);
                            continue;
                        }
                        Ok(None) => {}
                    }
                }
                report.orphans.push(name);
                if !dry_run {
                    self.discard(&path)?;
                }
            }
        }
        report.trash = self.sweep_trash(dry_run)?;
        if !dry_run {
            self.sweep_leftovers();
        }
        if !dry_run && self.dir.exists() {
            self.worktree(&["prune"])?;
        }
        report.orphans.sort();
        report.adopted.sort();
        report.unreadable.sort();
        report.corrupt.sort();
        Ok(report)
    }

    /// Whether an orphan workspace folder holds work: uncommitted changes,
    /// or a HEAD that no branch, remote branch or tag contains. Returns the
    /// base to register it with, `None` when deleting it loses nothing, and
    /// an error when Git cannot read it (then nobody should delete it).
    fn orphan_work(&self, path: &Path) -> Result<Option<String>> {
        // Without its own `.git`, Git would answer for an enclosing
        // repository. Only an empty folder is safe to remove then.
        if path.join(".git").symlink_metadata().is_err() {
            if std::fs::read_dir(path)?.next().is_none() {
                return Ok(None);
            }
            bail!("not a Git worktree");
        }
        let head = git::line(path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        // The folder must really be a worktree of this repository, not a
        // nested repository that answers for itself.
        let common = git::line(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let expected = self
            .root
            .join(".git")
            .canonicalize()
            .unwrap_or_else(|_| self.root.join(".git"));
        let actual = Path::new(&common)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&common));
        if actual != expected {
            bail!("not a worktree of this repository");
        }
        let dirty = is_dirty(path)?;
        let unreachable = !git::line(
            &self.root,
            &[
                "rev-list",
                "-1",
                &head,
                "--not",
                "--branches",
                "--remotes",
                "--tags",
            ],
        )?
        .is_empty();
        if !(dirty || unreachable) {
            return Ok(None);
        }
        let base = self
            .prune_base()
            .ok()
            .and_then(|b| git::line(&self.root, &["merge-base", &head, &b]).ok())
            .unwrap_or(head);
        Ok(Some(base))
    }

    /// Write a ready record for an existing workspace folder.
    fn adopt(&self, name: &str, path: &Path, base: &str) -> Result<()> {
        let meta = Meta {
            name: name.into(),
            path: path.to_path_buf(),
            base: base.into(),
            created: now(),
            state: ready(),
            pid: 0,
            pid_start: 0,
            key: String::new(),
            notes: Notes::default(),
            slot: None,
            branch: String::new(),
            origin: String::new(),
        };
        write_record(&self.meta_path(name), &meta, true)
    }

    /// Whether the background prune after `spawn` should run now: enabled
    /// (`auto_prune`, default on) and not run in the last 10 minutes.
    /// Marks it as run.
    pub fn auto_prune_due(&self) -> bool {
        if self.config.auto_prune == Some(false) {
            return false;
        }
        let stamp = self.dir.join("auto-prune.stamp");
        let recent = std::fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < AUTO_PRUNE_EVERY_SECS);
        !recent && std::fs::write(&stamp, b"").is_ok()
    }

    /// Restart the delete of trash entries whose background `rm` died (older
    /// than 10 minutes). Returns how many there were.
    fn sweep_trash(&self, dry_run: bool) -> Result<usize> {
        let mut n = 0;
        let now_ns = nanos();
        for e in std::fs::read_dir(self.dir.join(".trash"))
            .into_iter()
            .flatten()
        {
            let e = e?;
            // Entries are named `<name>-<nanos>`. Recent ones are still
            // being deleted by their own background `rm`.
            let name = e.file_name().to_string_lossy().into_owned();
            let born = name
                .rsplit('-')
                .next()
                .and_then(|n| n.parse::<u128>().ok())
                .unwrap_or(0);
            if now_ns.saturating_sub(born) < TRASH_GRACE_NS {
                continue;
            }
            if self.trash_delete_running(&name) {
                continue;
            }
            n += 1;
            if !dry_run {
                let pid = delete_in_background_pid(&e.path())?;
                self.record_trash_pid(&name, pid);
            }
        }
        Ok(n)
    }

    /// Run records of dropped workspaces, stale temp files in `meta`/`pool`,
    /// and snapshot index files that a killed process left behind.
    fn sweep_leftovers(&self) {
        for e in std::fs::read_dir(self.dir.join("runs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            // `<name>.json`, `<name>.log`, or `<name>.tmp.<pid>` from a write
            // that was cut short. Names may hold dots themselves.
            let file = e.file_name().to_string_lossy().into_owned();
            let name = file
                .strip_suffix(".json")
                .or_else(|| file.strip_suffix(".log"))
                .or_else(|| file.rsplit_once(".tmp.").map(|(n, _)| n));
            if name.is_some_and(|n| !self.meta_path(n).exists()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        for dir in [self.dir.join("meta"), self.dir.join("pool")] {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let file = e.file_name().to_string_lossy().into_owned();
                let Some((_, rest)) = file.rsplit_once(".tmp.") else {
                    continue;
                };
                let pid = rest
                    .split('.')
                    .next()
                    .and_then(|p| p.parse::<u32>().ok())
                    .unwrap_or(0);
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_nanos() > TRASH_GRACE_NS);
                if old || (pid != 0 && !pid_alive(pid, 0)) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        for e in std::fs::read_dir(self.dir.join("snaps"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if !self.meta_path(&name).exists() {
                let _ = self.trash_path(&e.path(), &format!("snaps-{name}"));
            }
        }
        for e in std::fs::read_dir(self.scratch())
            .into_iter()
            .flatten()
            .flatten()
        {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_nanos() > TRASH_GRACE_NS);
            if old {
                let _ = if e.file_type().is_ok_and(|ft| ft.is_dir()) {
                    std::fs::remove_dir_all(e.path())
                } else {
                    std::fs::remove_file(e.path())
                };
            }
        }
    }

    /// Warnings for `spawn`: many workspaces, little free disk, or private disk budget exceeded.
    fn spawn_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let limit = self.config.warn_workspaces.unwrap_or(WARN_WORKSPACES);
        let count = self.list().map(|l| l.len()).unwrap_or(0);
        if limit > 0 && count >= limit {
            out.push(format!(
                "{count} workspaces exist; drop finished ones or run `agent-ws prune`"
            ));
        }
        if let Ok(free) = fsutil::free_bytes(&self.dir)
            && free < WARN_FREE
        {
            out.push(format!(
                "only {:.1} GB free on disk; see `agent-ws du` and `agent-ws prune`",
                free as f64 / GB as f64
            ));
        }
        if let Some(max_gb) = self.config.max_private_gb
            && let Ok(du) = self.du_with_options(None, 0, false)
            && du.total.private > max_gb.saturating_mul(GB)
        {
            out.push(format!(
                "private disk usage {:.1} GB exceeds max_private_gb ({max_gb} GB); see `agent-ws du` and `agent-ws prune`",
                du.total.private as f64 / GB as f64
            ));
        }
        out
    }

    /// Real disk use of every workspace, pooled workspace and trash entry.
    /// `only` limits it to one named workspace.
    pub fn du(&self, only: Option<&str>) -> Result<DiskUsage> {
        self.du_with_options(only, 0, true)
    }

    /// Real disk use with optional per-directory `depth` breakdown and
    /// cohort shared-extent accounting (`include_cohort`).
    pub fn du_with_options(
        &self,
        only: Option<&str>,
        depth: usize,
        include_cohort: bool,
    ) -> Result<DiskUsage> {
        let mut todo: Vec<(String, PathBuf, &str)> = Vec::new();
        if let Some(n) = only {
            let m = self.meta(n)?;
            todo.push((m.name, m.path, "workspace"));
        } else {
            for m in self.list()? {
                todo.push((m.name, m.path, "workspace"));
            }
            for (_, m) in read_records(&self.dir.join("pool"), ".json") {
                if self.owns(&m.path) {
                    todo.push((m.name, m.path, "pool"));
                }
            }
            for e in std::fs::read_dir(self.dir.join(".trash"))
                .into_iter()
                .flatten()
            {
                let e = e?;
                todo.push((
                    e.file_name().to_string_lossy().into_owned(),
                    e.path(),
                    "trash",
                ));
            }
        }
        let compute_cohort = include_cohort && only.is_none();
        let paths: Vec<PathBuf> = todo.iter().map(|t| t.1.clone()).collect();
        let (detailed, source_ids) = std::thread::scope(|s| {
            let src_handle = if compute_cohort {
                Some(s.spawn(|| fsutil::shared_ids(&self.root)))
            } else {
                None
            };
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            let chunk = paths.len().div_ceil(threads).max(1);
            let handles: Vec<_> = paths
                .chunks(chunk)
                .map(|c| {
                    s.spawn(move || {
                        c.iter()
                            .map(|p| fsutil::usage_detailed(p, depth, compute_cohort))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let detailed: Vec<_> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect();
            let source_ids = src_handle.and_then(|h| h.join().ok()).unwrap_or_default();
            (detailed, source_ids)
        });

        let mut total = fsutil::Usage::default();
        let mut cohort_extents: HashMap<u64, u64> = HashMap::new();
        let entries = todo
            .into_iter()
            .zip(detailed)
            .map(|((name, path, kind), (usage, parts, samples))| {
                total += usage;
                if compute_cohort {
                    for sample in samples {
                        if !source_ids.contains(&sample.id) {
                            cohort_extents
                                .entry(sample.id)
                                .and_modify(|b| *b = (*b).max(sample.bytes))
                                .or_insert(sample.bytes);
                        }
                    }
                }
                let mut breakdown: Vec<DirUsage> = parts
                    .into_iter()
                    .map(|(p, u)| DirUsage { path: p, usage: u })
                    .collect();
                breakdown.sort_by(|a, b| {
                    b.usage
                        .private
                        .cmp(&a.usage.private)
                        .then_with(|| b.usage.allocated.cmp(&a.usage.allocated))
                        .then_with(|| a.path.cmp(&b.path))
                });
                WsUsage {
                    name,
                    path,
                    kind: kind.into(),
                    usage,
                    breakdown,
                }
            })
            .collect();
        let cohort_shared_bytes: u64 = cohort_extents.values().sum();
        let reclaimable_bytes = total.private.saturating_add(cohort_shared_bytes);
        let budget_bytes = self.config.max_private_gb.map(|g| g.saturating_mul(GB));
        Ok(DiskUsage {
            entries,
            total,
            free_bytes: fsutil::free_bytes(&self.dir).unwrap_or(0),
            cohort_shared_bytes,
            reclaimable_bytes,
            budget_bytes,
        })
    }

    /// The branch `prune` compares against: `prune_base` from the config,
    /// else the remote's default branch, else the first of `origin/main`,
    /// `origin/master`, `main`, `master` that exists.
    pub fn prune_base(&self) -> Result<String> {
        if let Some(b) = &self.config.prune_base {
            return Ok(b.clone());
        }
        if let Ok(r) = git::line(
            &self.root,
            &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
        ) && !r.is_empty()
        {
            return Ok(r);
        }
        for b in ["origin/main", "origin/master", "main", "master"] {
            if self.resolve(Some(b)).is_ok() {
                return Ok(b.into());
            }
        }
        bail!("no base branch found; pass --base BRANCH or set prune_base")
    }

    /// Why a clean workspace's work is already in `base`, or `None`.
    fn merged_reason(&self, m: &Meta, head: &str, base: &str, gh: bool) -> Option<String> {
        if head == m.base {
            return Some("unused".into());
        }
        if git_ok(&m.path, &["merge-base", "--is-ancestor", head, base]) {
            return Some("merged".into());
        }
        // Squash and rebase merges leave HEAD outside the base. Their work is
        // in the base when every path HEAD changed since the fork point is
        // identical in the base. Read-only (no merge objects written); a
        // base that changed those paths again later keeps the workspace.
        if let Ok(fork) = git::line(&m.path, &["merge-base", head, base])
            && let Ok(changed) = git_bytes(&m.path, &["diff", "--name-only", "-z", &fork, head])
        {
            let paths: Vec<&OsStr> = changed
                .split(|&b| b == 0)
                .filter(|p| !p.is_empty())
                .map(OsStr::from_bytes)
                .collect();
            if paths.is_empty()
                || paths.chunks(500).all(|chunk| {
                    std::process::Command::new("git")
                        .arg("-C")
                        .arg(&m.path)
                        .args(["--literal-pathspecs", "diff", "--quiet", base, head, "--"])
                        .args(chunk)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                })
            {
                return Some("squash-merged".into());
            }
        }
        // Ask about every local branch at HEAD, not just a checked-out one:
        // workspaces run detached, and `land --keep` leaves HEAD detached at
        // the `agent-ws/<name>` branch it created.
        if gh
            && let Ok(at) = git::run(
                &m.path,
                &[
                    "for-each-ref",
                    "--points-at",
                    head,
                    "--format=%(refname:short)",
                    "refs/heads",
                ],
            )
            && at
                .lines()
                .any(|branch| pr_merged_at(&self.root, branch, head))
        {
            return Some("pr-merged".into());
        }
        None
    }

    fn ws_cwd_users(&self) -> Vec<(u32, PathBuf)> {
        let ws = self.dir.join("ws");
        fsutil::cwd_users(&std::fs::canonicalize(&ws).unwrap_or(ws))
    }

    /// Drop workspaces whose work is already in the base branch: clean tree
    /// (ignored files aside) and HEAD merged, squash-merged, merged as a
    /// GitHub PR (`gh`), or never moved since spawn. Never touches a
    /// workspace with uncommitted or unmerged work, one younger than
    /// `min_age_secs`, or one that a process has as its current directory.
    pub fn prune(&self, o: &PruneOpts) -> Result<Prune> {
        let base = match &o.base {
            Some(b) => b.clone(),
            None => self.prune_base()?,
        };
        let base_commit = self
            .resolve(Some(&base))
            .with_context(|| format!("base {base:?} not found"))?;
        let mut pruned = Vec::new();
        let mut kept = Vec::new();
        let now = now();
        let users = self.ws_cwd_users();
        for m in self.list()? {
            let keep = |why: String| Kept {
                name: m.name.clone(),
                why,
            };
            if !m.path.exists() {
                kept.push(keep("folder is missing (run `agent-ws gc`)".into()));
                continue;
            }
            if let Some(pid) = processes_in(&users, &m.path).into_iter().next() {
                kept.push(keep(format!("in use by process {pid}")));
                continue;
            }
            let age = now.saturating_sub(m.created);
            if age < o.min_age_secs {
                kept.push(keep(format!("created {}m ago", age / 60)));
                continue;
            }
            // No optional locks: a dry run must not even refresh the index.
            let dirty = is_dirty(&m.path).unwrap_or(true);
            if dirty {
                kept.push(keep("uncommitted changes".into()));
                continue;
            }
            let Ok(head) = git::line(&m.path, &["rev-parse", "HEAD"]) else {
                kept.push(keep("cannot read HEAD".into()));
                continue;
            };
            // The workspace's own branch (`spawn --branch`) goes with it when
            // it holds nothing the base lacks.
            let own_branch = || {
                if !m.branch.is_empty()
                    && self.branch_tip(&m.branch).as_deref() == Some(head.as_str())
                {
                    m.branch.clone()
                } else {
                    String::new()
                }
            };
            // Claude Code never removes a subagent's worktree, so the
            // automatic prune drops unused ones too, after a day: a long
            // session may sit in one without changing anything.
            let stale_claude = m.origin == "claude" && age >= CLAUDE_UNUSED_SECS;
            match self.merged_reason(&m, &head, &base_commit, o.gh) {
                Some(reason) if o.merged_only && reason == "unused" && !stale_claude => {
                    kept.push(keep("unused since spawn".into()))
                }
                Some(reason) => pruned.push(Pruned {
                    name: m.name.clone(),
                    path: m.path.clone(),
                    branch: match reason.as_str() {
                        "merged" | "unused" => own_branch(),
                        _ => String::new(),
                    },
                    reason,
                    private_bytes: 0,
                }),
                None => kept.push(keep(format!("commits not in {base}"))),
            }
        }
        let sizes = usage_parallel(pruned.iter().map(|p| p.path.clone()).collect());
        for (p, u) in pruned.iter_mut().zip(sizes) {
            p.private_bytes = u.private;
        }
        let mut clear_pool = false;
        if let Some(target) = o.target_free_bytes {
            let free = fsutil::free_bytes(&self.dir)?;
            if free >= target {
                for p in pruned.drain(..) {
                    kept.push(Kept {
                        name: p.name,
                        why: "free space above target".into(),
                    });
                }
            } else {
                pruned.sort_by(|a, b| {
                    b.private_bytes
                        .cmp(&a.private_bytes)
                        .then_with(|| a.name.cmp(&b.name))
                });
                let mut acc = 0u64;
                let mut selected = Vec::new();
                for p in pruned.drain(..) {
                    if free.saturating_add(acc) < target {
                        acc = acc.saturating_add(p.private_bytes);
                        selected.push(p);
                    } else {
                        kept.push(Kept {
                            name: p.name,
                            why: "free space target reached".into(),
                        });
                    }
                }
                pruned = selected;
                if free.saturating_add(acc) < target {
                    clear_pool = true;
                }
            }
        }
        if !o.dry_run {
            for p in &pruned {
                self.drop(&p.name)?;
                if !p.branch.is_empty() {
                    let _ = git::run(&self.root, &["branch", "-q", "-D", &p.branch]);
                }
            }
            if clear_pool {
                let _ = self.pool_fill(Some(0))?;
            }
        }
        Ok(Prune {
            base,
            base_commit,
            dry_run: o.dry_run,
            freed_bytes: pruned.iter().map(|p| p.private_bytes).sum(),
            pruned,
            kept,
        })
    }

    /// Changed paths and dirty state per ready workspace, skipping folders deleted by hand.
    fn changes(&self) -> Result<Vec<(Meta, Vec<String>, bool)>> {
        let metas: Vec<Meta> = self
            .list()?
            .into_iter()
            .filter(|m| m.path.exists())
            .collect();
        if metas.is_empty() {
            return Ok(Vec::new());
        }
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(metas.len());
        let chunk = metas.len().div_ceil(threads);
        let results = std::thread::scope(|s| {
            let handles: Vec<_> = metas
                .chunks(chunk)
                .map(|c| {
                    s.spawn(move || -> Result<Vec<(Meta, Vec<String>, bool)>> {
                        let mut part = Vec::with_capacity(c.len());
                        for m in c {
                            let (changed, dirty) = git::changed_paths_status(&m.path, &m.base)
                                .with_context(|| format!("workspace {:?}", m.name))?;
                            part.push((m.clone(), changed, dirty));
                        }
                        Ok(part)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| bail!("worker thread panicked")))
                .collect::<Vec<_>>()
        });
        let mut out = Vec::with_capacity(metas.len());
        for r in results {
            out.extend(r?);
        }
        Ok(out)
    }

    pub fn status(&self, only: Option<&str>) -> Result<Status> {
        // Overlaps are computed across all workspaces, even when one is selected.
        let all = self.changes()?;
        let mut by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut dirty_map: HashMap<String, bool> = HashMap::with_capacity(all.len());
        let mut metas: Vec<Meta> = Vec::with_capacity(all.len());
        for (m, changed, dirty) in &all {
            for p in changed {
                by_path.entry(p.clone()).or_default().push(m.name.clone());
            }
            dirty_map.insert(m.name.clone(), *dirty);
            metas.push(m.clone());
        }
        let workspaces: Vec<WsStatus> = all
            .into_iter()
            .filter(|(m, _, _)| only.is_none_or(|n| n == m.name))
            .map(|(m, changed, _)| WsStatus {
                name: m.name,
                path: m.path,
                base: m.base,
                changed,
            })
            .collect();
        if let Some(n) = only
            && workspaces.is_empty()
        {
            return Err(coded_err(
                ErrorCode::NotFound,
                format!("no workspace named {n:?}"),
            ));
        }
        let overlaps: BTreeMap<String, Vec<String>> =
            by_path.into_iter().filter(|(_, v)| v.len() > 1).collect();
        let conflicts = self.conflicts(&metas, &dirty_map, &overlaps, only);
        Ok(Status {
            workspaces,
            overlaps,
            conflicts,
        })
    }

    /// Which overlapping workspaces would conflict if both were merged.
    /// Each dirty one is snapshotted as a commit on its HEAD (clean ones use
    /// HEAD directly) and `merge-tree` merges the pair in memory; nothing in
    /// the workspaces changes. A workspace Git cannot snapshot is left out.
    fn conflicts(
        &self,
        metas: &[Meta],
        dirty_map: &HashMap<String, bool>,
        overlaps: &BTreeMap<String, Vec<String>>,
        only: Option<&str>,
    ) -> Option<Vec<Conflict>> {
        let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
        for names in overlaps.values() {
            for (i, a) in names.iter().enumerate() {
                for b in &names[i + 1..] {
                    if let Some(target) = only
                        && a != target
                        && b != target
                    {
                        continue;
                    }
                    let (a, b) = if a < b { (a, b) } else { (b, a) };
                    pairs.insert((a.clone(), b.clone()));
                }
            }
        }
        if pairs.is_empty() {
            return Some(Vec::new());
        }
        let needed: Vec<String> = pairs
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let meta_map: HashMap<&str, &Meta> = metas.iter().map(|m| (m.name.as_str(), m)).collect();
        let c_threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(needed.len())
            .max(1);
        let c_chunk = needed.len().div_ceil(c_threads);
        let commits: HashMap<String, Option<String>> = std::thread::scope(|s| {
            let meta_map = &meta_map;
            let handles: Vec<_> = needed
                .chunks(c_chunk)
                .map(|chunk| {
                    s.spawn(move || {
                        chunk
                            .iter()
                            .map(|name| {
                                let snap = meta_map.get(name.as_str()).and_then(|m| {
                                    let head = git::line(&m.path, &["rev-parse", "HEAD"]).ok()?;
                                    let dirty = dirty_map.get(name).copied().unwrap_or(true);
                                    if !dirty {
                                        Some(head)
                                    } else {
                                        let tree =
                                            git::snapshot_tree(&m.path, &self.scratch()).ok()?;
                                        git::snapshot_commit(&m.path, &tree, &head).ok()
                                    }
                                });
                                (name.clone(), snap)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });
        let pair_vec: Vec<(String, String)> = pairs.into_iter().collect();
        let p_threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(pair_vec.len());
        let p_chunk = pair_vec.len().div_ceil(p_threads);
        let pair_results = std::thread::scope(|s| {
            let handles: Vec<_> = pair_vec
                .chunks(p_chunk)
                .map(|c| {
                    let commits = &commits;
                    s.spawn(move || {
                        c.iter()
                            .filter_map(|(a, b)| {
                                let ca = commits.get(a)?.as_ref()?;
                                let cb = commits.get(b)?.as_ref()?;
                                Some((
                                    (a.clone(), b.clone()),
                                    git::merge_conflicts(&self.root, ca, cb),
                                ))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        let mut out = Vec::new();
        let mut saw_none = false;
        for batch in pair_results {
            for ((a, b), res) in batch {
                match res {
                    Ok(Some(paths)) if !paths.is_empty() => out.push(Conflict {
                        workspaces: vec![a, b],
                        paths,
                    }),
                    Ok(Some(_)) | Err(_) => {}
                    Ok(None) => saw_none = true,
                }
            }
        }
        if saw_none && !supports_merge_tree(&self.root) {
            return None;
        }
        Some(out)
    }

    /// Temporary index files for snapshots.
    fn scratch(&self) -> PathBuf {
        self.dir.join("tmp")
    }

    /// A workspace's current state as a tree: its commits, uncommitted edits
    /// and new files, read without touching its index. Clean trees use HEAD directly.
    pub fn snapshot(&self, name: &str) -> Result<Snapshot> {
        let m = self.meta(name)?;
        let tree = if !is_dirty(&m.path)? {
            git::line(&m.path, &["rev-parse", "HEAD^{tree}"])?
        } else {
            git::snapshot_tree(&m.path, &self.scratch())?
        };
        Ok(Snapshot {
            path: m.path,
            base: m.base,
            tree,
        })
    }

    fn diff_files_from(&self, s: &Snapshot, paths: &[String]) -> Result<Vec<FileChange>> {
        let run = |what: &str| {
            let mut args = vec![
                "--literal-pathspecs",
                "diff",
                "--no-renames",
                "--no-ext-diff",
                "-z",
                what,
            ];
            args.extend([s.base.as_str(), s.tree.as_str(), "--"]);
            args.extend(paths.iter().map(String::as_str));
            git::run(&s.path, &args)
        };
        // `<status>\0<path>\0` per file.
        let mut files: BTreeMap<String, FileChange> = BTreeMap::new();
        let names = run("--name-status")?;
        let mut it = names.split('\0').filter(|x| !x.is_empty());
        while let (Some(status), Some(path)) = (it.next(), it.next()) {
            files.insert(
                path.to_string(),
                FileChange {
                    path: path.to_string(),
                    status: status.to_string(),
                    added: None,
                    deleted: None,
                },
            );
        }
        // `<added>\t<deleted>\t<path>\0`, `-` for binary files.
        for rec in run("--numstat")?.split('\0').filter(|x| !x.is_empty()) {
            let mut f = rec.splitn(3, '\t');
            if let (Some(a), Some(d), Some(p)) = (f.next(), f.next(), f.next())
                && let Some(c) = files.get_mut(p)
            {
                c.added = a.parse().ok();
                c.deleted = d.parse().ok();
            }
        }
        Ok(files.into_values().collect())
    }

    fn diff_patch_from(&self, s: &Snapshot, paths: &[String]) -> Result<String> {
        let mut args = vec![
            "--literal-pathspecs",
            "diff",
            "--no-color",
            "--no-ext-diff",
            &s.base,
            &s.tree,
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        git::run(&s.path, &args)
    }

    /// Files a workspace changed since its base, with line counts, limited
    /// to `paths` when given.
    pub fn diff_files(&self, name: &str, paths: &[String]) -> Result<Vec<FileChange>> {
        let s = self.snapshot(name)?;
        self.diff_files_from(&s, paths)
    }

    /// Files changed and unified patch from a single snapshot, optionally capped
    /// to `max_patch_lines` lines.
    pub fn diff_bundle(
        &self,
        name: &str,
        paths: &[String],
        max_patch_lines: Option<usize>,
    ) -> Result<DiffBundle> {
        let s = self.snapshot(name)?;
        let files = self.diff_files_from(&s, paths)?;
        let raw_patch = self.diff_patch_from(&s, paths)?;
        let (patch, truncated) = match max_patch_lines {
            Some(limit) => truncate_lines(raw_patch, limit),
            None => (raw_patch, false),
        };
        Ok(DiffBundle {
            files,
            patch,
            truncated,
        })
    }

    fn check_not_inside(
        &self,
        ws_path: &Path,
        allow_self: bool,
        cwd: Option<&Path>,
        action: &str,
    ) -> Result<()> {
        if allow_self {
            return Ok(());
        }
        let Some(cwd) = cwd else {
            return Ok(());
        };
        let canon_ws = std::fs::canonicalize(ws_path).unwrap_or_else(|_| ws_path.to_path_buf());
        let canon_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        if canon_cwd.starts_with(&canon_ws) {
            return Err(coded_err(
                ErrorCode::SelfActionBlocked,
                format!(
                    "refusing to {action} the workspace you are currently inside; pass --allow-self to override"
                ),
            ));
        }
        Ok(())
    }

    pub fn land(&self, o: &LandOpts) -> Result<Landed> {
        self.land_from(o, None)
    }

    pub fn land_from(&self, o: &LandOpts, cwd: Option<&Path>) -> Result<Landed> {
        let m = self.meta(&o.name)?;
        self.check_not_inside(&m.path, o.allow_self, cwd, "land")?;
        let files = git::changed_paths(&m.path, &m.base)?;
        if files.is_empty() {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!("workspace {:?} has no changes", o.name),
            ));
        }
        // A workspace on its own branch (`spawn --branch`) lands there.
        let branch = o.branch.clone().unwrap_or_else(|| {
            if m.branch.is_empty() {
                format!("agent-ws/{}", o.name)
            } else {
                m.branch.clone()
            }
        });
        check_branch(&self.root, &branch)?;
        let pre_land = match &self.config.hooks {
            Some(h) if !o.no_verify && !h.pre_land.is_empty() => self.trusted_hooks()?.pre_land,
            _ => vec![],
        };
        let existing = self.branch_tip(&branch);
        let head = git::line(&m.path, &["rev-parse", "HEAD"])?;
        // The workspace has the branch checked out: its commits move it.
        let own = git::line(&m.path, &["symbolic-ref", "-q", "--short", "HEAD"])
            .is_ok_and(|b| b == branch);
        let clean = !is_dirty(&m.path)?;
        let landed_by_this = |tip: &str| {
            git::line(
                &m.path,
                &[
                    "log",
                    "-1",
                    "--format=%(trailers:key=Agent-Ws,valueonly)",
                    tip,
                ],
            )
            .is_ok_and(|n| n == o.name)
        };
        // An earlier land created the branch at this workspace's HEAD and
        // stopped before removing the workspace: finish that land.
        let resume = existing.as_deref() == Some(head.as_str())
            && clean
            && head != m.base
            && (!own || landed_by_this(&head));
        // An earlier `land --keep` of this workspace: its tip is an ancestor
        // of HEAD and carries this workspace's trailer, so moving the branch
        // forward loses nothing. Branches anyone else made are left alone.
        let advance = !resume
            && !own
            && existing.as_deref().is_some_and(|tip| {
                git_ok(&m.path, &["merge-base", "--is-ancestor", tip, &head]) && landed_by_this(tip)
            });
        if existing.is_some() && !resume && !advance && !own && !o.force_branch {
            return Err(coded_err(
                ErrorCode::AlreadyExists,
                format!(
                    "branch {branch} already exists and does not match this workspace; pass --branch NAME or --force-branch"
                ),
            ));
        }
        if !resume {
            let conflicts = git_bytes(&m.path, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
            if !conflicts.is_empty() {
                return Err(coded_err(
                    ErrorCode::Conflict,
                    format!(
                        "workspace {:?} has unresolved merge conflicts; resolve them before landing",
                        o.name
                    ),
                ));
            }
            if !own && let Some(wt) = self.branch_checked_out_at(&branch)? {
                return Err(coded_err(
                    ErrorCode::Conflict,
                    format!(
                        "branch {branch} is checked out at {}; pass --branch NAME to land on another branch",
                        wt.display()
                    ),
                ));
            }
        }
        // Overlaps are advice: a workspace Git cannot read must not block
        // landing this one (`status` reports it).
        let file_set: HashSet<&str> = files.iter().map(String::as_str).collect();
        let others: Vec<Meta> = self
            .list()?
            .into_iter()
            .filter(|other| other.name != o.name && other.path.exists())
            .collect();
        let mut overlaps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if !others.is_empty() {
            let threads = std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .min(others.len());
            let chunk = others.len().div_ceil(threads);
            let collected: Vec<(String, Vec<String>)> = std::thread::scope(|s| {
                let handles: Vec<_> = others
                    .chunks(chunk)
                    .map(|c| {
                        s.spawn(move || {
                            c.iter()
                                .filter_map(|other| {
                                    let changed =
                                        git::changed_paths(&other.path, &other.base).ok()?;
                                    Some((other.name.clone(), changed))
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            for (other_name, changed) in collected {
                for p in changed {
                    if file_set.contains(p.as_str()) {
                        overlaps.entry(p).or_default().push(other_name.clone());
                    }
                }
            }
        }
        let commit = if resume {
            head
        } else {
            self.run_hooks("pre_land", &pre_land, &m)?;
            git::run(&m.path, &["add", "-A"])?;
            if o.squash {
                // Everything since the base becomes the one commit below.
                git::run(&m.path, &["reset", "-q", "--soft", &m.base])?;
            }
            // Always commit, empty if the agent already committed everything,
            // so the branch tip carries the message and the trailers.
            let ws_trailer = format!("Agent-Ws: {}", o.name);
            let base_trailer = format!("Agent-Ws-Base: {}", m.base);
            let mut args = vec![
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &o.message,
                "--trailer",
                &ws_trailer,
                "--trailer",
                &base_trailer,
            ];
            if o.no_verify {
                args.push("--no-verify");
            }
            git::run(&m.path, &args)?;
            let commit = git::line(&m.path, &["rev-parse", "HEAD"])?;
            if !own {
                let flag = if o.force_branch || advance {
                    "-f"
                } else {
                    "--no-track"
                };
                git::run(&self.root, &["branch", flag, &branch, &commit])?;
            }
            commit
        };
        // A failed push keeps the workspace, so the same command retries:
        // it resumes at the landed commit and pushes again.
        let mut pr_url = None;
        if o.pr {
            let url = self.open_pr(&branch, o, files.len()).with_context(|| {
                format!("landed on branch {branch}; run the same command again to retry")
            })?;
            pr_url = Some(url);
        }
        if !o.keep {
            self.drop(&o.name)?;
        }
        Ok(Landed {
            name: o.name.clone(),
            branch,
            commit,
            files: files.len(),
            dropped: !o.keep,
            overlaps,
            pushed: o.pr,
            pr_url,
        })
    }

    fn branch_checked_out_at(&self, branch: &str) -> Result<Option<PathBuf>> {
        let target = format!("refs/heads/{branch}");
        let raw = git::run(&self.root, &["worktree", "list", "--porcelain"])?;
        let mut current_wt: Option<PathBuf> = None;
        for line in raw.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                current_wt = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ")
                && b == target
            {
                return Ok(current_wt);
            } else if line.is_empty() {
                current_wt = None;
            }
        }
        Ok(None)
    }

    /// Push `branch` to `origin`, then open a pull request for it with the
    /// GitHub CLI, or find the one already open. Returns its URL.
    fn open_pr(&self, branch: &str, o: &LandOpts, files: usize) -> Result<String> {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let mut push = vec!["push", "-q"];
        if o.force_branch {
            push.push("--force-with-lease");
        }
        push.extend(["origin", &refspec]);
        git::run(&self.root, &push)?;
        let gh = |args: &[&str]| -> Result<String> {
            let out = std::process::Command::new("gh")
                .current_dir(&self.root)
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .context("run gh (the GitHub CLI)")?;
            if !out.status.success() {
                bail!(
                    "gh {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        let open = gh(&[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "url",
            "--jq",
            ".[0].url // empty",
        ])?;
        if !open.is_empty() {
            return Ok(open);
        }
        let trimmed_msg = o.message.trim();
        let (title, rest) = trimmed_msg.split_once('\n').unwrap_or((trimmed_msg, ""));
        let body = format!(
            "{}\n\nLanded with agent-ws from workspace `{}` ({files} files).",
            rest.trim(),
            o.name
        );
        let mut args = vec![
            "pr",
            "create",
            "--head",
            branch,
            "--title",
            title.trim(),
            "--body",
            body.trim_start(),
        ];
        if let Some(b) = &o.pr_base {
            args.extend(["--base", b]);
        }
        if o.draft {
            args.push("--draft");
        }
        // `gh pr create` prints the new pull request's URL last.
        Ok(gh(&args)?.lines().last().unwrap_or_default().to_string())
    }

    fn drop_meta(&self, m: &Meta) -> Result<()> {
        self.discard(&m.path)?;
        std::fs::remove_file(self.meta_path(&m.name))?;
        let _ = std::fs::remove_file(self.run_path(&m.name));
        let _ = std::fs::remove_file(self.log_path(&m.name));
        let snaps_dir = self.dir.join("snaps").join(&m.name);
        if snaps_dir.exists() {
            let _ = self.trash_path(&snaps_dir, &format!("snaps-{}", m.name));
        }
        // The branch `spawn --branch` made holds nothing until it moves.
        if !m.branch.is_empty() && self.branch_tip(&m.branch).as_deref() == Some(m.base.as_str()) {
            let _ = git::run(&self.root, &["branch", "-q", "-D", &m.branch]);
        }
        // Finish deletes that an earlier killed process left in the trash.
        self.sweep_trash(false)?;
        Ok(())
    }

    /// Remove a workspace. The tree is renamed into `.trash` (instant) and
    /// deleted by a detached `rm -rf`, which takes seconds for large trees.
    pub fn drop(&self, name: &str) -> Result<()> {
        self.drop_with_opts(name, false, true, None).map(|_| ())
    }

    /// Remove a workspace with optional `--if-exists` and self-action guard.
    pub fn drop_with_opts(
        &self,
        name: &str,
        if_exists: bool,
        allow_self: bool,
        cwd: Option<&Path>,
    ) -> Result<bool> {
        check_name(name)?;
        if if_exists && !self.meta_path(name).exists() {
            return Ok(false);
        }
        let m = self.meta(name)?;
        self.check_not_inside(&m.path, allow_self, cwd, "drop")?;
        self.drop_meta(&m)?;
        Ok(true)
    }

    /// CoW-refresh ignored dependency directories in `name` from `self.root`.
    pub fn resync_deps(&self, name: &str, paths: &[String]) -> Result<ResyncResult> {
        const DEFAULT_DEP_DIRS: &[&str] = &[
            "node_modules",
            ".venv",
            "venv",
            "target",
            ".next",
            ".turbo",
            ".nuxt",
            "vendor",
            "dist",
            "build",
        ];
        static RESYNC_SEQ: AtomicU64 = AtomicU64::new(0);
        let m = self.meta(name)?;
        let explicit = !paths.is_empty();
        let candidates: Vec<String> = if explicit {
            paths.to_vec()
        } else {
            DEFAULT_DEP_DIRS.iter().map(|s| (*s).to_string()).collect()
        };

        let mut resynced = Vec::new();
        let mut skipped = Vec::new();
        let mut freed_private_bytes = 0u64;

        for raw in candidates {
            let rel = normalize_rel_subpath(&raw)?;
            let rel_str = rel.to_string_lossy().into_owned();
            let src_target = self.root.join(&rel);
            let ws_target = m.path.join(&rel);

            let src_md = match src_target.symlink_metadata() {
                Ok(md) => md,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    skipped.push(rel_str);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let ws_md = match ws_target.symlink_metadata() {
                Ok(md) => md,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    skipped.push(rel_str);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if src_md.file_type().is_symlink() || ws_md.file_type().is_symlink() {
                if explicit {
                    return Err(coded_err(
                        ErrorCode::InvalidInput,
                        format!("refusing to resync symlink {rel_str:?}"),
                    ));
                }
                skipped.push(rel_str);
                continue;
            }

            let ignored = std::process::Command::new("git")
                .arg("-C")
                .arg(&m.path)
                .args(["check-ignore", "-q", "--"])
                .arg(&rel)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ignored {
                if explicit {
                    return Err(coded_err(
                        ErrorCode::InvalidInput,
                        format!("{rel_str:?} is not git-ignored in workspace {name:?}"),
                    ));
                }
                skipped.push(rel_str);
                continue;
            }

            let old_private = fsutil::usage(&ws_target).private;
            std::fs::create_dir_all(self.scratch())?;
            let seq = RESYNC_SEQ.fetch_add(1, Ordering::Relaxed);
            let staging = self.scratch().join(format!("resync-{}-{seq}", nanos()));
            fsutil::clone_path(&src_target, &staging)?;
            if staging.is_dir() {
                let tracked = HashSet::new();
                let secret = |n: &str| self.config.is_secret(n);
                fsutil::scan_with_skip(
                    &src_target,
                    &staging,
                    &tracked,
                    &secret,
                    &self.config.scan_skip,
                )?;
            } else if staging
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| self.config.is_secret(n))
                && !fsutil::only_certificates(&staging)
            {
                let _ = std::fs::remove_file(&staging);
                return Err(coded_err(
                    ErrorCode::InvalidInput,
                    format!("{rel_str:?} matches secret rules"),
                ));
            }

            std::fs::create_dir_all(self.dir.join(".trash"))?;
            let trash_name = format!("resync-{name}-{}", nanos());
            let trash = self.dir.join(".trash").join(&trash_name);
            std::fs::rename(&ws_target, &trash)?;
            if let Err(e) = std::fs::rename(&staging, &ws_target) {
                let _ = std::fs::rename(&trash, &ws_target);
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e.into());
            }
            let pid = delete_in_background_pid(&trash)?;
            self.record_trash_pid(&trash_name, pid);
            freed_private_bytes = freed_private_bytes.saturating_add(old_private);
            resynced.push(rel_str);
        }

        Ok(ResyncResult {
            name: m.name,
            resynced,
            skipped,
            freed_private_bytes,
        })
    }

    /// Rebase or merge a clean workspace onto `o.base` (or repository `HEAD`).
    pub fn sync(&self, o: &SyncOpts) -> Result<Synced> {
        let mut m = self.meta(&o.name)?;
        if is_dirty(&m.path)? {
            return Err(coded_err(
                ErrorCode::Dirty,
                format!(
                    "workspace {:?} has uncommitted changes; commit or stash before syncing",
                    o.name
                ),
            ));
        }
        let old_base = m.base.clone();
        let new_base = self.resolve(o.base.as_deref())?;
        let mode = if o.merge { "merge" } else { "rebase" }.to_string();

        if o.merge {
            if let Err(e) = git::run(&m.path, &["merge", "--no-edit", &new_base]) {
                if git::run(&m.path, &["merge", "--abort"]).is_ok() {
                    return Err(coded_err(
                        ErrorCode::Conflict,
                        format!(
                            "merge conflict syncing workspace {:?} onto {}",
                            o.name,
                            new_base.get(..12).unwrap_or(&new_base)
                        ),
                    ));
                }
                return Err(coded_err(ErrorCode::GitError, format!("{e:#}")));
            }
        } else if let Err(e) = git::run(&m.path, &["rebase", &new_base]) {
            if git::run(&m.path, &["rebase", "--abort"]).is_ok() {
                return Err(coded_err(
                    ErrorCode::Conflict,
                    format!(
                        "rebase conflict syncing workspace {:?} onto {}",
                        o.name,
                        new_base.get(..12).unwrap_or(&new_base)
                    ),
                ));
            }
            return Err(coded_err(ErrorCode::GitError, format!("{e:#}")));
        }

        m.base = new_base.clone();
        write_record(&self.meta_path(&o.name), &m, false)?;
        let head = git::line(&m.path, &["rev-parse", "HEAD"])?;
        let resync = if o.resync_deps {
            Some(self.resync_deps(&o.name, &[])?)
        } else {
            None
        };
        Ok(Synced {
            name: o.name.clone(),
            old_base,
            new_base,
            head,
            mode,
            resync,
        })
    }

    fn snap_dir(&self, ws_name: &str, snap_name: &str) -> PathBuf {
        self.dir.join("snaps").join(ws_name).join(snap_name)
    }

    /// Create an instant CoW checkpoint of workspace `ws_name`.
    pub fn snap(&self, ws_name: &str, snap_name: Option<&str>) -> Result<SnapInfo> {
        static SNAP_SEQ: AtomicU64 = AtomicU64::new(0);
        let m = self.meta(ws_name)?;
        let sname = match snap_name {
            Some(n) => {
                check_name(n)?;
                n.to_string()
            }
            None => {
                let seq = SNAP_SEQ.fetch_add(1, Ordering::Relaxed);
                format!("s-{}-{seq}", now())
            }
        };
        let dotgit_md = m.path.join(".git").symlink_metadata().map_err(|e| {
            coded_err(
                ErrorCode::GitError,
                format!("workspace {:?} missing .git: {e}", ws_name),
            )
        })?;
        if dotgit_md.file_type().is_symlink() {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!("workspace {:?} .git cannot be a symlink", ws_name),
            ));
        }
        let head = git::line(&m.path, &["rev-parse", "HEAD"])?;
        let snap_dir = self.snap_dir(ws_name, &sname);
        if snap_dir.exists() {
            return Err(coded_err(
                ErrorCode::AlreadyExists,
                format!("snapshot {sname:?} already exists for workspace {ws_name:?}"),
            ));
        }

        std::fs::create_dir_all(self.scratch())?;
        let seq = SNAP_SEQ.fetch_add(1, Ordering::Relaxed);
        let staging = self.scratch().join(format!("snap-{}-{seq}", nanos()));
        std::fs::create_dir_all(staging.join("tree"))?;
        let run_snap = || -> Result<SnapInfo> {
            fsutil::clone_tree(
                &m.path,
                &staging.join("tree"),
                Path::new(""),
                &[PathBuf::from(".git")],
            )?;
            let idx_str = git::line(
                &m.path,
                &[
                    "--no-optional-locks",
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "index",
                ],
            )?;
            let idx_path = Path::new(&idx_str);
            if idx_path.exists() {
                copy_file_cow_or_plain(idx_path, &staging.join("index"))?;
            }
            let created = now();
            #[derive(Serialize)]
            struct SnapMeta<'a> {
                name: &'a str,
                workspace: &'a str,
                head: &'a str,
                base: &'a str,
                created: u64,
            }
            write_json(
                &staging.join("snap.json"),
                &SnapMeta {
                    name: &sname,
                    workspace: &m.name,
                    head: &head,
                    base: &m.base,
                    created,
                },
            )?;
            if let Some(parent) = snap_dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if snap_dir.exists() {
                return Err(coded_err(
                    ErrorCode::AlreadyExists,
                    format!("snapshot {sname:?} already exists for workspace {ws_name:?}"),
                ));
            }
            std::fs::rename(&staging, &snap_dir)?;
            Ok(SnapInfo {
                name: sname,
                workspace: m.name,
                head,
                base: m.base,
                created,
                path: snap_dir,
            })
        };
        match run_snap() {
            Ok(info) => Ok(info),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(e)
            }
        }
    }

    /// List snapshots of `ws_name` sorted by `created` ascending.
    pub fn snaps(&self, ws_name: &str) -> Result<Vec<SnapInfo>> {
        let _ = self.meta(ws_name)?;
        let dir = self.dir.join("snaps").join(ws_name);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        #[derive(Deserialize)]
        struct SnapMeta {
            name: String,
            workspace: String,
            head: String,
            base: String,
            created: u64,
        }
        let mut out = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let Ok(raw) = std::fs::read(p.join("snap.json")) else {
                continue;
            };
            let Ok(sm) = serde_json::from_slice::<SnapMeta>(&raw) else {
                continue;
            };
            out.push(SnapInfo {
                name: sm.name,
                workspace: sm.workspace,
                head: sm.head,
                base: sm.base,
                created: sm.created,
                path: p,
            });
        }
        out.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    /// Restore `ws_name` from `snap_name` (or the latest snapshot if `None`).
    pub fn restore(&self, ws_name: &str, snap_name: Option<&str>) -> Result<SnapInfo> {
        let mut m = self.meta(ws_name)?;
        let snap = match snap_name {
            Some(n) => {
                check_name(n)?;
                let snap_dir = self.snap_dir(ws_name, n);
                let raw = std::fs::read(snap_dir.join("snap.json")).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        coded_err(
                            ErrorCode::NotFound,
                            format!("no snapshot named {n:?} for workspace {ws_name:?}"),
                        )
                    } else {
                        anyhow::Error::new(e)
                    }
                })?;
                #[derive(Deserialize)]
                struct SnapMeta {
                    name: String,
                    workspace: String,
                    head: String,
                    base: String,
                    created: u64,
                }
                let sm: SnapMeta = serde_json::from_slice(&raw)?;
                SnapInfo {
                    name: sm.name,
                    workspace: sm.workspace,
                    head: sm.head,
                    base: sm.base,
                    created: sm.created,
                    path: snap_dir,
                }
            }
            None => self.snaps(ws_name)?.pop().ok_or_else(|| {
                coded_err(
                    ErrorCode::NotFound,
                    format!("no snapshots for workspace {ws_name:?}"),
                )
            })?,
        };

        let dotgit = m.path.join(".git");
        let dotgit_md = dotgit.symlink_metadata().map_err(|e| {
            coded_err(
                ErrorCode::GitError,
                format!("workspace {ws_name:?} missing .git: {e}"),
            )
        })?;
        if dotgit_md.file_type().is_symlink() || !dotgit_md.is_file() {
            return Err(coded_err(
                ErrorCode::InvalidInput,
                format!("workspace {ws_name:?} .git must be a regular worktree file"),
            ));
        }

        std::fs::create_dir_all(self.scratch())?;
        let staging = self.scratch().join(format!("restore-{}", nanos()));
        std::fs::create_dir_all(&staging)?;
        let prep = || -> Result<()> {
            fsutil::clone_tree(&snap.path.join("tree"), &staging, Path::new(""), &[])?;
            std::fs::copy(&dotgit, staging.join(".git"))?;
            Ok(())
        };
        if let Err(e) = prep() {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        std::fs::create_dir_all(self.dir.join(".trash"))?;
        let trash_name = format!("{ws_name}-{}", nanos());
        let trash = self.dir.join(".trash").join(&trash_name);
        std::fs::rename(&m.path, &trash)?;
        if let Err(e) = std::fs::rename(&staging, &m.path) {
            let _ = std::fs::rename(&trash, &m.path);
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e.into());
        }
        let pid = delete_in_background_pid(&trash)?;
        self.record_trash_pid(&trash_name, pid);

        let idx_str = git::line(
            &m.path,
            &[
                "--no-optional-locks",
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "index",
            ],
        )?;
        let idx_path = Path::new(&idx_str);
        let snap_index = snap.path.join("index");
        if snap_index.exists() {
            copy_file_cow_or_plain(&snap_index, idx_path)?;
        } else {
            let _ = std::fs::remove_file(idx_path);
        }

        git::run(&m.path, &["update-ref", "--no-deref", "HEAD", &snap.head])?;
        if m.base != snap.base {
            m.base = snap.base.clone();
            write_record(&self.meta_path(ws_name), &m, false)?;
        }
        Ok(snap)
    }

    /// Delete a snapshot of `ws_name`.
    pub fn snap_drop(&self, ws_name: &str, snap_name: &str) -> Result<()> {
        check_name(ws_name)?;
        check_name(snap_name)?;
        let snap_dir = self.snap_dir(ws_name, snap_name);
        if !snap_dir.exists() {
            return Err(coded_err(
                ErrorCode::NotFound,
                format!("no snapshot named {snap_name:?} for workspace {ws_name:?}"),
            ));
        }
        self.trash_path(&snap_dir, &format!("snap-{ws_name}-{snap_name}"))
    }

    /// Identify the workspace containing `cwd` (if any) and optionally compute overlaps/conflicts.
    pub fn whoami(&self, cwd: &Path, with_status: bool) -> Result<WhoAmI> {
        let canon_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let found = self.list()?.into_iter().find(|m| {
            let canon_ws = std::fs::canonicalize(&m.path).unwrap_or_else(|_| m.path.clone());
            canon_cwd.starts_with(&canon_ws)
        });
        let (overlaps, conflicts) = if with_status {
            let st = self.status(found.as_ref().map(|m| m.name.as_str()))?;
            (st.overlaps, st.conflicts)
        } else {
            (BTreeMap::new(), None)
        };
        match found {
            Some(m) => {
                let branch = if !m.branch.is_empty() {
                    Some(m.branch.clone())
                } else {
                    git::line(&m.path, &["symbolic-ref", "-q", "--short", "HEAD"])
                        .ok()
                        .filter(|s| !s.is_empty())
                };
                let port = self.port(&m).and_then(|p| u16::try_from(p).ok());
                let origin = (!m.origin.is_empty()).then(|| m.origin.clone());
                Ok(WhoAmI {
                    inside_workspace: true,
                    repo_root: self.root.clone(),
                    name: Some(m.name),
                    path: Some(m.path),
                    base: Some(m.base),
                    branch,
                    port,
                    origin,
                    overlaps,
                    conflicts,
                })
            }
            None => Ok(WhoAmI {
                inside_workspace: false,
                repo_root: self.root.clone(),
                name: None,
                path: None,
                base: None,
                branch: None,
                port: None,
                origin: None,
                overlaps,
                conflicts,
            }),
        }
    }

    fn run_path(&self, name: &str) -> PathBuf {
        self.dir.join("runs").join(format!("{name}.json"))
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.dir.join("runs").join(format!("{name}.log"))
    }

    fn read_run(&self, name: &str) -> Option<Run> {
        serde_json::from_slice(&std::fs::read(self.run_path(name)).ok()?).ok()
    }

    /// The last `exec` in a workspace and whether it still runs.
    pub fn run_info(&self, name: &str) -> Option<RunInfo> {
        let run = self.read_run(name)?;
        let state = match run.ended {
            Some(_) => "exited",
            None if pid_alive(run.pid, run.pid_start) => "running",
            None => "interrupted",
        };
        Some(RunInfo {
            state: state.into(),
            run,
        })
    }

    /// The last `exec` of every workspace that has one. Cheap: no Git.
    pub fn runs(&self) -> Result<BTreeMap<String, RunInfo>> {
        Ok(self
            .list()?
            .into_iter()
            .filter_map(|m| Some((m.name.clone(), self.run_info(&m.name)?)))
            .collect())
    }

    /// The end of the output of the last `exec --log`, at most `max` bytes;
    /// `None` when it kept no log.
    pub fn run_log(&self, name: &str, max: u64) -> Result<Option<String>> {
        use std::io::{Read, Seek, SeekFrom};
        check_name(name)?;
        let mut f = match std::fs::File::open(self.log_path(name)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(max)))?;
        let mut raw = Vec::new();
        f.read_to_end(&mut raw)?;
        Ok(Some(String::from_utf8_lossy(&raw).into_owned()))
    }

    /// `diff` as text: the patch from the base to the workspace as it is now.
    pub fn diff_patch(&self, name: &str, paths: &[String]) -> Result<String> {
        let s = self.snapshot(name)?;
        self.diff_patch_from(&s, paths)
    }

    /// Keep a run record, unless the workspace was dropped meanwhile.
    fn write_run(&self, name: &str, run: &Run) {
        if self.meta_path(name).exists() {
            let _ = std::fs::create_dir_all(self.dir.join("runs"));
            let _ = write_json(&self.run_path(name), run);
        }
    }

    /// Run `command` in a workspace with its environment (`env`), wait for
    /// it, and keep how it ended for `list --long`. Ctrl-C reaches the
    /// command; agent-ws stays to record the result. Returns the exit code,
    /// 128 + the signal number when the command was killed.
    pub fn exec(&self, name: &str, command: &[String], log: bool) -> Result<i32> {
        use std::os::unix::process::ExitStatusExt;
        use std::process::Stdio;
        use std::sync::{Arc, Mutex};
        let m = self.meta(name)?;
        let (prog, args) = command.split_first().context("no command given")?;
        let mut cmd = std::process::Command::new(prog);
        cmd.args(args).current_dir(&m.path).envs(self.env(&m));
        // A log needs the output, so the command writes to pipes that are
        // copied to agent-ws's own stdout and stderr and to the log. That
        // takes the terminal away from it: interactive agents run without.
        let log_file = if log {
            std::fs::create_dir_all(self.dir.join("runs"))?;
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            Some(Arc::new(Mutex::new(std::fs::File::create(
                self.log_path(name),
            )?)))
        } else {
            None
        };
        let mut child = cmd.spawn().with_context(|| format!("run {prog}"))?;
        let mut copies = Vec::new();
        if let Some(file) = &log_file {
            if let Some(out) = child.stdout.take() {
                copies.push(tee(out, std::io::stdout(), file.clone()));
            }
            if let Some(err) = child.stderr.take() {
                copies.push(tee(err, std::io::stderr(), file.clone()));
            }
        }
        let restore = ignore_interrupts();
        let mut run = Run {
            command: command.to_vec(),
            pid: child.id(),
            pid_start: process_start(child.id()).unwrap_or(0),
            started: now(),
            ended: None,
            exit_code: None,
            log,
        };
        self.write_run(name, &run);
        let status = child.wait();
        for c in copies {
            let _ = c.join();
        }
        drop(restore);
        let status = status?;
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        run.ended = Some(now());
        run.exit_code = Some(code);
        self.write_run(name, &run);
        Ok(code)
    }

    /// `list` plus what a coordinator wants to know about each workspace:
    /// age, port, uncommitted changes, commits, processes inside, the last
    /// `exec`, and real disk use.
    /// `disk` adds the real disk use, which walks every file: a dashboard
    /// polling every few seconds asks for it less often.
    pub fn list_long(&self, disk: bool) -> Result<Vec<WsInfo>> {
        let metas = self.list()?;
        let users = self.ws_cwd_users();
        let sizes: Vec<Option<u64>> = if disk {
            usage_parallel(metas.iter().map(|m| m.path.clone()).collect())
                .into_iter()
                .map(|u| Some(u.private))
                .collect()
        } else {
            vec![None; metas.len()]
        };
        let now = now();
        let git_infos: Vec<(Option<bool>, Option<u64>)> = if metas.is_empty() {
            Vec::new()
        } else {
            let threads = std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .min(metas.len());
            let chunk = metas.len().div_ceil(threads);
            std::thread::scope(|s| {
                let handles: Vec<_> = metas
                    .chunks(chunk)
                    .map(|c| {
                        s.spawn(move || {
                            c.iter()
                                .map(|m| {
                                    let dirty = is_dirty(&m.path).ok();
                                    let ahead = git::line(
                                        &m.path,
                                        &["rev-list", "--count", &format!("{}..HEAD", m.base)],
                                    )
                                    .ok()
                                    .and_then(|n| n.parse().ok());
                                    (dirty, ahead)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            })
        };
        Ok(metas
            .into_iter()
            .zip(sizes)
            .zip(git_infos)
            .map(|((m, private_bytes), (dirty, ahead))| {
                let processes = processes_in(&users, &m.path);
                WsInfo {
                    age_secs: now.saturating_sub(m.created),
                    port: self.port(&m),
                    dirty,
                    ahead,
                    processes,
                    run: self.run_info(&m.name),
                    private_bytes,
                    meta: m,
                }
            })
            .collect())
    }
}

/// Copy `from` to `to` and to `log` as it arrives, until `from` closes.
fn tee(
    mut from: impl std::io::Read + Send + 'static,
    mut to: impl std::io::Write + Send + 'static,
    log: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::Write;
        let mut buf = [0u8; 8192];
        while let Ok(n) = from.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = to.write_all(&buf[..n]);
            let _ = to.flush();
            if let Ok(mut f) = log.lock() {
                let _ = f.write_all(&buf[..n]);
            }
        }
    })
}

fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn is_dirty(path: &Path) -> Result<bool> {
    Ok(
        !git::run(path, &["--no-optional-locks", "status", "--porcelain"])?
            .trim()
            .is_empty(),
    )
}

fn processes_in(users: &[(u32, PathBuf)], path: &Path) -> Vec<u32> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    users
        .iter()
        .filter(|(_, cwd)| cwd.starts_with(&real))
        .map(|(pid, _)| *pid)
        .collect()
}

fn truncate_lines(mut s: String, limit: usize) -> (String, bool) {
    if limit == 0 {
        let truncated = !s.is_empty();
        s.clear();
        return (s, truncated);
    }
    let mut count = 0;
    for (idx, _) in s.match_indices('\n') {
        count += 1;
        if count == limit {
            if idx + 1 < s.len() {
                s.truncate(idx + 1);
                return (s, true);
            }
            return (s, false);
        }
    }
    (s, false)
}

fn supports_merge_tree(root: &Path) -> bool {
    let Ok(v) = git::line(root, &["--version"]) else {
        return true;
    };
    let Some(ver) = v.split_whitespace().nth(2) else {
        return true;
    };
    let mut parts = ver.split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(38);
    (major, minor) >= (2, 38)
}

struct SignalGuard;

type SavedSignals = [(libc::c_int, libc::sighandler_t); 2];

static SIG_STATE: std::sync::Mutex<Option<(usize, SavedSignals)>> = std::sync::Mutex::new(None);

/// Ignore Ctrl-C and Ctrl-\ in this process until all returned guards drop,
/// so that `exec` outlives the command it waits for. Synchronized for
/// multi-threaded embedders.
fn ignore_interrupts() -> SignalGuard {
    if let Ok(mut guard) = SIG_STATE.lock() {
        match guard.as_mut() {
            Some((count, _)) => *count += 1,
            None => {
                let prev = [
                    (libc::SIGINT, unsafe {
                        libc::signal(libc::SIGINT, libc::SIG_IGN)
                    }),
                    (libc::SIGQUIT, unsafe {
                        libc::signal(libc::SIGQUIT, libc::SIG_IGN)
                    }),
                ];
                *guard = Some((1, prev));
            }
        }
    }
    SignalGuard
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = SIG_STATE.lock()
            && let Some((count, prev)) = guard.as_mut()
        {
            *count -= 1;
            if *count == 0 {
                for (s, handler) in *prev {
                    unsafe { libc::signal(s, handler) };
                }
                *guard = None;
            }
        }
    }
}

/// `fsutil::usage` of several trees at once (each walk is syscall-bound).
fn usage_parallel(paths: Vec<PathBuf>) -> Vec<fsutil::Usage> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|c| s.spawn(move || c.iter().map(|p| fsutil::usage(p)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Whether GitHub has a merged PR from `branch` whose head is `head`.
/// False when `gh` is missing, not logged in, or offline.
fn pr_merged_at(root: &Path, branch: &str, head: &str) -> bool {
    let out = std::process::Command::new("gh")
        .current_dir(root)
        .args([
            "pr",
            "list",
            "--state",
            "merged",
            "--head",
            branch,
            "--json",
            "headRefOid",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(out) = out else { return false };
    out.status.success()
        && serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
            .is_ok_and(|prs| prs.iter().any(|p| p["headRefOid"] == head))
}

/// Give every checked-out submodule its own copy of its git dir, then, with
/// `reset`, check out the commit HEAD records for it (recursively). Without
/// `reset` (`--carry`) each submodule keeps the source's state and edits.
///
/// A cloned submodule has a `.git` file pointing at `../.git/modules/<name>`
/// relative to the source checkout, which does not exist in a workspace.
/// Pointing it back at the source's git dir would share its index and HEAD,
/// so the git dir is cloned instead (copy-on-write, like everything else).
fn isolate_submodules(ws_repo: &Path, src_repo: &Path, top_src: &Path, reset: bool) -> Result<()> {
    let tree = git_bytes(ws_repo, &["ls-tree", "-r", "-z", "HEAD"])?;
    // Entries look like `160000 commit <sha>\t<path>`.
    for entry in tree
        .split(|&b| b == 0)
        .filter(|e| e.starts_with(b"160000 "))
    {
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let head_str = std::str::from_utf8(&entry[..tab]).unwrap_or_default();
        let sha = head_str.rsplit(' ').next().unwrap_or_default();
        let rel = Path::new(OsStr::from_bytes(&entry[tab + 1..]));
        let (ws_sm, src_sm) = (ws_repo.join(rel), src_repo.join(rel));
        let dotgit = ws_sm.join(".git");
        // Not initialised in the source: nothing checked out to fix.
        let Ok(md) = dotgit.symlink_metadata() else {
            continue;
        };
        if md.file_type().is_symlink() {
            bail!("submodule {}: .git cannot be a symlink", rel.display());
        }
        if md.is_file() {
            let raw = std::fs::read_to_string(&dotgit)?;
            let Some(gd) = raw.trim().strip_prefix("gitdir:").map(str::trim) else {
                bail!("submodule {}: unexpected .git file", rel.display());
            };
            let gd_path = Path::new(gd);
            if gd_path.is_absolute() {
                bail!(
                    "submodule {}: absolute gitdir {:?} is not allowed",
                    rel.display(),
                    gd
                );
            }
            let modules_dir = top_src
                .join(".git")
                .join("modules")
                .canonicalize()
                .with_context(|| format!("submodule {}: .git/modules not found", rel.display()))?;
            let gitdir = src_sm
                .join(gd_path)
                .canonicalize()
                .with_context(|| format!("submodule {}: git dir {gd} not found", rel.display()))?;
            if !gitdir.starts_with(&modules_dir) {
                bail!(
                    "submodule {}: gitdir {:?} resolves outside .git/modules",
                    rel.display(),
                    gd
                );
            }
            std::fs::remove_file(&dotgit)?;
            fsutil::clone_path(&gitdir, &dotgit)?;
            // The copy sits inside the tree it describes; the source's
            // core.worktree points elsewhere and breaks every git command
            // there (even `git -C <submodule> config`), so edit the config
            // file from a directory outside any repository.
            let config = dotgit.join("config");
            let unset = std::process::Command::new("git")
                .current_dir(Path::new("/"))
                .args(["config", "--file"])
                .arg(&config)
                .args(["--unset", "core.worktree"])
                .output()
                .context("unset submodule core.worktree")?;
            if !unset.status.success() && unset.status.code() != Some(5) {
                bail!(
                    "submodule {}: git config --unset core.worktree failed: {}",
                    rel.display(),
                    String::from_utf8_lossy(&unset.stderr).trim()
                );
            }
        }
        if reset {
            let top_bytes = git_bytes(&ws_sm, &["rev-parse", "--show-toplevel"])?;
            let trimmed = top_bytes.strip_suffix(b"\n").unwrap_or(&top_bytes);
            let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
            let real_top = Path::new(OsStr::from_bytes(trimmed))
                .canonicalize()
                .with_context(|| format!("submodule {}: canonicalize toplevel", rel.display()))?;
            let real_ws_sm = ws_sm
                .canonicalize()
                .with_context(|| format!("submodule {}: canonicalize path", rel.display()))?;
            if real_top != real_ws_sm {
                bail!(
                    "submodule {}: git toplevel {} does not match workspace {}",
                    rel.display(),
                    real_top.display(),
                    real_ws_sm.display()
                );
            }
            git::run(
                &ws_sm,
                &[
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "checkout",
                    "-q",
                    "-f",
                    "--detach",
                    sha,
                ],
            )?;
            git::run(&ws_sm, &["-c", "core.fsmonitor=false", "clean", "-fdq"])?;
        }
        isolate_submodules(&ws_sm, &src_sm, top_src, reset)?;
    }
    Ok(())
}

fn delete_in_background_pid(path: &Path) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    let child = std::process::Command::new("rm")
        .arg("-rf")
        .arg("--")
        .arg(path)
        // Own process group: closing the terminal or killing the agent's
        // group must not stop the delete and leave the tree in `.trash`.
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(child.id())
}

/// Start state and time of a process on Linux (`(state, starttime)`).
#[cfg(target_os = "linux")]
fn process_stat(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state …`: the name may hold spaces and parentheses, so
    // count fields from the last `)`; the first after it is field 3 (state).
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<u64> {
    let (state, start) = process_stat(pid)?;
    (state != 'Z' && state != 'X').then_some(start)
}

#[cfg(target_os = "macos")]
const SZOMB: u32 = 5;

#[cfg(target_os = "macos")]
fn process_bsdinfo(pid: u32) -> std::result::Result<libc::proc_bsdinfo, i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if got == size {
        Ok(info)
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

/// Start time of a process in microseconds since the epoch.
#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<u64> {
    let info = process_bsdinfo(pid).ok()?;
    (info.pbi_status != SZOMB).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

fn own_start() -> u64 {
    process_start(std::process::id()).unwrap_or(0)
}

/// Extension of a pool claim file: `claim-<pid>-<start>`.
fn claim_ext() -> String {
    format!("claim-{}-{}", std::process::id(), own_start())
}

/// Whether the process that wrote a record still runs. With a recorded start
/// time, a different process that got the same pid, a zombie, or an `EPERM`
/// process owned by another user does not count.
fn pid_alive(pid: u32, start: u64) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    if let Some((state, actual)) = process_stat(pid) {
        if state == 'Z' || state == 'X' {
            return false;
        }
        return start == 0 || actual == start;
    }
    #[cfg(target_os = "macos")]
    match process_bsdinfo(pid) {
        Ok(info) => {
            if info.pbi_status == SZOMB {
                return false;
            }
            let actual = info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec;
            return start == 0 || actual == start;
        }
        Err(libc::ESRCH) => return false,
        Err(libc::EPERM) if start != 0 => return false,
        Err(_) => {}
    }
    // Signal 0 checks existence for legacy records with `start == 0` or
    // unreadable `/proc` when `hidepid` is enabled.
    let exists = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    let err = std::io::Error::last_os_error().raw_os_error();
    if exists {
        return true;
    }
    start == 0 && err == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_init(dir: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "user.email", "t@t"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn test_repo(tmp: &Path) -> Repo {
        let root = tmp.join("repo").canonicalize().unwrap_or(tmp.join("repo"));
        if !root.exists() {
            std::fs::create_dir(&root).unwrap();
        }
        let root = root.canonicalize().unwrap();
        git_init(&root);
        let dir = tmp.join("home/repo-test");
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo {
            root,
            dir,
            config: Config::default(),
        };
        repo.prepare_home().unwrap();
        repo
    }

    #[test]
    fn read_records_ignores_tmp_for_json_named_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let meta_dir = tmp.path().join("meta");
        std::fs::create_dir_all(&meta_dir).unwrap();
        let m = Meta {
            name: "foo.json".into(),
            path: tmp.path().join("ws/foo.json"),
            base: "abc".into(),
            created: 1,
            state: "ready".into(),
            pid: 0,
            pid_start: 0,
            key: String::new(),
            notes: Notes::default(),
            slot: Some(0),
            branch: String::new(),
            origin: String::new(),
        };
        write_record(&meta_dir.join("foo.json.json"), &m, true).unwrap();
        std::fs::write(
            meta_dir.join("foo.json.tmp.99999"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        let recs = read_records(&meta_dir, ".json");
        assert_eq!(recs.len(), 1);
    }

    #[test]
    fn owns_rejects_symlinks_and_meta_checks_name_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        repo.spawn(&SpawnOpts {
            name: "real".into(),
            ..Default::default()
        })
        .unwrap();

        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = repo.dir.join("ws/sym");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(!repo.owns(&link));

        // Forged record pointing to another workspace's folder or symlink is corrupt.
        let forged = Meta {
            name: "forged".into(),
            path: repo.dir.join("ws/real"),
            base: "abc".into(),
            created: 1,
            state: "ready".into(),
            pid: 0,
            pid_start: 0,
            key: String::new(),
            notes: Notes::default(),
            slot: Some(1),
            branch: String::new(),
            origin: String::new(),
        };
        write_record(&repo.meta_path("forged"), &forged, true).unwrap();
        assert!(repo.meta("forged").is_err());

        // Truncated JSON is caught and cleaned by gc().
        std::fs::write(repo.meta_path("broken"), b"{not valid json").unwrap();
        let dry = repo.gc(true).unwrap();
        assert_eq!(dry.corrupt, vec!["broken", "forged"]);
        let cleaned = repo.gc(false).unwrap();
        assert_eq!(cleaned.corrupt, vec!["broken", "forged"]);
        assert!(!repo.meta_path("broken").exists());
        assert!(!repo.meta_path("forged").exists());
        assert!(repo.dir.join("ws/real").exists());
    }

    #[test]
    fn isolate_submodules_rejects_gitdir_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        let sm_dir = repo.root.join("sm");
        std::fs::create_dir(&sm_dir).unwrap();
        let evil = repo.root.join("evil-gitdir");
        std::fs::create_dir(&evil).unwrap();
        std::fs::create_dir_all(repo.root.join(".git/modules")).unwrap();
        std::fs::write(repo.root.join(".gitmodules"), "[submodule \"sm\"]\n").unwrap();
        std::fs::write(sm_dir.join(".git"), "gitdir: ../evil-gitdir\n").unwrap();
        let inner = tmp.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        git_init(&inner);
        let sha = git::line(&inner, &["rev-parse", "HEAD"]).unwrap();
        // Create a gitlink entry in repo HEAD.
        git::run(
            &repo.root,
            &["update-index", "--add", "--cacheinfo", "160000", &sha, "sm"],
        )
        .unwrap();
        git::run(&repo.root, &["commit", "-q", "-m", "gitlink"]).unwrap();

        let Err(err) = isolate_submodules(&repo.root, &repo.root, &repo.root, false) else {
            panic!("isolate_submodules should reject gitdir escape");
        };
        assert!(err.to_string().contains("outside .git/modules"), "{err}");
    }

    #[test]
    fn diff_bundle_and_truncate_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        let s = repo
            .spawn(&SpawnOpts {
                name: "d1".into(),
                ..Default::default()
            })
            .unwrap();
        std::fs::write(s.path.join("a.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n").unwrap();
        let full = repo.diff_bundle("d1", &[], None).unwrap();
        assert!(!full.truncated);
        assert_eq!(full.files.len(), 1);
        let capped = repo.diff_bundle("d1", &[], Some(3)).unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.patch.lines().count(), 3);
    }

    #[test]
    fn land_rejects_checked_out_branch_and_conflicts_before_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        let s = repo
            .spawn(&SpawnOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .unwrap();
        std::fs::write(s.path.join("a.txt"), "changed\n").unwrap();
        let head_before = git::line(&s.path, &["rev-parse", "HEAD"]).unwrap();
        let Err(err) = repo.land(&LandOpts {
            name: "w1".into(),
            message: "test".into(),
            branch: Some("main".into()),
            force_branch: true,
            keep: false,
            no_verify: true,
            squash: false,
            pr: false,
            pr_base: None,
            draft: false,
            allow_self: false,
        }) else {
            panic!("land onto checked-out branch should fail");
        };
        assert!(err.to_string().contains("checked out"), "{err}");
        assert_eq!(
            git::line(&s.path, &["rev-parse", "HEAD"]).unwrap(),
            head_before
        );
    }

    #[test]
    fn pid_alive_and_signal_guard_work() {
        assert!(!pid_alive(0, 0));
        assert!(!pid_alive(999_999, 0));
        let start = own_start();
        assert!(pid_alive(std::process::id(), start));
        assert!(!pid_alive(std::process::id(), start.wrapping_add(123)));

        let g1 = ignore_interrupts();
        let g2 = ignore_interrupts();
        drop(g1);
        assert!(SIG_STATE.lock().unwrap().is_some());
        drop(g2);
        assert!(SIG_STATE.lock().unwrap().is_none());
    }

    #[test]
    fn coded_error_mapping_and_chain_preservation() {
        let e = coded_err(ErrorCode::SelfActionBlocked, "inside ws");
        assert_eq!(error_code(&e), ErrorCode::SelfActionBlocked);
        let wrapped = e.context("outer context");
        assert_eq!(error_code(&wrapped), ErrorCode::SelfActionBlocked);

        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        let Err(missing) = repo.meta("does-not-exist") else {
            panic!("expected missing workspace error");
        };
        assert_eq!(error_code(&missing), ErrorCode::NotFound);
        let bad_name = repo.spawn(&SpawnOpts {
            name: "../bad".into(),
            ..Default::default()
        });
        assert_eq!(
            error_code(&bad_name.err().unwrap()),
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn incremental_pool_refresh_handles_commit_vs_deps_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        std::fs::write(repo.root.join(".gitignore"), "node_modules/\n").unwrap();
        git::run(&repo.root, &["add", ".gitignore"]).unwrap();
        git::run(&repo.root, &["commit", "-q", "-m", "gitignore"]).unwrap();
        std::fs::create_dir_all(repo.root.join("node_modules/pkg")).unwrap();
        std::fs::write(repo.root.join("node_modules/pkg/index.js"), "v1\n").unwrap();

        repo.pool_set(1).unwrap();
        let fill1 = repo.pool_fill(None).unwrap();
        assert_eq!((fill1.created, fill1.refreshed, fill1.ready), (1, 0, 1));

        // Moving HEAD alone makes the pooled workspace refreshable, not stale.
        std::fs::write(repo.root.join("a.txt"), "two\n").unwrap();
        git::run(&repo.root, &["commit", "-qam", "second"]).unwrap();
        let st = repo.pool_status().unwrap();
        assert_eq!((st.fresh, st.refreshable, st.stale), (0, 1, 0));

        // Claiming refreshes in place and serves from_pool = true.
        let claimed = repo
            .spawn(&SpawnOpts {
                name: "c1".into(),
                use_pool: true,
                ..Default::default()
            })
            .unwrap();
        assert!(claimed.from_pool);
        assert_eq!(
            std::fs::read_to_string(claimed.path.join("a.txt")).unwrap(),
            "two\n"
        );
        assert!(claimed.path.join("node_modules/pkg/index.js").exists());

        // Refill, advance HEAD again, and test pool_fill in-place refresh.
        repo.pool_fill(None).unwrap();
        std::fs::write(repo.root.join("a.txt"), "three\n").unwrap();
        git::run(&repo.root, &["commit", "-qam", "third"]).unwrap();
        let fill2 = repo.pool_fill(None).unwrap();
        assert_eq!(
            (
                fill2.created,
                fill2.refreshed,
                fill2.removed_stale,
                fill2.ready
            ),
            (0, 1, 0, 1)
        );
        assert_eq!(repo.pool_status().unwrap().fresh, 1);

        // Changing a dependency stamp makes the pool entry stale (not refreshable).
        std::fs::write(repo.root.join("node_modules/.modules.yaml"), "lock\n").unwrap();
        let st2 = repo.pool_status().unwrap();
        assert_eq!((st2.fresh, st2.refreshable, st2.stale), (0, 0, 1));
        let fill3 = repo.pool_fill(None).unwrap();
        assert_eq!((fill3.removed_stale, fill3.created), (1, 1));
    }

    #[test]
    fn snap_snaps_restore_and_drop_preserve_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        let ws = repo
            .spawn(&SpawnOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .unwrap();

        // Create staged, unstaged, and untracked changes, plus a commit.
        std::fs::write(ws.path.join("committed.txt"), "c1\n").unwrap();
        git::run(&ws.path, &["add", "committed.txt"]).unwrap();
        git::run(&ws.path, &["commit", "-q", "-m", "c1"]).unwrap();
        let head1 = git::line(&ws.path, &["rev-parse", "HEAD"]).unwrap();

        std::fs::write(ws.path.join("staged.txt"), "s1\n").unwrap();
        git::run(&ws.path, &["add", "staged.txt"]).unwrap();
        std::fs::write(ws.path.join("a.txt"), "unstaged\n").unwrap();
        std::fs::write(ws.path.join("untracked.txt"), "u1\n").unwrap();

        let s1 = repo.snap("w1", Some("cp1")).unwrap();
        assert_eq!(s1.name, "cp1");
        assert_eq!(s1.head, head1);

        // Mutate further, including a new commit.
        std::fs::write(ws.path.join("a.txt"), "mutated\n").unwrap();
        std::fs::write(ws.path.join("extra.txt"), "extra\n").unwrap();
        git::run(&ws.path, &["add", "extra.txt"]).unwrap();
        git::run(&ws.path, &["commit", "-q", "-m", "extra"]).unwrap();

        let list = repo.snaps("w1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "cp1");

        // Restore latest (None) and verify everything matches cp1.
        let restored = repo.restore("w1", None).unwrap();
        assert_eq!(restored.name, "cp1");
        assert_eq!(git::line(&ws.path, &["rev-parse", "HEAD"]).unwrap(), head1);
        assert_eq!(
            std::fs::read_to_string(ws.path.join("a.txt")).unwrap(),
            "unstaged\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path.join("untracked.txt")).unwrap(),
            "u1\n"
        );
        assert!(!ws.path.join("extra.txt").exists());
        let st = git::run(&ws.path, &["status", "--porcelain"]).unwrap();
        assert!(st.contains("A  staged.txt"), "{st}");
        assert!(st.contains(" M a.txt"), "{st}");
        assert!(st.contains("?? untracked.txt"), "{st}");

        // Snapshot stays intact after restore and can be dropped.
        assert_eq!(repo.snaps("w1").unwrap().len(), 1);
        repo.snap_drop("w1", "cp1").unwrap();
        assert!(repo.snaps("w1").unwrap().is_empty());
        assert_eq!(
            error_code(&repo.restore("w1", None).err().unwrap()),
            ErrorCode::NotFound
        );
    }

    #[test]
    fn sync_clean_rebase_and_conflict_abort() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        git::run(&repo.root, &["config", "user.name", "t"]).unwrap();
        git::run(&repo.root, &["config", "user.email", "t@t"]).unwrap();

        let ws = repo
            .spawn(&SpawnOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .unwrap();
        std::fs::write(ws.path.join("ws.txt"), "ws\n").unwrap();
        git::run(&ws.path, &["add", "ws.txt"]).unwrap();
        git::run(&ws.path, &["commit", "-q", "-m", "ws commit"]).unwrap();

        // Advance main without conflict.
        std::fs::write(repo.root.join("main.txt"), "main\n").unwrap();
        git::run(&repo.root, &["add", "main.txt"]).unwrap();
        git::run(&repo.root, &["commit", "-q", "-m", "main commit"]).unwrap();
        let new_main = git::line(&repo.root, &["rev-parse", "HEAD"]).unwrap();

        // Dirty tree is rejected with Dirty.
        std::fs::write(ws.path.join("dirty.txt"), "x\n").unwrap();
        let err = repo
            .sync(&SyncOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .err()
            .unwrap();
        assert_eq!(error_code(&err), ErrorCode::Dirty);
        std::fs::remove_file(ws.path.join("dirty.txt")).unwrap();

        let synced = repo
            .sync(&SyncOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(synced.new_base, new_main);
        assert_eq!(synced.mode, "rebase");
        assert!(ws.path.join("main.txt").exists());
        assert!(ws.path.join("ws.txt").exists());
        assert_eq!(repo.meta("w1").unwrap().base, new_main);

        // Now create conflicting changes in both repo and ws.
        std::fs::write(ws.path.join("a.txt"), "ws conflict\n").unwrap();
        git::run(&ws.path, &["commit", "-qam", "ws edit a"]).unwrap();
        let head_before = git::line(&ws.path, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(repo.root.join("a.txt"), "main conflict\n").unwrap();
        git::run(&repo.root, &["commit", "-qam", "main edit a"]).unwrap();

        let cerr = repo
            .sync(&SyncOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .err()
            .unwrap();
        assert_eq!(error_code(&cerr), ErrorCode::Conflict);
        // Workspace is cleanly aborted back to pre-sync state.
        assert!(!is_dirty(&ws.path).unwrap());
        assert_eq!(
            git::line(&ws.path, &["rev-parse", "HEAD"]).unwrap(),
            head_before
        );
    }

    #[test]
    fn resync_deps_refreshes_ignored_dirs_and_rejects_tracked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        std::fs::write(repo.root.join(".gitignore"), "node_modules/\n.env\n").unwrap();
        git::run(&repo.root, &["add", ".gitignore"]).unwrap();
        git::run(&repo.root, &["commit", "-q", "-m", "gi"]).unwrap();
        std::fs::create_dir_all(repo.root.join("node_modules/dep")).unwrap();
        std::fs::write(repo.root.join("node_modules/dep/index.js"), "v1\n").unwrap();

        let ws = repo
            .spawn(&SpawnOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .unwrap();

        // Update source deps (including a secret that should be stripped in ws).
        std::fs::write(repo.root.join("node_modules/dep/index.js"), "v2\n").unwrap();
        std::fs::write(repo.root.join("node_modules/dep/.env"), "SECRET=1\n").unwrap();

        let res = repo.resync_deps("w1", &[]).unwrap();
        assert_eq!(res.resynced, vec!["node_modules"]);
        assert_eq!(
            std::fs::read_to_string(ws.path.join("node_modules/dep/index.js")).unwrap(),
            "v2\n"
        );
        assert!(!ws.path.join("node_modules/dep/.env").exists());

        // Tracked path explicitly requested must fail with InvalidInput.
        let err = repo.resync_deps("w1", &["a.txt".into()]).err().unwrap();
        assert_eq!(error_code(&err), ErrorCode::InvalidInput);
    }

    #[test]
    fn spawn_from_ensure_and_auto_name_work() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());

        let w1 = repo
            .spawn(&SpawnOpts {
                name: "base-ws".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(!w1.reused);

        // --ensure reuses existing ready workspace.
        let ensured = repo
            .spawn(&SpawnOpts {
                name: "base-ws".into(),
                ensure: true,
                ..Default::default()
            })
            .unwrap();
        assert!(ensured.reused);
        assert_eq!(ensured.path, w1.path);

        // --ensure with conflicting branch fails.
        let berr = repo
            .spawn(&SpawnOpts {
                name: "base-ws".into(),
                ensure: true,
                branch: Some("other".into()),
                ..Default::default()
            })
            .err()
            .unwrap();
        assert_eq!(error_code(&berr), ErrorCode::InvalidInput);

        // --auto picks base-ws-2 when base-ws exists.
        let auto = repo
            .spawn(&SpawnOpts {
                name: "base-ws".into(),
                auto_name: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(auto.name, "base-ws-2");

        // --from preserves uncommitted edits, staged index, and untracked files, keeping original base.
        std::fs::write(w1.path.join("a.txt"), "unstaged\n").unwrap();
        std::fs::write(w1.path.join("staged.txt"), "staged\n").unwrap();
        git::run(&w1.path, &["add", "staged.txt"]).unwrap();
        std::fs::write(w1.path.join("notes.txt"), "untracked\n").unwrap();

        let child = repo
            .spawn(&SpawnOpts {
                name: "child-ws".into(),
                from: Some("base-ws".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(child.base, w1.base);
        assert_eq!(
            std::fs::read_to_string(child.path.join("a.txt")).unwrap(),
            "unstaged\n"
        );
        assert_eq!(
            std::fs::read_to_string(child.path.join("notes.txt")).unwrap(),
            "untracked\n"
        );
        let st = git::run(&child.path, &["status", "--porcelain"]).unwrap();
        assert!(st.contains("A  staged.txt"), "{st}");
    }

    #[test]
    fn whoami_and_self_action_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = test_repo(tmp.path());
        let w1 = repo
            .spawn(&SpawnOpts {
                name: "w1".into(),
                ..Default::default()
            })
            .unwrap();
        let w2 = repo
            .spawn(&SpawnOpts {
                name: "w2".into(),
                ..Default::default()
            })
            .unwrap();
        std::fs::write(w1.path.join("a.txt"), "one-changed\n").unwrap();
        std::fs::write(w2.path.join("a.txt"), "two\n").unwrap();

        let outside = repo.whoami(&repo.root, false).unwrap();
        assert!(!outside.inside_workspace);
        assert!(outside.name.is_none());

        let inside = repo.whoami(&w1.path, true).unwrap();
        assert!(inside.inside_workspace);
        assert_eq!(inside.name.as_deref(), Some("w1"));
        assert_eq!(inside.overlaps.get("a.txt").unwrap(), &vec!["w1", "w2"]);

        // Self-action guard blocks drop & land from inside w1 unless allow_self = true.
        let derr = repo
            .drop_with_opts("w1", false, false, Some(&w1.path))
            .err()
            .unwrap();
        assert_eq!(error_code(&derr), ErrorCode::SelfActionBlocked);

        let lerr = repo
            .land_from(
                &LandOpts {
                    name: "w1".into(),
                    message: "test".into(),
                    branch: None,
                    force_branch: false,
                    keep: false,
                    no_verify: true,
                    squash: false,
                    pr: false,
                    pr_base: None,
                    draft: false,
                    allow_self: false,
                },
                Some(&w1.path),
            )
            .err()
            .unwrap();
        assert_eq!(error_code(&lerr), ErrorCode::SelfActionBlocked);

        // --if-exists returns false when missing.
        assert!(!repo.drop_with_opts("missing", true, false, None).unwrap());
    }

    #[test]
    fn du_with_options_and_prune_target_free_bytes() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let mut repo = test_repo(tmp.path());
        repo.config.max_private_gb = Some(5);

        let big = repo
            .spawn(&SpawnOpts {
                name: "big".into(),
                ..Default::default()
            })
            .unwrap();
        let small = repo
            .spawn(&SpawnOpts {
                name: "small".into(),
                ..Default::default()
            })
            .unwrap();
        let dirty = repo
            .spawn(&SpawnOpts {
                name: "dirty".into(),
                ..Default::default()
            })
            .unwrap();

        // Make `big` have 16 MiB private, `small` have 0 private, `dirty` uncommitted.
        std::fs::create_dir_all(big.path.join("sub")).unwrap();
        let mut f = std::fs::File::create(big.path.join("sub/blob.bin")).unwrap();
        f.write_all(&vec![0x5Au8; 16 << 20]).unwrap();
        f.sync_all().unwrap();
        git::run(&big.path, &["add", "sub/blob.bin"]).unwrap();
        git::run(&big.path, &["commit", "-q", "-m", "big"]).unwrap();
        git::run(
            &repo.root,
            &[
                "merge",
                "-q",
                "--ff-only",
                &git::line(&big.path, &["rev-parse", "HEAD"]).unwrap(),
            ],
        )
        .unwrap();
        // Keep big clean & merged.
        std::fs::write(dirty.path.join("a.txt"), "dirty\n").unwrap();

        let du = repo.du_with_options(None, 1, true).unwrap();
        assert_eq!(du.budget_bytes, Some(5 * GB));
        let big_entry = du.entries.iter().find(|e| e.name == "big").unwrap();
        assert!(!big_entry.breakdown.is_empty());
        assert!(du.reclaimable_bytes >= du.total.private);

        // Target free bytes above current free triggers pruning largest first;
        // dropping `big` (16 MiB) alone suffices for target = free + 8 MiB, leaving `small` & `dirty`.
        let free_now = fsutil::free_bytes(&repo.dir).unwrap();
        let p = repo
            .prune(&PruneOpts {
                base: Some("main".into()),
                dry_run: true,
                min_age_secs: 0,
                target_free_bytes: Some(free_now.saturating_add(8 << 20)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            p.pruned.len(),
            1,
            "kept: {:?}",
            p.kept.iter().map(|k| (&k.name, &k.why)).collect::<Vec<_>>()
        );
        assert_eq!(p.pruned[0].name, "big");
        assert!(p.kept.iter().any(|k| k.name == "dirty"));
        assert!(p.kept.iter().any(|k| k.name == "small"));
        assert!(small.path.exists() && dirty.path.exists());
    }
}
