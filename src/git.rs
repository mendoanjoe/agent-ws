//! Thin wrapper over the `git` CLI.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    run_env(dir, args, &[])
}

/// `run` with extra environment variables.
pub fn run_env(dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().copied())
        .output()?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn line(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(run(dir, args)?.trim().to_string())
}

/// Author, committer and a fixed date for commits made only to compare
/// workspace states: the same content gives the same commit, so polling
/// `status` adds no objects once nothing changes.
const SNAPSHOT_ID: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "agent-ws"),
    ("GIT_AUTHOR_EMAIL", "agent-ws@localhost"),
    ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "agent-ws"),
    ("GIT_COMMITTER_EMAIL", "agent-ws@localhost"),
    ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
];

/// The tree of everything in the worktree at `dir` right now: commits,
/// uncommitted edits and untracked files (ignored files aside). The
/// worktree's index is never touched, so an agent committing meanwhile is
/// not disturbed: `add -A` runs on a copy of it under `scratch`.
pub fn snapshot_tree(dir: &Path, scratch: &Path) -> Result<String> {
    let index = line(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    std::fs::create_dir_all(scratch)?;
    let tmp = scratch.join(format!(
        "index-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // Starting from a copy keeps the cached file stats: only changed files
    // are read again. Git writes the index by rename, so the copy is whole.
    match std::fs::copy(&index, &tmp) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("copy {index}")),
    }
    let env = [("GIT_INDEX_FILE", tmp.as_os_str())];
    let tree = run_env(dir, &["add", "-A"], &env).and_then(|_| run_env(dir, &["write-tree"], &env));
    let _ = std::fs::remove_file(&tmp);
    Ok(tree?.trim().to_string())
}

/// A commit of `tree` on top of `parent`, to give `merge-tree` the history
/// it needs to find the merge base. Not referenced by anything.
pub fn snapshot_commit(dir: &Path, tree: &str, parent: &str) -> Result<String> {
    let env: Vec<(&str, &OsStr)> = SNAPSHOT_ID
        .iter()
        .map(|(k, v)| (*k, OsStr::new(v)))
        .collect();
    Ok(run_env(
        dir,
        &["commit-tree", tree, "-p", parent, "-m", "agent-ws snapshot"],
        &env,
    )?
    .trim()
    .to_string())
}

/// Paths that conflict when commits `a` and `b` are merged (empty when they
/// merge cleanly). `None` when this Git cannot tell: `merge-tree
/// --write-tree` needs Git 2.38.
pub fn merge_conflicts(dir: &Path, a: &str, b: &str) -> Result<Option<Vec<String>>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            a,
            b,
        ])
        .output()?;
    match out.status.code() {
        // `<tree>\0<path>\0<path>\0…`: the merged tree, then conflicted paths.
        Some(0) | Some(1) => {
            let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .skip(1)
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect();
            paths.sort();
            paths.dedup();
            Ok(Some(paths))
        }
        _ => Ok(None),
    }
}

/// Every path a workspace touched since `base`. A rename touches two paths
/// (the old one is removed), so rename detection is off: removing a file
/// another workspace edits is an overlap too.
///
/// Read-only: a coordinator polls this while agents commit, and a plain
/// `git status` rewrites the index under `index.lock`, failing their commits.
pub fn changed_paths(dir: &Path, base: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let out = run(
        dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )?;
    let mut parts = out.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = parts.next() {
        if entry.len() > 3 {
            paths.push(entry[3..].to_string());
        }
        if entry.starts_with('R') || entry.starts_with('C') {
            parts.next();
        }
    }
    // Commits the agent made inside the workspace count as changes too.
    for p in run(
        dir,
        &["diff", "--name-only", "--no-renames", "-z", base, "HEAD"],
    )?
    .split('\0')
    {
        if !p.is_empty() {
            paths.push(p.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
