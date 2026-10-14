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
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
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
        let repo = tmp.path().join("repo").canonicalize().unwrap_or(tmp.path().join("repo"));
        fs::create_dir(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "node_modules/\n.env\n.env.*\n!.env.example\ndist\nout/\next\n").unwrap();
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        fs::write(repo.join("b.txt"), "bee\n").unwrap();
        fs::write(repo.join(".env.example"), "KEY=\n").unwrap();
        fs::create_dir(repo.join(".claude")).unwrap();
        fs::write(repo.join(".claude/settings.json"), "{}").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        // Ignored state that a workspace should inherit or drop.
        fs::create_dir_all(repo.join("node_modules/dep")).unwrap();
        fs::write(repo.join("node_modules/dep/index.js"), "module.exports = 1\n").unwrap();
        fs::create_dir(repo.join("out")).unwrap();
        std::os::unix::fs::symlink(repo.join("out"), repo.join("dist")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), repo.join("ext")).unwrap();
        fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
        fs::create_dir_all(repo.join(".claude/worktrees/old")).unwrap();
        fs::write(repo.join(".claude/worktrees/old/x"), "x").unwrap();
        Env { _tmp: tmp, repo, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        cmd(env!("CARGO_BIN_EXE_agent-ws"), &self.repo)
            .env("AGENT_WS_HOME", &self.home)
            // Background refills would race with the assertions.
            .env("AGENT_WS_NO_REFILL", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.run(&a);
        assert!(out.status.success(), "agent-ws {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "agent-ws {args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}

#[test]
fn spawn_gives_base_commit_plus_ignored_files() {
    let e = Env::new();
    // Uncommitted work in the source must not leak into the workspace.
    fs::write(e.repo.join("a.txt"), "dirty\n").unwrap();
    fs::write(e.repo.join("scratch.txt"), "untracked\n").unwrap();

    let s = e.ok(&["spawn", "w1"]);
    let ws = PathBuf::from(s["path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "one\n");
    assert!(!ws.join("scratch.txt").exists());
    assert!(ws.join("node_modules/dep/index.js").exists(), "ignored deps are cloned");
    assert!(!ws.join(".env").exists(), "secrets are removed");
    assert!(ws.join(".env.example").exists());
    assert_eq!(s["secrets_removed"], serde_json::json!([".env"]));
    assert!(ws.join(".claude/settings.json").exists());
    assert!(!ws.join(".claude/worktrees").exists(), "other agents' worktrees are skipped");
    assert_eq!(fs::read_link(ws.join("dist")).unwrap(), ws.join("out"), "absolute symlink retargeted");
    assert_eq!(s["symlinks_external"][0]["path"], "ext", "links outside the source are reported");
    assert_eq!(s["symlinks_external"].as_array().unwrap().len(), 1);
    assert_eq!(git(&ws, &["status", "--porcelain"]), "");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), git(&e.repo, &["rev-parse", "HEAD"]));
    // The source keeps its own state.
    assert_eq!(fs::read_to_string(e.repo.join("a.txt")).unwrap(), "dirty\n");
    assert!(e.repo.join(".env").exists());
}

#[test]
fn workspace_writes_do_not_touch_source() {
    let e = Env::new();
    let ws = PathBuf::from(e.ok(&["spawn", "w1"])["path"].as_str().unwrap());
    fs::write(ws.join("b.txt"), "changed\n").unwrap();
    fs::write(ws.join("node_modules/dep/index.js"), "patched\n").unwrap();
    fs::write(ws.join("dist/built.js"), "x").unwrap();
    assert_eq!(fs::read_to_string(e.repo.join("b.txt")).unwrap(), "bee\n");
    assert_eq!(fs::read_to_string(e.repo.join("node_modules/dep/index.js")).unwrap(), "module.exports = 1\n");
    assert!(!e.repo.join("out/built.js").exists());
}

#[test]
fn status_reports_changes_and_overlaps() {
    let e = Env::new();
    let w1 = PathBuf::from(e.ok(&["spawn", "w1"])["path"].as_str().unwrap());
    let w2 = PathBuf::from(e.ok(&["spawn", "w2"])["path"].as_str().unwrap());
    fs::write(w1.join("a.txt"), "w1\n").unwrap();
    fs::write(w1.join("new.txt"), "n\n").unwrap();
    fs::write(w2.join("a.txt"), "w2\n").unwrap();
    // A commit made inside the workspace still counts as a change.
    fs::write(w2.join("b.txt"), "w2\n").unwrap();
    git(&w2, &["commit", "-qam", "agent commit"]);

    let s = e.ok(&["status"]);
    assert_eq!(s["workspaces"][0]["changed"], serde_json::json!(["a.txt", "new.txt"]));
    assert_eq!(s["workspaces"][1]["changed"], serde_json::json!(["a.txt", "b.txt"]));
    assert_eq!(s["overlaps"], serde_json::json!({ "a.txt": ["w1", "w2"] }));
    let one = e.ok(&["status", "w2"]);
    assert_eq!(one["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(one["overlaps"], s["overlaps"]);
}

#[test]
fn land_commits_to_branch_and_drops() {
    let e = Env::new();
    let ws = PathBuf::from(e.ok(&["spawn", "w1"])["path"].as_str().unwrap());
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
    let ws = PathBuf::from(e.ok(&["spawn", "w2"])["path"].as_str().unwrap());
    fs::write(ws.join("a.txt"), "x\n").unwrap();
    assert!(e.err(&["land", "w2", "-m", "x", "--branch", "agent-ws/w1"]).contains("already exists"));
    assert!(ws.exists(), "failed land keeps the workspace");
}

#[test]
fn drop_and_name_rules() {
    let e = Env::new();
    let ws = PathBuf::from(e.ok(&["spawn", "w1"])["path"].as_str().unwrap());
    assert!(e.err(&["spawn", "w1"]).contains("already exists"));
    assert!(e.err(&["spawn", "../x"]).contains("invalid workspace name"));
    assert!(e.err(&["spawn", ".hidden"]).contains("invalid workspace name"));
    e.ok(&["drop", "w1"]);
    assert!(!ws.exists());
    assert!(e.err(&["drop", "w1"]).contains("no workspace named"));
    // The name is free again.
    e.ok(&["spawn", "w1"]);
}

#[test]
fn works_from_inside_a_workspace() {
    let e = Env::new();
    let ws = PathBuf::from(e.ok(&["spawn", "w1"])["path"].as_str().unwrap());
    let out = cmd(env!("CARGO_BIN_EXE_agent-ws"), &ws).env("AGENT_WS_HOME", &e.home).args(["--json", "list"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let l: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(l[0]["name"], "w1");
}

#[test]
fn parallel_spawns_all_succeed() {
    let e = Env::new();
    let outs: Vec<Output> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8).map(|i| {
            let e = &e;
            s.spawn(move || e.run(&["--json", "spawn", &format!("p{i}")]))
        }).collect();
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
                    let ws = PathBuf::from(e.ok(&["path", &name])["path"].as_str().unwrap());
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
    assert_eq!(git(&e.repo, &["branch", "--list", "agent-ws/*"]).lines().count(), 4);
}

#[test]
fn same_name_race_has_one_winner() {
    let e = Env::new();
    let outs: Vec<Output> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..6).map(|_| {
            let e = &e;
            s.spawn(move || e.run(&["spawn", "dup"]))
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(outs.iter().filter(|o| o.status.success()).count(), 1);
    assert_eq!(e.ok(&["list"]).as_array().unwrap().len(), 1);
    assert_eq!(git(&e.repo, &["worktree", "list"]).lines().count(), 2);
}

#[test]
fn gc_cleans_crashes_but_not_live_spawns() {
    let e = Env::new();
    let keep = PathBuf::from(e.ok(&["spawn", "keep"])["path"].as_str().unwrap());
    let dir = keep.parent().unwrap().parent().unwrap().to_path_buf();
    let set_meta = |name: &str, state: &str, pid: u32| {
        let p = dir.join("meta").join(format!("{name}.json"));
        let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        m["state"] = state.into();
        m["pid"] = pid.into();
        fs::write(p, serde_json::to_vec(&m).unwrap()).unwrap();
    };
    // Spawn that died midway: record says creating, process is gone.
    let crashed = PathBuf::from(e.ok(&["spawn", "crashed"])["path"].as_str().unwrap());
    set_meta("crashed", "creating", 999_999);
    // Spawn still running in another process (this test process is alive).
    let live = PathBuf::from(e.ok(&["spawn", "live"])["path"].as_str().unwrap());
    set_meta("live", "creating", std::process::id());
    // Folder deleted by hand.
    let gone = PathBuf::from(e.ok(&["spawn", "gone"])["path"].as_str().unwrap());
    fs::remove_dir_all(&gone).unwrap();
    // Folder with no record, and a leftover in the trash.
    fs::create_dir(dir.join("ws/orphan")).unwrap();
    fs::create_dir_all(dir.join(".trash/leftover-1000/x")).unwrap();
    // A delete that started just now is not a leftover.
    let recent = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    fs::create_dir_all(dir.join(format!(".trash/fresh-{recent}"))).unwrap();

    assert!(e.err(&["land", "live", "-m", "x"]).contains("still being created"));
    let dry = e.ok(&["gc", "--dry-run"]);
    assert_eq!(dry["interrupted"], serde_json::json!(["crashed"]));
    assert_eq!(dry["missing"], serde_json::json!(["gone"]));
    assert_eq!(dry["orphans"], serde_json::json!(["orphan"]));
    assert_eq!(dry["trash"], 1);
    assert!(crashed.exists() && dir.join("ws/orphan").exists(), "dry run changes nothing");

    e.ok(&["gc"]);
    assert!(!crashed.exists() && !dir.join("ws/orphan").exists());
    assert!(live.exists() && keep.exists(), "live spawn and ready workspace survive");
    let names: Vec<String> = e.ok(&["list"]).as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["keep"]);
    let wt = git(&e.repo, &["worktree", "list"]);
    assert!(wt.contains("keep") && wt.contains("live") && !wt.contains("crashed") && !wt.contains("gone"));
    // The names are free again.
    e.ok(&["spawn", "crashed"]);
    e.ok(&["spawn", "gone"]);
    let again = e.ok(&["gc"]);
    assert_eq!(again["interrupted"], serde_json::json!([]));
}

fn path_of(v: &serde_json::Value) -> PathBuf {
    PathBuf::from(v["path"].as_str().unwrap())
}

#[test]
fn secrets_inside_ignored_folders_are_removed() {
    let e = Env::new();
    fs::write(e.repo.join(".gitignore"), "node_modules/\n.env\n.env.*\n!.env.example\ndist\nout/\next\n.wrangler/\n*.pem\n").unwrap();
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
        serde_json::json!([".env", ".wrangler/state/.dev.vars", "node_modules/dep/.env", "server.pem"])
    );
    assert!(ws.join(".wrangler/state/cache.json").exists(), "non-secret ignored files stay");
    assert!(ws.join(".env.ci").exists(), "tracked files are never removed");
    let kept = path_of(&e.ok(&["spawn", "w2", "--keep-secrets"]));
    assert!(kept.join(".wrangler/state/.dev.vars").exists());
}

#[test]
fn relative_symlinks_that_leave_the_repo_are_fixed_and_reported() {
    let e = Env::new();
    fs::write(e.repo.join(".gitignore"), "node_modules/\n.env\ndist\nout/\next\nup\nsub/\n").unwrap();
    git(&e.repo, &["commit", "-qam", "ignore links"]);
    let outside = e.repo.parent().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink("../outside", e.repo.join("up")).unwrap();
    fs::create_dir(e.repo.join("sub")).unwrap();
    std::os::unix::fs::symlink("../a.txt", e.repo.join("sub/inner")).unwrap();

    let s = e.ok(&["spawn", "w1"]);
    let ws = path_of(&s);
    assert_eq!(fs::read_link(ws.join("up")).unwrap(), outside, "keeps meaning the folder it meant in the source");
    assert_eq!(fs::read_link(ws.join("sub/inner")).unwrap(), PathBuf::from("../a.txt"), "links inside stay relative");
    let ext: Vec<&str> = s["symlinks_external"].as_array().unwrap().iter().map(|l| l["path"].as_str().unwrap()).collect();
    assert_eq!(ext, ["ext", "up"]);
}

#[test]
fn config_file_controls_excludes_secrets_and_strict_symlinks() {
    let e = Env::new();
    fs::write(e.repo.join(".agent-ws.toml"), "exclude = [\"node_modules\"]\nsecrets = [\"*.txt\"]\nsecrets_keep = []\n").unwrap();
    fs::write(e.repo.join("notes.txt"), "untracked but ignored? no").unwrap();
    let s = e.ok(&["spawn", "w1"]);
    let ws = path_of(&s);
    assert!(!ws.join("node_modules").exists(), "config exclude applied");
    assert!(ws.join(".env").exists(), "custom secret list replaces the defaults");
    assert!(ws.join("a.txt").exists(), "tracked files match but stay");

    fs::write(e.repo.join(".agent-ws.toml"), "strict_symlinks = true\n").unwrap();
    assert!(e.err(&["spawn", "w2"]).contains("strict_symlinks"));
    assert!(e.ok(&["list"]).as_array().unwrap().len() == 1, "failed strict spawn leaves nothing");
    fs::write(e.repo.join(".agent-ws.toml"), "typo = 1\n").unwrap();
    assert!(e.err(&["spawn", "w3"]).contains("invalid"));
}

#[test]
fn pool_serves_spawns_and_tracks_freshness() {
    let e = Env::new();
    e.ok(&["pool", "set", "2"]);
    let f = e.ok(&["pool", "fill"]);
    assert_eq!((f["created"].as_u64(), f["ready"].as_u64()), (Some(2), Some(2)));

    // Uncommitted work in the source after the fill must not matter.
    let s = e.ok(&["spawn", "a1"]);
    assert_eq!(s["from_pool"], true);
    let ws = path_of(&s);
    assert!(ws.file_name().unwrap().to_str().unwrap().starts_with("@pool-"));
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

    // A new commit makes the pool stale; spawns stop using it.
    fs::write(e.repo.join("b.txt"), "moved\n").unwrap();
    git(&e.repo, &["commit", "-qam", "move head"]);
    let st = e.ok(&["pool", "status"]);
    assert_eq!((st["fresh"].as_u64(), st["stale"].as_u64()), (Some(0), Some(1)));
    let fresh_spawn = e.ok(&["spawn", "c1"]);
    assert_eq!(fresh_spawn["from_pool"], false);
    assert_eq!(fs::read_to_string(path_of(&fresh_spawn).join("b.txt")).unwrap(), "moved\n");
    let f = e.ok(&["pool", "fill"]);
    assert_eq!((f["removed_stale"].as_u64(), f["created"].as_u64()), (Some(1), Some(2)));
    let d1 = e.ok(&["spawn", "d1"]);
    assert_eq!(d1["from_pool"], true);
    assert_eq!(fs::read_to_string(path_of(&d1).join("b.txt")).unwrap(), "moved\n");

    // Dependency changes make it stale too.
    fs::write(e.repo.join("node_modules/.modules.yaml"), "changed").unwrap();
    assert_eq!(e.ok(&["pool", "status"])["stale"], 1);
    assert_eq!(e.ok(&["pool", "clear"])["removed_stale"], 1);
    let st = e.ok(&["pool", "status"]);
    assert_eq!((st["target"].as_u64(), st["fresh"].as_u64(), st["stale"].as_u64()), (Some(2), Some(0), Some(0)));
    assert_eq!(e.ok(&["gc"])["orphans"], serde_json::json!([]));
}

#[test]
fn parallel_spawns_never_share_a_pooled_workspace() {
    let e = Env::new();
    e.ok(&["pool", "fill", "--size", "3"]);
    let outs: Vec<serde_json::Value> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..6).map(|i| {
            let e = &e;
            s.spawn(move || e.ok(&["spawn", &format!("r{i}")]))
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let pooled: Vec<&serde_json::Value> = outs.iter().filter(|o| o["from_pool"] == true).collect();
    assert_eq!(pooled.len(), 3, "each pooled workspace is taken once");
    let mut paths: Vec<String> = outs.iter().map(|o| o["path"].as_str().unwrap().to_string()).collect();
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
    let pool = fs::read_dir(e.home.read_dir().unwrap().flatten().find(|d| d.path().is_dir()).unwrap().path().join("pool"))
        .unwrap()
        .flatten()
        .map(|d| d.path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .unwrap();
    let ws = path_of(&serde_json::from_slice(&fs::read(&pool).unwrap()).unwrap());
    // A spawn that died right after taking the record.
    fs::rename(&pool, pool.with_extension("claim-999999")).unwrap();
    let g = e.ok(&["gc"]);
    assert_eq!(g["claims"], 1);
    assert!(!ws.exists());
    assert_eq!(e.ok(&["pool", "status"])["fresh"], 0);
}

// ---- Fixes for the 2026-10-15 review (REPORT-glm.md F0-F7) ----

fn home_dir(e: &Env) -> PathBuf {
    e.home.read_dir().unwrap().flatten().find(|d| d.path().is_dir()).unwrap().path()
}

fn forge(dir: &Path, sub: &str, name: &str, path: &Path, state: &str, base: &str) {
    let m = serde_json::json!({
        "name": name, "path": path, "base": base, "created": 0, "state": state, "pid": 0, "key": "",
        "notes": { "symlinks_retargeted": [], "symlinks_external": [], "secrets_removed": [] }
    });
    fs::write(dir.join(sub).join(format!("{name}.json")), serde_json::to_vec(&m).unwrap()).unwrap();
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
    assert!(e.repo.join("a.txt").exists() && e.repo.join(".git").exists(), "source untouched");
    let names: Vec<String> = e.ok(&["list"]).as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap().into()).collect();
    assert_eq!(names, ["w1"], "corrupt records are not listed");
    e.ok(&["status"]);

    let g = e.ok(&["gc"]);
    assert_eq!(g["corrupt"], serde_json::json!(["@pool-1", "evil"]));
    assert!(e.repo.join("a.txt").exists(), "gc only forgets corrupt records");
    assert!(!dir.join("meta/evil.json").exists() && !dir.join("pool/@pool-1.json").exists());
}

#[test]
fn f1_gc_is_not_fooled_by_a_reused_pid() {
    let e = Env::new();
    let ws = path_of(&e.ok(&["spawn", "reused"]));
    let p = home_dir(&e).join("meta/reused.json");
    let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    // A live process (this test) whose start time differs from the spawner's.
    m["state"] = "creating".into();
    m["pid"] = std::process::id().into();
    m["pid_start"] = 1.into();
    fs::write(&p, serde_json::to_vec(&m).unwrap()).unwrap();
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
    assert_eq!((l["dropped"].as_bool(), l["commit"].as_str()), (Some(true), Some(tip.as_str())));
    assert!(!ws.exists());

    // A branch that does not match the workspace is still refused, with a hint.
    let ws = path_of(&e.ok(&["spawn", "kk2"]));
    fs::write(ws.join("a.txt"), "other\n").unwrap();
    assert!(e.err(&["land", "kk2", "-m", "x", "--branch", "agent-ws/kk"]).contains("--branch"));
}

#[test]
fn f4_corrupt_short_base_does_not_crash_the_cli() {
    let e = Env::new();
    e.ok(&["spawn", "w1"]);
    let p = home_dir(&e).join("meta/w1.json");
    let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    m["base"] = "x".into();
    fs::write(&p, serde_json::to_vec(&m).unwrap()).unwrap();
    let out = e.run(&["list"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
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
    assert!(msg.starts_with("m15") && msg.contains("Agent-Ws: L15"), "{msg}");
    assert_eq!(git(&e.repo, &["log", "-1", "--format=%s", "agent-ws/L15~1"]), "agent work");
}

#[test]
fn f6_renames_count_both_paths_consistently() {
    let e = Env::new();
    let w1 = path_of(&e.ok(&["spawn", "w1"]));
    let w2 = path_of(&e.ok(&["spawn", "w2"]));
    git(&w1, &["mv", "a.txt", "renamed.txt"]);
    fs::write(w2.join("a.txt"), "edited\n").unwrap();
    let s = e.ok(&["status", "w1"]);
    assert_eq!(s["workspaces"][0]["changed"], serde_json::json!(["a.txt", "renamed.txt"]));
    assert_eq!(s["overlaps"], serde_json::json!({ "a.txt": ["w1", "w2"] }), "removing a.txt overlaps an edit of it");
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
    git(&e.repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", inner.to_str().unwrap(), "sm"]);
    git(&e.repo, &["commit", "-qm", "add sm"]);
    let src_sm_head = git(&e.repo.join("sm"), &["rev-parse", "HEAD"]);
    fs::write(e.repo.join("sm/i.txt"), "dirty in source\n").unwrap();

    let ws = path_of(&e.ok(&["spawn", "s1"]));
    assert!(ws.join("sm/.git").is_dir(), "submodule has its own git dir");
    assert_eq!(git(&ws, &["status", "--porcelain"]), "");
    assert_eq!(fs::read_to_string(ws.join("sm/i.txt")).unwrap(), "i\n", "source edits do not leak");

    fs::write(ws.join("sm/new.txt"), "n\n").unwrap();
    git(&ws.join("sm"), &["add", "-A"]);
    git(&ws.join("sm"), &["commit", "-qm", "inner change"]);
    let ws_sm_head = git(&ws.join("sm"), &["rev-parse", "HEAD"]);
    assert_eq!(git(&e.repo.join("sm"), &["rev-parse", "HEAD"]), src_sm_head, "source submodule untouched");
    assert_eq!(fs::read_to_string(e.repo.join("sm/i.txt")).unwrap(), "dirty in source\n");

    e.ok(&["land", "s1", "-m", "bump sm"]);
    let tree = git(&e.repo, &["ls-tree", "agent-ws/s1", "sm"]);
    assert!(tree.contains(&ws_sm_head), "landed branch records the new submodule commit: {tree}");
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
    assert_eq!(g["corrupt"], serde_json::json!(["@pool-7", "evil", "evil2"]));
    assert!(victim.join("a.txt").exists(), "the named workspace survives");
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
    assert_eq!(git(&e.repo, &["rev-parse", "agent-ws/k~1"]), first["commit"].as_str().unwrap(), "fast-forward on top of the first land");
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
    assert_eq!(s["secrets_removed"], serde_json::json!([".env", "cert.pem", "conf/id_rsa"]));
    assert!(!ws.join("cert.pem").exists() && !ws.join("scratch.txt").exists());

    let kept = path_of(&e.ok(&["spawn", "w2", "--keep-secrets"]));
    assert_eq!(fs::read_to_string(kept.join("cert.pem")).unwrap(), "key");
    assert!(kept.join("conf/id_rsa").exists() && kept.join(".env").exists());
    assert!(!kept.join("scratch.txt").exists(), "other untracked files are still left out");
}
