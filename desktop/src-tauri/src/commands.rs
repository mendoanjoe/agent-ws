//! What the window asks for. Each command runs its library call on a worker
//! thread, so the window stays responsive while Git works.

use agent_ws::{
    DiffBundle, KnownRepo, LandOpts, Landed, Prune, PruneOpts, Repo, Status, WsInfo, WsUsage,
};
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_DIFF_LINES: usize = 4000;

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

fn resolve_repo_in(root: &str, known: &[KnownRepo]) -> anyhow::Result<PathBuf> {
    let canon = std::fs::canonicalize(Path::new(root))
        .with_context(|| format!("invalid repository path: {root}"))?;
    known
        .iter()
        .find(|k| std::fs::canonicalize(&k.root).is_ok_and(|r| r == canon))
        .map(|k| k.root.clone())
        .ok_or_else(|| anyhow::anyhow!("unknown repository: {root}"))
}

fn repo(root: &str) -> anyhow::Result<Repo> {
    let known = agent_ws::known_repos()?;
    let known_root = resolve_repo_in(root, &known)?;
    Repo::discover(&known_root)
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
pub async fn disk_usage(root: String) -> Res<Vec<WsUsage>> {
    blocking(move || Ok(repo(&root)?.du(None)?.entries)).await
}

#[tauri::command]
pub async fn status(root: String) -> Res<Status> {
    blocking(move || repo(&root)?.status(None)).await
}

#[tauri::command]
pub async fn diff(root: String, name: String) -> Res<DiffBundle> {
    blocking(move || repo(&root)?.diff_bundle(&name, &[], Some(MAX_DIFF_LINES))).await
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

/// Open a workspace/repo folder (`files`, `editor`, `terminal`) or an `http(s)` URL (`url`).
#[tauri::command]
pub async fn open(target: String, how: String) -> Res<()> {
    blocking(move || open_with(&target, &how)).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenHow {
    Files,
    Editor,
    Terminal,
    Url,
}

impl OpenHow {
    fn parse(how: &str) -> anyhow::Result<Self> {
        match how {
            "files" => Ok(Self::Files),
            "editor" => Ok(Self::Editor),
            "terminal" => Ok(Self::Terminal),
            "url" => Ok(Self::Url),
            _ => anyhow::bail!("unsupported open target kind: {how}"),
        }
    }
}

fn validate_url(target: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!target.is_empty(), "empty URL");
    anyhow::ensure!(!target.starts_with('-'), "invalid URL");
    anyhow::ensure!(
        !target.chars().any(|c| c.is_control() || c.is_whitespace()),
        "invalid characters in URL"
    );
    let (scheme, rest) = target
        .split_once("://")
        .context("URL must use http:// or https://")?;
    anyhow::ensure!(
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"),
        "URL must use http:// or https://"
    );
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    anyhow::ensure!(!host.is_empty(), "URL missing host");
    Ok(())
}

fn validate_known_dir_in(target: &str, known: &[KnownRepo]) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!target.is_empty(), "empty path");
    anyhow::ensure!(!target.starts_with('-'), "invalid path");
    let canon = std::fs::canonicalize(Path::new(target))
        .with_context(|| format!("invalid target path: {target}"))?;
    anyhow::ensure!(canon.is_dir(), "target is not a directory: {target}");
    let allowed = known.iter().any(|k| {
        if std::fs::canonicalize(&k.root).is_ok_and(|r| r == canon) {
            return true;
        }
        Repo::discover(&k.root).ok().is_some_and(|r| {
            r.list().ok().is_some_and(|ms| {
                ms.iter().any(|m| {
                    r.owns(&m.path) && std::fs::canonicalize(&m.path).is_ok_and(|p| p == canon)
                })
            })
        })
    });
    anyhow::ensure!(allowed, "target is outside known repositories: {target}");
    Ok(canon)
}

enum OpenTarget {
    Url(String),
    Dir(PathBuf),
}

fn validate_open(target: &str, how: &str) -> anyhow::Result<(OpenHow, OpenTarget)> {
    let kind = OpenHow::parse(how)?;
    match kind {
        OpenHow::Url => {
            validate_url(target)?;
            Ok((kind, OpenTarget::Url(target.to_string())))
        }
        OpenHow::Files | OpenHow::Editor | OpenHow::Terminal => {
            let known = agent_ws::known_repos()?;
            let dir = validate_known_dir_in(target, &known)?;
            Ok((kind, OpenTarget::Dir(dir)))
        }
    }
}

/// Editors tried in order; the first one installed opens the folder.
#[cfg(target_os = "macos")]
const EDITORS: &[&str] = &["Cursor", "Visual Studio Code", "Zed"];

#[cfg(target_os = "macos")]
fn open_with(target: &str, how: &str) -> anyhow::Result<()> {
    let (kind, validated) = validate_open(target, how)?;
    let installed = |app: &&str| {
        Command::new("open")
            .args(["-Ra", app])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let app = match kind {
        OpenHow::Editor => EDITORS.iter().copied().find(installed),
        OpenHow::Terminal => Some("Terminal"),
        OpenHow::Files | OpenHow::Url => None,
    };
    let mut cmd = Command::new("open");
    if let Some(app) = app {
        cmd.args(["-a", app]);
    }
    match &validated {
        OpenTarget::Url(u) => {
            cmd.arg(u);
        }
        OpenTarget::Dir(d) => {
            cmd.arg(d);
        }
    }
    let out = cmd.output()?;
    anyhow::ensure!(
        out.status.success(),
        "open {target}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_with(target: &str, how: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let (kind, validated) = validate_open(target, how)?;
    let on_path = |p: &&str| {
        std::env::var_os("PATH").is_some_and(|dirs| {
            std::env::split_paths(&dirs).any(|d| {
                let candidate = d.join(p);
                candidate.is_file()
                    && std::fs::metadata(&candidate)
                        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            })
        })
    };
    let mut cmd = match (kind, &validated) {
        (OpenHow::Url, OpenTarget::Url(u)) => default_handler(std::ffi::OsStr::new(u)),
        (OpenHow::Files, OpenTarget::Dir(d)) => default_handler(d.as_os_str()),
        (OpenHow::Editor, OpenTarget::Dir(d)) => {
            match ["cursor", "code", "zed"].into_iter().find(on_path) {
                Some(editor) => {
                    let mut c = Command::new(editor);
                    c.arg("--").arg(d);
                    c
                }
                None => default_handler(d.as_os_str()),
            }
        }
        (OpenHow::Terminal, OpenTarget::Dir(d)) => {
            match ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"]
                .into_iter()
                .find(on_path)
            {
                Some("gnome-terminal") => {
                    let mut c = Command::new("gnome-terminal");
                    c.arg(format!("--working-directory={}", d.display()));
                    c
                }
                Some("konsole") => {
                    let mut c = Command::new("konsole");
                    c.arg("--workdir").arg(d);
                    c
                }
                Some(terminal) => {
                    let mut c = Command::new(terminal);
                    c.current_dir(d);
                    c
                }
                None => default_handler(d.as_os_str()),
            }
        }
        _ => anyhow::bail!("invalid open target"),
    };
    // Editors and terminals keep running; reap them off-thread to avoid zombies.
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "linux")]
fn default_handler(target: &std::ffi::OsStr) -> Command {
    let mut c = Command::new("xdg-open");
    c.arg(target);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ws-desktop-test-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_how_allowlist() {
        assert_eq!(OpenHow::parse("files").unwrap(), OpenHow::Files);
        assert_eq!(OpenHow::parse("editor").unwrap(), OpenHow::Editor);
        assert_eq!(OpenHow::parse("terminal").unwrap(), OpenHow::Terminal);
        assert_eq!(OpenHow::parse("url").unwrap(), OpenHow::Url);
        assert!(OpenHow::parse("").is_err());
        assert!(OpenHow::parse("shell").is_err());
    }

    #[test]
    fn url_validation() {
        assert!(validate_url("http://localhost:4100").is_ok());
        assert!(validate_url("https://github.com/org/repo/pull/1").is_ok());
        assert!(validate_url("").is_err());
        assert!(validate_url("-a").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("https://").is_err());
        assert!(validate_url("https://example.com/a b").is_err());
        assert!(validate_url("https://example.com/\n").is_err());
    }

    #[test]
    fn repo_and_dir_validation_rejects_unknown_and_flags() {
        let root = temp_dir("known");
        let other = temp_dir("other");
        let file = root.join("file.txt");
        std::fs::write(&file, "hi").unwrap();

        let known = vec![KnownRepo {
            root: root.clone(),
            dir: root.join(".state"),
            workspaces: 0,
        }];

        assert!(resolve_repo_in(&root.to_string_lossy(), &known).is_ok());
        assert!(resolve_repo_in(&other.to_string_lossy(), &known).is_err());
        assert!(resolve_repo_in("-bad", &known).is_err());

        assert!(validate_known_dir_in(&root.to_string_lossy(), &known).is_ok());
        assert!(validate_known_dir_in(&other.to_string_lossy(), &known).is_err());
        assert!(validate_known_dir_in(&file.to_string_lossy(), &known).is_err());
        assert!(validate_known_dir_in("-a", &known).is_err());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
    }
}
