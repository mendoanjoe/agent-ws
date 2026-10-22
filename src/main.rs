use agent_ws::{LandOpts, PruneOpts, Repo, SpawnOpts};
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

fn run(cli: Cli) -> Result<()> {
    let repo = Repo::discover(&std::env::current_dir()?)?;
    match cli.cmd {
        Cmd::Spawn {
            name,
            base,
            exclude,
            keep_secrets,
            min_free_gb,
            no_pool,
        } => {
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
            prune_in_background(&repo);
            print(cli.json, &s, |s| {
                println!("{}  {}", s.name, s.path.display());
                let how = if s.from_pool {
                    "from pool".to_string()
                } else {
                    format!("clone {} ms", s.clone_ms)
                };
                println!("base {}  {how}  total {} ms", short(&s.base), s.total_ms);
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
            print(cli.json, &serde_json::json!({ "path": path }), |_| {
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
        } => {
            let l = repo.land(&LandOpts {
                name,
                message,
                branch,
                force_branch,
                keep,
                no_verify,
            })?;
            print(cli.json, &l, |l| {
                println!(
                    "{} -> {} {} ({} files){}",
                    l.name,
                    l.branch,
                    short(&l.commit),
                    l.files,
                    if l.dropped { ", dropped" } else { "" }
                );
                for (p, names) in &l.overlaps {
                    println!("OVERLAP {p}: also changed in {}", names.join(", "));
                }
            })
        }
        Cmd::Drop { name } => {
            repo.drop(&name)?;
            print(cli.json, &serde_json::json!({ "dropped": name }), |_| {
                println!("dropped {name}")
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
            cli.json,
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
                    println!(
                        "{verb} {:20} {:>9}  {}",
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
        Cmd::Du { name } => print(cli.json, &repo.du(name.as_deref())?, |d| {
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
                print(cli.json, &serde_json::json!({ "target": size }), |_| {
                    println!("pool target {size}")
                })
            }
            PoolCmd::Fill { size } => print(cli.json, &repo.pool_fill(size)?, |f| {
                if f.busy {
                    println!("another fill is running");
                } else {
                    println!(
                        "{} ready (created {}, removed {} stale, {} extra)",
                        f.ready, f.created, f.removed_stale, f.removed_extra
                    );
                }
            }),
            PoolCmd::Status => print(cli.json, &repo.pool_status()?, |s| {
                println!(
                    "target {}  fresh {}  stale {}  creating {}",
                    s.target, s.fresh, s.stale, s.creating
                );
            }),
            PoolCmd::Clear => print(cli.json, &repo.pool_fill(Some(0))?, |f| {
                println!(
                    "removed {} pooled workspaces",
                    f.removed_stale + f.removed_extra
                );
            }),
        },
    }
}
