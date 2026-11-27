//! Claude Code integration: agent-ws as the WorktreeCreate/WorktreeRemove
//! hook, so `claude --worktree`, subagents with `isolation: "worktree"`,
//! workflows and background sessions get workspaces with dependencies.
//!
//! Claude Code sends JSON on stdin. For WorktreeCreate it reads the
//! worktree path from stdout, so everything else goes to stderr; a
//! non-zero exit fails the creation. For WorktreeRemove an exit of 0 means
//! removed, and non-zero keeps the directory.

use crate::{Repo, SpawnOpts, Spawned};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The command Claude Code runs; `agent-ws` must be on its PATH.
pub const HOOK_COMMAND: &str = "agent-ws claude hook";

const EVENTS: [&str; 2] = ["WorktreeCreate", "WorktreeRemove"];

/// The fields agent-ws reads from Claude Code's hook input.
#[derive(Deserialize)]
pub struct HookInput {
    pub hook_event_name: String,
    /// WorktreeCreate: a slug such as `bold-oak-a3f2`.
    #[serde(default)]
    pub name: Option<String>,
    /// WorktreeRemove: the path WorktreeCreate returned.
    #[serde(default)]
    pub worktree_path: Option<PathBuf>,
    /// The session's directory, inside the repository.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// Which Claude Code settings file `install` edits.
#[derive(Clone, Copy)]
pub enum Scope {
    /// `.claude/settings.local.json`: this repository, only you.
    Local,
    /// `.claude/settings.json`: this repository, everyone who clones it.
    Project,
    /// `~/.claude/settings.json`: every repository you open.
    User,
}

/// The settings file for `scope`. `root` is the repository (not needed for
/// `User`, which honours `CLAUDE_CONFIG_DIR` as Claude Code does).
pub fn settings_path(root: Option<&Path>, scope: Scope) -> Result<PathBuf> {
    Ok(match scope {
        Scope::Local => root
            .context("not inside a Git repository")?
            .join(".claude/settings.local.json"),
        Scope::Project => root
            .context("not inside a Git repository")?
            .join(".claude/settings.json"),
        Scope::User => match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir).join("settings.json"),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".claude/settings.json"),
        },
    })
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            bail!("refusing to follow symlink at {}", path.display())
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(e.into()),
    }
    let raw = std::fs::read(path)?;
    if raw.iter().all(u8::is_ascii_whitespace) {
        Ok(json!({}))
    } else {
        serde_json::from_slice(&raw).with_context(|| format!("invalid JSON in {}", path.display()))
    }
}

fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let existing_perms = match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            bail!("refusing to overwrite symlink at {}", path.display())
        }
        Ok(md) => Some(md.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut raw = serde_json::to_string_pretty(settings)?;
    raw.push('\n');
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".settings.tmp-{}-{seq}", std::process::id()));
    let write_res = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        if let Some(perms) = existing_perms {
            f.set_permissions(perms)?;
        }
        f.write_all(raw.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_res
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .is_some_and(|hooks| hooks.iter().any(|h| h["command"] == HOOK_COMMAND))
}

/// Add the WorktreeCreate and WorktreeRemove hooks to a settings file,
/// keeping everything else in it, key order included. Returns whether the
/// file changed (false when both hooks were there already).
pub fn install(path: &Path) -> Result<bool> {
    let mut settings = read_settings(path)?;
    let root = settings
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", path.display()))?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("\"hooks\" is not an object")?;
    let mut changed = false;
    for event in EVENTS {
        let list = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("\"hooks.{event}\" is not a list"))?;
        if !list.iter().any(is_ours) {
            list.push(json!({ "hooks": [{ "type": "command", "command": HOOK_COMMAND }] }));
            changed = true;
        }
    }
    if changed {
        write_settings(path, &settings)?;
    }
    Ok(changed)
}

/// Remove what `install` added. Returns whether the file changed.
pub fn uninstall(path: &Path) -> Result<bool> {
    let mut settings = read_settings(path)?;
    let Some(root) = settings.as_object_mut() else {
        return Ok(false);
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let mut changed = false;
    for event in EVENTS {
        if let Some(list) = hooks.get_mut(event).and_then(Value::as_array_mut) {
            let mut keep_entries = Vec::with_capacity(list.len());
            for mut entry in list.drain(..) {
                let mut drop_entry = false;
                if let Some(inner) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                    let before = inner.len();
                    inner.retain(|h| h["command"] != HOOK_COMMAND);
                    if inner.len() != before {
                        changed = true;
                        drop_entry = inner.is_empty();
                    }
                }
                if !drop_entry {
                    keep_entries.push(entry);
                }
            }
            *list = keep_entries;
            if list.is_empty() {
                hooks.shift_remove(event);
            }
        }
    }
    if hooks.is_empty() {
        root.shift_remove("hooks");
    }
    if changed {
        write_settings(path, &settings)?;
    }
    Ok(changed)
}

/// A workspace name from Claude Code's worktree name: anything but
/// letters, digits, `_` and `-` becomes `-`, so it also works in a branch.
fn workspace_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    match name.trim_matches('-') {
        "" => "claude".into(),
        n => n.into(),
    }
}

impl Repo {
    /// WorktreeCreate: a workspace named after Claude Code's worktree (with
    /// `-2`, `-3`… when taken) on a new branch `agent-ws/<name>`, from the
    /// pool when one is ready. Commits the session makes land on that branch,
    /// which outlives the workspace like the branch of a git worktree.
    pub fn claude_create(&self, name: &str) -> Result<Spawned> {
        let base = workspace_name(name);
        for i in 1..=100 {
            let name = if i == 1 {
                base.clone()
            } else {
                format!("{base}-{i}")
            };
            let branch = format!("agent-ws/{name}");
            if self.meta_path(&name).exists() || self.branch_tip(&branch).is_some() {
                continue;
            }
            match self.spawn(&SpawnOpts {
                name,
                use_pool: true,
                branch: Some(branch),
                origin: "claude".into(),
                ..Default::default()
            }) {
                Ok(s) => return Ok(s),
                Err(e) if i < 100 && e.to_string().contains("already exists") => continue,
                Err(e) => return Err(e),
            }
        }
        bail!("no free workspace name for {name:?}")
    }

    /// WorktreeRemove: drop the workspace at `path`. Its branch stays if it
    /// has commits. A path that is not an agent-ws workspace is left alone.
    pub fn claude_remove(&self, path: &Path) -> Result<()> {
        let exists = match std::fs::symlink_metadata(path) {
            Ok(md) if md.file_type().is_symlink() => {
                bail!("refusing to remove symlink at {}", path.display())
            }
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        let real = if exists {
            std::fs::canonicalize(path).ok()
        } else {
            let parent = path.parent().and_then(|p| std::fs::canonicalize(p).ok());
            parent.and_then(|p| path.file_name().map(|f| p.join(f)))
        };
        let found = self.list()?.into_iter().find(|m| {
            m.path == path
                || real.as_ref().is_some_and(|r| {
                    m.path == *r || std::fs::canonicalize(&m.path).is_ok_and(|p| p == *r)
                })
        });
        match found {
            Some(m) => self.drop(&m.name),
            None if !exists => Ok(()),
            None => bail!(
                "{} is not an agent-ws workspace; left it alone",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_names_work_as_branches() {
        assert_eq!(workspace_name("bold-oak-a3f2"), "bold-oak-a3f2");
        assert_eq!(workspace_name("feat/login.lock"), "feat-login-lock");
        assert_eq!(workspace_name("a..b"), "a--b");
        assert_eq!(workspace_name("..."), "claude");
    }

    #[test]
    fn uninstall_preserves_sibling_hooks_and_rejects_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "WorktreeCreate": [
                        { "hooks": [{ "type": "command", "command": "other-hook" }, { "type": "command", "command": HOOK_COMMAND }] }
                    ],
                    "WorktreeRemove": [
                        { "hooks": [{ "type": "command", "command": HOOK_COMMAND }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(uninstall(&settings).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert_eq!(
            v["hooks"]["WorktreeCreate"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            v["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
            "other-hook"
        );
        assert!(v["hooks"].get("WorktreeRemove").is_none());

        let link = tmp.path().join("link.json");
        std::os::unix::fs::symlink(&settings, &link).unwrap();
        assert!(install(&link).is_err());
        assert!(uninstall(&link).is_err());
    }
}
