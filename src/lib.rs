//! agent-ws: instant, isolated workspaces for coding agents.
//!
//! A workspace is a Git worktree whose files are APFS clones of the source
//! checkout, ignored files included (`node_modules`, build caches). Spawning
//! costs metadata only, so agents skip `install` and start with warm caches.
//! The library holds all logic so other tools (e.g. `firman`) can embed it.

pub mod fsutil;
pub mod git;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Paths never cloned. `.claude/worktrees` holds other agents' checkouts.
pub const DEFAULT_EXCLUDES: &[&str] = &[".git", ".claude/worktrees"];

pub struct Repo {
    /// Main checkout of the repository.
    pub root: PathBuf,
    /// State and workspaces for this repository.
    pub dir: PathBuf,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Meta {
    pub name: String,
    pub path: PathBuf,
    /// Empty while the workspace is being created.
    pub base: String,
    pub created: u64,
    /// `creating` until spawn finishes, then `ready`.
    #[serde(default = "ready")]
    pub state: String,
    /// Process that is creating the workspace (0 once ready).
    #[serde(default)]
    pub pid: u32,
}

fn ready() -> String {
    "ready".into()
}

#[derive(Serialize, Default)]
pub struct Gc {
    /// Workspaces whose spawn died midway.
    pub interrupted: Vec<String>,
    /// Workspaces whose directory was deleted by hand.
    pub missing: Vec<String>,
    /// Directories under `ws/` with no workspace record.
    pub orphans: Vec<String>,
    /// Leftovers in `.trash` from interrupted background deletes.
    pub trash: usize,
}

#[derive(Serialize)]
pub struct Spawned {
    pub name: String,
    pub path: PathBuf,
    pub base: String,
    pub clone_ms: u128,
    pub total_ms: u128,
    pub clone_calls: usize,
    pub symlinks_retargeted: Vec<PathBuf>,
    /// Absolute symlinks pointing outside the source checkout. Writes through
    /// them leave the workspace.
    pub symlinks_external: Vec<ExternalLink>,
    pub secrets_removed: Vec<String>,
}

#[derive(Serialize)]
pub struct ExternalLink {
    pub path: PathBuf,
    pub target: PathBuf,
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
}

pub struct SpawnOpts {
    pub name: String,
    pub base: Option<String>,
    pub exclude: Vec<String>,
    pub keep_secrets: bool,
    pub min_free_bytes: u64,
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

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok {
        bail!("invalid workspace name {name:?}: use letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

impl Repo {
    /// Find the repository containing `cwd`. Works from inside a workspace too.
    pub fn discover(cwd: &Path) -> Result<Repo> {
        let common = git::line(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .context("not inside a Git repository")?;
        let common = PathBuf::from(common);
        if common.file_name().and_then(|n| n.to_str()) != Some(".git") {
            bail!("bare repositories are not supported ({})", common.display());
        }
        let root = common.parent().context("no repository root")?.to_path_buf();
        let base = root.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
        let dir = home()?.join(format!("{base}-{:08x}", fnv(&root.to_string_lossy())));
        Ok(Repo { root, dir })
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.dir.join("meta").join(format!("{name}.json"))
    }

    pub fn meta(&self, name: &str) -> Result<Meta> {
        let p = self.meta_path(name);
        let raw = std::fs::read(&p).with_context(|| format!("no workspace named {name:?}"))?;
        let m: Meta = serde_json::from_slice(&raw)?;
        if m.state != "ready" {
            bail!("workspace {name:?} is still being created (run `agent-ws gc` if its spawn died)");
        }
        Ok(m)
    }

    /// Workspaces that finished spawning.
    pub fn list(&self) -> Result<Vec<Meta>> {
        Ok(self.records()?.into_iter().filter(|m| m.state == "ready").collect())
    }

    /// All workspace records, including ones still being created.
    fn records(&self) -> Result<Vec<Meta>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.dir.join("meta")) else { return Ok(out) };
        for e in rd {
            let p = e?.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                // A record can vanish between read_dir and read when another
                // process drops it.
                let Ok(raw) = std::fs::read(&p) else { continue };
                out.push(serde_json::from_slice(&raw)?);
            }
        }
        out.sort_by(|a: &Meta, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn write_meta(&self, m: &Meta, create_new: bool) -> Result<()> {
        let p = self.meta_path(&m.name);
        let raw = serde_json::to_vec_pretty(m)?;
        if create_new {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::AlreadyExists => anyhow::anyhow!("workspace {:?} already exists", m.name),
                    _ => e.into(),
                })?;
            f.write_all(&raw)?;
        } else {
            // Write then rename so readers never see a half-written record.
            let tmp = p.with_extension(format!("json.{}", std::process::id()));
            std::fs::write(&tmp, raw)?;
            std::fs::rename(tmp, p)?;
        }
        Ok(())
    }

    fn prepare_home(&self) -> Result<()> {
        for d in ["meta", "ws", ".trash"] {
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

    pub fn spawn(&self, o: &SpawnOpts) -> Result<Spawned> {
        let t0 = Instant::now();
        check_name(&o.name)?;
        self.prepare_home()?;
        let path = self.dir.join("ws").join(&o.name);
        // Claim the name first: the record marks the directory as in use, so
        // `gc` leaves it alone while the owning process is alive.
        let mut meta = Meta {
            name: o.name.clone(),
            path: path.clone(),
            base: String::new(),
            created: now(),
            state: "creating".into(),
            pid: std::process::id(),
        };
        self.write_meta(&meta, true)?;
        let result = self.create(o, &path, t0);
        match result {
            Ok(s) => {
                meta.base = s.base.clone();
                meta.state = ready();
                meta.pid = 0;
                self.write_meta(&meta, false)?;
                Ok(s)
            }
            Err(e) => {
                let _ = std::fs::remove_file(self.meta_path(&o.name));
                Err(e)
            }
        }
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
        let free = fsutil::free_bytes(&self.dir)?;
        if free < o.min_free_bytes {
            bail!("only {} MB free on disk (minimum {} MB)", free / 1_000_000, o.min_free_bytes / 1_000_000);
        }
        let rev = format!("{}^{{commit}}", o.base.as_deref().unwrap_or("HEAD"));
        let base = git::line(&self.root, &["rev-parse", "--verify", &rev])?;

        let p = path.to_string_lossy().to_string();
        git::run(&self.root, &["worktree", "add", "--no-checkout", "--detach", &p, &base])?;
        let result = self.fill(o, path);
        match result {
            Ok((clone_ms, clone_calls, symlinks, secrets)) => {
                Ok(Spawned {
                    name: o.name.clone(),
                    path: path.to_path_buf(),
                    base,
                    clone_ms,
                    total_ms: t0.elapsed().as_millis(),
                    clone_calls,
                    symlinks_retargeted: symlinks.retargeted,
                    symlinks_external: symlinks
                        .external
                        .into_iter()
                        .map(|(path, target)| ExternalLink { path, target })
                        .collect(),
                    secrets_removed: secrets,
                })
            }
            Err(e) => {
                let _ = self.discard(path);
                Err(e)
            }
        }
    }

    /// Move a workspace tree into `.trash`, delete it in the background and
    /// forget its worktree registration.
    fn discard(&self, path: &Path) -> Result<()> {
        if path.exists() {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("ws");
            let trash = self.dir.join(".trash").join(format!("{name}-{nanos}"));
            std::fs::create_dir_all(self.dir.join(".trash"))?;
            std::fs::rename(path, &trash)?;
            delete_in_background(&trash)?;
        }
        git::run(&self.root, &["worktree", "prune"])?;
        Ok(())
    }

    /// Clean up after crashes and manual deletes. Never touches a workspace
    /// whose spawning process is still alive.
    pub fn gc(&self, dry_run: bool) -> Result<Gc> {
        let mut report = Gc::default();
        let mut known = std::collections::HashSet::new();
        for m in self.records()? {
            known.insert(m.name.clone());
            let dead = m.state != "ready" && !pid_alive(m.pid);
            let missing = m.state == "ready" && !m.path.exists();
            if !(dead || missing) {
                continue;
            }
            if dead { report.interrupted.push(m.name.clone()) } else { report.missing.push(m.name.clone()) }
            if !dry_run {
                self.discard(&m.path)?;
                let _ = std::fs::remove_file(self.meta_path(&m.name));
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.dir.join("ws")) {
            for e in rd {
                let e = e?;
                let name = e.file_name().to_string_lossy().into_owned();
                // Re-check the record: a spawn may have claimed it after the scan.
                if known.contains(&name) || self.meta_path(&name).exists() {
                    continue;
                }
                report.orphans.push(name);
                if !dry_run {
                    self.discard(&e.path())?;
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.dir.join(".trash")) {
            let now_ns = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            for e in rd {
                let e = e?;
                // Entries are named `<name>-<nanos>`. Recent ones are still
                // being deleted by their own background `rm`.
                let name = e.file_name().to_string_lossy().into_owned();
                let born = name.rsplit('-').next().and_then(|n| n.parse::<u128>().ok()).unwrap_or(0);
                if now_ns.saturating_sub(born) < 600 * 1_000_000_000 {
                    continue;
                }
                report.trash += 1;
                if !dry_run {
                    delete_in_background(&e.path())?;
                }
            }
        }
        if !dry_run && self.dir.exists() {
            git::run(&self.root, &["worktree", "prune"])?;
        }
        report.orphans.sort();
        Ok(report)
    }

    /// Clone the checkout into `path`, then make its tracked files equal the
    /// worktree's HEAD (the base commit).
    fn fill(&self, o: &SpawnOpts, path: &Path) -> Result<(u128, usize, fsutil::Symlinks, Vec<String>)> {
        let mut excludes: Vec<PathBuf> = DEFAULT_EXCLUDES.iter().map(PathBuf::from).collect();
        excludes.extend(o.exclude.iter().map(PathBuf::from));
        // Never clone the agent-ws home if it lives inside the repository.
        if let Ok(rel) = self.dir.strip_prefix(&self.root) {
            excludes.push(rel.components().take(1).collect());
        }
        let t = Instant::now();
        let calls = fsutil::clone_tree(&self.root, path, Path::new(""), &excludes)?;
        let clone_ms = t.elapsed().as_millis();

        // Index from `base`, then drop the source's uncommitted edits and
        // untracked files. Ignored files (deps, caches) stay.
        git::run(path, &["reset", "-q"])?;
        git::run(path, &["checkout", "-q", "-f", "--", "."])?;
        git::run(path, &["clean", "-fdq"])?;

        let mut secrets = Vec::new();
        if !o.keep_secrets {
            let ignored = git::run(path, &["ls-files", "-o", "-i", "--exclude-standard", "--directory", "-z"])?;
            for rel in ignored.split('\0').filter(|s| !s.is_empty() && !s.ends_with('/')) {
                let name = Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or("");
                if fsutil::is_secret(name) {
                    std::fs::remove_file(path.join(rel))?;
                    secrets.push(rel.to_string());
                }
            }
        }
        let symlinks = fsutil::retarget_symlinks(&self.root, path)?;
        Ok((clone_ms, calls, symlinks, secrets))
    }

    pub fn status(&self, only: Option<&str>) -> Result<Status> {
        // Overlaps are computed across all workspaces, even when one is selected.
        let mut all = Vec::new();
        let mut by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for m in self.list()? {
            // Deleted by hand; `agent-ws gc` reports and forgets it.
            if !m.path.exists() {
                continue;
            }
            let changed = git::changed_paths(&m.path, &m.base)?;
            for p in &changed {
                by_path.entry(p.clone()).or_default().push(m.name.clone());
            }
            all.push(WsStatus { name: m.name, path: m.path, base: m.base, changed });
        }
        let workspaces: Vec<WsStatus> = all.into_iter().filter(|w| only.is_none_or(|n| n == w.name)).collect();
        if let Some(n) = only
            && workspaces.is_empty()
        {
            bail!("no workspace named {n:?}");
        }
        let overlaps = by_path.into_iter().filter(|(_, v)| v.len() > 1).collect();
        Ok(Status { workspaces, overlaps })
    }

    pub fn land(&self, o: &LandOpts) -> Result<Landed> {
        let m = self.meta(&o.name)?;
        let files = git::changed_paths(&m.path, &m.base)?;
        if files.is_empty() {
            bail!("workspace {:?} has no changes", o.name);
        }
        let branch = o.branch.clone().unwrap_or_else(|| format!("agent-ws/{}", o.name));
        if !o.force_branch && git::run(&self.root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
            bail!("branch {branch} already exists (use --force-branch)");
        }
        git::run(&m.path, &["add", "-A"])?;
        let staged = std::process::Command::new("git")
            .arg("-C")
            .arg(&m.path)
            .args(["diff", "--cached", "--quiet"])
            .status()?;
        if !staged.success() {
            let ws_trailer = format!("Agent-Ws: {}", o.name);
            let base_trailer = format!("Agent-Ws-Base: {}", m.base);
            let mut args = vec!["commit", "-q", "-m", &o.message, "--trailer", &ws_trailer, "--trailer", &base_trailer];
            if o.no_verify {
                args.push("--no-verify");
            }
            git::run(&m.path, &args)?;
        }
        let commit = git::line(&m.path, &["rev-parse", "HEAD"])?;
        let flag = if o.force_branch { "-f" } else { "--no-track" };
        git::run(&self.root, &["branch", flag, &branch, &commit])?;
        if !o.keep {
            self.drop(&o.name)?;
        }
        Ok(Landed { name: o.name.clone(), branch, commit, files: files.len(), dropped: !o.keep })
    }

    /// Remove a workspace. The tree is renamed into `.trash` (instant) and
    /// deleted by a detached `rm -rf`, which takes seconds for large trees.
    pub fn drop(&self, name: &str) -> Result<()> {
        let m = self.meta(name)?;
        self.discard(&m.path)?;
        std::fs::remove_file(self.meta_path(name))?;
        Ok(())
    }
}

fn delete_in_background(path: &Path) -> Result<()> {
    std::process::Command::new("rm")
        .arg("-rf")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Signal 0 checks existence; EPERM means it exists under another user.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || *libc::__error() == libc::EPERM }
}
