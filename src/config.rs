//! Per-repository settings from `.agent-ws.toml` at the repository root.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

pub const FILE: &str = ".agent-ws.toml";

const SECRETS: &[&str] = &[
    ".env",
    ".env.*",
    ".dev.vars",
    "*.pem",
    "*.key",
    "*.p12",
    "id_rsa",
    "id_ed25519",
];
const SECRETS_KEEP: &[&str] = &[".env.example", ".env.sample", ".env.template"];

/// Files whose change makes pooled workspaces stale even when HEAD did not
/// move (dependencies installed or upgraded in the source checkout).
const STAMP: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "Cargo.lock",
    "node_modules/.modules.yaml",
    "node_modules/.package-lock.json",
    "node_modules/.yarn-state.yml",
];

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Extra paths never cloned, relative to the repository root.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Name patterns of secret files removed from workspaces (replaces the defaults).
    pub secrets: Option<Vec<String>>,
    /// Names that match `secrets` but stay (replaces the defaults).
    pub secrets_keep: Option<Vec<String>>,
    /// Fail a spawn when a symlink points outside the workspace.
    #[serde(default)]
    pub strict_symlinks: bool,
    /// Refuse to spawn below this much free disk space.
    pub min_free_gb: Option<u64>,
    /// Pooled workspaces to keep ready.
    pub pool: Option<usize>,
    /// Files whose change makes pooled workspaces stale (replaces the defaults).
    pub pool_stamp: Option<Vec<String>>,
    /// Branch `prune` compares against (default origin/HEAD, origin/main, …).
    pub prune_base: Option<String>,
    /// `spawn` warns once this many workspaces exist (default 20, 0 = never).
    pub warn_workspaces: Option<usize>,
    /// After `spawn`, drop merged workspaces in the background (default on).
    pub auto_prune: Option<bool>,
}

impl Config {
    pub fn load(root: &Path) -> Result<Config> {
        let p = root.join(FILE);
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("invalid {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn is_secret(&self, name: &str) -> bool {
        let pats = self
            .secrets
            .as_deref()
            .map(own)
            .unwrap_or_else(|| own_static(SECRETS));
        let keep = self
            .secrets_keep
            .as_deref()
            .map(own)
            .unwrap_or_else(|| own_static(SECRETS_KEEP));
        pats.iter().any(|p| crate::fsutil::glob(p, name))
            && !keep.iter().any(|p| crate::fsutil::glob(p, name))
    }

    pub fn stamp_files(&self) -> Vec<String> {
        self.pool_stamp
            .clone()
            .unwrap_or_else(|| STAMP.iter().map(|s| s.to_string()).collect())
    }
}

fn own(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn own_static(v: &'static [&'static str]) -> Vec<&'static str> {
    v.to_vec()
}
