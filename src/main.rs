use agent_ws::claude::{self, Scope};
use agent_ws::{LandOpts, PruneOpts, Repo, SpawnOpts, SyncOpts};
use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::io::Read;

const SKILL_MD: &str = r#"---
name: agent-ws
description: Create, checkpoint, sync, inspect, land, and clean up isolated copy-on-write workspaces with agent-ws.
---

# agent-ws Quick Workflow

- **Create or reuse a workspace**:
  - `agent-ws spawn <NAME> --ensure --json` (idempotent: reuses existing ready workspace)
  - `agent-ws spawn <NAME> --auto --json` (picks `<NAME>`, `<NAME>-2`, ...)
  - `agent-ws spawn <NAME> --from <SRC_WS> --json` (forks edits, staged index, and untracked files from `<SRC_WS>`)
- **Inspect current context**:
  - `agent-ws whoami --json` (shows `inside_workspace`, `name`, `base`, `branch`, `port`)
  - `agent-ws whoami --status --json` (adds file `overlaps` and `conflicts`)
- **Checkpoints (instant CoW snapshots)**:
  - `agent-ws snap <WS> [SNAP] --json`
  - `agent-ws snaps <WS> --json`
  - `agent-ws restore <WS> [SNAP] --json` (latest if `SNAP` omitted)
  - `agent-ws snap <WS> <SNAP> --drop --json`
- **Sync & dependency refresh**:
  - `agent-ws sync <WS> [--base REV] [--merge] [--resync-deps] --json`
  - `agent-ws resync-deps <WS> [PATH ...] --json`
- **Inspect & finish**:
  - `agent-ws status [WS] --json`
  - `agent-ws diff <WS> [--stat] --json`
  - `agent-ws land <WS> -m "<msg>" [--allow-self] --json`
  - `agent-ws drop <WS> --if-exists [--allow-self] --json`
- **JSON errors**:
  - With `--json`, failures print `{"ok":false,"code":"<CODE>","error":"..."}` to `stderr` and exit `1`.
  - Codes: `ALREADY_EXISTS`, `NOT_FOUND`, `CONFLICT`, `DIRTY`, `UNTRUSTED_HOOKS`, `LOW_DISK`, `INVALID_INPUT`, `SELF_ACTION_BLOCKED`, `UNSUPPORTED_FS`, `GIT_ERROR`, `INTERNAL`.
"#;

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
        /// Clone from an existing workspace instead of the main checkout.
        #[arg(long, value_name = "WS", conflicts_with_all = ["base", "carry"])]
        from: Option<String>,
        /// Reuse an existing ready workspace instead of failing.
        #[arg(long, conflicts_with = "auto_name")]
        ensure: bool,
        /// Pick the first unused name (`NAME`, `NAME-2`, ..., `NAME-100`).
        #[arg(long = "auto")]
        auto_name: bool,
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
        /// Also keep the output in a log (`agent-ws log NAME`). On by default
        /// when stdout is not a terminal; interactive agents need one.
        #[arg(long)]
        log: bool,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Print the output of the last `exec --log` in a workspace.
    Log { name: String },
    /// List workspaces.
    List {
        /// Also age, port, changes, processes inside, last exec, real disk use.
        #[arg(short, long)]
        long: bool,
        /// With --long: skip the real disk use, which reads every file.
        #[arg(long, requires = "long")]
        no_disk: bool,
    },
    /// List every repository agent-ws keeps workspaces for.
    Repos,
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
        /// Allow landing even if your current directory is inside the workspace.
        #[arg(long)]
        allow_self: bool,
    },
    /// Remove a workspace without landing it.
    Drop {
        name: String,
        /// Succeed with dropped=false if the workspace does not exist.
        #[arg(long)]
        if_exists: bool,
        /// Allow dropping even if your current directory is inside the workspace.
        #[arg(long)]
        allow_self: bool,
    },
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
        /// Prune largest eligible workspaces (and pool if needed) until this much disk is free.
        #[arg(long)]
        target_free_gb: Option<u64>,
    },
    /// Real disk use per workspace: bytes not shared with any other file.
    Du {
        name: Option<String>,
        /// Show per-directory breakdown to this depth (0 = summary only).
        #[arg(long, default_value_t = 0)]
        depth: usize,
    },
    /// Rebase or merge a clean workspace onto an updated base commit.
    Sync {
        name: String,
        /// Target commit or branch (default: current repository HEAD).
        #[arg(long)]
        base: Option<String>,
        /// Merge instead of rebasing.
        #[arg(long)]
        merge: bool,
        /// Also CoW-refresh ignored dependency directories from the source checkout.
        #[arg(long)]
        resync_deps: bool,
    },
    /// Refresh ignored dependency directories from the source checkout.
    ResyncDeps {
        name: String,
        /// Relative directory paths to refresh (auto-detected if omitted).
        paths: Vec<String>,
    },
    /// Create or delete an instant CoW checkpoint of a workspace.
    Snap {
        name: String,
        /// Snapshot name (generated automatically if omitted).
        snap: Option<String>,
        /// Delete the named snapshot.
        #[arg(long, requires = "snap")]
        drop: bool,
    },
    /// List snapshots of a workspace.
    Snaps { name: String },
    /// Restore a workspace from a snapshot (latest if SNAP is omitted).
    Restore { name: String, snap: Option<String> },
    /// Identify the workspace containing the current directory.
    Whoami {
        /// Also report file overlaps and merge conflicts.
        #[arg(long)]
        status: bool,
    },
    /// Print a self-contained SKILL.md guide for coding agents.
    Skill,
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

/// Bytes as `512 B`, `14.2 MB`, `1.3 GB` (powers of 1000, like `df -H`).
fn human(b: u64) -> String {
    if b < 1_000 {
        return format!("{b} B");
    }
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
    let json = cli.json;
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if json {
                let code = agent_ws::error_code(&e);
                let payload = serde_json::json!({
                    "ok": false,
                    "code": code,
                    "error": format!("{e:#}"),
                });
                eprintln!("{payload}");
            } else {
                eprintln!("agent-ws: {e:#}");
            }
            std::process::exit(1);
        }
    }
}

/// Spawn a detached `agent-ws` background subcommand in its own process group,
/// reaping the child asynchronously so long-lived `exec --spawn` does not keep zombies.
fn spawn_background(repo: &Repo, args: &[&str]) {
    if let Ok(exe) = std::env::current_exe() {
        use std::os::unix::process::CommandExt;
        if let Ok(mut child) = std::process::Command::new(exe)
            .args(args)
            .current_dir(&repo.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Top the pool up in a detached process so the caller does not wait.
fn refill_in_background(repo: &Repo) {
    if repo.pool_target() == 0 || std::env::var_os("AGENT_WS_NO_REFILL").is_some() {
        return;
    }
    spawn_background(repo, &["pool", "fill"]);
}

/// Drop merged workspaces in a detached process, at most every 10 minutes,
/// so forgotten ones do not pile up. Never-used workspaces stay.
fn prune_in_background(repo: &Repo) {
    if std::env::var_os("AGENT_WS_NO_AUTO_PRUNE").is_some() || !repo.auto_prune_due() {
        return;
    }
    spawn_background(repo, &["prune", "--merged-only"]);
}

/// Runs a command; returns the process exit code.
fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Skill => {
            print!("{SKILL_MD}");
            Ok(0)
        }
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
        Cmd::Repos => print(cli.json, &agent_ws::known_repos()?, |repos| {
            for r in repos {
                let name = r.root.file_name().unwrap_or_default().to_string_lossy();
                println!("{name:20} {:>3}  {}", r.workspaces, r.root.display());
            }
        }),
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
                    if changed {
                        println!("added the agent-ws worktree hooks to {}", path.display());
                    } else {
                        println!("agent-ws hooks already in {}", path.display());
                    }
                    println!(
                        "Claude Code worktrees (--worktree, isolation: \"worktree\") now come from agent-ws; Claude Code runs `{}`, so `agent-ws` must be on its PATH",
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
                |_| {
                    if changed {
                        println!("removed the agent-ws hooks from {}", path.display());
                    } else {
                        println!("no agent-ws hooks in {}", path.display());
                    }
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
            .args(["check-ignore", "-q", "--"])
            .arg(path)
            .status()
            .is_ok_and(|s| s.success())
}

/// Claude Code's WorktreeCreate/WorktreeRemove hook. For WorktreeCreate,
/// stdout holds the workspace path and nothing else.
fn claude_hook() -> Result<i32> {
    const MAX_HOOK_INPUT: u64 = 1 << 20;
    let mut raw = String::new();
    let n = std::io::stdin()
        .take(MAX_HOOK_INPUT + 1)
        .read_to_string(&mut raw)?;
    if n as u64 > MAX_HOOK_INPUT {
        bail!("hook input from Claude Code exceeds 1 MiB limit");
    }
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
            let repo = match input.cwd.as_deref() {
                Some(cwd) => Repo::discover(cwd).or_else(|_| Repo::discover(&path)),
                None => Repo::discover(&path),
            };
            match repo {
                Ok(repo) => repo.claude_remove(&path)?,
                Err(_) if path.symlink_metadata().is_err() => return Ok(0),
                Err(e) => return Err(e),
            }
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
            from,
            ensure,
            auto_name,
        } => {
            let opts = SpawnOpts {
                name,
                base,
                exclude,
                keep_secrets,
                min_free_bytes: min_free_gb.map(|g| g.saturating_mul(1_000_000_000)),
                use_pool: !no_pool,
                carry,
                branch,
                no_hooks,
                from,
                ensure,
                auto_name,
                ..Default::default()
            };
            let s = repo.spawn(&opts)?;
            if !s.reused {
                refill_in_background(repo);
                prune_in_background(repo);
            }
            print(json, &s, |s| {
                println!("{}  {}", s.name, s.path.display());
                let how = if s.reused {
                    "reused".to_string()
                } else if s.from_pool {
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
            log,
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
            use std::io::IsTerminal;
            repo.exec(&name, &command, log || !std::io::stdout().is_terminal())
        }
        Cmd::Log { name } => match repo.run_log(&name, 1 << 20)? {
            Some(text) => print(
                json,
                &serde_json::json!({ "name": name, "log": text }),
                |_| {
                    print!("{text}");
                },
            ),
            None => Err(agent_ws::coded_err(
                agent_ws::ErrorCode::NotFound,
                format!("no log for {name:?}: run `agent-ws exec {name} --log -- …`"),
            )),
        },
        Cmd::List { long: false, .. } => print(json, &repo.list()?, |l| {
            for m in l {
                println!("{:20} {}  {}", m.name, short(&m.base), m.path.display());
            }
        }),
        Cmd::List {
            long: true,
            no_disk,
        } => print(json, &repo.list_long(!no_disk)?, |l| {
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
                        if parts.is_empty() {
                            "clean".into()
                        } else {
                            parts.join(" ")
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
                    w.private_bytes.map_or("-".into(), human)
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
            if s.base.starts_with('-') || s.tree.starts_with('-') {
                bail!("invalid snapshot revision");
            }
            let mut git = std::process::Command::new("git");
            git.arg("-C").arg(&s.path).arg("diff");
            if stat {
                git.arg("--stat");
            }
            let status = git
                .arg("--end-of-options")
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
            allow_self,
        } => {
            let cwd = std::env::current_dir().ok();
            let l = repo.land_from(
                &LandOpts {
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
                    allow_self,
                },
                cwd.as_deref(),
            )?;
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
        Cmd::Drop {
            name,
            if_exists,
            allow_self,
        } => {
            let cwd = std::env::current_dir().ok();
            let dropped = repo.drop_with_opts(&name, if_exists, allow_self, cwd.as_deref())?;
            print(
                json,
                &serde_json::json!({ "name": name, "dropped": dropped }),
                |_| {
                    if dropped {
                        println!("dropped {name}");
                    } else {
                        println!("workspace {name:?} does not exist");
                    }
                },
            )
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
            target_free_gb,
        } => print(
            json,
            &repo.prune(&PruneOpts {
                base,
                dry_run,
                gh,
                min_age_secs: min_age_min.saturating_mul(60),
                merged_only,
                target_free_bytes: target_free_gb.map(|g| g.saturating_mul(1_000_000_000)),
            })?,
            |p| {
                let verb = if dry_run { "would drop" } else { "dropped" };
                for w in &p.pruned {
                    let branch = if w.branch.is_empty() {
                        String::new()
                    } else {
                        format!(" (and branch {})", w.branch)
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
        Cmd::Du { name, depth } => print(
            json,
            &repo.du_with_options(name.as_deref(), depth, true)?,
            |d| {
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
                    for b in &e.breakdown {
                        println!(
                            "  {:>7} {:>9}    {}",
                            human(b.usage.private),
                            human(b.usage.allocated),
                            b.path.display()
                        );
                    }
                }
                println!(
                    "{:>9} {:>9}  total, {} free on disk",
                    human(d.total.private),
                    human(d.total.allocated),
                    human(d.free_bytes)
                );
                if d.cohort_shared_bytes > 0 || d.reclaimable_bytes > d.total.private {
                    println!(
                        "cohort shared {}  reclaimable {}",
                        human(d.cohort_shared_bytes),
                        human(d.reclaimable_bytes)
                    );
                }
                if let Some(budget) = d.budget_bytes {
                    println!("budget {}", human(budget));
                }
            },
        ),
        Cmd::Sync {
            name,
            base,
            merge,
            resync_deps,
        } => {
            let s = repo.sync(&SyncOpts {
                name,
                base,
                merge,
                resync_deps,
            })?;
            print(json, &s, |s| {
                println!(
                    "{}  {} {} -> {} (HEAD {})",
                    s.name,
                    s.mode,
                    short(&s.old_base),
                    short(&s.new_base),
                    short(&s.head)
                );
                if let Some(r) = &s.resync {
                    println!(
                        "resynced {} deps (freed {})",
                        r.resynced.len(),
                        human(r.freed_private_bytes)
                    );
                }
            })
        }
        Cmd::ResyncDeps { name, paths } => {
            let r = repo.resync_deps(&name, &paths)?;
            print(json, &r, |r| {
                if r.resynced.is_empty() {
                    println!("no dependency directories resynced for {}", r.name);
                } else {
                    println!(
                        "resynced {} for {} (freed {})",
                        r.resynced.join(", "),
                        r.name,
                        human(r.freed_private_bytes)
                    );
                }
            })
        }
        Cmd::Snap { name, snap, drop } => {
            if drop {
                let sname = snap.as_deref().context("--drop requires SNAP")?;
                repo.snap_drop(&name, sname)?;
                print(
                    json,
                    &serde_json::json!({ "workspace": name, "snap": sname, "dropped": true }),
                    |_| println!("dropped snapshot {sname} for {name}"),
                )
            } else {
                let info = repo.snap(&name, snap.as_deref())?;
                print(json, &info, |s| {
                    println!(
                        "{}  {} (HEAD {}  base {})",
                        s.name,
                        s.path.display(),
                        short(&s.head),
                        short(&s.base)
                    );
                })
            }
        }
        Cmd::Snaps { name } => print(json, &repo.snaps(&name)?, |list| {
            for s in list {
                println!("{:24} {}  {}", s.name, short(&s.head), s.path.display());
            }
        }),
        Cmd::Restore { name, snap } => {
            let info = repo.restore(&name, snap.as_deref())?;
            print(json, &info, |s| {
                println!(
                    "restored {} to {} (HEAD {})",
                    s.workspace,
                    s.name,
                    short(&s.head)
                );
            })
        }
        Cmd::Whoami { status } => {
            let cwd = std::env::current_dir()?;
            let w = repo.whoami(&cwd, status)?;
            print(json, &w, |w| {
                if let Some(name) = &w.name {
                    let mut line = name.to_string();
                    if let Some(base) = &w.base {
                        line.push_str(&format!("  base {}", short(base)));
                    }
                    if let Some(branch) = &w.branch {
                        line.push_str(&format!("  branch {branch}"));
                    }
                    if let Some(port) = w.port {
                        line.push_str(&format!("  port {port}"));
                    }
                    println!("{line}");
                    for (p, names) in &w.overlaps {
                        println!("OVERLAP {p}: {}", names.join(", "));
                    }
                    if let Some(conflicts) = &w.conflicts {
                        for c in conflicts {
                            println!(
                                "CONFLICT {}: {}",
                                c.workspaces.join(" + "),
                                c.paths.join(", ")
                            );
                        }
                    }
                } else {
                    println!("not inside a workspace (repo {})", w.repo_root.display());
                }
            })
        }
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
                        "{} ready (created {}, refreshed {}, removed {} stale, {} extra)",
                        f.ready, f.created, f.refreshed, f.removed_stale, f.removed_extra
                    );
                }
            }),
            PoolCmd::Status => print(json, &repo.pool_status()?, |s| {
                println!(
                    "target {}  fresh {}  refreshable {}  stale {}  creating {}",
                    s.target, s.fresh, s.refreshable, s.stale, s.creating
                );
            }),
            PoolCmd::Clear => print(json, &repo.pool_fill(Some(0))?, |f| {
                if f.busy {
                    println!("another fill is running");
                } else {
                    println!(
                        "removed {} pooled workspaces",
                        f.removed_stale + f.removed_extra
                    );
                }
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
        Cmd::Skill | Cmd::Completions { .. } | Cmd::Claude { .. } | Cmd::Repos => {
            unreachable!("handled in run")
        }
    }
}
