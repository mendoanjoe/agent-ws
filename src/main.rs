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
        /// Keep ignored secret files (.env, .env.*, .dev.vars).
        #[arg(long)]
        keep_secrets: bool,
        /// Refuse to spawn below this much free disk space.
        #[arg(long, default_value_t = 2)]
        min_free_gb: u64,
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

fn run(cli: Cli) -> Result<()> {
    let repo = Repo::discover(&std::env::current_dir()?)?;
    match cli.cmd {
        Cmd::Spawn { name, base, exclude, keep_secrets, min_free_gb } => {
            let s = repo.spawn(&SpawnOpts { name, base, exclude, keep_secrets, min_free_bytes: min_free_gb * 1_000_000_000 })?;
            print(cli.json, &s, |s| {
                println!("{}  {}", s.name, s.path.display());
                println!("base {}  clone {} ms  total {} ms", &s.base[..12], s.clone_ms, s.total_ms);
                for p in &s.symlinks_retargeted {
                    println!("retargeted symlink {}", p.display());
                }
                for l in &s.symlinks_external {
                    println!("WARNING symlink {} -> {} points outside the workspace", l.path.display(), l.target.display());
                }
                for p in &s.secrets_removed {
                    println!("removed secret {p}");
                }
            })
        }
        Cmd::List => print(cli.json, &repo.list()?, |l| {
            for m in l {
                println!("{:20} {}  {}", m.name, &m.base[..12], m.path.display());
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
                println!("{} -> {} {} ({} files){}", l.name, l.branch, &l.commit[..12], l.files, if l.dropped { ", dropped" } else { "" });
            })
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
            if g.trash > 0 {
                println!("{verb} {} trash entries", g.trash);
            }
            if g.interrupted.is_empty() && g.missing.is_empty() && g.orphans.is_empty() && g.trash == 0 {
                println!("nothing to clean");
            }
        }),
        Cmd::Drop { name } => {
            repo.drop(&name)?;
            print(cli.json, &serde_json::json!({ "dropped": name }), |_| println!("dropped {name}"))
        }
    }
}
