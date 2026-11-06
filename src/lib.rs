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
use std::path::{Path, PathBuf};
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
}

/// What `Repo::fill` did to a new workspace.
struct Filled {
    clone_ms: u128,
    calls: usize,
    notes: Notes,
    carried: Vec<String>,
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
}

#[derive(Serialize)]
pub struct DiskUsage {
    pub entries: Vec<WsUsage>,
    pub total: fsutil::Usage,
    pub free_bytes: u64,
}

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
    pub removed_stale: usize,
    pub removed_extra: usize,
    pub ready: usize,
}

#[derive(Serialize)]
pub struct PoolStatus {
    pub target: usize,
    pub fresh: usize,
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
        let root = noted.or_else(|| {
            read_records(&dir.join("meta"), ".json")
                .into_iter()
                .find(|(_, m)| m.path.exists())
                .and_then(|(_, m)| {
                    git::line(
                        &m.path,
                        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    )
                    .ok()
                })
                .and_then(|common| Path::new(&common).parent().map(Path::to_path_buf))
        });
        let Some(root) = root.filter(|r| r.join(".git").exists()) else {
            continue;
        };
        let workspaces = read_records(&dir.join("meta"), ".json")
            .into_iter()
            .filter(|(_, m)| m.state == "ready")
            .count();
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
        bail!("invalid workspace name {name:?}: use letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

/// Rules for a new workspace: `check_name`, and the name must work in the
/// branch `land` creates. Kept apart from `check_name`, which also decides
/// what `owns` accepts: workspaces made before this rule stay manageable.
fn check_new_name(name: &str) -> Result<()> {
    check_name(name)?;
    if name.contains("..") || name.ends_with('.') || name.ends_with(".lock") {
        bail!(
            "invalid workspace name {name:?}: it becomes the branch agent-ws/{name}, and Git branch names cannot contain '..' or end with '.' or '.lock'"
        );
    }
    Ok(())
}

/// Fail unless Git accepts `branch` as a new branch name, before `land`
/// commits anything. The output must equal the input: `--branch` mode
/// expands `@{-1}` to the previous branch.
fn check_branch(root: &Path, branch: &str) -> Result<()> {
    match git::line(root, &["check-ref-format", "--branch", branch]) {
        Ok(out) if out == branch => Ok(()),
        _ => bail!("{branch:?} is not a valid branch name; pick another with --branch"),
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
                .is_some_and(|n| n.contains(suffix))
        })
        .filter_map(|p| read_meta(&p).map(|m| (p, m)))
        .collect();
    out.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    out
}

/// Hold an exclusive `flock` on `path` until the returned file is dropped.
/// The lock goes away with the process, so a crash cannot leave it held.
fn lock_file(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    while unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e).with_context(|| format!("lock {}", path.display()));
        }
    }
    Ok(f)
}

/// Write `value` as JSON to `p` by rename, so readers never see half of it.
fn write_json<T: Serialize>(p: &Path, value: &T) -> Result<()> {
    let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

fn write_record(p: &Path, m: &Meta, create_new: bool) -> Result<()> {
    let raw = serde_json::to_vec_pretty(m)?;
    if create_new {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    anyhow::anyhow!("workspace {:?} already exists", m.name)
                }
                _ => e.into(),
            })?;
        f.write_all(&raw)?;
    } else {
        // Write then rename so readers never see a half-written record.
        let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, raw)?;
        std::fs::rename(tmp, p)?;
    }
    Ok(())
}

impl Repo {
    /// Find the repository containing `cwd`. Works from inside a workspace too.
    pub fn discover(cwd: &Path) -> Result<Repo> {
        let common = git::line(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .context("not inside a Git repository")?;
        let common = PathBuf::from(common);
        if common.file_name().and_then(|n| n.to_str()) != Some(".git") {
            bail!("bare repositories are not supported ({})", common.display());
        }
        let root = common.parent().context("no repository root")?.to_path_buf();
        let base = root.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
        let dir = home()?.join(format!(
            "{base}-{:08x}",
            fnv(&root.to_string_lossy()) as u32
        ));
        let config = Config::load(&root)?;
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
    /// of `<dir>/ws` named like a workspace or a pool entry. Records are
    /// plain files anyone can edit, so every destructive or git operation on
    /// a record's path checks this first.
    pub fn owns(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let named = check_name(name).is_ok()
            || name
                .strip_prefix(POOL_PREFIX)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        // Compare the exact text: `Path` equality ignores a trailing `/` and
        // repeated separators, which would let a record name `ws/x/` pass.
        named && path.as_os_str() == self.dir.join("ws").join(name).as_os_str()
    }

    pub fn meta(&self, name: &str) -> Result<Meta> {
        check_name(name)?;
        let p = self.meta_path(name);
        let raw = std::fs::read(&p).with_context(|| format!("no workspace named {name:?}"))?;
        let m: Meta = serde_json::from_slice(&raw)?;
        if !self.owns(&m.path) {
            bail!(
                "corrupt record for {name:?}: path {} is not a workspace folder (run `agent-ws gc`)",
                m.path.display()
            );
        }
        if m.state != "ready" {
            bail!(
                "workspace {name:?} is still being created (run `agent-ws gc` if its spawn died)"
            );
        }
        Ok(m)
    }

    /// Workspaces that finished spawning.
    pub fn list(&self) -> Result<Vec<Meta>> {
        Ok(read_records(&self.dir.join("meta"), ".json")
            .into_iter()
            .map(|(_, m)| m)
            .filter(|m| m.state == "ready" && self.owns(&m.path))
            .collect())
    }

    fn prepare_home(&self) -> Result<()> {
        for d in ["meta", "pool", "ws", ".trash"] {
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
        let rev = format!("{}^{{commit}}", rev.unwrap_or("HEAD"));
        git::line(&self.root, &["rev-parse", "--verify", &rev])
    }

    /// A pooled workspace is reusable while HEAD, the settings that shape a
    /// workspace and the dependency stamp files are unchanged.
    fn pool_key(&self, head: &str) -> String {
        use std::os::unix::fs::MetadataExt;
        let c = &self.config;
        let mut s = format!(
            "{head}|{:?}|{:?}|{:?}|{}",
            c.exclude, c.secrets, c.secrets_keep, c.strict_symlinks
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
            bail!("pool size {size} is above the maximum of {MAX_POOL}");
        }
        self.prepare_home()?;
        std::fs::write(
            self.dir.join("pool.json"),
            serde_json::to_vec(&serde_json::json!({ "size": size }))?,
        )?;
        Ok(())
    }

    pub fn spawn(&self, o: &SpawnOpts) -> Result<Spawned> {
        let t0 = Instant::now();
        check_new_name(&o.name)?;
        if let Some(b) = &o.branch {
            check_branch(&self.root, b)?;
            if self.branch_tip(b).is_some() {
                bail!("branch {b} already exists");
            }
        }
        let hooks = match &self.config.hooks {
            Some(h) if !o.no_hooks && !h.post_spawn.is_empty() => self.trusted_hooks()?.post_spawn,
            _ => vec![],
        };
        self.prepare_home()?;
        let mut pooled = None;
        if o.use_pool && o.exclude.is_empty() && !o.keep_secrets && !o.carry {
            let head = self.resolve(None)?;
            if self.resolve(o.base.as_deref())? == head {
                pooled = self.claim(o, &self.pool_key(&head))?;
            }
        }
        let mut s = match pooled {
            Some(s) => s,
            None => self.spawn_fresh(o, t0)?,
        };
        // The workspace exists from here on; a failure removes it again so
        // that a spawn either fully happens or leaves nothing.
        let finish = || -> Result<()> {
            let m = self.meta(&o.name)?;
            if let Some(b) = &o.branch {
                git::run(&m.path, &["switch", "-q", "-c", b])?;
            }
            self.run_hooks("post_spawn", &hooks, &m)
        };
        if let Err(e) = finish() {
            let _ = self.drop(&o.name);
            return Err(e.context(format!("spawn of {:?} undone", o.name)));
        }
        s.total_ms = t0.elapsed().as_millis();
        s.warnings = self.spawn_warnings();
        Ok(s)
    }

    /// Clone a new workspace for `o` and register it.
    fn spawn_fresh(&self, o: &SpawnOpts, t0: Instant) -> Result<Spawned> {
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
                meta.state = ready();
                meta.pid = 0;
                meta.pid_start = 0;
                meta.notes = s.notes.clone();
                write_record(&rec, &meta, false)?;
                s.port = self.port(&meta);
                s.branch = meta.branch;
                Ok(s)
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

    /// Take a fresh pooled workspace and register it under `o.name`.
    fn claim(&self, o: &SpawnOpts, key: &str) -> Result<Option<Spawned>> {
        for (rec, m) in read_records(&self.dir.join("pool"), ".json") {
            if m.state != "pooled" || m.key != key || !self.owns(&m.path) {
                continue;
            }
            // rename() is atomic: of several spawns racing for this entry,
            // exactly one moves the record; the others get NotFound.
            let claim = rec.with_extension(claim_ext());
            if std::fs::rename(&rec, &claim).is_err() {
                continue;
            }
            if !m.path.exists() {
                let _ = std::fs::remove_file(&claim);
                continue;
            }
            let mut named = Meta {
                name: o.name.clone(),
                created: now(),
                state: ready(),
                pid: 0,
                pid_start: 0,
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
                let _ = std::fs::rename(&claim, &rec);
                return Err(e);
            }
            std::fs::remove_file(&claim)?;
            return Ok(Some(Spawned {
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
                branch: named.branch,
                carried: vec![],
            }));
        }
        Ok(None)
    }

    fn create(&self, o: &SpawnOpts, path: &Path, t0: Instant) -> Result<Spawned> {
        if path.exists() {
            bail!("{} already exists; run `agent-ws gc`", path.display());
        }
        if fsutil::device(&self.root)? != fsutil::device(&self.dir)? {
            bail!(
                "{} and {} are on different volumes; clones need one volume (set AGENT_WS_HOME)",
                self.root.display(),
                self.dir.display()
            );
        }
        let min_free = o
            .min_free_bytes
            .or(self.config.min_free_gb.map(|g| g * GB))
            .unwrap_or(2 * GB);
        let free = fsutil::free_bytes(&self.dir)?;
        if free < min_free {
            bail!(
                "only {} MB free on disk (minimum {} MB)",
                free / 1_000_000,
                min_free / 1_000_000
            );
        }
        let base = self.resolve(o.base.as_deref())?;
        if o.carry && base != self.resolve(None)? {
            bail!("--carry brings changes made on top of HEAD, so the base must be HEAD");
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
        let mut excludes: Vec<PathBuf> = DEFAULT_EXCLUDES.iter().map(PathBuf::from).collect();
        excludes.extend(
            self.config
                .exclude
                .iter()
                .chain(&o.exclude)
                .map(PathBuf::from),
        );
        // Never clone the agent-ws home if it lives inside the repository.
        if let Ok(rel) = self.dir.strip_prefix(&self.root) {
            excludes.push(rel.components().take(1).collect());
        }
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
        let untracked = git::run(path, &["ls-files", "-o", "--exclude-standard", "-z"])?;
        let mut cleaned_secrets: Vec<String> = untracked
            .split('\0')
            .filter(|r| {
                Path::new(r)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| self.config.is_secret(n))
                    && !fsutil::only_certificates(&path.join(r))
            })
            .map(String::from)
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
            isolate_submodules(path, &self.root, !o.carry)?;
        }

        let tracked: HashSet<String> = git::run(path, &["ls-files", "-z", "--recurse-submodules"])?
            .split('\0')
            .map(String::from)
            .collect();
        let never = |_: &str| false;
        let secret = |n: &str| self.config.is_secret(n);
        let is_secret: &dyn Fn(&str) -> bool = if o.keep_secrets { &never } else { &secret };
        let scan = fsutil::scan(&self.root, path, &tracked, is_secret)?;
        if self.config.strict_symlinks && !scan.external.is_empty() {
            let list: Vec<String> = scan
                .external
                .iter()
                .map(|(p, t)| format!("{} -> {}", p.display(), t.display()))
                .collect();
            bail!(
                "symlinks point outside the workspace (strict_symlinks): {}",
                list.join(", ")
            );
        }
        let notes = Notes {
            symlinks_retargeted: scan.retargeted,
            symlinks_external: scan
                .external
                .into_iter()
                .map(|(path, target)| ExternalLink { path, target })
                .collect(),
            secrets_removed: {
                let mut all = scan.secrets;
                all.extend(cleaned_secrets);
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
    /// workspaces, removing stale ones. Only one fill runs at a time.
    pub fn pool_fill(&self, size: Option<usize>) -> Result<PoolFill> {
        self.prepare_home()?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("pool.lock"))?;
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Ok(PoolFill {
                busy: true,
                ..Default::default()
            });
        }
        let target = size.unwrap_or_else(|| self.pool_target());
        let key = self.pool_key(&self.resolve(None)?);
        let mut report = PoolFill::default();
        let mut fresh = 0;
        for (rec, m) in read_records(&self.dir.join("pool"), ".json") {
            // Corrupt records are left for `gc`, which forgets them safely.
            if m.state != "pooled" || !self.owns(&m.path) {
                continue;
            }
            let stale = m.key != key;
            if !stale && fresh < target {
                fresh += 1;
                continue;
            }
            // Claim before removing so a racing spawn cannot take it.
            let claim = rec.with_extension(claim_ext());
            if std::fs::rename(&rec, &claim).is_ok() {
                self.discard(&m.path)?;
                std::fs::remove_file(&claim)?;
                if stale {
                    report.removed_stale += 1
                } else {
                    report.removed_extra += 1
                }
            }
        }
        let opts = SpawnOpts::default();
        while fresh < target {
            let id = format!("{POOL_PREFIX}{}", nanos());
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
        let key = self.pool_key(&self.resolve(None)?);
        let recs = read_records(&self.dir.join("pool"), ".json");
        let count = |f: &dyn Fn(&Meta) -> bool| recs.iter().filter(|(_, m)| f(m)).count();
        Ok(PoolStatus {
            target: self.pool_target(),
            fresh: count(&|m| m.state == "pooled" && m.key == key),
            stale: count(&|m| m.state == "pooled" && m.key != key),
            creating: count(&|m| m.state == "creating"),
        })
    }

    /// Move a workspace tree into `.trash`, delete it in the background and
    /// forget its worktree registration. Refuses any path this tool does not
    /// own, whatever a record says.
    fn discard(&self, path: &Path) -> Result<()> {
        if !self.owns(path) {
            bail!(
                "refusing to remove {}: not a workspace folder of this repository",
                path.display()
            );
        }
        if path.symlink_metadata().is_ok() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("ws");
            let trash = self.dir.join(".trash").join(format!("{name}-{}", nanos()));
            std::fs::create_dir_all(self.dir.join(".trash"))?;
            std::fs::rename(path, &trash)?;
            delete_in_background(&trash)?;
        }
        self.worktree(&["prune"])?;
        Ok(())
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
        bail!(
            "{} has hooks that run commands in your workspaces:{list}\nReview them, then run `agent-ws trust` (or skip them with --no-hooks / --no-verify)",
            config::FILE
        )
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
        for (_, m) in read_records(&self.dir.join("pool"), ".") {
            out.insert(m.path);
        }
        out
    }

    /// Clean up after crashes and manual deletes. Never touches a workspace
    /// whose creating or claiming process is still alive.
    pub fn gc(&self, dry_run: bool) -> Result<Gc> {
        let mut report = Gc::default();
        let recs = read_records(&self.dir.join("meta"), ".json")
            .into_iter()
            .chain(read_records(&self.dir.join("pool"), ".json"));
        for (rec, m) in recs {
            if !self.owns(&m.path) {
                report.corrupt.push(m.name.clone());
                if !dry_run {
                    let _ = std::fs::remove_file(rec);
                }
                continue;
            }
            let dead = m.state == "creating" && !pid_alive(m.pid, m.pid_start);
            let missing = m.state != "creating" && !m.path.exists();
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
                let _ = std::fs::remove_file(rec);
            }
        }
        // A claim file whose process died: if a named record already points
        // at the workspace the claim had succeeded; otherwise it is lost.
        for (rec, m) in read_records(&self.dir.join("pool"), ".claim-") {
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
                    .iter()
                    .any(|(_, n)| n.path == m.path);
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
        if Path::new(&common) != self.root.join(".git") {
            bail!("not a worktree of this repository");
        }
        let dirty = !git::run(path, &["--no-optional-locks", "status", "--porcelain"])?
            .trim()
            .is_empty();
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
            n += 1;
            if !dry_run {
                delete_in_background(&e.path())?;
            }
        }
        Ok(n)
    }

    /// Run records of dropped workspaces, and snapshot index files that a
    /// killed process left behind.
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
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Warnings for `spawn`: many workspaces, or little free disk.
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
        out
    }

    /// Real disk use of every workspace, pooled workspace and trash entry.
    /// `only` limits it to one named workspace.
    pub fn du(&self, only: Option<&str>) -> Result<DiskUsage> {
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
        let sizes = usage_parallel(todo.iter().map(|t| t.1.clone()).collect());
        let mut total = fsutil::Usage::default();
        let entries = todo
            .into_iter()
            .zip(sizes)
            .map(|((name, path, kind), usage)| {
                total += usage;
                WsUsage {
                    name,
                    path,
                    kind: kind.into(),
                    usage,
                }
            })
            .collect();
        Ok(DiskUsage {
            entries,
            total,
            free_bytes: fsutil::free_bytes(&self.dir).unwrap_or(0),
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
        let ok = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&m.path)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if ok(&["merge-base", "--is-ancestor", head, base]) {
            return Some("merged".into());
        }
        // Squash and rebase merges leave HEAD outside the base. Their work is
        // in the base when every path HEAD changed since the fork point is
        // identical in the base. Read-only (no merge objects written); a
        // base that changed those paths again later keeps the workspace.
        if let Ok(fork) = git::line(&m.path, &["merge-base", head, base])
            && let Ok(changed) = git::run(&m.path, &["diff", "--name-only", "-z", &fork, head])
        {
            let paths: Vec<&str> = changed.split('\0').filter(|p| !p.is_empty()).collect();
            let mut args = vec!["--literal-pathspecs", "diff", "--quiet", base, head, "--"];
            args.extend(&paths);
            if paths.is_empty() || ok(&args) {
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
        let users = fsutil::cwd_users(
            &std::fs::canonicalize(self.dir.join("ws")).unwrap_or(self.dir.join("ws")),
        );
        for m in self.list()? {
            let keep = |why: String| Kept {
                name: m.name.clone(),
                why,
            };
            if !m.path.exists() {
                kept.push(keep("folder is missing (run `agent-ws gc`)".into()));
                continue;
            }
            let real = std::fs::canonicalize(&m.path).unwrap_or(m.path.clone());
            if let Some((pid, _)) = users.iter().find(|(_, cwd)| cwd.starts_with(&real)) {
                kept.push(keep(format!("in use by process {pid}")));
                continue;
            }
            let age = now.saturating_sub(m.created);
            if age < o.min_age_secs {
                kept.push(keep(format!("created {}m ago", age / 60)));
                continue;
            }
            // No optional locks: a dry run must not even refresh the index.
            let dirty = match git::run(&m.path, &["--no-optional-locks", "status", "--porcelain"]) {
                Ok(s) => !s.trim().is_empty(),
                Err(_) => true,
            };
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
        if !o.dry_run {
            for p in &pruned {
                self.drop(&p.name)?;
                if !p.branch.is_empty() {
                    let _ = git::run(&self.root, &["branch", "-q", "-D", &p.branch]);
                }
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

    /// Changed paths per ready workspace, skipping folders deleted by hand.
    fn changes(&self) -> Result<Vec<(Meta, Vec<String>)>> {
        let mut out = Vec::new();
        for m in self.list()? {
            if m.path.exists() {
                let c = git::changed_paths(&m.path, &m.base)
                    .with_context(|| format!("workspace {:?}", m.name))?;
                out.push((m, c));
            }
        }
        Ok(out)
    }

    pub fn status(&self, only: Option<&str>) -> Result<Status> {
        // Overlaps are computed across all workspaces, even when one is selected.
        let all = self.changes()?;
        let mut by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (m, changed) in &all {
            for p in changed {
                by_path.entry(p.clone()).or_default().push(m.name.clone());
            }
        }
        let metas: Vec<Meta> = all.iter().map(|(m, _)| m.clone()).collect();
        let workspaces: Vec<WsStatus> = all
            .into_iter()
            .filter(|(m, _)| only.is_none_or(|n| n == m.name))
            .map(|(m, changed)| WsStatus {
                name: m.name,
                path: m.path,
                base: m.base,
                changed,
            })
            .collect();
        if let Some(n) = only
            && workspaces.is_empty()
        {
            bail!("no workspace named {n:?}");
        }
        let overlaps: BTreeMap<String, Vec<String>> =
            by_path.into_iter().filter(|(_, v)| v.len() > 1).collect();
        let conflicts = self.conflicts(&metas, &overlaps);
        Ok(Status {
            workspaces,
            overlaps,
            conflicts,
        })
    }

    /// Which overlapping workspaces would conflict if both were merged.
    /// Each one is snapshotted as a commit on its HEAD and `merge-tree`
    /// merges the pair in memory; nothing in the workspaces changes. A
    /// workspace Git cannot snapshot is left out.
    fn conflicts(
        &self,
        metas: &[Meta],
        overlaps: &BTreeMap<String, Vec<String>>,
    ) -> Option<Vec<Conflict>> {
        let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
        for names in overlaps.values() {
            for (i, a) in names.iter().enumerate() {
                for b in &names[i + 1..] {
                    let (a, b) = if a < b { (a, b) } else { (b, a) };
                    pairs.insert((a.clone(), b.clone()));
                }
            }
        }
        let mut commits: HashMap<String, Option<String>> = HashMap::new();
        let mut commit = |name: &str| -> Option<String> {
            commits
                .entry(name.to_string())
                .or_insert_with(|| {
                    let m = metas.iter().find(|m| m.name == name)?;
                    let tree = git::snapshot_tree(&m.path, &self.scratch()).ok()?;
                    let head = git::line(&m.path, &["rev-parse", "HEAD"]).ok()?;
                    git::snapshot_commit(&m.path, &tree, &head).ok()
                })
                .clone()
        };
        let mut out = Vec::new();
        for (a, b) in pairs {
            let (Some(ca), Some(cb)) = (commit(&a), commit(&b)) else {
                continue;
            };
            match git::merge_conflicts(&self.root, &ca, &cb) {
                Ok(Some(paths)) if !paths.is_empty() => out.push(Conflict {
                    workspaces: vec![a, b],
                    paths,
                }),
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => return None,
            }
        }
        Some(out)
    }

    /// Temporary index files for snapshots.
    fn scratch(&self) -> PathBuf {
        self.dir.join("tmp")
    }

    /// A workspace's current state as a tree: its commits, uncommitted edits
    /// and new files, read without touching its index.
    pub fn snapshot(&self, name: &str) -> Result<Snapshot> {
        let m = self.meta(name)?;
        let tree = git::snapshot_tree(&m.path, &self.scratch())?;
        Ok(Snapshot {
            path: m.path,
            base: m.base,
            tree,
        })
    }

    /// Files a workspace changed since its base, with line counts, limited
    /// to `paths` when given.
    pub fn diff_files(&self, name: &str, paths: &[String]) -> Result<Vec<FileChange>> {
        let s = self.snapshot(name)?;
        let run = |what: &str| {
            let mut args = vec!["diff", "--no-renames", "--no-ext-diff", "-z", what];
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

    pub fn land(&self, o: &LandOpts) -> Result<Landed> {
        let m = self.meta(&o.name)?;
        let files = git::changed_paths(&m.path, &m.base)?;
        if files.is_empty() {
            bail!("workspace {:?} has no changes", o.name);
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
        let clean = git::run(&m.path, &["status", "--porcelain"])?
            .trim()
            .is_empty();
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
                let ancestor = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&m.path)
                    .args(["merge-base", "--is-ancestor", tip, &head])
                    .status()
                    .is_ok_and(|s| s.success());
                ancestor && landed_by_this(tip)
            });
        if existing.is_some() && !resume && !advance && !own && !o.force_branch {
            bail!(
                "branch {branch} already exists and does not match this workspace; pass --branch NAME or --force-branch"
            );
        }
        // Overlaps are advice: a workspace Git cannot read must not block
        // landing this one (`status` reports it).
        let mut overlaps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for other in self.list()? {
            if other.name == o.name || !other.path.exists() {
                continue;
            }
            let Ok(changed) = git::changed_paths(&other.path, &other.base) else {
                continue;
            };
            for p in changed.into_iter().filter(|p| files.contains(p)) {
                overlaps.entry(p).or_default().push(other.name.clone());
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
        let (title, rest) = o.message.split_once('\n').unwrap_or((&o.message, ""));
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

    /// Remove a workspace. The tree is renamed into `.trash` (instant) and
    /// deleted by a detached `rm -rf`, which takes seconds for large trees.
    pub fn drop(&self, name: &str) -> Result<()> {
        let m = self.meta(name)?;
        self.discard(&m.path)?;
        std::fs::remove_file(self.meta_path(name))?;
        let _ = std::fs::remove_file(self.run_path(name));
        let _ = std::fs::remove_file(self.log_path(name));
        // The branch `spawn --branch` made holds nothing until it moves.
        if !m.branch.is_empty() && self.branch_tip(&m.branch).as_deref() == Some(m.base.as_str()) {
            let _ = git::run(&self.root, &["branch", "-q", "-D", &m.branch]);
        }
        // Finish deletes that an earlier killed process left in the trash.
        self.sweep_trash(false)?;
        Ok(())
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
        let mut args = vec![
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
        restore();
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
        let users = fsutil::cwd_users(
            &std::fs::canonicalize(self.dir.join("ws")).unwrap_or(self.dir.join("ws")),
        );
        let sizes: Vec<Option<u64>> = if disk {
            usage_parallel(metas.iter().map(|m| m.path.clone()).collect())
                .into_iter()
                .map(|u| Some(u.private))
                .collect()
        } else {
            vec![None; metas.len()]
        };
        let now = now();
        Ok(metas
            .into_iter()
            .zip(sizes)
            .map(|(m, private_bytes)| {
                let real = std::fs::canonicalize(&m.path).unwrap_or(m.path.clone());
                let dirty = git::run(&m.path, &["--no-optional-locks", "status", "--porcelain"])
                    .ok()
                    .map(|s| !s.trim().is_empty());
                let ahead = git::line(
                    &m.path,
                    &["rev-list", "--count", &format!("{}..HEAD", m.base)],
                )
                .ok()
                .and_then(|n| n.parse().ok());
                let processes = users
                    .iter()
                    .filter(|(_, cwd)| cwd.starts_with(&real))
                    .map(|(pid, _)| *pid)
                    .collect();
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

/// Ignore Ctrl-C and Ctrl-\ in this process until the returned closure runs,
/// so that `exec` outlives the command it waits for. Set after the child
/// starts: ignored signals would otherwise carry over into it.
fn ignore_interrupts() -> impl FnOnce() {
    let previous: Vec<(libc::c_int, libc::sighandler_t)> = [libc::SIGINT, libc::SIGQUIT]
        .into_iter()
        .map(|s| (s, unsafe { libc::signal(s, libc::SIG_IGN) }))
        .collect();
    move || {
        for (s, handler) in previous {
            unsafe { libc::signal(s, handler) };
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
fn isolate_submodules(ws_repo: &Path, src_repo: &Path, reset: bool) -> Result<()> {
    let tree = git::run(ws_repo, &["ls-tree", "-r", "-z", "HEAD"])?;
    // Entries look like `160000 commit <sha>\t<path>`.
    for entry in tree.split('\0').filter(|e| e.starts_with("160000 ")) {
        let Some((head, rel)) = entry.split_once('\t') else {
            continue;
        };
        let sha = head.rsplit(' ').next().unwrap_or_default();
        let (ws_sm, src_sm) = (ws_repo.join(rel), src_repo.join(rel));
        let dotgit = ws_sm.join(".git");
        // Not initialised in the source: nothing checked out to fix.
        let Ok(md) = dotgit.symlink_metadata() else {
            continue;
        };
        if md.is_file() {
            let raw = std::fs::read_to_string(&dotgit)?;
            let Some(gd) = raw.trim().strip_prefix("gitdir:").map(str::trim) else {
                bail!("submodule {rel}: unexpected .git file");
            };
            let gitdir = src_sm
                .join(gd)
                .canonicalize()
                .with_context(|| format!("submodule {rel}: git dir {gd} not found"))?;
            std::fs::remove_file(&dotgit)?;
            fsutil::clone_path(&gitdir, &dotgit)?;
            // The copy sits inside the tree it describes; the source's
            // core.worktree points elsewhere and breaks every git command
            // there (even `git -C <submodule> config`), so edit the config
            // file from a directory outside any repository.
            let config = dotgit.join("config").to_string_lossy().into_owned();
            let _ = git::run(
                Path::new("/"),
                &["config", "--file", &config, "--unset", "core.worktree"],
            );
        }
        if reset {
            git::run(&ws_sm, &["checkout", "-q", "-f", "--detach", sha])?;
            git::run(&ws_sm, &["clean", "-fdq"])?;
        }
        isolate_submodules(&ws_sm, &src_sm, reset)?;
    }
    Ok(())
}

fn delete_in_background(path: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new("rm")
        .arg("-rf")
        .arg(path)
        // Own process group: closing the terminal or killing the agent's
        // group must not stop the delete and leave the tree in `.trash`.
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Start time of a process, only ever compared with another reading on the
/// same machine: clock ticks since boot (field 22 of `/proc/<pid>/stat`).
#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state …`: the name may hold spaces and parentheses, so
    // count fields from the last `)`; the first after it is field 3.
    stat.get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Start time of a process in microseconds since the epoch.
#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<u64> {
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
    (got == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

fn own_start() -> u64 {
    process_start(std::process::id()).unwrap_or(0)
}

/// Extension of a pool claim file: `claim-<pid>-<start>`.
fn claim_ext() -> String {
    format!("claim-{}-{}", std::process::id(), own_start())
}

/// Whether the process that wrote a record still runs. With a recorded start
/// time, a different process that got the same pid does not count.
fn pid_alive(pid: u32, start: u64) -> bool {
    if pid == 0 {
        return false;
    }
    if start != 0
        && let Some(actual) = process_start(pid)
    {
        return actual == start;
    }
    // Signal 0 checks existence; EPERM means it exists under another user.
    let exists = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    exists || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
