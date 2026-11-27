//! Per-repository settings from `.agent-ws.toml` at the repository root.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const FILE: &str = ".agent-ws.toml";
const MAX_CONFIG_BYTES: u64 = 1 << 20;

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
    /// `PORT` of the workspace with slot 0 (default 4100).
    pub port_base: Option<u16>,
    /// Ports between consecutive slots (default 10).
    pub port_step: Option<u16>,
    /// Commands run in workspaces; they run only once trusted (`agent-ws trust`).
    pub hooks: Option<Hooks>,
}

/// `[hooks]`: shell commands run in the workspace with its environment.
#[derive(Deserialize, Serialize, Default, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// After `spawn` (also from the pool). A failure removes the workspace.
    #[serde(default)]
    pub post_spawn: Vec<String>,
    /// Before `land` commits. A failure stops the land; `--no-verify` skips.
    #[serde(default)]
    pub pre_land: Vec<String>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.post_spawn.is_empty() && self.pre_land.is_empty()
    }
}

impl Config {
    pub fn load(root: &Path) -> Result<Config> {
        let p = root.join(FILE);
        let file = match std::fs::File::open(&p) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e.into()),
        };
        if file.metadata()?.len() > MAX_CONFIG_BYTES {
            bail!("invalid {}: config exceeds 1 MiB limit", p.display());
        }
        let mut s = String::new();
        let read = file.take(MAX_CONFIG_BYTES + 1).read_to_string(&mut s)?;
        if read as u64 > MAX_CONFIG_BYTES {
            bail!("invalid {}: config exceeds 1 MiB limit", p.display());
        }
        let mut cfg: Config =
            toml::from_str(&s).with_context(|| format!("invalid {}", p.display()))?;
        cfg.validate(&p)?;
        Ok(cfg)
    }

    fn validate(&mut self, path: &Path) -> Result<()> {
        for e in &mut self.exclude {
            *e = normalize_rel_path(e, "exclude")
                .with_context(|| format!("invalid {}", path.display()))?;
        }
        if let Some(stamps) = &mut self.pool_stamp {
            for s in stamps {
                *s = normalize_rel_path(s, "pool_stamp")
                    .with_context(|| format!("invalid {}", path.display()))?;
            }
        }
        for (field, pats) in [
            ("secrets", self.secrets.as_deref()),
            ("secrets_keep", self.secrets_keep.as_deref()),
        ] {
            if let Some(pats) = pats {
                for pat in pats {
                    if pat.is_empty()
                        || pat.contains('/')
                        || pat.contains('\\')
                        || pat.contains('\0')
                    {
                        bail!(
                            "invalid {}: {field} pattern {pat:?} must be a non-empty filename pattern without path separators",
                            path.display()
                        );
                    }
                }
            }
        }
        if self.port_base == Some(0) {
            bail!("invalid {}: port_base must be >= 1", path.display());
        }
        if self.port_step == Some(0) {
            bail!("invalid {}: port_step must be >= 1", path.display());
        }
        if let Some(base) = &self.prune_base
            && (base.is_empty()
                || base.starts_with('-')
                || base
                    .bytes()
                    .any(|b| b == 0 || b.is_ascii_control() || b.is_ascii_whitespace()))
        {
            bail!(
                "invalid {}: prune_base {base:?} is not a valid ref name",
                path.display()
            );
        }
        Ok(())
    }

    pub fn is_secret(&self, name: &str) -> bool {
        let matches_pat = match &self.secrets {
            Some(pats) => pats.iter().any(|p| crate::fsutil::glob(p, name)),
            None => SECRETS.iter().any(|p| crate::fsutil::glob(p, name)),
        };
        if !matches_pat {
            return false;
        }
        match &self.secrets_keep {
            Some(keep) => !keep.iter().any(|p| crate::fsutil::glob(p, name)),
            None => !SECRETS_KEEP.iter().any(|p| crate::fsutil::glob(p, name)),
        }
    }

    pub fn stamp_files(&self) -> Vec<String> {
        self.pool_stamp
            .clone()
            .unwrap_or_else(|| STAMP.iter().map(|s| s.to_string()).collect())
    }
}

fn normalize_rel_path(raw: &str, field: &str) -> Result<String> {
    if raw.is_empty() || raw.contains('\0') {
        bail!("{field} entry {raw:?} must be a non-empty relative path");
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        bail!("{field} entry {raw:?} must be relative");
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => bail!("{field} entry {raw:?} must not contain '..'"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("{field} entry {raw:?} must be relative")
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        bail!("{field} entry {raw:?} must not be '.'");
    }
    Ok(out.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_rejects_bad_values_and_normalizes_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(FILE),
            "exclude = [\"./dist/\", \"a/./b\"]\npool_stamp = [\"./Cargo.lock\"]\n",
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.exclude, ["dist", "a/b"]);
        assert_eq!(cfg.pool_stamp.unwrap(), ["Cargo.lock"]);

        for bad in [
            "exclude = [\"../out\"]\n",
            "exclude = [\"/etc\"]\n",
            "exclude = [\".\"]\n",
            "secrets = [\"dir/.env\"]\n",
            "port_base = 0\n",
            "port_step = 0\n",
            "prune_base = \"--exec=sh\"\n",
        ] {
            std::fs::write(tmp.path().join(FILE), bad).unwrap();
            assert!(Config::load(tmp.path()).is_err(), "{bad}");
        }
    }
}
