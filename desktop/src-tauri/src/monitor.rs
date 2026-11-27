//! Watches the last `exec` of every workspace in every repository: the menu
//! bar counts the running agents, a notification says when one finishes,
//! and the window hears of every change so it refreshes at once.

use agent_ws::RunInfo;
use std::collections::HashMap;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

/// How often run records are read when an agent is running vs idle.
const ACTIVE_EVERY: Duration = Duration::from_secs(3);
const IDLE_EVERY: Duration = Duration::from_secs(8);

/// Tells the window something changed (`listen("workspaces-changed")`).
pub const CHANGED: &str = "workspaces-changed";

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // (repository root, workspace) → (start time, state) of its last run.
        let mut seen: HashMap<(String, String), (u64, String)> = HashMap::new();
        let mut repos: HashMap<std::path::PathBuf, agent_ws::Repo> = HashMap::new();
        let mut prev_running: Option<usize> = None;
        let mut first = true;
        loop {
            let mut now = HashMap::new();
            let mut running = 0;
            let known_list = agent_ws::known_repos().unwrap_or_default();
            let known_roots: std::collections::HashSet<std::path::PathBuf> =
                known_list.iter().map(|k| k.root.clone()).collect();
            repos.retain(|root, _| known_roots.contains(root));
            for known in known_list {
                let repo = match repos.entry(known.root.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let Ok(discovered) = agent_ws::Repo::discover(&known.root) else {
                            continue;
                        };
                        e.insert(discovered)
                    }
                };
                let repo_name = known
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                for (name, info) in repo.runs().unwrap_or_default() {
                    if info.state == "running" {
                        running += 1;
                    }
                    let key = (known.root.to_string_lossy().into_owned(), name.clone());
                    // The same run (same start time) was running last time.
                    let ended = info.state != "running"
                        && seen.get(&key).is_some_and(|(start, state)| {
                            *start == info.run.started && state == "running"
                        });
                    if ended && !first {
                        notify(&app, &repo_name, &name, &info);
                    }
                    now.insert(key, (info.run.started, info.state));
                }
            }
            if now != seen {
                let _ = app.emit(CHANGED, ());
            }
            if prev_running != Some(running) {
                crate::tray::update(&app, running);
                prev_running = Some(running);
            }
            seen = now;
            first = false;
            std::thread::sleep(if running > 0 {
                ACTIVE_EVERY
            } else {
                IDLE_EVERY
            });
        }
    });
}

fn notify(app: &AppHandle, repo: &str, name: &str, info: &RunInfo) {
    let title = match (info.state.as_str(), info.run.exit_code) {
        ("exited", Some(0)) => format!("{name} finished"),
        ("exited", Some(code)) => format!("{name} failed (exit {code})"),
        _ => format!("{name} was interrupted"),
    };
    let mut command = info.run.command.join(" ");
    if command.chars().count() > 80 {
        command = command.chars().take(79).collect::<String>() + "…";
    }
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(format!("{repo} · {command}"))
        .show();
}
