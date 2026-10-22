//! agent-ws: instant, isolated workspaces for coding agents.
//!
//! A workspace is a Git worktree whose files are APFS clones of the source
//! checkout, ignored files included (`node_modules`, build caches). Spawning
//! costs metadata only, so agents skip `install` and start with warm caches.
//! A pool of ready workspaces makes spawning a rename-sized operation.
//! The library holds all logic so other tools can embed it.

pub mod config;
pub mod fsutil;
pub mod git;

use anyhow::{Context, Result, bail};
use config::Config;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
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
}

fn ready() -> String {
    "ready".into()
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
    /// Also ask GitHub (`gh`) whether the workspace branch's PR was merged.
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

#[derive(Clone)]
pub struct SpawnOpts {
    pub name: String,
    pub base: Option<String>,
    pub exclude: Vec<String>,
    pub keep_secrets: bool,
    /// Defaults to the config value, else 2 GB.
    pub min_free_bytes: Option<u64>,
    /// Take a ready workspace from the pool when one matches.
    pub use_pool: bool,
}

pub struct LandOpts {
    pub name: String,
    pub message: String,
    pub branch: Option<String>,
    pub force_branch: bool,
    pub keep: bool,
    pub no_verify: bool,
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
        let top = self.dir.parent().context("no home")?;
        // Keep Spotlight from indexing every new tree, and Time Machine from
        // backing up clones at their full logical size. `tmutil` can take
        // ~10 s, so it runs detached instead of blocking the first spawn.
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
        check_name(&o.name)?;
        self.prepare_home()?;
        if o.use_pool && o.exclude.is_empty() && !o.keep_secrets {
            let head = self.resolve(None)?;
            if self.resolve(o.base.as_deref())? == head
                && let Some(mut s) = self.claim(&o.name, &self.pool_key(&head), t0)?
            {
                s.warnings = self.spawn_warnings();
                return Ok(s);
            }
        }
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
        };
        write_record(&rec, &meta, true)?;
        match self.create(o, &path, t0) {
            Ok(mut s) => {
                meta.base = s.base.clone();
                meta.state = ready();
                meta.pid = 0;
                meta.pid_start = 0;
                meta.notes = s.notes.clone();
                write_record(&rec, &meta, false)?;
                s.warnings = self.spawn_warnings();
                Ok(s)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&rec);
                Err(e)
            }
        }
    }

    /// Take a fresh pooled workspace and register it under `name`.
    fn claim(&self, name: &str, key: &str, t0: Instant) -> Result<Option<Spawned>> {
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
            let named = Meta {
                name: name.into(),
                created: now(),
                state: ready(),
                pid: 0,
                pid_start: 0,
                key: String::new(),
                ..m.clone()
            };
            if let Err(e) = write_record(&self.meta_path(name), &named, true) {
                let _ = std::fs::rename(&claim, &rec);
                return Err(e);
            }
            std::fs::remove_file(&claim)?;
            return Ok(Some(Spawned {
                name: name.into(),
                path: m.path,
                base: m.base,
                from_pool: true,
                clone_ms: 0,
                total_ms: t0.elapsed().as_millis(),
                clone_calls: 0,
                notes: m.notes,
                warnings: vec![],
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
                "{} and {} are on different volumes; clones need one APFS volume (set AGENT_WS_HOME)",
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
        let p = path.to_string_lossy().to_string();
        git::run(
            &self.root,
            &["worktree", "add", "--no-checkout", "--detach", &p, &base],
        )?;
        match self.fill(o, path) {
            Ok((clone_ms, clone_calls, notes)) => Ok(Spawned {
                name: o.name.clone(),
                path: path.to_path_buf(),
                base,
                from_pool: false,
                clone_ms,
                total_ms: t0.elapsed().as_millis(),
                clone_calls,
                notes,
                warnings: vec![],
            }),
            Err(e) => {
                let _ = self.discard(path);
                Err(e)
            }
        }
    }

    /// Clone the checkout into `path`, then make its tracked files equal the
    /// worktree's HEAD (the base commit).
    fn fill(&self, o: &SpawnOpts, path: &Path) -> Result<(u128, usize, Notes)> {
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
        git::run(path, &["reset", "-q"])?;
        git::run(path, &["checkout", "-q", "-f", "--", "."])?;
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
            })
            .map(String::from)
            .collect();
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
        if path.join(".gitmodules").exists() {
            isolate_submodules(path, &self.root, "HEAD")?;
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
        Ok((clone_ms, calls, notes))
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
        let opts = SpawnOpts {
            name: String::new(),
            base: None,
            exclude: vec![],
            keep_secrets: false,
            min_free_bytes: None,
            use_pool: false,
        };
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
        git::run(&self.root, &["worktree", "prune"])?;
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
        if !dry_run && self.dir.exists() {
            git::run(&self.root, &["worktree", "prune"])?;
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
        if gh
            && let Ok(branch) = git::line(&m.path, &["symbolic-ref", "-q", "--short", "HEAD"])
            && pr_merged_at(&self.root, &branch, head)
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
            match self.merged_reason(&m, &head, &base_commit, o.gh) {
                Some(reason) if o.merged_only && reason == "unused" => {
                    kept.push(keep("unused since spawn".into()))
                }
                Some(reason) => pruned.push(Pruned {
                    name: m.name.clone(),
                    path: m.path.clone(),
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
                let c = git::changed_paths(&m.path, &m.base)?;
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
        let overlaps = by_path.into_iter().filter(|(_, v)| v.len() > 1).collect();
        Ok(Status {
            workspaces,
            overlaps,
        })
    }

    pub fn land(&self, o: &LandOpts) -> Result<Landed> {
        let m = self.meta(&o.name)?;
        let files = git::changed_paths(&m.path, &m.base)?;
        if files.is_empty() {
            bail!("workspace {:?} has no changes", o.name);
        }
        let branch = o
            .branch
            .clone()
            .unwrap_or_else(|| format!("agent-ws/{}", o.name));
        let existing = git::line(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .ok();
        let head = git::line(&m.path, &["rev-parse", "HEAD"])?;
        let clean = git::run(&m.path, &["status", "--porcelain"])?
            .trim()
            .is_empty();
        // An earlier land created the branch at this workspace's HEAD and
        // stopped before removing the workspace: finish that land.
        let resume = existing.as_deref() == Some(head.as_str()) && clean && head != m.base;
        // An earlier `land --keep` of this workspace: its tip is an ancestor
        // of HEAD and carries this workspace's trailer, so moving the branch
        // forward loses nothing. Branches anyone else made are left alone.
        let advance = !resume
            && existing.as_deref().is_some_and(|tip| {
                let ancestor = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&m.path)
                    .args(["merge-base", "--is-ancestor", tip, &head])
                    .status()
                    .is_ok_and(|s| s.success());
                let landed_by = git::line(
                    &m.path,
                    &[
                        "log",
                        "-1",
                        "--format=%(trailers:key=Agent-Ws,valueonly)",
                        tip,
                    ],
                );
                ancestor && landed_by.is_ok_and(|n| n == o.name)
            });
        if existing.is_some() && !resume && !advance && !o.force_branch {
            bail!(
                "branch {branch} already exists and does not match this workspace; pass --branch NAME or --force-branch"
            );
        }
        let mut overlaps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (other, changed) in self.changes()? {
            if other.name != o.name {
                for p in changed.into_iter().filter(|p| files.contains(p)) {
                    overlaps.entry(p).or_default().push(other.name.clone());
                }
            }
        }
        let commit = if resume {
            head
        } else {
            git::run(&m.path, &["add", "-A"])?;
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
            let flag = if o.force_branch || advance {
                "-f"
            } else {
                "--no-track"
            };
            git::run(&self.root, &["branch", flag, &branch, &commit])?;
            commit
        };
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
        })
    }

    /// Remove a workspace. The tree is renamed into `.trash` (instant) and
    /// deleted by a detached `rm -rf`, which takes seconds for large trees.
    pub fn drop(&self, name: &str) -> Result<()> {
        let m = self.meta(name)?;
        self.discard(&m.path)?;
        std::fs::remove_file(self.meta_path(name))?;
        // Finish deletes that an earlier killed process left in the trash.
        self.sweep_trash(false)?;
        Ok(())
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

/// Give every checked-out submodule its own copy of its git dir, then check
/// out the commit `rev` records for it (recursively).
///
/// A cloned submodule has a `.git` file pointing at `../.git/modules/<name>`
/// relative to the source checkout, which does not exist in a workspace.
/// Pointing it back at the source's git dir would share its index and HEAD,
/// so the git dir is cloned instead (copy-on-write, like everything else).
fn isolate_submodules(ws_repo: &Path, src_repo: &Path, rev: &str) -> Result<()> {
    let tree = git::run(ws_repo, &["ls-tree", "-r", "-z", rev])?;
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
        git::run(&ws_sm, &["checkout", "-q", "-f", "--detach", sha])?;
        git::run(&ws_sm, &["clean", "-fdq"])?;
        isolate_submodules(&ws_sm, &src_sm, sha)?;
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

/// Start time of a process in microseconds since the epoch.
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
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || *libc::__error() == libc::EPERM }
}
