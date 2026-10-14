//! End-to-end tests against a throwaway repository on the temp volume.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        cmd(env!("CARGO_BIN_EXE_agent-ws"), &self.repo).env("AGENT_WS_HOME", &self.home).args(args).output().unwrap()
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
