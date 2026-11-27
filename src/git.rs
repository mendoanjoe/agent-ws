//! Thin wrapper over the `git` CLI.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

fn run_raw(dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().copied())
        .env("GIT_TERMINAL_PROMPT", "0")
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

pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    run_env(dir, args, &[])
}

pub fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run_raw(dir, args, &[])
}

/// `run` with extra environment variables.
pub fn run_env(dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Result<String> {
    let out = run_raw(dir, args, env)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub fn line(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(run(dir, args)?.trim().to_string())
}

fn check_rev(rev: &str, label: &str) -> Result<()> {
    if rev.is_empty() || rev.starts_with('-') || rev.bytes().any(|b| b == 0 || b.is_ascii_control())
    {
        bail!("invalid {label} {rev:?}");
    }
    Ok(())
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
        &[
            "--no-optional-locks",
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index",
        ],
    )?;
    std::fs::create_dir_all(scratch)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = scratch.join(format!("index-{}-{nanos}-{seq}", std::process::id()));
    // Starting from a copy keeps the cached file stats: only changed files
    // are read again. Git writes the index by rename, so the copy is whole.
    let index_path = Path::new(&index);
    match std::fs::symlink_metadata(index_path) {
        Ok(_) => {
            if crate::fsutil::clone_path(index_path, &tmp).is_err() {
                match std::fs::copy(index_path, &tmp) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("copy {index}")),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {index}")),
    }
    let env = [("GIT_INDEX_FILE", tmp.as_os_str())];
    let tree = run_env(dir, &["add", "-A"], &env).and_then(|_| run_env(dir, &["write-tree"], &env));
    let _ = std::fs::remove_file(&tmp);
    Ok(tree?.trim().to_string())
}

/// A commit of `tree` on top of `parent`, to give `merge-tree` the history
/// it needs to find the merge base. Not referenced by anything.
pub fn snapshot_commit(dir: &Path, tree: &str, parent: &str) -> Result<String> {
    check_rev(tree, "tree")?;
    check_rev(parent, "parent")?;
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
    check_rev(a, "commit")?;
    check_rev(b, "commit")?;
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            "--end-of-options",
            a,
            b,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    match out.status.code() {
        // `<tree>\0<path>\0<path>\0…`: the merged tree, then conflicted paths.
        Some(0) | Some(1) => {
            let mut paths: Vec<String> = out
                .stdout
                .split(|&b| b == 0)
                .skip(1)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            paths.sort();
            paths.dedup();
            Ok(Some(paths))
        }
        code => {
            let err = String::from_utf8_lossy(&out.stderr);
            let low = err.to_ascii_lowercase();
            if code == Some(129)
                || low.contains("unknown option")
                || low.contains("unrecognized option")
                || low.contains("usage: git merge-tree")
            {
                Ok(None)
            } else {
                bail!("git merge-tree failed ({}): {}", out.status, err.trim());
            }
        }
    }
}

/// Every path a workspace touched since `base`. A rename touches two paths
/// (the old one is removed), so rename detection is off: removing a file
/// another workspace edits is an overlap too.
///
/// Read-only: a coordinator polls this while agents commit, and a plain
/// `git status` rewrites the index under `index.lock`, failing their commits.
pub fn changed_paths(dir: &Path, base: &str) -> Result<Vec<String>> {
    check_rev(base, "base")?;
    let mut paths = Vec::new();
    let out = run_bytes(
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
    let mut parts = out.split(|&b| b == 0).filter(|s| !s.is_empty());
    while let Some(entry) = parts.next() {
        if entry.len() > 3 {
            paths.push(String::from_utf8_lossy(&entry[3..]).into_owned());
        }
        if entry.starts_with(b"R") || entry.starts_with(b"C") {
            parts.next();
        }
    }
    // Commits the agent made inside the workspace count as changes too.
    for p in run_bytes(
        dir,
        &[
            "--no-optional-locks",
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            "--end-of-options",
            base,
            "HEAD",
            "--",
        ],
    )?
    .split(|&b| b == 0)
    {
        if !p.is_empty() {
            paths.push(String::from_utf8_lossy(p).into_owned());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_rev_rejects_option_like_strings() {
        assert!(check_rev("--output=/tmp/x", "base").is_err());
        assert!(check_rev("-p", "parent").is_err());
        assert!(check_rev("", "tree").is_err());
        assert!(check_rev("HEAD", "base").is_ok());
    }
}
