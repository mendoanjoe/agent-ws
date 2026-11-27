//! End-to-end tests against a throwaway repository on the temp volume.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Env {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = cmd("git", dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn cmd(bin: &str, dir: &Path) -> Command {
    let mut c = Command::new(bin);
    c.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    c
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp
            .path()
            .join("repo")
            .canonicalize()
            .unwrap_or(tmp.path().join("repo"));
        fs::create_dir(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(
            repo.join(".gitignore"),
            "node_modules/\n.env\n.env.*\n!.env.example\ndist\nout/\next\n",
        )
        .unwrap();
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        fs::write(repo.join("b.txt"), "bee\n").unwrap();
        fs::write(repo.join(".env.example"), "KEY=\n").unwrap();
        fs::create_dir(repo.join(".claude")).unwrap();
        fs::write(repo.join(".claude/settings.json"), "{}").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        // Ignored state that a workspace should inherit or drop.
        fs::create_dir_all(repo.join("node_modules/dep")).unwrap();
        fs::write(
            repo.join("node_modules/dep/index.js"),
            "module.exports = 1\n",
        )
        .unwrap();
        fs::create_dir(repo.join("out")).unwrap();
        std::os::unix::fs::symlink(repo.join("out"), repo.join("dist")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), repo.join("ext")).unwrap();
        fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
        fs::create_dir_all(repo.join(".claude/worktrees/old")).unwrap();
        fs::write(repo.join(".claude/worktrees/old/x"), "x").unwrap();
        Env {
            _tmp: tmp,
            repo,
            home,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        cmd(env!("CARGO_BIN_EXE_agent-ws"), &self.repo)
            .env("AGENT_WS_HOME", &self.home)
            // Background refills would race with the assertions.
            .env("AGENT_WS_NO_REFILL", "1")
            .env("AGENT_WS_NO_AUTO_PRUNE", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.run(&a);
        assert!(
            out.status.success(),
            "agent-ws {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "agent-ws {args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn err_json_in(&self, dir: &Path, args: &[&str]) -> serde_json::Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = cmd(env!("CARGO_BIN_EXE_agent-ws"), dir)
            .env("AGENT_WS_HOME", &self.home)
            .env("AGENT_WS_NO_REFILL", "1")
            .env("AGENT_WS_NO_AUTO_PRUNE", "1")
            .args(&a)
            .output()
            .unwrap();
        assert!(!out.status.success(), "agent-ws {a:?} should fail");
        assert!(
            out.stdout.is_empty(),
            "expected empty stdout on --json error, got: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap_or_else(|err| {
            panic!(
                "stderr was not valid JSON ({err}): {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        assert_eq!(v["ok"], false, "{v}");
        assert!(v["error"].is_string(), "{v}");
        v
    }

    fn err_json(&self, args: &[&str]) -> serde_json::Value {
        self.err_json_in(&self.repo, args)
    }
}

fn path_of(v: &serde_json::Value) -> PathBuf {
    PathBuf::from(v["path"].as_str().unwrap())
}

fn home_dir(e: &Env) -> PathBuf {
    e.home
        .read_dir()
        .unwrap()
        .flatten()
        .find(|d| d.path().is_dir())
        .unwrap()
        .path()
}

/// The agent-ws binary with this test's home, extra environment, and no
/// background work.
fn bin(e: &Env, env: &[(&str, &str)]) -> Command {
    let mut c = cmd(env!("CARGO_BIN_EXE_agent-ws"), &e.repo);
    c.env("AGENT_WS_HOME", &e.home)
        .env("AGENT_WS_NO_REFILL", "1")
        .env("AGENT_WS_NO_AUTO_PRUNE", "1")
        .envs(env.iter().copied());
    c
}

/// Spawn and reap a short-lived process to obtain a guaranteed-dead PID.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    let _ = child.wait().unwrap();
    pid
}

fn mutate_meta(dir: &Path, name: &str, f: impl FnOnce(&mut serde_json::Value)) {
    let p = dir.join("meta").join(format!("{name}.json"));
    let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    f(&mut m);
    fs::write(p, serde_json::to_vec(&m).unwrap()).unwrap();
}

fn list_names(e: &Env) -> Vec<String> {
    e.ok(&["list"])
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect()
}

fn names(v: &serde_json::Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> = v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

/// A directory with a stand-in `gh` that logs its arguments to `gh.log`
/// next to it and answers `pr create` (or `pr list` when `FAKE_GH_HEAD` is set);
/// PATH with it first.
fn fake_gh(e: &Env) -> (PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = e.home.parent().unwrap().join("fake-gh");
    fs::create_dir_all(&dir).unwrap();
    let log = dir.join("gh.log");
    fs::write(
        dir.join("gh"),
        "#!/bin/sh\n\
         log=\"${FAKE_GH_LOG:-$(dirname \"$0\")/gh.log}\"\n\
         printf '%s|' \"$@\" >> \"$log\"\necho >> \"$log\"\n\
         case \"$1 $2\" in\n  \
           'pr create') echo https://github.com/o/r/pull/7 ;;\n  \
           'pr list')\n    \
             if [ -n \"${FAKE_GH_HEAD:-}\" ]; then\n      \
               case \"$*\" in\n        \
                 *'--head agent-ws/kept'*) printf '[{\"headRefOid\":\"%s\"}]' \"$FAKE_GH_HEAD\" ;;\n        \
                 *) printf '[]' ;;\n      \
               esac\n    \
             fi\n    ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(dir.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
    (log, path)
}

#[test]
fn spawn_gives_base_commit_plus_ignored_files() {
    let e = Env::new();
    // Uncommitted work in the source must not leak into the workspace.
    fs::write(e.repo.join("a.txt"), "dirty\n").unwrap();
    fs::write(e.repo.join("scratch.txt"), "untracked\n").unwrap();

    let s = e.ok(&["spawn", "w1"]);
    let ws = path_of(&s);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "one\n");
    assert!(!ws.join("scratch.txt").exists());
    assert!(
        ws.join("node_modules/dep/index.js").exists(),
        "ignored deps are cloned"
    );
    assert!(!ws.join(".env").exists(), "secrets are removed");
    assert!(ws.join(".env.example").exists());
    assert_eq!(s["secrets_removed"], serde_json::json!([".env"]));
    assert!(ws.join(".claude/settings.json").exists());
    assert!(
        !ws.join(".claude/worktrees").exists(),
        "other agents' worktrees are skipped"
    );
    assert_eq!(
        fs::read_link(ws.join("dist")).unwrap(),
        ws.join("out"),
        "absolute symlink retargeted"
    );
    assert_eq!(
        s["symlinks_external"][0]["path"], "ext",
        "links outside the source are reported"
    );
    assert_eq!(s["symlinks_external"].as_array().unwrap().len(), 1);
    assert_eq!(git(&ws, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&ws, &["rev-parse", "HEAD"]),
        git(&e.repo, &["rev-parse", "HEAD"])
    );
    // The source keeps its own state.
    assert_eq!(fs::read_to_string(e.repo.join("a.txt")).unwrap(), "dirty\n");
    assert!(e.repo.join(".env").exists());
}

#[test]
fn workspace_writes_do_not_touch_source() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w1"]));
    fs::write(ws.join("b.txt"), "changed\n").unwrap();
    fs::write(ws.join("node_modules/dep/index.js"), "patched\n").unwrap();
    fs::write(ws.join("dist/built.js"), "x").unwrap();
    assert_eq!(fs::read_to_string(e.repo.join("b.txt")).unwrap(), "bee\n");
    assert_eq!(
        fs::read_to_string(e.repo.join("node_modules/dep/index.js")).unwrap(),
        "module.exports = 1\n"
    );
    assert!(!e.repo.join("out/built.js").exists());
}

#[test]
fn status_reports_changes_and_overlaps() {
    let e = Env::new();
    let w1 = path_of(&e.ok(&["spawn", "w1"]));
    let w2 = path_of(&e.ok(&["spawn", "w2"]));
    fs::write(w1.join("a.txt"), "w1\n").unwrap();
    fs::write(w1.join("new.txt"), "n\n").unwrap();
    fs::write(w2.join("a.txt"), "w2\n").unwrap();
    // A commit made inside the workspace still counts as a change.
    fs::write(w2.join("b.txt"), "w2\n").unwrap();
    git(&w2, &["commit", "-qam", "agent commit"]);

    let s = e.ok(&["status"]);
    assert_eq!(
        s["workspaces"][0]["changed"],
        serde_json::json!(["a.txt", "new.txt"])
    );
    assert_eq!(
        s["workspaces"][1]["changed"],
        serde_json::json!(["a.txt", "b.txt"])
    );
    assert_eq!(s["overlaps"], serde_json::json!({ "a.txt": ["w1", "w2"] }));
    let one = e.ok(&["status", "w2"]);
    assert_eq!(one["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(one["overlaps"], s["overlaps"]);
}

#[test]
fn land_commits_to_branch_and_drops() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w1"]));
    fs::write(ws.join("a.txt"), "landed\n").unwrap();
    fs::write(ws.join("c.txt"), "new\n").unwrap();
    let l = e.ok(&["land", "w1", "-m", "feat: from agent"]);
    assert_eq!(l["branch"], "agent-ws/w1");
    assert_eq!(l["files"], 2);
    assert!(!ws.exists(), "workspace removed");
    assert_eq!(git(&e.repo, &["show", "agent-ws/w1:a.txt"]), "landed");
    let msg = git(&e.repo, &["log", "-1", "--format=%B", "agent-ws/w1"]);
    assert!(msg.starts_with("feat: from agent"));
    assert!(msg.contains("Agent-Ws: w1"));
    assert!(msg.contains("Agent-Ws-Base: "));
    assert!(!git(&e.repo, &["worktree", "list"]).contains("w1"));
    assert_eq!(e.ok(&["list"]), serde_json::json!([]));
    // The source checkout and branch are untouched.
    assert_eq!(git(&e.repo, &["branch", "--show-current"]), "main");
    assert_eq!(fs::read_to_string(e.repo.join("a.txt")).unwrap(), "one\n");
}

#[test]
fn land_refuses_existing_branch_and_empty_workspace() {
    let e = Env::new();
    e.ok(&["spawn", "w1"]);
    assert!(e.err(&["land", "w1", "-m", "x"]).contains("no changes"));
    git(&e.repo, &["branch", "agent-ws/w1"]);
    let ws = path_of(&e.ok(&["spawn", "w2"]));
    fs::write(ws.join("a.txt"), "x\n").unwrap();
    assert!(
        e.err(&["land", "w2", "-m", "x", "--branch", "agent-ws/w1"])
            .contains("already exists")
    );
    assert!(ws.exists(), "failed land keeps the workspace");
}

#[test]
fn drop_and_name_rules() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w1"]));
    assert!(e.err(&["spawn", "w1"]).contains("already exists"));
    assert!(e.err(&["spawn", "../x"]).contains("invalid workspace name"));
    assert!(
        e.err(&["spawn", ".hidden"])
            .contains("invalid workspace name")
    );
    e.ok(&["drop", "w1"]);
    assert!(!ws.exists());
    assert!(e.err(&["drop", "w1"]).contains("no workspace named"));
    // The name is free again.
    e.ok(&["spawn", "w1"]);
}

#[test]
fn works_from_inside_a_workspace() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w1"]));
    let out = cmd(env!("CARGO_BIN_EXE_agent-ws"), &ws)
        .env("AGENT_WS_HOME", &e.home)
        .args(["--json", "list"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let l: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(l[0]["name"], "w1");
}

#[test]
fn parallel_spawns_all_succeed() {
    let e = Env::new();
    let outs: Vec<Output> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let e = &e;
                s.spawn(move || e.run(&["--json", "spawn", &format!("p{i}")]))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for o in &outs {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    assert_eq!(e.ok(&["list"]).as_array().unwrap().len(), 8);
    assert_eq!(git(&e.repo, &["worktree", "list"]).lines().count(), 9);
    // Landing and dropping in parallel leaves nothing behind.
    std::thread::scope(|s| {
        for i in 0..8 {
            let e = &e;
            s.spawn(move || {
                let name = format!("p{i}");
                if i % 2 == 0 {
                    let ws = path_of(&e.ok(&["path", &name]));
                    fs::write(ws.join(format!("f{i}.txt")), "x").unwrap();
                    e.ok(&["land", &name, "-m", "x"]);
                } else {
                    e.ok(&["drop", &name]);
                }
            });
        }
    });
    assert_eq!(e.ok(&["list"]), serde_json::json!([]));
    assert_eq!(git(&e.repo, &["worktree", "list"]).lines().count(), 1);
    assert_eq!(
        git(&e.repo, &["branch", "--list", "agent-ws/*"])
            .lines()
            .count(),
        4
    );
}

#[test]
fn same_name_race_has_one_winner() {
    let e = Env::new();
    let outs: Vec<Output> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..6)
            .map(|_| {
                let e = &e;
                s.spawn(move || e.run(&["spawn", "dup"]))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(outs.iter().filter(|o| o.status.success()).count(), 1);
    assert_eq!(e.ok(&["list"]).as_array().unwrap().len(), 1);
    assert_eq!(git(&e.repo, &["worktree", "list"]).lines().count(), 2);
}

#[test]
fn gc_cleans_crashes_but_not_live_spawns() {
    let e = Env::new();
    let keep = path_of(&e.ok(&["spawn", "keep"]));
    let dir = home_dir(&e);
    let set_meta = |name: &str, state: &str, pid: u32| {
        mutate_meta(&dir, name, |m| {
            m["state"] = state.into();
            m["pid"] = pid.into();
        });
    };
    // Spawn that died midway: record says creating, process is gone.
    let crashed = path_of(&e.ok(&["spawn", "crashed"]));
    set_meta("crashed", "creating", dead_pid());
    // Spawn still running in another process (this test process is alive).
    let live = path_of(&e.ok(&["spawn", "live"]));
    set_meta("live", "creating", std::process::id());
    // Folder deleted by hand.
    let gone = path_of(&e.ok(&["spawn", "gone"]));
    fs::remove_dir_all(&gone).unwrap();
    // Folder with no record, and a leftover in the trash.
    fs::create_dir(dir.join("ws/orphan")).unwrap();
    fs::create_dir_all(dir.join(".trash/leftover-1000/x")).unwrap();
    // A delete that started just now is not a leftover.
    let recent = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    fs::create_dir_all(dir.join(format!(".trash/fresh-{recent}"))).unwrap();

    assert!(
        e.err(&["land", "live", "-m", "x"])
            .contains("still being created")
    );
    let dry = e.ok(&["gc", "--dry-run"]);
    assert_eq!(dry["interrupted"], serde_json::json!(["crashed"]));
    assert_eq!(dry["missing"], serde_json::json!(["gone"]));
    assert_eq!(dry["orphans"], serde_json::json!(["orphan"]));
    assert_eq!(dry["trash"], 1);
    assert!(
        crashed.exists() && dir.join("ws/orphan").exists(),
        "dry run changes nothing"
    );

    e.ok(&["gc"]);
    assert!(!crashed.exists() && !dir.join("ws/orphan").exists());
    assert!(
        live.exists() && keep.exists(),
        "live spawn and ready workspace survive"
    );
    assert_eq!(list_names(&e), ["keep"]);
    let wt = git(&e.repo, &["worktree", "list"]);
    assert!(
        wt.contains("keep")
            && wt.contains("live")
            && !wt.contains("crashed")
            && !wt.contains("gone")
    );
    // The names are free again.
    e.ok(&["spawn", "crashed"]);
    e.ok(&["spawn", "gone"]);
    let again = e.ok(&["gc"]);
    assert_eq!(again["interrupted"], serde_json::json!([]));
}

#[test]
fn secrets_inside_ignored_folders_are_removed() {
    let e = Env::new();
    fs::write(
        e.repo.join(".gitignore"),
        "node_modules/\n.env\n.env.*\n!.env.example\ndist\nout/\next\n.wrangler/\n*.pem\n",
    )
    .unwrap();
    fs::write(e.repo.join(".env.ci"), "tracked on purpose\n").unwrap();
    git(&e.repo, &["add", "-f", ".gitignore", ".env.ci"]);
    git(&e.repo, &["commit", "-q", "-m", "ignore more"]);
    fs::create_dir_all(e.repo.join(".wrangler/state")).unwrap();
    fs::write(e.repo.join(".wrangler/state/.dev.vars"), "TOKEN=1\n").unwrap();
    fs::write(e.repo.join(".wrangler/state/cache.json"), "{}").unwrap();
    fs::write(e.repo.join("node_modules/dep/.env"), "X=1\n").unwrap();
    fs::write(e.repo.join("server.pem"), "key").unwrap();

    let s = e.ok(&["spawn", "w1"]);
    let ws = path_of(&s);
    assert_eq!(
        s["secrets_removed"],
        serde_json::json!([
            ".env",
            ".wrangler/state/.dev.vars",
            "node_modules/dep/.env",
            "server.pem"
        ])
    );
    assert!(
        ws.join(".wrangler/state/cache.json").exists(),
        "non-secret ignored files stay"
    );
    assert!(
        ws.join(".env.ci").exists(),
        "tracked files are never removed"
    );
    let kept = path_of(&e.ok(&["spawn", "w2", "--keep-secrets"]));
    assert!(kept.join(".wrangler/state/.dev.vars").exists());
}

#[test]
fn relative_symlinks_that_leave_the_repo_are_fixed_and_reported() {
    let e = Env::new();
    fs::write(
        e.repo.join(".gitignore"),
        "node_modules/\n.env\ndist\nout/\next\nup\nsub/\n",
    )
    .unwrap();
    git(&e.repo, &["commit", "-qam", "ignore links"]);
    let outside = e.repo.parent().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink("../outside", e.repo.join("up")).unwrap();
    fs::create_dir(e.repo.join("sub")).unwrap();
    std::os::unix::fs::symlink("../a.txt", e.repo.join("sub/inner")).unwrap();

    let s = e.ok(&["spawn", "w1"]);
    let ws = path_of(&s);
    assert_eq!(
        fs::read_link(ws.join("up")).unwrap(),
        outside,
        "keeps meaning the folder it meant in the source"
    );
    assert_eq!(
        fs::read_link(ws.join("sub/inner")).unwrap(),
        PathBuf::from("../a.txt"),
        "links inside stay relative"
    );
    let ext: Vec<&str> = s["symlinks_external"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["path"].as_str().unwrap())
        .collect();
    assert_eq!(ext, ["ext", "up"]);
}

#[test]
fn config_file_controls_excludes_secrets_and_strict_symlinks() {
    let e = Env::new();
    fs::write(
        e.repo.join(".agent-ws.toml"),
        "exclude = [\"node_modules\"]\nsecrets = [\"*.txt\"]\nsecrets_keep = []\n",
    )
    .unwrap();
    fs::write(e.repo.join("notes.txt"), "untracked but ignored? no").unwrap();
    let s = e.ok(&["spawn", "w1"]);
    let ws = path_of(&s);
    assert!(!ws.join("node_modules").exists(), "config exclude applied");
    assert!(
        ws.join(".env").exists(),
        "custom secret list replaces the defaults"
    );
    assert!(ws.join("a.txt").exists(), "tracked files match but stay");

    fs::write(e.repo.join(".agent-ws.toml"), "strict_symlinks = true\n").unwrap();
    assert!(e.err(&["spawn", "w2"]).contains("strict_symlinks"));
    assert!(
        e.ok(&["list"]).as_array().unwrap().len() == 1,
        "failed strict spawn leaves nothing"
    );
    fs::write(e.repo.join(".agent-ws.toml"), "typo = 1\n").unwrap();
    assert!(e.err(&["spawn", "w3"]).contains("invalid"));
}

#[test]
fn pool_serves_spawns_and_tracks_freshness() {
    let e = Env::new();
    e.ok(&["pool", "set", "2"]);
    let f = e.ok(&["pool", "fill"]);
    assert_eq!(
        (f["created"].as_u64(), f["ready"].as_u64()),
        (Some(2), Some(2))
    );

    // Uncommitted work in the source after the fill must not matter.
    let s = e.ok(&["spawn", "a1"]);
    assert_eq!(s["from_pool"], true);
    let ws = path_of(&s);
    assert!(
        ws.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("@pool-")
    );
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "one\n");
    assert!(ws.join("node_modules/dep/index.js").exists());
    assert_eq!(s["secrets_removed"], serde_json::json!([".env"]));
    assert_eq!(e.ok(&["pool", "status"])["fresh"], 1);

    // Landing a pooled workspace works like any other.
    fs::write(ws.join("a.txt"), "pooled\n").unwrap();
    e.ok(&["land", "a1", "-m", "from pool"]);
    assert_eq!(git(&e.repo, &["show", "agent-ws/a1:a.txt"]), "pooled");

    // Options that change the workspace bypass the pool.
    assert_eq!(e.ok(&["spawn", "b1", "--keep-secrets"])["from_pool"], false);
    assert_eq!(e.ok(&["pool", "status"])["fresh"], 1);

    // A new commit makes the pool refreshable (not stale); spawns refresh in-place.
    fs::write(e.repo.join("b.txt"), "moved\n").unwrap();
    git(&e.repo, &["commit", "-qam", "move head"]);
    let st = e.ok(&["pool", "status"]);
    assert_eq!(
        (
            st["fresh"].as_u64(),
            st["refreshable"].as_u64(),
            st["stale"].as_u64()
        ),
        (Some(0), Some(1), Some(0))
    );
    let refreshed_spawn = e.ok(&["spawn", "c1"]);
    assert_eq!(refreshed_spawn["from_pool"], true);
    assert_eq!(
        fs::read_to_string(path_of(&refreshed_spawn).join("b.txt")).unwrap(),
        "moved\n"
    );

    // Refill to 2, advance HEAD again, and verify `pool fill` refreshes in-place without re-cloning.
    let f = e.ok(&["pool", "fill"]);
    assert_eq!(
        (f["created"].as_u64(), f["ready"].as_u64()),
        (Some(2), Some(2))
    );
    fs::write(e.repo.join("b.txt"), "moved2\n").unwrap();
    git(&e.repo, &["commit", "-qam", "move head again"]);
    let st = e.ok(&["pool", "status"]);
    assert_eq!(
        (
            st["fresh"].as_u64(),
            st["refreshable"].as_u64(),
            st["stale"].as_u64()
        ),
        (Some(0), Some(2), Some(0))
    );
    let f = e.ok(&["pool", "fill"]);
    assert_eq!(
        (
            f["refreshed"].as_u64(),
            f["created"].as_u64(),
            f["removed_stale"].as_u64(),
            f["ready"].as_u64()
        ),
        (Some(2), Some(0), Some(0), Some(2))
    );
    let d1 = e.ok(&["spawn", "d1"]);
    assert_eq!(d1["from_pool"], true);
    assert_eq!(
        fs::read_to_string(path_of(&d1).join("b.txt")).unwrap(),
        "moved2\n"
    );

    // Dependency stamp changes make the remaining entry stale (not refreshable);
    // spawns bypass it and `pool clear` removes it.
    fs::write(e.repo.join("node_modules/.modules.yaml"), "changed").unwrap();
    let st = e.ok(&["pool", "status"]);
    assert_eq!(
        (
            st["fresh"].as_u64(),
            st["refreshable"].as_u64(),
            st["stale"].as_u64()
        ),
        (Some(0), Some(0), Some(1))
    );
    let bypassed = e.ok(&["spawn", "e1"]);
    assert_eq!(bypassed["from_pool"], false);
    assert_eq!(
        fs::read_to_string(path_of(&bypassed).join("node_modules/.modules.yaml")).unwrap(),
        "changed"
    );
    assert_eq!(e.ok(&["pool", "clear"])["removed_stale"], 1);
    let st = e.ok(&["pool", "status"]);
    assert_eq!(
        (
            st["target"].as_u64(),
            st["fresh"].as_u64(),
            st["refreshable"].as_u64(),
            st["stale"].as_u64()
        ),
        (Some(2), Some(0), Some(0), Some(0))
    );
    assert_eq!(e.ok(&["gc"])["orphans"], serde_json::json!([]));
}

#[test]
fn parallel_spawns_never_share_a_pooled_workspace() {
    let e = Env::new();
    e.ok(&["pool", "fill", "--size", "3"]);
    let outs: Vec<serde_json::Value> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..6)
            .map(|i| {
                let e = &e;
                s.spawn(move || e.ok(&["spawn", &format!("r{i}")]))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let pooled: Vec<&serde_json::Value> = outs.iter().filter(|o| o["from_pool"] == true).collect();
    assert_eq!(pooled.len(), 3, "each pooled workspace is taken once");
    let mut paths: Vec<String> = outs
        .iter()
        .map(|o| o["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 6);
    assert_eq!(e.ok(&["list"]).as_array().unwrap().len(), 6);
}

#[test]
fn land_reports_overlaps_with_other_workspaces() {
    let e = Env::new();
    let w1 = path_of(&e.ok(&["spawn", "w1"]));
    let w2 = path_of(&e.ok(&["spawn", "w2"]));
    fs::write(w1.join("a.txt"), "1\n").unwrap();
    fs::write(w1.join("b.txt"), "1\n").unwrap();
    fs::write(w2.join("a.txt"), "2\n").unwrap();
    let l = e.ok(&["land", "w1", "-m", "x"]);
    assert_eq!(l["overlaps"], serde_json::json!({ "a.txt": ["w2"] }));
}

#[test]
fn gc_recovers_dead_pool_claims() {
    let e = Env::new();
    e.ok(&["pool", "fill", "--size", "1"]);
    let pool = fs::read_dir(home_dir(&e).join("pool"))
        .unwrap()
        .flatten()
        .map(|d| d.path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .unwrap();
    let ws = path_of(&serde_json::from_slice(&fs::read(&pool).unwrap()).unwrap());
    // A spawn that died right after taking the record.
    fs::rename(&pool, pool.with_extension(format!("claim-{}", dead_pid()))).unwrap();
    let g = e.ok(&["gc"]);
    assert_eq!(g["claims"], 1);
    assert!(!ws.exists());
    assert_eq!(e.ok(&["pool", "status"])["fresh"], 0);
}

// ---- Fixes for the 2026-10-15 review (REPORT-glm.md F0-F7) ----

fn forge(dir: &Path, sub: &str, name: &str, path: &Path, state: &str, base: &str) {
    let m = serde_json::json!({
        "name": name, "path": path, "base": base, "created": 0, "state": state, "pid": 0, "key": "",
        "notes": { "symlinks_retargeted": [], "symlinks_external": [], "secrets_removed": [] }
    });
    fs::write(
        dir.join(sub).join(format!("{name}.json")),
        serde_json::to_vec(&m).unwrap(),
    )
    .unwrap();
}

#[test]
fn f0_records_pointing_outside_the_workspace_folder_are_never_followed() {
    let e = Env::new();
    e.ok(&["spawn", "w1"]);
    let dir = home_dir(&e);
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    forge(&dir, "meta", "evil", &e.repo, "ready", &head);
    forge(&dir, "pool", "@pool-1", &e.repo, "pooled", &head);

    assert!(e.err(&["drop", "evil"]).contains("corrupt"));
    assert!(e.err(&["land", "evil", "-m", "x"]).contains("corrupt"));
    assert!(e.err(&["path", "evil"]).contains("corrupt"));
    e.ok(&["pool", "clear"]);
    assert!(
        e.repo.join("a.txt").exists() && e.repo.join(".git").exists(),
        "source untouched"
    );
    assert_eq!(list_names(&e), ["w1"], "corrupt records are not listed");
    e.ok(&["status"]);

    let g = e.ok(&["gc"]);
    assert_eq!(g["corrupt"], serde_json::json!(["@pool-1", "evil"]));
    assert!(
        e.repo.join("a.txt").exists(),
        "gc only forgets corrupt records"
    );
    assert!(!dir.join("meta/evil.json").exists() && !dir.join("pool/@pool-1.json").exists());
}

#[test]
fn f1_gc_is_not_fooled_by_a_reused_pid() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "reused"]));
    // A live process (this test) whose start time differs from the spawner's.
    mutate_meta(&home_dir(&e), "reused", |m| {
        m["state"] = "creating".into();
        m["pid"] = std::process::id().into();
        m["pid_start"] = 1.into();
    });
    assert_eq!(e.ok(&["gc"])["interrupted"], serde_json::json!(["reused"]));
    assert!(!ws.exists());
}

#[test]
fn f2_land_resumes_after_a_crash_between_branch_and_drop() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "kk"]));
    fs::write(ws.join("f.txt"), "hello\n").unwrap();
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-qm", "x"]);
    let tip = git(&ws, &["rev-parse", "HEAD"]);
    git(&e.repo, &["branch", "agent-ws/kk", &tip]);
    let l = e.ok(&["land", "kk", "-m", "retry"]);
    assert_eq!(
        (l["dropped"].as_bool(), l["commit"].as_str()),
        (Some(true), Some(tip.as_str()))
    );
    assert!(!ws.exists());

    // A branch that does not match the workspace is still refused, with a hint.
    let ws = path_of(&e.ok(&["spawn", "kk2"]));
    fs::write(ws.join("a.txt"), "other\n").unwrap();
    assert!(
        e.err(&["land", "kk2", "-m", "x", "--branch", "agent-ws/kk"])
            .contains("--branch")
    );
}

#[test]
fn f4_corrupt_short_base_does_not_crash_the_cli() {
    let e = Env::new();
    e.ok(&["spawn", "w1"]);
    mutate_meta(&home_dir(&e), "w1", |m| m["base"] = "x".into());
    let out = e.run(&["list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn f5_land_marks_the_branch_even_when_the_agent_committed_everything() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "L15"]));
    fs::write(ws.join("f.txt"), "z\n").unwrap();
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-qm", "agent work"]);
    e.ok(&["land", "L15", "-m", "m15"]);
    let msg = git(&e.repo, &["log", "-1", "--format=%B", "agent-ws/L15"]);
    assert!(
        msg.starts_with("m15") && msg.contains("Agent-Ws: L15"),
        "{msg}"
    );
    assert_eq!(
        git(&e.repo, &["log", "-1", "--format=%s", "agent-ws/L15~1"]),
        "agent work"
    );
}

#[test]
fn f6_renames_count_both_paths_consistently() {
    let e = Env::new();
    let w1 = path_of(&e.ok(&["spawn", "w1"]));
    let w2 = path_of(&e.ok(&["spawn", "w2"]));
    git(&w1, &["mv", "a.txt", "renamed.txt"]);
    fs::write(w2.join("a.txt"), "edited\n").unwrap();
    let s = e.ok(&["status", "w1"]);
    assert_eq!(
        s["workspaces"][0]["changed"],
        serde_json::json!(["a.txt", "renamed.txt"])
    );
    assert_eq!(
        s["overlaps"],
        serde_json::json!({ "a.txt": ["w1", "w2"] }),
        "removing a.txt overlaps an edit of it"
    );
    git(&w1, &["commit", "-qm", "rename"]);
    fs::write(w1.join("b.txt"), "b\n").unwrap();
    assert_eq!(e.ok(&["land", "w1", "-m", "x"])["files"], 3);
}

#[test]
fn f7_submodules_get_their_own_isolated_git_dir() {
    let e = Env::new();
    let inner = e.repo.parent().unwrap().join("inner");
    fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    fs::write(inner.join("i.txt"), "i\n").unwrap();
    git(&inner, &["add", "-A"]);
    git(&inner, &["commit", "-qm", "i"]);
    git(
        &e.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            inner.to_str().unwrap(),
            "sm",
        ],
    );
    git(&e.repo, &["commit", "-qm", "add sm"]);
    let src_sm_head = git(&e.repo.join("sm"), &["rev-parse", "HEAD"]);
    fs::write(e.repo.join("sm/i.txt"), "dirty in source\n").unwrap();

    let ws = path_of(&e.ok(&["spawn", "s1"]));
    assert!(ws.join("sm/.git").is_dir(), "submodule has its own git dir");
    assert_eq!(git(&ws, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(ws.join("sm/i.txt")).unwrap(),
        "i\n",
        "source edits do not leak"
    );

    fs::write(ws.join("sm/new.txt"), "n\n").unwrap();
    git(&ws.join("sm"), &["add", "-A"]);
    git(&ws.join("sm"), &["commit", "-qm", "inner change"]);
    let ws_sm_head = git(&ws.join("sm"), &["rev-parse", "HEAD"]);
    assert_eq!(
        git(&e.repo.join("sm"), &["rev-parse", "HEAD"]),
        src_sm_head,
        "source submodule untouched"
    );
    assert_eq!(
        fs::read_to_string(e.repo.join("sm/i.txt")).unwrap(),
        "dirty in source\n"
    );

    e.ok(&["land", "s1", "-m", "bump sm"]);
    let tree = git(&e.repo, &["ls-tree", "agent-ws/s1", "sm"]);
    assert!(
        tree.contains(&ws_sm_head),
        "landed branch records the new submodule commit: {tree}"
    );
}

// ---- Fixes for the re-review (REPORT-glm-2.md N1-N3) ----

#[test]
fn n1_paths_must_match_exactly_not_just_by_components() {
    let e = Env::new();
    let victim = path_of(&e.ok(&["spawn", "victim"]));
    let dir = home_dir(&e);
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    let slash = PathBuf::from(format!("{}/", victim.display()));
    let double = PathBuf::from(victim.display().to_string().replace("/ws/", "/ws//"));
    forge(&dir, "meta", "evil", &slash, "ready", &head);
    forge(&dir, "meta", "evil2", &double, "ready", &head);
    forge(&dir, "pool", "@pool-7", &slash, "creating", &head);

    assert!(e.err(&["drop", "evil"]).contains("corrupt"));
    assert!(e.err(&["drop", "evil2"]).contains("corrupt"));
    let g = e.ok(&["gc"]);
    assert_eq!(
        g["corrupt"],
        serde_json::json!(["@pool-7", "evil", "evil2"])
    );
    assert!(
        victim.join("a.txt").exists(),
        "the named workspace survives"
    );
    assert_eq!(e.ok(&["path", "victim"])["path"], serde_json::json!(victim));
}

#[test]
fn n2_land_again_after_keep_fast_forwards_its_own_branch() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "k"]));
    fs::write(ws.join("a.txt"), "first\n").unwrap();
    let first = e.ok(&["land", "k", "-m", "first", "--keep"]);
    fs::write(ws.join("b.txt"), "second\n").unwrap();
    let second = e.ok(&["land", "k", "-m", "second"]);
    assert_eq!(second["branch"], "agent-ws/k");
    assert_eq!(
        git(&e.repo, &["rev-parse", "agent-ws/k~1"]),
        first["commit"].as_str().unwrap(),
        "fast-forward on top of the first land"
    );
    assert!(git(&e.repo, &["log", "-1", "--format=%B", "agent-ws/k"]).starts_with("second"));
    assert!(!ws.exists());

    // A branch this workspace did not land is still refused, even when it is an ancestor.
    let ws = path_of(&e.ok(&["spawn", "k2"]));
    git(&e.repo, &["branch", "agent-ws/k2", "HEAD"]);
    fs::write(ws.join("a.txt"), "x\n").unwrap();
    assert!(e.err(&["land", "k2", "-m", "x"]).contains("--branch"));
}

#[test]
fn n3_untracked_unignored_secrets_are_reported_and_keepable() {
    let e = Env::new();
    fs::write(e.repo.join("cert.pem"), "key").unwrap();
    fs::write(e.repo.join("scratch.txt"), "work in progress").unwrap();
    fs::create_dir(e.repo.join("conf")).unwrap();
    fs::write(e.repo.join("conf/id_rsa"), "key").unwrap();

    let s = e.ok(&["spawn", "w1"]);
    let ws = path_of(&s);
    assert_eq!(
        s["secrets_removed"],
        serde_json::json!([".env", "cert.pem", "conf/id_rsa"])
    );
    assert!(!ws.join("cert.pem").exists() && !ws.join("scratch.txt").exists());

    let kept = path_of(&e.ok(&["spawn", "w2", "--keep-secrets"]));
    assert_eq!(fs::read_to_string(kept.join("cert.pem")).unwrap(), "key");
    assert!(kept.join("conf/id_rsa").exists() && kept.join(".env").exists());
    assert!(
        !kept.join("scratch.txt").exists(),
        "other untracked files are still left out"
    );
}

#[test]
fn p1_prune_drops_only_clean_merged_workspaces() {
    let e = Env::new();
    let commit = |ws: &Path, file: &str, text: &str| {
        fs::write(ws.join(file), text).unwrap();
        git(ws, &["add", "-A"]);
        git(ws, &["commit", "-q", "-m", file]);
        git(ws, &["rev-parse", "HEAD"])
    };
    // Merged with a fast-forward.
    let merged = path_of(&e.ok(&["spawn", "merged"]));
    let merged_head = commit(&merged, "c.txt", "see\n");
    // Rebuilt deps after the merge: ignored files never make a tree dirty.
    fs::write(merged.join("node_modules/dep/index.js"), "rebuilt\n").unwrap();
    // Squash-merged: the same change lands as a different commit.
    let squashed = path_of(&e.ok(&["spawn", "squashed"]));
    commit(&squashed, "b.txt", "bee 2\n");
    commit(&squashed, "d.txt", "dee\n");
    // Never used, uncommitted work, unmerged commits, and an agent inside.
    e.ok(&["spawn", "unused"]);
    let dirty = path_of(&e.ok(&["spawn", "dirty"]));
    fs::write(dirty.join("a.txt"), "edited\n").unwrap();
    let ahead = path_of(&e.ok(&["spawn", "ahead"]));
    commit(&ahead, "e.txt", "only here\n");
    let busy = path_of(&e.ok(&["spawn", "busy"]));
    let mut agent = Command::new("sleep")
        .arg("60")
        .current_dir(&busy)
        .spawn()
        .unwrap();

    git(&e.repo, &["merge", "-q", "--ff-only", &merged_head]);
    fs::write(e.repo.join("b.txt"), "bee 2\n").unwrap();
    fs::write(e.repo.join("d.txt"), "dee\n").unwrap();
    git(&e.repo, &["add", "b.txt", "d.txt"]);
    git(&e.repo, &["commit", "-q", "-m", "squash"]);

    // Young workspaces are left alone by default.
    let young = e.ok(&["prune", "--dry-run", "--base", "main"]);
    assert!(names(&young, "pruned").is_empty());

    let args = ["prune", "--base", "main", "--min-age-min", "0"];
    let dry = e.ok(&[&args[..], &["--dry-run"]].concat());
    assert_eq!(names(&dry, "pruned"), ["merged", "squashed", "unused"]);
    assert_eq!(names(&dry, "kept"), ["ahead", "busy", "dirty"]);
    let reasons: Vec<&str> = dry["pruned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["merged", "squash-merged", "unused"]);
    // Unflushed writes may not be allocated yet, so only the sum is exact.
    let sum: u64 = dry["pruned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["private_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(dry["freed_bytes"].as_u64(), Some(sum));
    assert!(
        merged.exists() && squashed.exists(),
        "dry run changes nothing"
    );

    let done = e.ok(&args);
    assert_eq!(names(&done, "pruned"), ["merged", "squashed", "unused"]);
    agent.kill().unwrap();
    agent.wait().unwrap();
    assert!(!merged.exists() && !squashed.exists());
    assert_eq!(list_names(&e), ["ahead", "busy", "dirty"]);
    // The agent left: the busy workspace is now an unused one.
    let again = e.ok(&args);
    assert_eq!(names(&again, "pruned"), ["busy"]);
    assert!(e.err(&["prune", "--base", "nope"]).contains("nope"));
}

#[test]
fn p2_du_reports_real_unshared_bytes() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w"]));
    e.ok(&["spawn", "idle"]);
    let du = e.ok(&["du"]);
    let idle = &du["entries"][0];
    assert_eq!(idle["name"], "idle");
    assert_eq!(idle["kind"], "workspace");
    assert!(idle["allocated"].as_u64().unwrap() > 0);

    {
        use std::io::Write;
        // Incompressible bytes so transparent filesystem compression (e.g. Btrfs)
        // still allocates >= 2 MiB of private extents.
        let mut x: u32 = 0x1234_5678;
        let data: Vec<u8> = (0..(2 << 20))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect();
        let mut f = fs::File::create(ws.join("big.bin")).unwrap();
        f.write_all(&data).unwrap();
        f.sync_all().unwrap();
    }
    let du = e.ok(&["du", "w"]);
    assert_eq!(du["entries"].as_array().unwrap().len(), 1);
    let w = &du["entries"][0];
    assert!(w["private"].as_u64().unwrap() >= 2 << 20);
    assert!(w["allocated"].as_u64() >= w["private"].as_u64());
    assert_eq!(du["total"]["private"], w["private"]);
    assert!(du["free_bytes"].as_u64().unwrap() > 0);

    // Forgotten trash shows up, and the next drop finishes deleting it.
    let leftover = home_dir(&e).join(".trash/leftover-1000");
    fs::create_dir_all(&leftover).unwrap();
    fs::write(leftover.join("x"), "x").unwrap();
    let du = e.ok(&["du"]);
    assert!(
        du["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["kind"] == "trash" && t["name"] == "leftover-1000")
    );
    e.ok(&["drop", "idle"]);
    for _ in 0..100 {
        if !leftover.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!leftover.exists(), "drop restarts stale trash deletes");
}

#[test]
fn p3_pool_is_capped_and_spawn_warns() {
    let e = Env::new();
    assert!(e.err(&["pool", "set", "9"]).contains("maximum"));
    e.ok(&["pool", "set", "8"]);
    e.ok(&["pool", "set", "0"]);

    fs::write(e.repo.join(".agent-ws.toml"), "warn_workspaces = 2\n").unwrap();
    let first = e.ok(&["spawn", "one"]);
    assert!(
        !first["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("workspaces exist"))
    );
    let second = e.ok(&["spawn", "two"]);
    assert!(
        second["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("2 workspaces exist"))
    );
    let human = String::from_utf8(e.run(&["spawn", "three"]).stdout).unwrap();
    assert!(human.contains("WARNING 3 workspaces exist"), "{human}");
}

#[test]
fn p4_gc_registers_orphans_that_hold_work_instead_of_deleting_them() {
    let e = Env::new();
    let dirty = path_of(&e.ok(&["spawn", "dirty"]));
    fs::write(dirty.join("a.txt"), "unsaved\n").unwrap();
    // Commits on a detached HEAD live nowhere else.
    let detached = path_of(&e.ok(&["spawn", "detached"]));
    fs::write(detached.join("f.txt"), "eff\n").unwrap();
    git(&detached, &["add", "-A"]);
    git(&detached, &["commit", "-q", "-m", "f"]);
    let clean = path_of(&e.ok(&["spawn", "clean"]));
    let dir = home_dir(&e);
    for n in ["dirty", "detached", "clean"] {
        fs::remove_file(dir.join(format!("meta/{n}.json"))).unwrap();
    }
    fs::create_dir(dir.join("ws/notgit")).unwrap();
    fs::write(dir.join("ws/notgit/notes.txt"), "keep me").unwrap();

    let dry = e.ok(&["gc", "--dry-run"]);
    assert_eq!(dry["adopted"], serde_json::json!(["detached", "dirty"]));
    assert_eq!(dry["orphans"], serde_json::json!(["clean"]));
    assert_eq!(dry["unreadable"], serde_json::json!(["notgit"]));
    assert!(e.ok(&["list"]).as_array().unwrap().is_empty());

    e.ok(&["gc"]);
    assert!(!clean.exists());
    assert!(dirty.exists() && detached.exists() && dir.join("ws/notgit/notes.txt").exists());
    let st = e.ok(&["status", "dirty"]);
    assert_eq!(st["workspaces"][0]["changed"], serde_json::json!(["a.txt"]));
    let landed = e.ok(&["land", "detached", "-m", "rescued"]);
    assert_eq!(landed["files"], 1);
    assert_eq!(
        git(&e.repo, &["show", "agent-ws/detached:f.txt"]),
        "eff",
        "the rescued commit reached a branch"
    );
}

#[test]
fn p5_spawn_prunes_merged_workspaces_in_the_background() {
    let e = Env::new();
    let age = |name: &str| mutate_meta(&home_dir(&e), name, |m| m["created"] = 0.into());
    let merged = path_of(&e.ok(&["spawn", "merged"]));
    fs::write(merged.join("c.txt"), "see\n").unwrap();
    git(&merged, &["add", "-A"]);
    git(&merged, &["commit", "-q", "-m", "c"]);
    git(
        &e.repo,
        &[
            "merge",
            "-q",
            "--ff-only",
            &git(&merged, &["rev-parse", "HEAD"]),
        ],
    );
    let unused = path_of(&e.ok(&["spawn", "unused"]));
    age("merged");
    age("unused");

    let spawn_with_auto_prune = |name: &str| {
        let out = cmd(env!("CARGO_BIN_EXE_agent-ws"), &e.repo)
            .env("AGENT_WS_HOME", &e.home)
            .env("AGENT_WS_NO_REFILL", "1")
            .args(["spawn", name])
            .output()
            .unwrap();
        assert!(out.status.success());
    };
    spawn_with_auto_prune("trigger");
    for _ in 0..200 {
        if !merged.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!merged.exists(), "the merged workspace was pruned");
    assert!(unused.exists(), "never-used workspaces stay");
    assert!(home_dir(&e).join("auto-prune.stamp").exists());

    // `auto_prune = false` turns it off.
    fs::remove_file(home_dir(&e).join("auto-prune.stamp")).unwrap();
    fs::write(e.repo.join(".agent-ws.toml"), "auto_prune = false\n").unwrap();
    spawn_with_auto_prune("quiet");
    assert!(!home_dir(&e).join("auto-prune.stamp").exists());
}

#[test]
fn r1_certificate_files_stay_private_keys_go() {
    let e = Env::new();
    fs::write(
        e.repo.join(".gitignore"),
        "node_modules/\n.env\n.env.*\n!.env.example\ndist\nout/\next\n.venv/\ncerts/\n",
    )
    .unwrap();
    git(&e.repo, &["commit", "-q", "-am", "ignore venv"]);
    // certifi's CA bundle: comments and certificates, nothing secret. pip
    // and requests fail every HTTPS call without it.
    let bundle = "# Issuer: CN=Example Root\n\
                  -----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n-----END CERTIFICATE-----\n\n\
                  # Issuer: CN=Other Root\n\
                  -----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIV\n-----END CERTIFICATE-----\n";
    let certifi = e.repo.join(".venv/lib/python3.12/site-packages/certifi");
    fs::create_dir_all(&certifi).unwrap();
    fs::write(certifi.join("cacert.pem"), bundle).unwrap();
    // A certificate with its key, bare keys, and a non-PEM `.key`: secret.
    fs::create_dir(e.repo.join("certs")).unwrap();
    fs::write(
        e.repo.join("certs/dev.pem"),
        format!("{bundle}-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----\n"),
    )
    .unwrap();
    fs::write(
        e.repo.join("certs/dev.key"),
        "-----BEGIN EC PRIVATE KEY-----\nMHcC\n-----END EC PRIVATE KEY-----\n",
    )
    .unwrap();
    fs::write(e.repo.join("certs/raw.key"), [0x30u8, 0x82, 0x04, 0xa4]).unwrap();
    fs::write(
        e.repo.join("node_modules/dep/server.key"),
        "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----\n",
    )
    .unwrap();

    let s = e.ok(&["spawn", "w"]);
    let ws = path_of(&s);
    assert_eq!(
        fs::read_to_string(ws.join(".venv/lib/python3.12/site-packages/certifi/cacert.pem"))
            .unwrap(),
        bundle,
        "certificates are public"
    );
    assert_eq!(
        s["secrets_removed"],
        serde_json::json!([
            ".env",
            "certs/dev.key",
            "certs/dev.pem",
            "certs/raw.key",
            "node_modules/dep/server.key"
        ])
    );
}

#[test]
fn r2_status_leaves_the_workspace_index_alone() {
    use std::os::unix::fs::MetadataExt;
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w"]));
    let index = PathBuf::from(git(&ws, &["rev-parse", "--absolute-git-dir"])).join("index");
    // Same content, new mtime: Git refreshes the index and rewrites it,
    // taking index.lock, unless told not to. An agent committing at that
    // moment fails with "index.lock: File exists".
    let touched = Command::new("touch")
        .args(["-t", "200101010000"])
        .arg(ws.join("a.txt"))
        .status()
        .unwrap();
    assert!(touched.success());
    let before = fs::metadata(&index).unwrap().ino();
    e.ok(&["status"]);
    e.ok(&["status", "w"]);
    assert_eq!(
        fs::metadata(&index).unwrap().ino(),
        before,
        "status must not rewrite the workspace index"
    );
}

#[test]
fn r3_names_and_branches_git_rejects_fail_before_any_commit() {
    let e = Env::new();
    for bad in ["fix.lock", "a..b", "v2."] {
        let err = e.err(&["spawn", bad]);
        assert!(err.contains("invalid workspace name"), "{bad}: {err}");
    }
    assert!(e.ok(&["list"]).as_array().unwrap().is_empty());

    let ws = path_of(&e.ok(&["spawn", "w"]));
    let base = git(&ws, &["rev-parse", "HEAD"]);
    fs::write(ws.join("a.txt"), "work\n").unwrap();
    for bad in ["bad..name", "-x", "HEAD", "has space", "@{-1}"] {
        let err = e.err(&["land", "w", "-m", "x", &format!("--branch={bad}")]);
        assert!(err.contains("not a valid branch name"), "{bad}: {err}");
    }
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), base, "no commit was made");
    let landed = e.ok(&["land", "w", "-m", "x", "--branch", "good"]);
    assert_eq!(landed["branch"], "good");
    assert_eq!(
        git(&e.repo, &["rev-list", "--count", "main..good"]),
        "1",
        "exactly one commit"
    );
}

#[test]
fn r4_prune_gh_asks_about_the_branch_land_keep_created() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "kept"]));
    fs::write(ws.join("c.txt"), "see\n").unwrap();
    let landed = e.ok(&["land", "kept", "-m", "c", "--keep"]);
    let head = landed["commit"].as_str().unwrap().to_string();
    // main has not fetched the merge: only GitHub knows the PR was merged.
    // A stand-in `gh` reports a merged PR from agent-ws/kept at `head`.
    let (_log, path) = fake_gh(&e);
    let prune = |extra: &[&str]| -> serde_json::Value {
        let out = bin(&e, &[("PATH", &path), ("FAKE_GH_HEAD", &head)])
            .args(["--json", "prune", "--base", "main", "--min-age-min", "0"])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let without = prune(&["--dry-run"]);
    assert_eq!(names(&without, "kept"), ["kept"]);
    let with = prune(&["--dry-run", "--gh"]);
    assert_eq!(names(&with, "pruned"), ["kept"]);
    assert_eq!(with["pruned"][0]["reason"], "pr-merged");
}

#[test]
fn r5_a_broken_workspace_does_not_block_landing_another() {
    let e = Env::new();
    let good = path_of(&e.ok(&["spawn", "good"]));
    let broken = path_of(&e.ok(&["spawn", "broken"]));
    fs::remove_file(broken.join(".git")).unwrap();
    fs::write(good.join("a.txt"), "work\n").unwrap();
    let landed = e.ok(&["land", "good", "-m", "x"]);
    assert_eq!(landed["files"], 1);
    // `status` still fails, and says which workspace Git cannot read.
    assert!(e.err(&["status"]).contains("broken"));
}

#[test]
fn r6_worktree_registry_changes_take_turns() {
    use std::os::fd::AsRawFd;
    // `git worktree prune` deletes `.git/worktrees` once it is empty, and
    // Git 2.56 dies with "Invalid path" when a parallel prune or add still
    // uses it: lands and drops in parallel failed after doing their work.
    // Hold the lock another agent-ws process would hold: drop must wait.
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w"]));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home_dir(&e).join("worktrees.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let mut child = bin(&e, &[]).args(["drop", "w"]).spawn().unwrap();
    // `drop` renames `ws/w` into `.trash` right before calling `worktree prune`
    // (which waits on `worktrees.lock`), so polling `!ws.exists()` proves the
    // child actually started and reached the lock.
    for _ in 0..200 {
        if !ws.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !ws.exists(),
        "drop moved workspace before locking worktrees"
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    let early = child.try_wait().unwrap();
    drop(lock);
    let status = child.wait().unwrap();
    assert!(early.is_none(), "drop waits for the worktree lock");
    assert!(status.success());
    assert_eq!(git(&e.repo, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn s1_diff_shows_commits_edits_and_new_files_without_touching_the_index() {
    use std::os::unix::fs::MetadataExt;
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w"]));
    fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
    git(&ws, &["commit", "-q", "-am", "edit a"]);
    fs::write(ws.join("b.txt"), "bee\nbuzz\n").unwrap();
    fs::write(ws.join("new.txt"), "fresh\n").unwrap();
    fs::remove_file(ws.join(".env.example")).unwrap();
    let index = PathBuf::from(git(&ws, &["rev-parse", "--absolute-git-dir"])).join("index");
    let before = fs::metadata(&index).unwrap().ino();

    let files = e.ok(&["diff", "w"]);
    assert_eq!(
        files,
        serde_json::json!([
            {"path": ".env.example", "status": "D", "added": 0, "deleted": 1},
            {"path": "a.txt", "status": "M", "added": 1, "deleted": 0},
            {"path": "b.txt", "status": "M", "added": 1, "deleted": 0},
            {"path": "new.txt", "status": "A", "added": 1, "deleted": 0},
        ])
    );
    assert_eq!(e.ok(&["diff", "w", "new.txt"]).as_array().unwrap().len(), 1);
    let patch = String::from_utf8(e.run(&["diff", "w"]).stdout).unwrap();
    assert!(
        patch.contains("+two") && patch.contains("+fresh"),
        "{patch}"
    );
    assert_eq!(
        fs::metadata(&index).unwrap().ino(),
        before,
        "the workspace index is untouched"
    );
    assert!(
        git(&ws, &["status", "--porcelain"]).contains("?? new.txt"),
        "nothing was staged"
    );
}

#[test]
fn s2_status_tells_conflicts_from_overlaps_that_merge_cleanly() {
    let e = Env::new();
    fs::write(e.repo.join("f.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n").unwrap();
    git(&e.repo, &["add", "f.txt"]);
    git(&e.repo, &["commit", "-q", "-m", "f"]);
    let top = path_of(&e.ok(&["spawn", "top"]));
    let bottom = path_of(&e.ok(&["spawn", "bottom"]));
    let clash = path_of(&e.ok(&["spawn", "clash"]));
    // Uncommitted and committed edits both count.
    fs::write(top.join("f.txt"), "one\n2\n3\n4\n5\n6\n7\n8\n").unwrap();
    fs::write(bottom.join("f.txt"), "1\n2\n3\n4\n5\n6\n7\neight\n").unwrap();
    git(&bottom, &["commit", "-q", "-am", "bottom"]);
    fs::write(clash.join("f.txt"), "1\n2\n3\n4\n5\n6\n7\nEIGHT\n").unwrap();

    let s = e.ok(&["status"]);
    assert_eq!(
        s["overlaps"]["f.txt"],
        serde_json::json!(["bottom", "clash", "top"])
    );
    assert_eq!(
        s["conflicts"],
        serde_json::json!([{"workspaces": ["bottom", "clash"], "paths": ["f.txt"]}])
    );
    let human = String::from_utf8(e.run(&["status"]).stdout).unwrap();
    assert!(human.contains("CONFLICT bottom + clash: f.txt"), "{human}");
    // Polling adds no objects once nothing changes.
    let count = || git(&e.repo, &["count-objects"]);
    let before = count();
    e.ok(&["status"]);
    assert_eq!(count(), before);
}

#[test]
fn s3_carry_brings_uncommitted_work_along() {
    let e = Env::new();
    git(&e.repo, &["commit", "-q", "--allow-empty", "-m", "two"]);
    e.ok(&["pool", "set", "1"]);
    e.ok(&["pool", "fill"]);
    fs::write(e.repo.join("a.txt"), "edited in the checkout\n").unwrap();
    fs::write(e.repo.join("notes.md"), "draft\n").unwrap();
    fs::remove_file(e.repo.join("b.txt")).unwrap();
    fs::write(e.repo.join("id_rsa"), "key").unwrap();

    let s = e.ok(&["spawn", "w", "--carry"]);
    let ws = path_of(&s);
    assert_eq!(s["from_pool"], false, "the pool holds clean workspaces");
    assert_eq!(
        s["carried"],
        serde_json::json!(["a.txt", "b.txt", "notes.md"])
    );
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "edited in the checkout\n"
    );
    assert!(ws.join("notes.md").exists() && !ws.join("b.txt").exists());
    assert!(!ws.join("id_rsa").exists(), "secrets still stay behind");
    assert_eq!(
        e.ok(&["status", "w"])["workspaces"][0]["changed"],
        serde_json::json!(["a.txt", "b.txt", "notes.md"]),
        "uncommitted changes on HEAD"
    );
    assert_eq!(
        fs::read_to_string(e.repo.join("a.txt")).unwrap(),
        "edited in the checkout\n",
        "the checkout keeps its edits"
    );
    let err = e.err(&["spawn", "w2", "--carry", "--base", "HEAD~1"]);
    assert!(err.contains("base must be HEAD"), "{err}");
}

#[test]
fn s4_land_squash_and_pull_request() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "sq"]));
    for f in ["c.txt", "d.txt"] {
        fs::write(ws.join(f), "x\n").unwrap();
        git(&ws, &["add", "-A"]);
        git(&ws, &["commit", "-q", "-m", f]);
    }
    fs::write(ws.join("e.txt"), "uncommitted\n").unwrap();
    e.ok(&["land", "sq", "-m", "feat: squashed", "--squash"]);
    assert_eq!(
        git(&e.repo, &["rev-list", "--count", "main..agent-ws/sq"]),
        "1"
    );
    assert_eq!(
        git(&e.repo, &["log", "-1", "--format=%s", "agent-ws/sq"]),
        "feat: squashed"
    );
    let files = git(&e.repo, &["ls-tree", "--name-only", "agent-ws/sq"]);
    assert!(
        ["c.txt", "d.txt", "e.txt"]
            .iter()
            .all(|f| files.contains(f))
    );

    // No remote yet: the land stops after the branch, keeping the workspace.
    let ws = path_of(&e.ok(&["spawn", "feat"]));
    fs::write(ws.join("f.txt"), "eff\n").unwrap();
    let (log, path) = fake_gh(&e);
    let land = || {
        bin(&e, &[("PATH", &path)])
            .args(["--json", "land", "feat", "-m", "feat: f\n\nWhy it matters."])
            .args(["--pr", "--pr-base", "main", "--draft"])
            .output()
            .unwrap()
    };
    let failed = land();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("run the same command again"));
    assert!(ws.exists());
    let origin = e.home.parent().unwrap().join("origin.git");
    git(&e.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &e.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    let out = land();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let l: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(l["pr_url"], "https://github.com/o/r/pull/7");
    assert_eq!(l["pushed"], true);
    assert_eq!(
        git(&origin, &["rev-parse", "agent-ws/feat"]),
        l["commit"].as_str().unwrap()
    );
    assert_eq!(
        git(&e.repo, &["rev-list", "--count", "main..agent-ws/feat"]),
        "1",
        "the retry resumed instead of committing again"
    );
    // The body spans lines, so the logged `pr create` call does too.
    let calls = fs::read_to_string(&log).unwrap();
    let create = &calls[calls.find("pr|create|").unwrap()..];
    assert!(
        create.starts_with("pr|create|--head|agent-ws/feat|--title|feat: f|--body|Why it matters."),
        "{create}"
    );
    assert!(
        create
            .contains("Landed with agent-ws from workspace `feat` (1 files).|--base|main|--draft|"),
        "{create}"
    );
    assert!(!ws.exists());
}

#[test]
fn s5_exec_runs_in_the_workspace_with_its_port_and_records_the_run() {
    let e = Env::new();
    let a = path_of(&e.ok(&["spawn", "a"]));
    assert_eq!(e.ok(&["spawn", "b"])["port"], 4110, "slot 1");
    let out = e.run(&[
        "exec",
        "a",
        "--",
        "sh",
        "-c",
        "echo \"$PORT $AGENT_WS_NAME $(pwd -P)\"; exit 3",
    ]);
    assert_eq!(out.status.code(), Some(3), "the command's exit code");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("4100 a {}", a.canonicalize().unwrap().display())
    );
    let long = e.ok(&["list", "--long"]);
    assert_eq!(long[0]["name"], "a");
    assert_eq!(long[0]["port"], 4100);
    assert_eq!(long[0]["run"]["state"], "exited");
    assert_eq!(long[0]["run"]["exit_code"], 3);
    assert_eq!(
        (long[0]["dirty"].clone(), long[0]["ahead"].clone()),
        (false.into(), 0.into())
    );

    // A running command shows up, with its process inside the workspace.
    let mut child = bin(&e, &[])
        .args(["exec", "b", "--", "sleep", "30"])
        .spawn()
        .unwrap();
    let mut running = serde_json::Value::Null;
    for _ in 0..100 {
        running = e.ok(&["list", "--long"])[1].clone();
        if running["run"]["state"] == "running"
            && !running["processes"].as_array().unwrap().is_empty()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(running["run"]["state"], "running");
    let pid = running["run"]["pid"].as_u64().unwrap();
    assert!(
        running["processes"]
            .as_array()
            .unwrap()
            .contains(&pid.into())
    );
    Command::new("kill").arg(pid.to_string()).status().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(143), "128 + SIGTERM");
    assert_eq!(e.ok(&["list", "--long"])[1]["run"]["exit_code"], 143);

    // Dropping frees the slot; --spawn creates the workspace first.
    e.ok(&["drop", "a"]);
    assert_eq!(e.ok(&["spawn", "c"])["port"], 4100);
    assert!(
        e.run(&["exec", "d", "--spawn", "--", "true"])
            .status
            .success()
    );
    assert_eq!(e.ok(&["list"]).as_array().unwrap().len(), 3);
    assert!(
        e.err(&["exec", "nope", "--", "true"])
            .contains("no workspace named")
    );
}

#[test]
fn s6_hooks_run_only_once_trusted() {
    let e = Env::new();
    fs::write(
        e.repo.join(".agent-ws.toml"),
        "[hooks]\npost_spawn = [\"echo \\\"$AGENT_WS_NAME:$PORT\\\" > .ready; echo noisy\"]\npre_land = [\"test -f ok\"]\n",
    )
    .unwrap();
    let err = e.err(&["spawn", "w"]);
    assert!(
        err.contains("agent-ws trust") && err.contains("post_spawn: echo"),
        "{err}"
    );
    assert!(e.ok(&["list"]).as_array().unwrap().is_empty());

    e.ok(&["trust"]);
    // `ok` parses stdout as JSON: the hook's own output went to stderr.
    let ws = path_of(&e.ok(&["spawn", "w"]));
    assert_eq!(fs::read_to_string(ws.join(".ready")).unwrap(), "w:4100\n");
    fs::write(ws.join("a.txt"), "x\n").unwrap();
    let head = git(&ws, &["rev-parse", "HEAD"]);
    assert!(
        e.err(&["land", "w", "-m", "x"])
            .contains("pre_land hook `test -f ok` failed")
    );
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), head, "nothing committed");
    fs::write(ws.join("ok"), "").unwrap();
    e.ok(&["land", "w", "-m", "x"]);

    // Changed hooks need trust again; a failing post_spawn undoes the spawn.
    fs::write(
        e.repo.join(".agent-ws.toml"),
        "[hooks]\npost_spawn = [\"exit 7\"]\n",
    )
    .unwrap();
    assert!(e.err(&["spawn", "v"]).contains("agent-ws trust"));
    e.ok(&["trust"]);
    let err = e.err(&["spawn", "v"]);
    assert!(err.contains("post_spawn hook `exit 7` failed"), "{err}");
    assert!(e.ok(&["list"]).as_array().unwrap().is_empty());
    assert!(!home_dir(&e).join("ws/v").exists());
    e.ok(&["spawn", "v", "--no-hooks"]);
}

/// Run `agent-ws claude hook` with Claude Code's JSON on stdin.
fn claude_hook(e: &Env, input: serde_json::Value) -> Output {
    use std::io::Write;
    let mut child = bin(e, &[])
        .args(["claude", "hook"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn s7_claude_code_worktrees_come_from_agent_ws() {
    let e = Env::new();
    let create = |name: &str| {
        let out = claude_hook(
            &e,
            serde_json::json!({"session_id": "s", "cwd": e.repo, "hook_event_name": "WorktreeCreate", "name": name}),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            stdout.lines().count(),
            1,
            "stdout is the path alone: {stdout:?}"
        );
        PathBuf::from(stdout.trim())
    };
    let remove = |p: &Path| {
        claude_hook(
            &e,
            serde_json::json!({"session_id": "s", "cwd": e.repo, "hook_event_name": "WorktreeRemove", "worktree_path": p}),
        )
    };
    let ws = create("bold-oak-a3f2");
    assert!(
        ws.join("node_modules/dep/index.js").exists(),
        "dependencies come along"
    );
    assert_eq!(
        git(&ws, &["symbolic-ref", "--short", "HEAD"]),
        "agent-ws/bold-oak-a3f2"
    );
    let again = create("bold-oak-a3f2");
    assert!(again.ends_with("bold-oak-a3f2-2"), "{}", again.display());
    assert_eq!(e.ok(&["list"])[0]["origin"], "claude");

    // The session's commits outlive the worktree on its branch.
    fs::write(ws.join("c.txt"), "see\n").unwrap();
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "c"]);
    assert!(remove(&ws).status.success());
    assert!(!ws.exists());
    assert_eq!(
        git(
            &e.repo,
            &["log", "-1", "--format=%s", "agent-ws/bold-oak-a3f2"]
        ),
        "c"
    );
    // An unused one leaves no branch behind.
    assert!(remove(&again).status.success());
    let gone = cmd("git", &e.repo)
        .args([
            "rev-parse",
            "--verify",
            "-q",
            "refs/heads/agent-ws/bold-oak-a3f2-2",
        ])
        .status()
        .unwrap();
    assert!(!gone.success());
    // A folder agent-ws does not own stays; one already gone counts as removed.
    let out = remove(&e.repo.join("out"));
    assert!(!out.status.success());
    assert!(e.repo.join("out").exists());
    assert!(remove(&e.home.join("ws-already-gone")).status.success());
}

#[test]
fn s8_claude_install_keeps_the_rest_of_the_settings() {
    let e = Env::new();
    let settings = e.repo.join(".claude/settings.local.json");
    fs::write(
        &settings,
        r#"{"permissions": {"allow": ["Bash(ls)"]}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}, "model": "opus"}"#,
    )
    .unwrap();
    assert_eq!(e.ok(&["claude", "install"])["changed"], true);
    let read =
        || -> serde_json::Value { serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap() };
    let v = read();
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["permissions", "hooks", "model"], "key order kept");
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");
    for event in ["WorktreeCreate", "WorktreeRemove"] {
        assert_eq!(
            v["hooks"][event][0]["hooks"][0]["command"],
            "agent-ws claude hook"
        );
    }
    assert_eq!(
        e.ok(&["claude", "install"])["changed"],
        false,
        "installs once"
    );
    assert_eq!(e.ok(&["claude", "uninstall"])["changed"], true);
    let v = read();
    assert!(v["hooks"].get("WorktreeCreate").is_none());
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");

    // --scope user writes where Claude Code reads it, never a real home here.
    let config = e.home.parent().unwrap().join("claude-config");
    let out = bin(&e, &[("CLAUDE_CONFIG_DIR", config.to_str().unwrap())])
        .args(["--json", "claude", "install", "--scope", "user"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        fs::read_to_string(config.join("settings.json"))
            .unwrap()
            .contains("agent-ws claude hook")
    );
}

#[test]
fn s9_prune_cleans_up_claude_worktrees_and_merged_branches() {
    let e = Env::new();
    let set_created = |name: &str, created: u64| {
        mutate_meta(&home_dir(&e), name, |m| m["created"] = created.into());
    };
    let age = |name: &str| set_created(name, 0);
    e.ok(&["spawn", "idle"]);
    for name in ["sub", "young"] {
        let out = claude_hook(
            &e,
            serde_json::json!({"cwd": e.repo, "hook_event_name": "WorktreeCreate", "name": name}),
        );
        assert!(out.status.success());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    set_created("young", now - 2 * 3600);
    let feat = path_of(&e.ok(&["spawn", "feat", "--branch", "feature/x"]));
    fs::write(feat.join("c.txt"), "see\n").unwrap();
    git(&feat, &["add", "-A"]);
    git(&feat, &["commit", "-q", "-m", "c"]);
    git(&e.repo, &["merge", "-q", "--ff-only", "feature/x"]);
    for n in ["idle", "sub", "feat"] {
        age(n);
    }

    // The automatic prune keeps unused workspaces, except Claude Code's,
    // which Claude never removes itself.
    let p = e.ok(&["prune", "--merged-only", "--base", "main"]);
    assert_eq!(names(&p, "pruned"), ["feat", "sub"]);
    assert_eq!(
        names(&p, "kept"),
        ["idle", "young"],
        "a Claude worktree gets a day"
    );
    let branches = git(&e.repo, &["branch", "--format=%(refname:short)"]);
    assert!(
        !branches.contains("feature/x") && !branches.contains("agent-ws/sub"),
        "{branches}"
    );
}

#[test]
fn s10_completions_need_no_repository() {
    let e = Env::new();
    for shell in ["bash", "zsh", "fish"] {
        let out = cmd(env!("CARGO_BIN_EXE_agent-ws"), e.home.parent().unwrap())
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("agent-ws"),
            "{shell}"
        );
    }
}

#[test]
fn d1_repos_lists_every_repository_with_state() {
    let e = Env::new();
    e.ok(&["spawn", "w"]);
    // From outside any repository, as the desktop app asks.
    let repos = || {
        let out = cmd(env!("CARGO_BIN_EXE_agent-ws"), e.home.parent().unwrap())
            .env("AGENT_WS_HOME", &e.home)
            .args(["--json", "repos"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let r = repos();
    assert_eq!(r.as_array().unwrap().len(), 1);
    assert_eq!(r[0]["root"], e.repo.to_str().unwrap());
    assert_eq!(r[0]["workspaces"], 1);
    // State written before repo.json existed is found through a workspace.
    fs::remove_file(home_dir(&e).join("repo.json")).unwrap();
    assert_eq!(repos()[0]["root"], e.repo.to_str().unwrap());
}

#[test]
fn d2_exec_keeps_a_log_of_the_output() {
    let e = Env::new();
    e.ok(&["spawn", "w"]);
    // Test stdout is a pipe, not a terminal: the log is on by default.
    let out = e.run(&[
        "exec",
        "w",
        "--",
        "sh",
        "-c",
        "echo out; echo err >&2; exit 2",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "out\n",
        "still printed"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    let log = e.run(&["log", "w"]);
    let text = String::from_utf8_lossy(&log.stdout);
    assert!(text.contains("out\n") && text.contains("err\n"), "{text}");
    let long = e.ok(&["list", "--long", "--no-disk"]);
    assert_eq!(long[0]["run"]["log"], true);
    assert_eq!(
        long[0]["private_bytes"],
        serde_json::Value::Null,
        "--no-disk"
    );
    assert!(e.ok(&["list", "--long"])[0]["private_bytes"].is_u64());
    e.ok(&["spawn", "quiet"]);
    assert!(e.err(&["log", "quiet"]).contains("no log"));
}

#[test]
fn d3_gc_keeps_the_runs_of_names_with_dots() {
    let e = Env::new();
    e.ok(&["spawn", "v1.2"]);
    assert!(e.run(&["exec", "v1.2", "--", "true"]).status.success());
    e.ok(&["gc"]);
    let long = e.ok(&["list", "--long", "--no-disk"]);
    assert_eq!(long[0]["run"]["state"], "exited", "gc kept v1.2.json");
    assert!(e.run(&["log", "v1.2"]).status.success(), "and v1.2.log");
    e.ok(&["drop", "v1.2"]);
    assert!(
        fs::read_dir(home_dir(&e).join("runs"))
            .unwrap()
            .next()
            .is_none()
    );
}

// ---- Wave 2 regression tests (security, bugfixes, CLI flags & config) ----

#[test]
fn w2_submodule_gitdir_escape_and_symlink_are_rejected() {
    let e = Env::new();
    let inner = e.repo.parent().unwrap().join("inner");
    fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    fs::write(inner.join("i.txt"), "i\n").unwrap();
    git(&inner, &["add", "-A"]);
    git(&inner, &["commit", "-qm", "i"]);
    git(
        &e.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            inner.to_str().unwrap(),
            "sm",
        ],
    );
    git(&e.repo, &["commit", "-qm", "add sm"]);

    // Case 1: `gitdir:` pointing outside `.git/modules`.
    let evil = e.repo.parent().unwrap().join("evil-gitdir");
    fs::create_dir(&evil).unwrap();
    fs::write(e.repo.join("sm/.git"), "gitdir: ../../evil-gitdir\n").unwrap();
    let err = e.err(&["spawn", "sm-escape"]);
    assert!(err.contains("outside .git/modules"), "{err}");
    assert!(list_names(&e).is_empty());

    // Case 2: symlinked `.git` inside submodule.
    fs::remove_file(e.repo.join("sm/.git")).unwrap();
    std::os::unix::fs::symlink(e.repo.join(".git/modules/sm"), e.repo.join("sm/.git")).unwrap();
    let err = e.err(&["spawn", "sm-sym"]);
    assert!(err.contains("cannot be a symlink"), "{err}");
    assert!(list_names(&e).is_empty());
}

#[test]
fn w2_gitignored_env_symlink_and_mixed_cert_pem_are_stripped() {
    let e = Env::new();
    fs::write(
        e.repo.join(".gitignore"),
        "node_modules/\n.env\n.env.*\n!.env.example\ndist\nout/\next\n*.pem\n",
    )
    .unwrap();
    git(&e.repo, &["commit", "-qam", "ignore pem"]);
    // Replace `.env` with a symlink; it must still be removed as a secret.
    fs::remove_file(e.repo.join(".env")).unwrap();
    std::os::unix::fs::symlink(e.repo.join("a.txt"), e.repo.join(".env")).unwrap();

    // Mixed file (valid cert header + secret key/env content) must be stripped,
    // while a valid certificate-only PEM stays.
    let cert = "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n-----END CERTIFICATE-----\n";
    fs::write(e.repo.join("valid.pem"), cert).unwrap();
    fs::write(
        e.repo.join("mixed.pem"),
        format!("{cert}API_KEY=supersecret\n"),
    )
    .unwrap();

    let s = e.ok(&["spawn", "w"]);
    let ws = path_of(&s);
    assert_eq!(
        s["secrets_removed"],
        serde_json::json!([".env", "mixed.pem"])
    );
    assert!(ws.join(".env").symlink_metadata().is_err());
    assert!(!ws.join("mixed.pem").exists());
    assert!(ws.join("valid.pem").exists());
    assert!(e.repo.join(".env").symlink_metadata().is_ok());
}

#[test]
fn w2_strict_symlinks_catches_unnormalized_escape_and_retargets_returning_link() {
    let e = Env::new();
    fs::write(
        e.repo.join(".gitignore"),
        "node_modules/\n.env\ndist\nout/\next\nescape\nsub/\n",
    )
    .unwrap();
    git(&e.repo, &["commit", "-qam", "ignore test links"]);
    let outside = e.repo.parent().unwrap().join("outside-target");
    fs::create_dir(&outside).unwrap();

    // Remove `ext` so only the unnormalized `<src_root>/sub/../../outside-target`
    // symlink exists, proving it triggers `strict_symlinks` on its own.
    fs::remove_file(e.repo.join("ext")).unwrap();

    // Absolute path starting with `<src_root>` that climbs out via `..`.
    let escaped = e.repo.join("sub/../..").join("outside-target");
    std::os::unix::fs::symlink(&escaped, e.repo.join("escape")).unwrap();

    fs::write(e.repo.join(".agent-ws.toml"), "strict_symlinks = true\n").unwrap();
    let err = e.err(&["spawn", "strict"]);
    assert!(err.contains("strict_symlinks"), "{err}");
    assert!(err.contains("escape"), "{err}");

    // Remove the escaping link; test a relative link that climbs out and back in.
    fs::remove_file(e.repo.join("escape")).unwrap();
    fs::create_dir(e.repo.join("sub")).unwrap();
    let repo_name = e.repo.file_name().unwrap().to_str().unwrap();
    std::os::unix::fs::symlink(
        format!("../../{repo_name}/a.txt"),
        e.repo.join("sub/roundtrip"),
    )
    .unwrap();
    let s = e.ok(&["spawn", "ret"]);
    let ws = path_of(&s);
    assert_eq!(
        fs::read_link(ws.join("sub/roundtrip")).unwrap(),
        ws.join("a.txt")
    );
    assert_eq!(
        s["symlinks_retargeted"],
        serde_json::json!(["dist", "sub/roundtrip"])
    );
}

#[test]
fn w2_cross_record_spoofing_and_corrupt_json_are_cleaned_by_gc() {
    let e = Env::new();
    let victim = path_of(&e.ok(&["spawn", "victim"]));
    let broken = path_of(&e.ok(&["spawn", "broken"]));
    let dir = home_dir(&e);
    let head = git(&e.repo, &["rev-parse", "HEAD"]);

    // Forged `meta/evil.json` pointing to `victim`'s legitimate workspace path.
    forge(&dir, "meta", "evil", &victim, "ready", &head);
    assert!(e.err(&["drop", "evil"]).contains("corrupt"));
    assert!(e.err(&["land", "evil", "-m", "x"]).contains("corrupt"));
    assert!(e.err(&["path", "evil"]).contains("corrupt"));
    assert_eq!(list_names(&e), ["broken", "victim"]);

    // Truncated/corrupt JSON in `meta/broken.json`.
    fs::write(dir.join("meta/broken.json"), "{broken").unwrap();
    let dry = e.ok(&["gc", "--dry-run"]);
    assert_eq!(dry["corrupt"], serde_json::json!(["broken", "evil"]));
    assert!(dir.join("meta/broken.json").exists());

    let g = e.ok(&["gc"]);
    assert_eq!(g["corrupt"], serde_json::json!(["broken", "evil"]));
    assert_eq!(g["orphans"], serde_json::json!(["broken"]));
    assert!(!dir.join("meta/evil.json").exists());
    assert!(!dir.join("meta/broken.json").exists());
    assert!(!broken.exists());
    assert!(victim.join("a.txt").exists(), "victim workspace untouched");
    assert_eq!(list_names(&e), ["victim"]);

    // `broken` can be spawned again once `gc` cleaned the corrupt record.
    e.ok(&["spawn", "broken"]);
}

#[test]
fn w2_dot_json_workspace_name_and_option_injection_guards() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "foo.json"]));
    assert_eq!(list_names(&e), ["foo.json"]);
    let dir = home_dir(&e);
    // Leftover `.tmp.<pid>` files must not be read as records and are swept by `gc`.
    fs::write(dir.join("meta/foo.json.tmp.12345"), "{bad").unwrap();
    fs::write(dir.join("pool/@pool-1.json.tmp.12345"), "{bad").unwrap();
    let g = e.ok(&["gc"]);
    assert_eq!(g["corrupt"], serde_json::json!([]));
    assert!(!dir.join("meta/foo.json.tmp.12345").exists());
    assert!(!dir.join("pool/@pool-1.json.tmp.12345").exists());
    assert!(ws.exists());
    e.ok(&["drop", "foo.json"]);

    // Option-like refs/bases must be rejected cleanly.
    assert!(!e.err(&["spawn", "w1", "--base", "-foo"]).is_empty());
    assert!(
        !e.err(&["spawn", "w2", "--base", "--output=/tmp/x"])
            .is_empty()
    );
    assert!(!e.err(&["prune", "--base", "-foo"]).is_empty());
    fs::write(e.repo.join(".agent-ws.toml"), "prune_base = \"-foo\"\n").unwrap();
    assert!(e.err(&["list"]).contains("not a valid ref name"));
}

#[test]
fn w2_land_force_branch_and_no_verify_and_spawn_flags() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    fs::write(e.repo.join("second.txt"), "2\n").unwrap();
    git(&e.repo, &["add", "second.txt"]);
    git(&e.repo, &["commit", "-qm", "second"]);
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    let parent = git(&e.repo, &["rev-parse", "HEAD~1"]);

    // `spawn --base`, `--exclude`, `--no-pool`, `--min-free-gb`, `--branch`.
    e.ok(&["pool", "set", "1"]);
    e.ok(&["pool", "fill"]);
    let s = e.ok(&[
        "spawn",
        "custom",
        "--base",
        "HEAD~1",
        "--exclude",
        "node_modules",
        "--no-pool",
    ]);
    let ws_custom = path_of(&s);
    assert_eq!(s["from_pool"], false);
    assert_eq!(s["base"], parent);
    assert!(!ws_custom.join("second.txt").exists());
    assert!(!ws_custom.join("node_modules").exists());
    assert!(
        e.err(&[
            "spawn",
            "too-big",
            "--no-pool",
            "--min-free-gb",
            "999999999"
        ])
        .contains("free on disk")
    );

    // `pool clear` empties the pool while preserving target.
    assert_eq!(e.ok(&["pool", "clear"])["removed_extra"], 1);
    assert_eq!(e.ok(&["pool", "status"])["target"], 1);
    assert_eq!(e.ok(&["pool", "status"])["fresh"], 0);

    // `spawn --branch` + `drop` deletes unused branch; `land` lands onto checked-out branch.
    let _unused_b = path_of(&e.ok(&["spawn", "unused-b", "--branch", "feat/unused"]));
    e.ok(&["drop", "unused-b"]);
    assert!(
        !git(&e.repo, &["branch", "--list", "feat/unused"]).contains("feat/unused"),
        "unused branch removed on drop"
    );

    let ws_b = path_of(&e.ok(&["spawn", "b", "--branch", "feat/cli"]));
    fs::write(ws_b.join("a.txt"), "branch edit\n").unwrap();
    let lb = e.ok(&["land", "b", "-m", "feat: cli branch"]);
    assert_eq!(lb["branch"], "feat/cli");
    assert_eq!(git(&e.repo, &["show", "feat/cli:a.txt"]), "branch edit");

    // `land --force-branch` overwrites an unrelated branch, and `--no-verify`
    // skips both `pre_land` hooks and Git's `pre-commit` hook.
    git(&e.repo, &["branch", "agent-ws/custom", &head]);
    fs::write(
        e.repo.join(".agent-ws.toml"),
        "[hooks]\npre_land = [\"exit 1\"]\n",
    )
    .unwrap();
    e.ok(&["trust"]);
    let hook = e.repo.join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    fs::write(ws_custom.join("a.txt"), "forced\n").unwrap();
    assert!(
        e.err(&["land", "custom", "-m", "f"])
            .contains("already exists")
    );
    assert!(
        e.err(&["land", "custom", "-m", "f", "--force-branch"])
            .contains("pre_land hook")
    );
    let lf = e.ok(&["land", "custom", "-m", "f", "--force-branch", "--no-verify"]);
    assert_eq!(lf["branch"], "agent-ws/custom");
    assert_eq!(git(&e.repo, &["show", "agent-ws/custom:a.txt"]), "forced");
}

#[test]
fn w2_diff_stat_claude_project_scope_and_config_validation() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "w"]));
    fs::write(ws.join("a.txt"), "one\nchanged\n").unwrap();
    let stat = e.run(&["diff", "w", "--stat"]);
    assert!(stat.status.success());
    let text = String::from_utf8_lossy(&stat.stdout);
    assert!(
        text.contains("a.txt") && text.contains("1 file changed"),
        "{text}"
    );
    let stat_json = e.ok(&["diff", "w", "--stat"]);
    let arr = stat_json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["path"], "a.txt");
    assert_eq!(arr[0]["status"], "M");
    assert_eq!(arr[0]["added"], 1);
    assert_eq!(arr[0]["deleted"], 0);

    // `claude install --scope project` writes `.claude/settings.json`.
    assert_eq!(
        e.ok(&["claude", "install", "--scope", "project"])["changed"],
        true
    );
    assert!(
        fs::read_to_string(e.repo.join(".claude/settings.json"))
            .unwrap()
            .contains("agent-ws claude hook")
    );
    assert_eq!(
        e.ok(&["claude", "uninstall", "--scope", "project"])["changed"],
        true
    );

    // Config validation: `port_step = 0`, `..` in `exclude`, `/` in `secrets`.
    for (toml, expected) in [
        ("port_step = 0\n", "port_step must be >= 1"),
        ("exclude = [\"../x\"]\n", "must not contain '..'"),
        ("secrets = [\"a/b\"]\n", "without path separators"),
    ] {
        fs::write(e.repo.join(".agent-ws.toml"), toml).unwrap();
        let err = e.err(&["list"]);
        assert!(err.contains(expected), "{toml} -> {err}");
    }
}

// ---- v0.5.0 integration tests (perf, storage, CoW, agent-native CLI) ----

fn write_synced(path: &Path, data: &[u8]) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(data).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn v05_glob_exclude_and_scan_skip() {
    let e = Env::new();
    fs::write(
        e.repo.join(".gitignore"),
        "node_modules/\n**/node_modules/\napps/*/dist/\n*.log\ntarget/\n.env\n.env.*\n!.env.example\ndist\nout/\next\n",
    )
    .unwrap();
    fs::create_dir_all(e.repo.join("apps/web/src")).unwrap();
    fs::write(e.repo.join("apps/web/src/app.ts"), "export const x = 1;\n").unwrap();
    git(&e.repo, &["add", ".gitignore", "apps/web/src/app.ts"]);
    git(&e.repo, &["commit", "-qm", "glob layout"]);

    fs::create_dir_all(e.repo.join("packages/ui/node_modules/pkg")).unwrap();
    fs::write(e.repo.join("packages/ui/node_modules/pkg/index.js"), "1").unwrap();
    fs::create_dir_all(e.repo.join("apps/web/dist")).unwrap();
    fs::write(e.repo.join("apps/web/dist/bundle.js"), "b").unwrap();
    fs::create_dir_all(e.repo.join("apps/api/dist")).unwrap();
    fs::write(e.repo.join("apps/api/dist/server.js"), "s").unwrap();
    fs::write(e.repo.join("debug.log"), "root log").unwrap();
    fs::write(e.repo.join("apps/web/error.log"), "nested log").unwrap();
    fs::create_dir_all(e.repo.join("target/debug")).unwrap();
    fs::write(e.repo.join("target/debug/.env"), "SKIPPED_SECRET=1\n").unwrap();

    fs::write(
        e.repo.join(".agent-ws.toml"),
        "exclude = [\"**/node_modules\", \"apps/*/dist\", \"*.log\"]\n\
         scan_skip = [\"target\"]\n",
    )
    .unwrap();

    let s = e.ok(&["spawn", "glob-ws"]);
    let ws = path_of(&s);
    assert!(!ws.join("node_modules").exists());
    assert!(!ws.join("packages/ui/node_modules").exists());
    assert!(!ws.join("apps/web/dist").exists());
    assert!(!ws.join("apps/api/dist").exists());
    assert!(!ws.join("debug.log").exists());
    // `*.log` matches only top-level `*.log`, not nested paths.
    assert!(ws.join("apps/web/error.log").exists());
    assert!(ws.join("apps/web/src/app.ts").exists());

    // `scan_skip = ["target"]` keeps `target/debug/.env` untouched while root `.env` is stripped.
    assert!(ws.join("target/debug/.env").exists());
    assert!(!ws.join(".env").exists());
    let removed = s["secrets_removed"].as_array().unwrap();
    assert!(removed.contains(&serde_json::json!(".env")), "{removed:?}");
    assert!(
        !removed.contains(&serde_json::json!("target/debug/.env")),
        "{removed:?}"
    );
}

#[test]
fn v05_spawn_from_forks_workspace_and_rejects_conflicting_flags() {
    let e = Env::new();
    let parent = path_of(&e.ok(&["spawn", "parent"]));
    let parent_base = git(&parent, &["rev-parse", "HEAD"]);

    // Advance source repo HEAD after parent spawned so parent's base differs from main HEAD.
    fs::write(e.repo.join("upstream.txt"), "upstream\n").unwrap();
    git(&e.repo, &["add", "upstream.txt"]);
    git(&e.repo, &["commit", "-qm", "upstream"]);

    // Parent has a local commit, unstaged edit, staged file, untracked file, and warm cache file.
    fs::write(parent.join("committed.txt"), "c\n").unwrap();
    git(&parent, &["add", "committed.txt"]);
    git(&parent, &["commit", "-qm", "parent commit"]);
    fs::write(parent.join("a.txt"), "unstaged edit\n").unwrap();
    fs::write(parent.join("staged.txt"), "staged\n").unwrap();
    git(&parent, &["add", "staged.txt"]);
    fs::write(parent.join("notes.txt"), "untracked\n").unwrap();
    fs::write(parent.join("node_modules/dep/cached.js"), "cached\n").unwrap();

    let child_json = e.ok(&["spawn", "child", "--from", "parent"]);
    let child = path_of(&child_json);
    assert_eq!(child_json["base"], parent_base);
    assert_eq!(child_json["from_pool"], false);
    assert!(!child.join("upstream.txt").exists());
    assert_eq!(
        fs::read_to_string(child.join("committed.txt")).unwrap(),
        "c\n"
    );
    assert_eq!(
        fs::read_to_string(child.join("a.txt")).unwrap(),
        "unstaged edit\n"
    );
    assert_eq!(
        fs::read_to_string(child.join("notes.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(
        fs::read_to_string(child.join("node_modules/dep/cached.js")).unwrap(),
        "cached\n"
    );
    let st = git(&child, &["status", "--porcelain"]);
    assert!(st.contains("A  staged.txt"), "{st}");
    assert!(st.contains("M a.txt"), "{st}");
    assert!(st.contains("?? notes.txt"), "{st}");

    // Independent git worktree: edits in child do not leak to parent.
    let wt = git(&e.repo, &["worktree", "list"]);
    assert!(wt.contains("parent") && wt.contains("child"), "{wt}");
    fs::write(child.join("a.txt"), "child only\n").unwrap();
    assert_eq!(
        fs::read_to_string(parent.join("a.txt")).unwrap(),
        "unstaged edit\n"
    );

    // Rejects `--from` combined with `--base` or `--carry`, and missing source workspace.
    assert!(
        !e.err(&["spawn", "bad1", "--from", "parent", "--base", "HEAD"])
            .is_empty()
    );
    assert!(
        !e.err(&["spawn", "bad2", "--from", "parent", "--carry"])
            .is_empty()
    );
    assert_eq!(
        e.err_json(&["spawn", "bad3", "--from", "missing"])["code"],
        "NOT_FOUND"
    );
}

#[test]
fn v05_spawn_ensure_and_auto_name() {
    let e = Env::new();
    let first = e.ok(&["spawn", "worker", "--ensure"]);
    assert_eq!(first["name"], "worker");
    assert_eq!(first["reused"], false);

    let reused = e.ok(&["spawn", "worker", "--ensure"]);
    assert_eq!(reused["name"], "worker");
    assert_eq!(reused["reused"], true);
    assert_eq!(reused["path"], first["path"]);
    assert_eq!(reused["port"], first["port"]);

    // `--ensure` with conflicting `--branch` fails with INVALID_INPUT.
    assert_eq!(
        e.err_json(&["spawn", "worker", "--ensure", "--branch", "other"])["code"],
        "INVALID_INPUT"
    );

    // `--auto` picks `-2`, `-3` when the base name is taken.
    let auto2 = e.ok(&["spawn", "worker", "--auto"]);
    assert_eq!(auto2["name"], "worker-2");
    assert_eq!(auto2["reused"], false);

    let auto3 = e.ok(&["spawn", "worker", "--auto"]);
    assert_eq!(auto3["name"], "worker-3");
    assert_eq!(auto3["reused"], false);

    // `--ensure` and `--auto` cannot be combined.
    assert!(!e.err(&["spawn", "worker", "--ensure", "--auto"]).is_empty());
}

#[test]
fn v05_drop_if_exists_and_self_action_guard() {
    let e = Env::new();
    let absent = e.ok(&["drop", "nope", "--if-exists"]);
    assert_eq!(absent["name"], "nope");
    assert_eq!(absent["dropped"], false);

    let ws = path_of(&e.ok(&["spawn", "self-ws"]));
    let sub = ws.join("nested");
    fs::create_dir(&sub).unwrap();
    fs::write(ws.join("a.txt"), "edited\n").unwrap();

    // `drop` and `land` from inside the target workspace are blocked by default.
    let drop_err = e.err_json_in(&sub, &["drop", "self-ws"]);
    assert_eq!(drop_err["code"], "SELF_ACTION_BLOCKED");
    assert!(ws.exists());

    let land_err = e.err_json_in(&sub, &["land", "self-ws", "-m", "blocked"]);
    assert_eq!(land_err["code"], "SELF_ACTION_BLOCKED");
    assert!(ws.exists());

    // `--allow-self` permits both `land` and `drop` from inside the workspace.
    let landed = cmd(env!("CARGO_BIN_EXE_agent-ws"), &sub)
        .env("AGENT_WS_HOME", &e.home)
        .env("AGENT_WS_NO_REFILL", "1")
        .env("AGENT_WS_NO_AUTO_PRUNE", "1")
        .args([
            "--json",
            "land",
            "self-ws",
            "-m",
            "allowed",
            "--keep",
            "--allow-self",
        ])
        .output()
        .unwrap();
    assert!(
        landed.status.success(),
        "{}",
        String::from_utf8_lossy(&landed.stderr)
    );
    let landed_v: serde_json::Value = serde_json::from_slice(&landed.stdout).unwrap();
    assert_eq!(landed_v["branch"], "agent-ws/self-ws");
    assert_eq!(landed_v["dropped"], false);

    let dropped = cmd(env!("CARGO_BIN_EXE_agent-ws"), &sub)
        .env("AGENT_WS_HOME", &e.home)
        .env("AGENT_WS_NO_REFILL", "1")
        .env("AGENT_WS_NO_AUTO_PRUNE", "1")
        .args(["--json", "drop", "self-ws", "--allow-self"])
        .output()
        .unwrap();
    assert!(
        dropped.status.success(),
        "{}",
        String::from_utf8_lossy(&dropped.stderr)
    );
    let dropped_v: serde_json::Value = serde_json::from_slice(&dropped.stdout).unwrap();
    assert_eq!(dropped_v["dropped"], true);
    assert!(!ws.exists());

    assert_eq!(e.ok(&["drop", "self-ws", "--if-exists"])["dropped"], false);
}

#[test]
fn v05_snap_snaps_restore_and_drop_cleanup() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "snap-ws"]));

    fs::write(ws.join("committed.txt"), "c1\n").unwrap();
    git(&ws, &["add", "committed.txt"]);
    git(&ws, &["commit", "-qm", "c1"]);
    let head1 = git(&ws, &["rev-parse", "HEAD"]);

    fs::write(ws.join("a.txt"), "at-cp1\n").unwrap();
    fs::write(ws.join("staged.txt"), "staged\n").unwrap();
    git(&ws, &["add", "staged.txt"]);
    fs::write(ws.join("untracked.txt"), "untracked\n").unwrap();

    let cp1 = e.ok(&["snap", "snap-ws", "cp1"]);
    assert_eq!(cp1["name"], "cp1");
    assert_eq!(cp1["workspace"], "snap-ws");
    assert_eq!(cp1["head"], head1);

    // Auto-named snapshot + listing.
    let auto_snap = e.ok(&["snap", "snap-ws"]);
    let snaps = e.ok(&["snaps", "snap-ws"]);
    let snap_names: Vec<&str> = snaps
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(snap_names.contains(&"cp1"));
    assert!(snap_names.contains(&auto_snap["name"].as_str().unwrap()));

    // Mutate further: modify, create new file, delete existing tracked file, and advance HEAD.
    fs::write(ws.join("a.txt"), "mutated\n").unwrap();
    fs::write(ws.join("extra.txt"), "should vanish\n").unwrap();
    fs::remove_file(ws.join("b.txt")).unwrap();
    fs::write(ws.join("c2.txt"), "c2\n").unwrap();
    git(&ws, &["add", "c2.txt"]);
    git(&ws, &["commit", "-qm", "c2"]);

    // Restore `cp1`.
    let r1 = e.ok(&["restore", "snap-ws", "cp1"]);
    assert_eq!(r1["name"], "cp1");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), head1);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "at-cp1\n");
    assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "bee\n");
    assert_eq!(
        fs::read_to_string(ws.join("untracked.txt")).unwrap(),
        "untracked\n"
    );
    assert!(!ws.join("extra.txt").exists());
    assert!(!ws.join("c2.txt").exists());
    let dotgit_md = fs::symlink_metadata(ws.join(".git")).unwrap();
    assert!(dotgit_md.is_file() && !dotgit_md.file_type().is_symlink());
    let st = git(&ws, &["status", "--porcelain"]);
    assert!(st.contains("A  staged.txt"), "{st}");
    assert!(st.contains("M a.txt"), "{st}");
    assert!(st.contains("?? untracked.txt"), "{st}");

    // Repeatable restore: mutate again and restore `cp1` a second time.
    fs::write(ws.join("a.txt"), "mutated-again\n").unwrap();
    fs::remove_file(ws.join("b.txt")).unwrap();
    fs::write(ws.join("extra2.txt"), "vanish2\n").unwrap();
    let r2 = e.ok(&["restore", "snap-ws", "cp1"]);
    assert_eq!(r2["name"], "cp1");
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "at-cp1\n");
    assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "bee\n");
    assert!(!ws.join("extra2.txt").exists());

    // `snap --drop` deletes `cp1`.
    let dropped = e.ok(&["snap", "snap-ws", "cp1", "--drop"]);
    assert_eq!(dropped["dropped"], true);
    let remaining = e.ok(&["snaps", "snap-ws"]);
    assert_eq!(remaining.as_array().unwrap().len(), 1);
    assert_eq!(
        e.err_json(&["restore", "snap-ws", "cp1"])["code"],
        "NOT_FOUND"
    );

    // Dropping the workspace cleans up its snapshots folder.
    let snaps_dir = home_dir(&e).join("snaps/snap-ws");
    assert!(snaps_dir.exists());
    e.ok(&["drop", "snap-ws"]);
    assert!(!snaps_dir.exists());
}

#[test]
fn v05_sync_rebase_merge_conflict_and_resync_deps() {
    let e = Env::new();
    let w1 = path_of(&e.ok(&["spawn", "w1"]));
    let w2 = path_of(&e.ok(&["spawn", "w2"]));

    fs::write(w1.join("w1.txt"), "w1\n").unwrap();
    git(&w1, &["add", "w1.txt"]);
    git(&w1, &["commit", "-qm", "w1 commit"]);

    fs::write(w2.join("w2.txt"), "w2\n").unwrap();
    git(&w2, &["add", "w2.txt"]);
    git(&w2, &["commit", "-qm", "w2 commit"]);

    // Advance main and update source deps.
    fs::write(e.repo.join("main.txt"), "main\n").unwrap();
    git(&e.repo, &["add", "main.txt"]);
    git(&e.repo, &["commit", "-qm", "main commit"]);
    let new_main = git(&e.repo, &["rev-parse", "HEAD"]);
    fs::write(e.repo.join("node_modules/dep/index.js"), "v2\n").unwrap();
    fs::write(e.repo.join("node_modules/dep/.env"), "SECRET=bad\n").unwrap();

    // Dirty tree is rejected with DIRTY.
    fs::write(w1.join("dirty.txt"), "x\n").unwrap();
    assert_eq!(e.err_json(&["sync", "w1"])["code"], "DIRTY");
    fs::remove_file(w1.join("dirty.txt")).unwrap();

    // Rebase + `--resync-deps`.
    let synced = e.ok(&["sync", "w1", "--resync-deps"]);
    assert_eq!(synced["mode"], "rebase");
    assert_eq!(synced["new_base"], new_main);
    assert!(w1.join("main.txt").exists());
    assert!(w1.join("w1.txt").exists());
    assert_eq!(
        fs::read_to_string(w1.join("node_modules/dep/index.js")).unwrap(),
        "v2\n"
    );
    assert!(!w1.join("node_modules/dep/.env").exists());
    assert!(
        synced["resync"]["resynced"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("node_modules"))
    );

    // `--merge` mode.
    let merged = e.ok(&["sync", "w2", "--merge"]);
    assert_eq!(merged["mode"], "merge");
    assert_eq!(merged["new_base"], new_main);
    assert!(w2.join("main.txt").exists());
    assert!(w2.join("w2.txt").exists());

    // `resync-deps` alone refreshes ignored deps without touching tracked edits and rejects tracked paths.
    fs::write(e.repo.join("node_modules/dep/index.js"), "v3\n").unwrap();
    fs::write(w2.join("a.txt"), "tracked-edit\n").unwrap();
    let rd = e.ok(&["resync-deps", "w2"]);
    assert!(
        rd["resynced"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("node_modules"))
    );
    assert_eq!(
        fs::read_to_string(w2.join("node_modules/dep/index.js")).unwrap(),
        "v3\n"
    );
    assert_eq!(
        fs::read_to_string(w2.join("a.txt")).unwrap(),
        "tracked-edit\n"
    );
    assert_eq!(
        e.err_json(&["resync-deps", "w2", "a.txt"])["code"],
        "INVALID_INPUT"
    );

    // Conflict during rebase and merge aborts cleanly leaving workspace untouched.
    fs::write(w1.join("a.txt"), "ws-conflict\n").unwrap();
    git(&w1, &["commit", "-qam", "ws conflict"]);
    let w1_head_before = git(&w1, &["rev-parse", "HEAD"]);
    fs::write(e.repo.join("a.txt"), "main-conflict\n").unwrap();
    git(&e.repo, &["commit", "-qam", "main conflict"]);

    let rebase_err = e.err_json(&["sync", "w1"]);
    assert_eq!(rebase_err["code"], "CONFLICT");
    assert_eq!(git(&w1, &["status", "--porcelain"]), "");
    assert_eq!(git(&w1, &["rev-parse", "HEAD"]), w1_head_before);

    let merge_err = e.err_json(&["sync", "w1", "--merge"]);
    assert_eq!(merge_err["code"], "CONFLICT");
    assert_eq!(git(&w1, &["status", "--porcelain"]), "");
    assert_eq!(git(&w1, &["rev-parse", "HEAD"]), w1_head_before);
}

#[test]
fn v05_du_depth_cohort_reclaimable_and_max_private_gb() {
    let e = Env::new();
    // Write a 1 MiB synced ignored file so both workspaces share a real CoW extent.
    write_synced(
        &e.repo.join("node_modules/shared.bin"),
        &vec![0x42u8; 1 << 20],
    );

    let w1 = path_of(&e.ok(&["spawn", "w1"]));
    let _w2 = path_of(&e.ok(&["spawn", "w2"]));

    // Remove the file in the source checkout so `w1` and `w2` become an orphan CoW cohort.
    fs::remove_file(e.repo.join("node_modules/shared.bin")).unwrap();

    // Add a 1 MiB private file inside `w1/privdir` to populate private bytes & depth-1 breakdown.
    write_synced(&w1.join("privdir/priv.bin"), &vec![0x77u8; 1 << 20]);

    fs::write(e.repo.join(".agent-ws.toml"), "max_private_gb = 5\n").unwrap();
    let du = e.ok(&["du", "--depth", "1"]);
    assert_eq!(du["budget_bytes"], 5_000_000_000u64);

    let total_private = du["total"]["private"].as_u64().unwrap();
    let cohort_shared = du["cohort_shared_bytes"].as_u64().unwrap();
    let reclaimable = du["reclaimable_bytes"].as_u64().unwrap();
    assert!(total_private >= (1 << 20), "{du}");
    assert!(
        cohort_shared > 0,
        "expected cohort_shared_bytes > 0 when 2 workspaces share orphan clone: {du}"
    );
    assert!(
        reclaimable >= total_private + cohort_shared,
        "reclaimable={reclaimable}, total_private={total_private}, cohort_shared={cohort_shared}"
    );

    let w1_entry = du["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "w1")
        .unwrap();
    let breakdown = w1_entry["breakdown"].as_array().unwrap();
    assert!(!breakdown.is_empty(), "{w1_entry}");
    assert!(
        breakdown.iter().any(|b| b["path"] == "privdir"),
        "{breakdown:?}"
    );

    // Setting `max_private_gb = 0` emits a warning on `spawn` when private bytes > 0.
    fs::write(e.repo.join(".agent-ws.toml"), "max_private_gb = 0\n").unwrap();
    let w3 = e.ok(&["spawn", "w3"]);
    let warnings = w3["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|s| s.contains("max_private_gb"))),
        "{warnings:?}"
    );
}

#[test]
fn v05_prune_target_free_gb_dry_run_and_exec() {
    let e = Env::new();
    let big = path_of(&e.ok(&["spawn", "big"]));
    let small = path_of(&e.ok(&["spawn", "small"]));
    let dirty = path_of(&e.ok(&["spawn", "dirty"]));
    let unmerged = path_of(&e.ok(&["spawn", "unmerged"]));

    // Make `big` hold 1 MiB private and merge its commit into `main` so it is clean & merged.
    write_synced(&big.join("big.bin"), &vec![0x5Au8; 1 << 20]);
    git(&big, &["add", "big.bin"]);
    git(&big, &["commit", "-qm", "big commit"]);
    let big_head = git(&big, &["rev-parse", "HEAD"]);
    git(&e.repo, &["merge", "-q", "--ff-only", &big_head]);

    // `dirty` has uncommitted edits; `unmerged` has an unmerged commit.
    fs::write(dirty.join("a.txt"), "dirty\n").unwrap();
    fs::write(unmerged.join("u.txt"), "unmerged\n").unwrap();
    git(&unmerged, &["add", "u.txt"]);
    git(&unmerged, &["commit", "-qm", "unmerged commit"]);

    // Put 1 entry in the pool so we can verify high `--target-free-gb` clears the pool on execution.
    e.ok(&["pool", "fill", "--size", "1"]);

    // Target 0 GB: free disk is already above target, so nothing is pruned.
    let zero_dry = e.ok(&[
        "prune",
        "--dry-run",
        "--base",
        "main",
        "--min-age-min",
        "0",
        "--target-free-gb",
        "0",
    ]);
    assert_eq!(zero_dry["pruned"], serde_json::json!([]));
    assert!(big.exists() && small.exists());

    // Huge target in dry-run: selects clean eligible workspaces largest-first, keeps dirty/unmerged, touches nothing.
    let huge_dry = e.ok(&[
        "prune",
        "--dry-run",
        "--base",
        "main",
        "--min-age-min",
        "0",
        "--target-free-gb",
        "999999999",
    ]);
    assert_eq!(huge_dry["dry_run"], true);
    assert_eq!(names(&huge_dry, "pruned"), ["big", "small"]);
    assert_eq!(huge_dry["pruned"][0]["name"], "big");
    assert_eq!(names(&huge_dry, "kept"), ["dirty", "unmerged"]);
    assert!(big.exists() && small.exists() && dirty.exists() && unmerged.exists());
    assert_eq!(e.ok(&["pool", "status"])["fresh"], 1);

    // Execute with huge target: drops `big` and `small`, clears pool, never drops `dirty` or `unmerged`.
    let huge_exec = e.ok(&[
        "prune",
        "--base",
        "main",
        "--min-age-min",
        "0",
        "--target-free-gb",
        "999999999",
    ]);
    assert_eq!(huge_exec["dry_run"], false);
    assert_eq!(names(&huge_exec, "pruned"), ["big", "small"]);
    assert_eq!(names(&huge_exec, "kept"), ["dirty", "unmerged"]);
    assert!(!big.exists() && !small.exists());
    assert!(dirty.exists() && unmerged.exists());
    assert_eq!(e.ok(&["pool", "status"])["fresh"], 0);
}

#[test]
fn v05_whoami_structured_json_errors_log_json_and_skill() {
    let e = Env::new();

    // `whoami` from source repo.
    let outside = e.ok(&["whoami"]);
    assert_eq!(outside["inside_workspace"], false);
    assert_eq!(outside["name"], serde_json::Value::Null);
    assert_eq!(outside["repo_root"], e.repo.to_str().unwrap());

    // `whoami` from inside a workspace (nested dir), with and without `--status`.
    let w1 = path_of(&e.ok(&["spawn", "w1", "--branch", "feat/who"]));
    let w2 = path_of(&e.ok(&["spawn", "w2"]));
    fs::write(w1.join("a.txt"), "w1-edit\n").unwrap();
    fs::write(w2.join("a.txt"), "w2-edit\n").unwrap();
    let sub = w1.join("nested");
    fs::create_dir(&sub).unwrap();

    let run_in_sub = |args: &[&str]| -> serde_json::Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = cmd(env!("CARGO_BIN_EXE_agent-ws"), &sub)
            .env("AGENT_WS_HOME", &e.home)
            .args(&a)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let who = run_in_sub(&["whoami"]);
    assert_eq!(who["inside_workspace"], true);
    assert_eq!(who["name"], "w1");
    assert_eq!(who["branch"], "feat/who");
    assert_eq!(who["port"], 4100);
    assert!(who.get("overlaps").is_none());
    assert!(who.get("conflicts").is_none());

    let who_status = run_in_sub(&["whoami", "--status"]);
    assert_eq!(who_status["inside_workspace"], true);
    assert_eq!(who_status["name"], "w1");
    assert_eq!(
        who_status["overlaps"],
        serde_json::json!({ "a.txt": ["w1", "w2"] })
    );
    let conflicts = who_status["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["paths"], serde_json::json!(["a.txt"]));

    // Structured `--json` errors on stderr.
    assert_eq!(e.err_json(&["spawn", "w1"])["code"], "ALREADY_EXISTS");
    assert_eq!(e.err_json(&["drop", "nope"])["code"], "NOT_FOUND");
    assert_eq!(
        e.err_json_in(&sub, &["drop", "w1"])["code"],
        "SELF_ACTION_BLOCKED"
    );
    assert_eq!(e.err_json(&["spawn", "../bad"])["code"], "INVALID_INPUT");

    // `log --json`.
    assert_eq!(e.err_json(&["log", "w1"])["code"], "NOT_FOUND");
    let ex = e.run(&["exec", "w1", "--", "sh", "-c", "echo json-log-line"]);
    assert!(ex.status.success());
    let log = e.ok(&["log", "w1"]);
    assert_eq!(log["name"], "w1");
    assert!(
        log["log"].as_str().unwrap().contains("json-log-line"),
        "{log}"
    );

    // `agent-ws skill` prints valid YAML frontmatter and key commands.
    let skill = e.run(&["skill"]);
    assert!(skill.status.success());
    let text = String::from_utf8(skill.stdout).unwrap();
    assert!(text.starts_with("---\nname: agent-ws\n"), "{text}");
    let after_open = &text[4..];
    assert!(after_open.contains("\n---\n"), "{text}");
    for kw in [
        "spawn",
        "whoami",
        "snap",
        "restore",
        "sync",
        "resync-deps",
        "land",
        "drop",
    ] {
        assert!(text.contains(kw), "skill missing {kw}");
    }
}
