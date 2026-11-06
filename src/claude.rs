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
    match std::fs::read(path) {
        Ok(raw) if raw.iter().all(u8::is_ascii_whitespace) => Ok(json!({})),
        Ok(raw) => serde_json::from_slice(&raw)
            .with_context(|| format!("invalid JSON in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}

fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut raw = serde_json::to_string_pretty(settings)?;
    raw.push('\n');
    std::fs::write(path, raw)?;
    Ok(())
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
            let before = list.len();
            list.retain(|e| !is_ours(e));
            changed |= list.len() != before;
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
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                true => c,
                false => '-',
            },
        )
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
            let name = match i {
                1 => base.clone(),
                _ => format!("{base}-{i}"),
            };
            let branch = format!("agent-ws/{name}");
            if self.meta_path(&name).exists() || self.branch_tip(&branch).is_some() {
                continue;
            }
            return self.spawn(&SpawnOpts {
                name,
                use_pool: true,
                branch: Some(branch),
                origin: "claude".into(),
                ..Default::default()
            });
        }
        bail!("no free workspace name for {name:?}")
    }

    /// WorktreeRemove: drop the workspace at `path`. Its branch stays if it
    /// has commits. A path that is not an agent-ws workspace is left alone.
    pub fn claude_remove(&self, path: &Path) -> Result<()> {
        let real = std::fs::canonicalize(path).unwrap_or(path.to_path_buf());
        let m = self
            .list()?
            .into_iter()
            .find(|m| m.path == path || std::fs::canonicalize(&m.path).is_ok_and(|p| p == real))
            .with_context(|| {
                format!(
                    "{} is not an agent-ws workspace; left it alone",
                    path.display()
                )
            })?;
        self.drop(&m.name)
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
}
