use agent_ws::{LandOpts, Repo, SpawnOpts};
use anyhow::Result;
use clap::{Parser, Subcommand};

/// Instant, isolated workspaces for coding agents (APFS clones + Git worktrees).
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
    },
    /// List workspaces.
    List,
    /// Show changed files per workspace and files changed in more than one.
    Status { name: Option<String> },
    /// Print a workspace path (for `cd "$(agent-ws path NAME)"`).
    Path { name: String },
    /// Commit the workspace changes to a branch, then remove the workspace.
    Land {
        name: String,
        #[arg(short, long)]
        message: String,
        /// Branch to create (default agent-ws/NAME).
        #[arg(long)]
        branch: Option<String>,
        /// Overwrite the branch if it exists.
        #[arg(long)]
        force_branch: bool,
        /// Keep the workspace after landing.
        #[arg(long)]
        keep: bool,
        /// Skip commit hooks.
        #[arg(long)]
        no_verify: bool,
    },
    /// Remove a workspace without landing it.
    Drop { name: String },
    /// Clean up after interrupted spawns, deleted folders and stale trash.
    Gc {
        /// Report only; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Keep ready workspaces so spawning takes milliseconds.
    Pool {
        #[command(subcommand)]
        cmd: PoolCmd,
    },
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

/// First 12 characters of a commit id; records are editable files, so a
/// shorter value must not panic.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn print<T: serde::Serialize>(json: bool, v: &T, human: impl FnOnce(&T)) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        human(v);
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("agent-ws: {e:#}");
        std::process::exit(1);
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

fn run(cli: Cli) -> Result<()> {
    let repo = Repo::discover(&std::env::current_dir()?)?;
    match cli.cmd {
        Cmd::Spawn { name, base, exclude, keep_secrets, min_free_gb, no_pool } => {
            let opts = SpawnOpts {
                name,
                base,
                exclude,
                keep_secrets,
                min_free_bytes: min_free_gb.map(|g| g * 1_000_000_000),
                use_pool: !no_pool,
            };
            let s = repo.spawn(&opts)?;
            refill_in_background(&repo);
            print(cli.json, &s, |s| {
                println!("{}  {}", s.name, s.path.display());
                let how = if s.from_pool { "from pool".to_string() } else { format!("clone {} ms", s.clone_ms) };
                println!("base {}  {how}  total {} ms", short(&s.base), s.total_ms);
                for p in &s.notes.symlinks_retargeted {
                    println!("retargeted symlink {}", p.display());
                }
                for l in &s.notes.symlinks_external {
                    println!("WARNING symlink {} -> {} points outside the workspace", l.path.display(), l.target.display());
                }
                for p in &s.notes.secrets_removed {
                    println!("removed secret {p}");
                }
            })
        }
        Cmd::List => print(cli.json, &repo.list()?, |l| {
            for m in l {
                println!("{:20} {}  {}", m.name, short(&m.base), m.path.display());
            }
        }),
        Cmd::Status { name } => print(cli.json, &repo.status(name.as_deref())?, |s| {
            for w in &s.workspaces {
                println!("{} ({} changed)", w.name, w.changed.len());
                for p in &w.changed {
                    println!("  {p}");
                }
            }
            for (p, names) in &s.overlaps {
                println!("OVERLAP {p}: {}", names.join(", "));
            }
        }),
        Cmd::Path { name } => {
            let path = repo.meta(&name)?.path;
            print(cli.json, &serde_json::json!({ "path": path }), |_| println!("{}", path.display()))
        }
        Cmd::Land { name, message, branch, force_branch, keep, no_verify } => {
            let l = repo.land(&LandOpts { name, message, branch, force_branch, keep, no_verify })?;
            print(cli.json, &l, |l| {
                println!("{} -> {} {} ({} files){}", l.name, l.branch, short(&l.commit), l.files, if l.dropped { ", dropped" } else { "" });
                for (p, names) in &l.overlaps {
                    println!("OVERLAP {p}: also changed in {}", names.join(", "));
                }
            })
        }
        Cmd::Drop { name } => {
            repo.drop(&name)?;
            print(cli.json, &serde_json::json!({ "dropped": name }), |_| println!("dropped {name}"))
        }
        Cmd::Gc { dry_run } => print(cli.json, &repo.gc(dry_run)?, |g| {
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
            if g.claims > 0 {
                println!("{verb} {} dead pool claims", g.claims);
            }
            if g.trash > 0 {
                println!("{verb} {} trash entries", g.trash);
            }
            if g.interrupted.is_empty() && g.missing.is_empty() && g.orphans.is_empty() && g.claims == 0 && g.trash == 0 {
                println!("nothing to clean");
            }
        }),
        Cmd::Pool { cmd } => match cmd {
            PoolCmd::Set { size } => {
                repo.pool_set(size)?;
                print(cli.json, &serde_json::json!({ "target": size }), |_| println!("pool target {size}"))
            }
            PoolCmd::Fill { size } => print(cli.json, &repo.pool_fill(size)?, |f| {
                if f.busy {
                    println!("another fill is running");
                } else {
                    println!("{} ready (created {}, removed {} stale, {} extra)", f.ready, f.created, f.removed_stale, f.removed_extra);
                }
            }),
            PoolCmd::Status => print(cli.json, &repo.pool_status()?, |s| {
                println!("target {}  fresh {}  stale {}  creating {}", s.target, s.fresh, s.stale, s.creating);
            }),
            PoolCmd::Clear => print(cli.json, &repo.pool_fill(Some(0))?, |f| {
                println!("removed {} pooled workspaces", f.removed_stale + f.removed_extra);
            }),
        },
    }
}
