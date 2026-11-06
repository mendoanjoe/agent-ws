use agent_ws::claude::{self, Scope};
use agent_ws::{LandOpts, PruneOpts, Repo, SpawnOpts};
use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::io::Read;

/// Instant, isolated workspaces for coding agents (copy-on-write clones + Git worktrees).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a workspace from the current checkout (deps and caches included).
    Spawn {
        name: String,
        /// Commit to start from (default HEAD).
        #[arg(long)]
        base: Option<String>,
        /// Extra paths to skip, relative to the repository root.
        #[arg(long)]
        exclude: Vec<String>,
        /// Keep secret files (see `secrets` in .agent-ws.toml).
        #[arg(long)]
        keep_secrets: bool,
        /// Refuse to spawn below this much free disk space (default 2).
        #[arg(long)]
        min_free_gb: Option<u64>,
        /// Always clone fresh instead of taking a pooled workspace.
        #[arg(long)]
        no_pool: bool,
        /// Bring your uncommitted edits and untracked files along (base: HEAD).
        #[arg(long)]
        carry: bool,
        /// Create this branch and check it out in the workspace (default:
        /// detached HEAD).
        #[arg(long)]
        branch: Option<String>,
        /// Skip the post_spawn hooks of .agent-ws.toml.
        #[arg(long)]
        no_hooks: bool,
    },
    /// Run a command in a workspace, with PORT and AGENT_WS_* set.
    Exec {
        name: String,
        /// Spawn the workspace first if it does not exist.
        #[arg(long)]
        spawn: bool,
        /// With --spawn: commit to start from (default HEAD).
        #[arg(long, requires = "spawn")]
        base: Option<String>,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List workspaces.
    List {
        /// Also age, port, changes, processes inside, last exec, real disk use.
        #[arg(short, long)]
        long: bool,
    },
    /// Show changed files per workspace, files changed in more than one, and
    /// which of those would conflict when merged.
    Status { name: Option<String> },
    /// Show what a workspace changed since its base, new files included.
    Diff {
        name: String,
        /// Changed files with line counts instead of the patch.
        #[arg(long)]
        stat: bool,
        /// Limit the diff to these paths.
        paths: Vec<String>,
    },
    /// Print a workspace path (for `cd "$(agent-ws path NAME)"`).
    Path { name: String },
    /// Commit the workspace changes to a branch, then remove the workspace.
    Land {
        name: String,
        #[arg(short, long)]
        message: String,
        /// Branch to create (default agent-ws/NAME, or the workspace's own
        /// branch from `spawn --branch`).
        #[arg(long)]
        branch: Option<String>,
        /// Overwrite the branch if it exists.
        #[arg(long)]
        force_branch: bool,
        /// Keep the workspace after landing.
        #[arg(long)]
        keep: bool,
        /// Skip Git's commit hooks and the pre_land hooks.
        #[arg(long)]
        no_verify: bool,
        /// One commit with everything since the base.
        #[arg(long)]
        squash: bool,
        /// Push the branch to origin and open a GitHub pull request (`gh`).
        #[arg(long)]
        pr: bool,
        /// Branch the pull request targets (default: the repository's default).
        #[arg(long, requires = "pr")]
        pr_base: Option<String>,
        /// Open the pull request as a draft.
        #[arg(long, requires = "pr")]
        draft: bool,
    },
    /// Remove a workspace without landing it.
    Drop { name: String },
    /// Clean up after interrupted spawns, deleted folders and stale trash.
    Gc {
        /// Report only; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Drop workspaces whose work is already merged (clean tree only).
    Prune {
        /// Branch the work must be in (default: prune_base, origin/HEAD,
        /// origin/main, origin/master, main, master).
        #[arg(long)]
        base: Option<String>,
        /// Report what would be dropped and the space it frees; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Also count a workspace branch whose GitHub PR was merged at HEAD.
        #[arg(long)]
        gh: bool,
        /// Leave workspaces younger than this many minutes.
        #[arg(long, default_value_t = agent_ws::PRUNE_MIN_AGE_SECS / 60)]
        min_age_min: u64,
        /// Keep workspaces that were never used (the automatic prune).
        #[arg(long, hide = true)]
        merged_only: bool,
    },
    /// Real disk use per workspace: bytes not shared with any other file.
    Du { name: Option<String> },
    /// Keep ready workspaces so spawning takes milliseconds.
    Pool {
        #[command(subcommand)]
        cmd: PoolCmd,
    },
    /// Trust the hooks in .agent-ws.toml (again after they change).
    Trust,
    /// Make Claude Code create its worktrees with agent-ws.
    Claude {
        #[command(subcommand)]
        cmd: ClaudeCmd,
    },
    /// Print a shell completion script.
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand)]
enum PoolCmd {
    /// Set how many ready workspaces to keep (0 disables the pool).
    Set { size: usize },
    /// Create fresh and remove stale pooled workspaces now.
    Fill {
        /// Fill to this size instead of the configured target.
        #[arg(long)]
        size: Option<usize>,
    },
    /// Show target, fresh and stale pooled workspaces.
    Status,
    /// Remove every pooled workspace (the target is kept).
    Clear,
}

#[derive(Subcommand)]
enum ClaudeCmd {
    /// Add agent-ws as Claude Code's WorktreeCreate and WorktreeRemove hook.
    Install {
        #[arg(long, value_enum, default_value_t = ScopeArg::Local)]
        scope: ScopeArg,
    },
    /// Remove those hooks.
    Uninstall {
        #[arg(long, value_enum, default_value_t = ScopeArg::Local)]
        scope: ScopeArg,
    },
    /// The hook Claude Code runs, with its JSON on stdin.
    Hook,
}

/// Which Claude Code settings file to change.
#[derive(Clone, Copy, ValueEnum)]
enum ScopeArg {
    /// .claude/settings.local.json: this repository, only you.
    Local,
    /// .claude/settings.json: this repository, everyone who clones it.
    Project,
    /// ~/.claude/settings.json: every repository.
    User,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Scope {
        match s {
            ScopeArg::Local => Scope::Local,
            ScopeArg::Project => Scope::Project,
            ScopeArg::User => Scope::User,
        }
    }
}

/// First 12 characters of a commit id; records are editable files, so a
/// shorter value must not panic.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// Bytes as `812 KB`, `14.2 MB`, `1.3 GB` (powers of 1000, like `df -H`).
fn human(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

/// Seconds as `45s`, `12m`, `3h`, `2d`.
fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn print<T: serde::Serialize>(json: bool, v: &T, human: impl FnOnce(&T)) -> Result<i32> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        human(v);
    }
    Ok(0)
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("agent-ws: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Top the pool up in a detached process so the caller does not wait.
fn refill_in_background(repo: &Repo) {
    if repo.pool_target() == 0 || std::env::var_os("AGENT_WS_NO_REFILL").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(["pool", "fill"])
            .current_dir(&repo.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Drop merged workspaces in a detached process, at most every 10 minutes,
/// so forgotten ones do not pile up. Never-used workspaces stay.
fn prune_in_background(repo: &Repo) {
    if std::env::var_os("AGENT_WS_NO_AUTO_PRUNE").is_some() || !repo.auto_prune_due() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        use std::os::unix::process::CommandExt;
        let _ = std::process::Command::new(exe)
            .args(["prune", "--merged-only"])
            .current_dir(&repo.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn();
    }
}

/// Runs a command; returns the process exit code.
fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "agent-ws",
                &mut std::io::stdout(),
            );
            Ok(0)
        }
        Cmd::Claude { cmd } => claude_cmd(cli.json, cmd),
        cmd => {
            let repo = Repo::discover(&std::env::current_dir()?)?;
            repo_cmd(cli.json, &repo, cmd)
        }
    }
}

fn claude_cmd(json: bool, cmd: ClaudeCmd) -> Result<i32> {
    let settings = |scope: ScopeArg| {
        let root = Repo::discover(&std::env::current_dir()?)
            .ok()
            .map(|r| r.root);
        claude::settings_path(root.as_deref(), scope.into())
    };
    match cmd {
        ClaudeCmd::Hook => claude_hook(),
        ClaudeCmd::Install { scope } => {
            let path = settings(scope)?;
            let changed = claude::install(&path)?;
            print(
                json,
                &serde_json::json!({ "settings": path, "changed": changed }),
                |_| {
                    match changed {
                        true => println!("added the agent-ws worktree hooks to {}", path.display()),
                        false => println!("agent-ws hooks already in {}", path.display()),
                    }
                    println!(
                        "Claude Code worktrees (--worktree, isolation: \"worktree\") now come from agent-ws; it needs `{}` on its PATH",
                        claude::HOOK_COMMAND
                    );
                    if matches!(scope, ScopeArg::Local) && !git_ignores(&path) {
                        println!("note: {} is not ignored by Git", path.display());
                    }
                },
            )
        }
        ClaudeCmd::Uninstall { scope } => {
            let path = settings(scope)?;
            let changed = claude::uninstall(&path)?;
            print(
                json,
                &serde_json::json!({ "settings": path, "changed": changed }),
                |_| match changed {
                    true => println!("removed the agent-ws hooks from {}", path.display()),
                    false => println!("no agent-ws hooks in {}", path.display()),
                },
            )
        }
    }
}

/// Whether Git ignores `path` in its repository.
fn git_ignores(path: &std::path::Path) -> bool {
    let dir = path.parent().unwrap_or(path);
    std::fs::create_dir_all(dir).is_ok()
        && std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["check-ignore", "-q"])
            .arg(path)
            .status()
            .is_ok_and(|s| s.success())
}

/// Claude Code's WorktreeCreate/WorktreeRemove hook. For WorktreeCreate,
/// stdout holds the workspace path and nothing else.
fn claude_hook() -> Result<i32> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let input: claude::HookInput =
        serde_json::from_str(&raw).context("read the hook input from Claude Code")?;
    match input.hook_event_name.as_str() {
        "WorktreeCreate" => {
            let cwd = match input.cwd {
                Some(cwd) => cwd,
                None => std::env::current_dir()?,
            };
            let repo = Repo::discover(&cwd)?;
            let s = repo.claude_create(input.name.as_deref().unwrap_or("claude"))?;
            refill_in_background(&repo);
            prune_in_background(&repo);
            for w in &s.warnings {
                eprintln!("agent-ws: WARNING {w}");
            }
            println!("{}", s.path.display());
            Ok(0)
        }
        "WorktreeRemove" => {
            let path = input
                .worktree_path
                .context("WorktreeRemove input without worktree_path")?;
            // Already gone counts as removed.
            if path.symlink_metadata().is_err() {
                return Ok(0);
            }
            Repo::discover(&path)?.claude_remove(&path)?;
            Ok(0)
        }
        other => {
            bail!("`agent-ws claude hook` handles WorktreeCreate and WorktreeRemove, not {other}")
        }
    }
}

fn repo_cmd(json: bool, repo: &Repo, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Spawn {
            name,
            base,
            exclude,
            keep_secrets,
            min_free_gb,
            no_pool,
            carry,
            branch,
            no_hooks,
        } => {
            let opts = SpawnOpts {
                name,
                base,
                exclude,
                keep_secrets,
                min_free_bytes: min_free_gb.map(|g| g * 1_000_000_000),
                use_pool: !no_pool,
                carry,
                branch,
                no_hooks,
                ..Default::default()
            };
            let s = repo.spawn(&opts)?;
            refill_in_background(repo);
            prune_in_background(repo);
            print(json, &s, |s| {
                println!("{}  {}", s.name, s.path.display());
                let how = if s.from_pool {
                    "from pool".to_string()
                } else {
                    format!("clone {} ms", s.clone_ms)
                };
                let mut line = format!("base {}  {how}  total {} ms", short(&s.base), s.total_ms);
                if let Some(p) = s.port {
                    line.push_str(&format!("  port {p}"));
                }
                if !s.branch.is_empty() {
                    line.push_str(&format!("  branch {}", s.branch));
                }
                println!("{line}");
                if !s.carried.is_empty() {
                    println!("carried {} changed files", s.carried.len());
                }
                for p in &s.notes.symlinks_retargeted {
                    println!("retargeted symlink {}", p.display());
                }
                for l in &s.notes.symlinks_external {
                    println!(
                        "WARNING symlink {} -> {} points outside the workspace",
                        l.path.display(),
                        l.target.display()
                    );
                }
                for p in &s.notes.secrets_removed {
                    println!("removed secret {p}");
                }
                for w in &s.warnings {
                    println!("WARNING {w}");
                }
            })
        }
        Cmd::Exec {
            name,
            spawn,
            base,
            command,
        } => {
            if spawn && !repo.exists(&name) {
                let s = repo.spawn(&SpawnOpts {
                    name: name.clone(),
                    base,
                    use_pool: true,
                    ..Default::default()
                })?;
                refill_in_background(repo);
                prune_in_background(repo);
                eprintln!("agent-ws: spawned {}  {}", s.name, s.path.display());
            }
            repo.exec(&name, &command)
        }
        Cmd::List { long: false } => print(json, &repo.list()?, |l| {
            for m in l {
                println!("{:20} {}  {}", m.name, short(&m.base), m.path.display());
            }
        }),
        Cmd::List { long: true } => print(json, &repo.list_long()?, |l| {
            println!(
                "{:20} {:>4} {:>5}  {:10} {:>5}  {:12} {:>9}",
                "NAME", "AGE", "PORT", "CHANGES", "PROCS", "LAST EXEC", "REAL"
            );
            for w in l {
                let changes = match (w.dirty, w.ahead) {
                    (None, _) => "?".to_string(),
                    (Some(dirty), ahead) => {
                        let mut parts = Vec::new();
                        if dirty {
                            parts.push("dirty".to_string());
                        }
                        if let Some(n) = ahead.filter(|n| *n > 0) {
                            parts.push(format!("+{n}"));
                        }
                        match parts.is_empty() {
                            true => "clean".into(),
                            false => parts.join(" "),
                        }
                    }
                };
                let run = match &w.run {
                    None => "-".to_string(),
                    Some(r) if r.state == "exited" => {
                        format!("exit {}", r.run.exit_code.unwrap_or(-1))
                    }
                    Some(r) => r.state.clone(),
                };
                let port = w.port.map_or("-".into(), |p| p.to_string());
                println!(
                    "{:20} {:>4} {:>5}  {:10} {:>5}  {:12} {:>9}",
                    w.meta.name,
                    age(w.age_secs),
                    port,
                    changes,
                    w.processes.len(),
                    run,
                    human(w.private_bytes)
                );
            }
        }),
        Cmd::Status { name } => print(json, &repo.status(name.as_deref())?, |s| {
            for w in &s.workspaces {
                println!("{} ({} changed)", w.name, w.changed.len());
                for p in &w.changed {
                    println!("  {p}");
                }
            }
            for (p, names) in &s.overlaps {
                println!("OVERLAP {p}: {}", names.join(", "));
            }
            match &s.conflicts {
                Some(conflicts) => {
                    for c in conflicts {
                        println!(
                            "CONFLICT {}: {}",
                            c.workspaces.join(" + "),
                            c.paths.join(", ")
                        );
                    }
                }
                None if !s.overlaps.is_empty() => {
                    println!("(conflict check needs Git 2.38 or later)")
                }
                None => {}
            }
        }),
        Cmd::Diff { name, stat, paths } => {
            if json {
                return print(true, &repo.diff_files(&name, &paths)?, |_| {});
            }
            // Git pages and colours the patch as it does for `git diff`.
            let s = repo.snapshot(&name)?;
            let mut git = std::process::Command::new("git");
            git.arg("-C").arg(&s.path).arg("diff");
            if stat {
                git.arg("--stat");
            }
            let status = git
                .arg(&s.base)
                .arg(&s.tree)
                .arg("--")
                .args(&paths)
                .status()?;
            if !status.success() {
                bail!("git diff failed ({status})");
            }
            Ok(0)
        }
        Cmd::Path { name } => {
            let path = repo.meta(&name)?.path;
            print(json, &serde_json::json!({ "path": path }), |_| {
                println!("{}", path.display())
            })
        }
        Cmd::Land {
            name,
            message,
            branch,
            force_branch,
            keep,
            no_verify,
            squash,
            pr,
            pr_base,
            draft,
        } => {
            let l = repo.land(&LandOpts {
                name,
                message,
                branch,
                force_branch,
                keep,
                no_verify,
                squash,
                pr,
                pr_base,
                draft,
            })?;
            print(json, &l, |l| {
                println!(
                    "{} -> {} {} ({} files){}",
                    l.name,
                    l.branch,
                    short(&l.commit),
                    l.files,
                    if l.dropped { ", dropped" } else { "" }
                );
                if let Some(url) = &l.pr_url {
                    println!("pull request {url}");
                }
                for (p, names) in &l.overlaps {
                    println!("OVERLAP {p}: also changed in {}", names.join(", "));
                }
            })
        }
        Cmd::Drop { name } => {
            repo.drop(&name)?;
            print(json, &serde_json::json!({ "dropped": name }), |_| {
                println!("dropped {name}")
            })
        }
        Cmd::Gc { dry_run } => print(json, &repo.gc(dry_run)?, |g| {
            let verb = if dry_run { "would remove" } else { "removed" };
            for n in &g.interrupted {
                println!("{verb} interrupted spawn {n}");
            }
            for n in &g.missing {
                println!("{verb} record of deleted workspace {n}");
            }
            for n in &g.orphans {
                println!("{verb} orphan folder {n}");
            }
            for n in &g.adopted {
                let how = if dry_run {
                    "would register"
                } else {
                    "registered"
                };
                println!(
                    "{how} orphan folder {n} again: it holds work (see `agent-ws status {n}`)"
                );
            }
            for n in &g.unreadable {
                println!("left orphan folder {n}: Git cannot read it; check it by hand");
            }
            if g.claims > 0 {
                println!("{verb} {} dead pool claims", g.claims);
            }
            if g.trash > 0 {
                println!("{verb} {} trash entries", g.trash);
            }
            if g.interrupted.is_empty()
                && g.missing.is_empty()
                && g.orphans.is_empty()
                && g.adopted.is_empty()
                && g.unreadable.is_empty()
                && g.claims == 0
                && g.trash == 0
            {
                println!("nothing to clean");
            }
        }),
        Cmd::Prune {
            base,
            dry_run,
            gh,
            min_age_min,
            merged_only,
        } => print(
            json,
            &repo.prune(&PruneOpts {
                base,
                dry_run,
                gh,
                min_age_secs: min_age_min * 60,
                merged_only,
            })?,
            |p| {
                let verb = if dry_run { "would drop" } else { "dropped" };
                for w in &p.pruned {
                    let branch = match w.branch.is_empty() {
                        true => String::new(),
                        false => format!(" (and branch {})", w.branch),
                    };
                    println!(
                        "{verb} {:20} {:>9}  {}{branch}",
                        w.name,
                        human(w.private_bytes),
                        w.reason
                    );
                }
                for k in &p.kept {
                    println!("keep {:25} {}", k.name, k.why);
                }
                println!(
                    "{} {} workspaces merged into {} ({}), frees at least {}",
                    verb,
                    p.pruned.len(),
                    p.base,
                    short(&p.base_commit),
                    human(p.freed_bytes)
                );
            },
        ),
        Cmd::Du { name } => print(json, &repo.du(name.as_deref())?, |d| {
            println!("{:>9} {:>9}  NAME", "REAL", "DU");
            for e in &d.entries {
                let kind = if e.kind == "workspace" {
                    String::new()
                } else {
                    format!(" ({})", e.kind)
                };
                println!(
                    "{:>9} {:>9}  {}{kind}",
                    human(e.usage.private),
                    human(e.usage.allocated),
                    e.name
                );
            }
            println!(
                "{:>9} {:>9}  total, {} free on disk",
                human(d.total.private),
                human(d.total.allocated),
                human(d.free_bytes)
            );
        }),
        Cmd::Pool { cmd } => match cmd {
            PoolCmd::Set { size } => {
                repo.pool_set(size)?;
                print(json, &serde_json::json!({ "target": size }), |_| {
                    println!("pool target {size}")
                })
            }
            PoolCmd::Fill { size } => print(json, &repo.pool_fill(size)?, |f| {
                if f.busy {
                    println!("another fill is running");
                } else {
                    println!(
                        "{} ready (created {}, removed {} stale, {} extra)",
                        f.ready, f.created, f.removed_stale, f.removed_extra
                    );
                }
            }),
            PoolCmd::Status => print(json, &repo.pool_status()?, |s| {
                println!(
                    "target {}  fresh {}  stale {}  creating {}",
                    s.target, s.fresh, s.stale, s.creating
                );
            }),
            PoolCmd::Clear => print(json, &repo.pool_fill(Some(0))?, |f| {
                println!(
                    "removed {} pooled workspaces",
                    f.removed_stale + f.removed_extra
                );
            }),
        },
        Cmd::Trust => print(json, &repo.trust()?, |h| {
            if h.is_empty() {
                println!("no hooks in .agent-ws.toml");
            }
            for c in &h.post_spawn {
                println!("trusted post_spawn: {c}");
            }
            for c in &h.pre_land {
                println!("trusted pre_land: {c}");
            }
        }),
        Cmd::Completions { .. } | Cmd::Claude { .. } => unreachable!("handled in run"),
    }
}
