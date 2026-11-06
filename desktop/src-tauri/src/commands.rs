//! What the window asks for. Each command runs its library call on a worker
//! thread, so the window stays responsive while Git works.

use agent_ws::{FileChange, KnownRepo, LandOpts, Landed, Prune, PruneOpts, Repo, Status, WsInfo};
use serde::Serialize;
use std::path::Path;
use std::process::Command;

type Res<T> = Result<T, String>;

async fn blocking<T, F>(work: F) -> Res<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

fn repo(root: &str) -> anyhow::Result<Repo> {
    Repo::discover(Path::new(root))
}

#[tauri::command]
pub async fn repos() -> Res<Vec<KnownRepo>> {
    blocking(agent_ws::known_repos).await
}

/// `list --long`; `disk` adds the real disk use, which reads every file.
#[tauri::command]
pub async fn workspaces(root: String, disk: bool) -> Res<Vec<WsInfo>> {
    blocking(move || repo(&root)?.list_long(disk)).await
}

#[tauri::command]
pub async fn status(root: String) -> Res<Status> {
    blocking(move || repo(&root)?.status(None)).await
}

#[derive(Serialize)]
pub struct Diff {
    files: Vec<FileChange>,
    patch: String,
}

#[tauri::command]
pub async fn diff(root: String, name: String) -> Res<Diff> {
    blocking(move || {
        let r = repo(&root)?;
        Ok(Diff {
            files: r.diff_files(&name, &[])?,
            patch: r.diff_patch(&name, &[])?,
        })
    })
    .await
}

/// The end of the last `exec --log` output, or nothing.
#[tauri::command]
pub async fn run_log(root: String, name: String) -> Res<Option<String>> {
    blocking(move || repo(&root)?.run_log(&name, 256 * 1024)).await
}

#[tauri::command]
pub async fn land(
    root: String,
    name: String,
    message: String,
    squash: bool,
    pr: bool,
) -> Res<Landed> {
    blocking(move || {
        repo(&root)?.land(&LandOpts {
            name,
            message,
            branch: None,
            force_branch: false,
            keep: false,
            no_verify: false,
            squash,
            pr,
            pr_base: None,
            draft: false,
        })
    })
    .await
}

#[tauri::command]
pub async fn drop_workspace(root: String, name: String) -> Res<()> {
    blocking(move || repo(&root)?.drop(&name)).await
}

#[tauri::command]
pub async fn prune(root: String, dry_run: bool) -> Res<Prune> {
    blocking(move || {
        repo(&root)?.prune(&PruneOpts {
            base: None,
            dry_run,
            gh: false,
            min_age_secs: agent_ws::PRUNE_MIN_AGE_SECS,
            merged_only: false,
        })
    })
    .await
}

/// Open a folder in the file manager (`how`: `files`), an editor or a
/// terminal, or a URL in the browser (`url`).
#[tauri::command]
pub async fn open(target: String, how: String) -> Res<()> {
    blocking(move || open_with(&target, &how)).await
}

/// Editors tried in order; the first one installed opens the folder.
#[cfg(target_os = "macos")]
const EDITORS: &[&str] = &["Cursor", "Visual Studio Code", "Zed"];

#[cfg(target_os = "macos")]
fn open_with(target: &str, how: &str) -> anyhow::Result<()> {
    let installed = |app: &&str| {
        Command::new("open")
            .args(["-Ra", app])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let app = match how {
        "editor" => EDITORS.iter().copied().find(installed),
        "terminal" => Some("Terminal"),
        _ => None,
    };
    let mut cmd = Command::new("open");
    if let Some(app) = app {
        cmd.args(["-a", app]);
    }
    let out = cmd.arg(target).output()?;
    anyhow::ensure!(
        out.status.success(),
        "open {target}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_with(target: &str, how: &str) -> anyhow::Result<()> {
    let on_path = |p: &&str| {
        std::env::var_os("PATH")
            .is_some_and(|dirs| std::env::split_paths(&dirs).any(|d| d.join(p).is_file()))
    };
    // The first of these on PATH; the desktop's default handler otherwise.
    // A terminal starts in the folder; an editor gets it as an argument.
    let mut cmd = match how {
        "editor" => match ["cursor", "code", "zed"].iter().find(on_path) {
            Some(editor) => {
                let mut c = Command::new(editor);
                c.arg(target);
                c
            }
            None => default_handler(target),
        },
        "terminal" => match ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"]
            .iter()
            .find(on_path)
        {
            Some(terminal) => {
                let mut c = Command::new(terminal);
                c.current_dir(target);
                c
            }
            None => default_handler(target),
        },
        _ => default_handler(target),
    };
    // Editors and terminals keep running; do not wait for them.
    cmd.spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn default_handler(target: &str) -> Command {
    let mut c = Command::new("xdg-open");
    c.arg(target);
    c
}
