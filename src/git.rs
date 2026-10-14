//! Thin wrapper over the `git` CLI.

use anyhow::{Result, bail};
use std::path::Path;
use std::process::Command;

pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn line(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(run(dir, args)?.trim().to_string())
}

/// Paths in `-z` porcelain status output (`XY path`, renames carry a second path).
pub fn changed_paths(dir: &Path, base: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let out = run(dir, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
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
    for p in run(dir, &["diff", "--name-only", "-z", base, "HEAD"])?.split('\0') {
        if !p.is_empty() {
            paths.push(p.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
