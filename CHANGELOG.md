# Changelog

All notable changes to agent-ws are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] - 2026-11-06

### Added

- **Claude Code worktrees.** `agent-ws claude install` (`--scope
  local|project|user`) registers agent-ws as Claude Code's `WorktreeCreate`
  and `WorktreeRemove` hook. `claude --worktree`, subagents with `isolation:
  "worktree"`, workflows and background sessions then get a workspace with
  dependencies, from the pool when one is ready, on a branch
  `agent-ws/<name>` that keeps the session's commits. Unused Claude worktrees,
  which Claude Code never removes for subagents, are pruned after a day.
  `agent-ws claude uninstall` undoes it.
- **Linux**, on Btrfs and XFS: files are reflinked (`FICLONE`), `du` reads
  shared extents with `FIEMAP`, and processes come from `/proc`. Releases
  include static x86_64 and arm64 Linux binaries.
- `agent-ws exec NAME [--spawn] -- CMD` runs a command in a workspace with
  `PORT` (4100, 4110, … one range per workspace, `port_base`/`port_step`) and
  `AGENT_WS_NAME`, `AGENT_WS_PATH`, `AGENT_WS_BASE`, `AGENT_WS_REPO`,
  `AGENT_WS_SLOT`, and exits with the command's exit code.
- `agent-ws diff NAME [--stat] [PATH…]`: the full patch since the base, new
  files included, without touching the workspace's index; `--json` lists
  files with line counts.
- `status` merges overlapping workspaces in memory (Git 2.38+) and lists the
  pairs that would conflict as `CONFLICT`, with their paths.
- `agent-ws list --long`: age, port, uncommitted changes, commits, processes
  inside, the last `exec` (running, exit code, interrupted) and real disk use.
- `[hooks]` in `.agent-ws.toml`: `post_spawn` runs in each new workspace,
  `pre_land` before `land` commits. They run only after `agent-ws trust`, and
  again only after another `trust` once they change.
- `spawn --carry` brings your uncommitted edits and untracked files along;
  `spawn --branch B` checks out a new branch in the workspace.
- `land --squash` makes one commit; `land --pr` pushes the branch and opens a
  pull request with the GitHub CLI (`--pr-base`, `--draft`). If the push
  fails, the workspace stays and the same command resumes.
- `agent-ws completions bash|zsh|fish|elvish|powershell`; the Homebrew formula
  installs them.

## [0.2.2] - 2026-11-06

### Fixed

- `spawn` no longer deletes files that hold only certificates. They match
  `*.pem` by name, but they are public: without certifi's `cacert.pem` CA
  bundle, pip and requests in a workspace's Python `.venv` failed every
  HTTPS call. A file is still removed if it holds a private key, or if it is
  not a PEM file at all.
- `agent-ws status` no longer rewrites the workspace index. A coordinator
  polling `status` while an agent committed could make the agent's commit
  fail with `index.lock: File exists`.
- `spawn` refuses workspace names that cannot become the branch
  `agent-ws/NAME` (`fix.lock`, `a..b`, `v2.`). `land` checks the branch
  name, including `--branch`, before committing anything. Before, it
  committed first and then failed, and landing again left two commits.
- `prune --gh` now asks GitHub about every local branch at the workspace's
  HEAD. A workspace kept with `land --keep` runs on a detached HEAD, so it
  was never matched with its merged `agent-ws/NAME` PR.
- `land` no longer fails when another workspace cannot be read by Git; that
  workspace is left out of the overlap check. `status` names the workspace
  in its error.
- Lands and drops run in parallel no longer fail with `git worktree prune:
  fatal: Invalid path '.../.git/worktrees'` (Git 2.56) after doing their
  work. `git worktree add` and `prune` now take turns.
- The published crate holds only the source, tests, README, license and
  changelog.

### Changed

- CI and release workflows use the Node 24 versions of the GitHub actions
  (`checkout` v7, `upload-artifact` v7, `download-artifact` v8).

## [0.2.1] - 2026-10-26

### Added

- Prebuilt macOS binaries (Apple silicon and Intel) attached to each GitHub
  release, installable with `cargo binstall agent-ws` or
  `brew install mendoanjoe/tap/agent-ws`.

### Fixed

- Building on a platform other than macOS now stops with one clear error
  ("agent-ws only supports macOS") at the top, before the unresolved
  macOS-only symbols.
- `Cargo.toml` declares `rust-version = "1.88"`, so older toolchains get a
  clear message instead of a compile error.
- docs.rs builds the documentation for macOS targets.

## [0.2.0] - 2026-10-26

### Added

- `agent-ws prune` removes clean workspaces whose HEAD is already merged,
  squash-merged, merged as a PR (with `--gh`), or never moved. It skips
  workspaces that are dirty, unmerged, too young (`--min-age-min`) or in use.
  Flags: `--base`, `--dry-run`.
- `agent-ws du [NAME]` shows the real disk usage of each workspace: the bytes
  not shared with any other file (APFS private size), next to what `du`
  reports, plus the pool and the trash.
- After a spawn, a background `prune --merged-only` runs at most every 10
  minutes (`auto_prune`, on by default). Workspaces that were never used are
  kept.
- `spawn` warns when there are 20 or more workspaces (`warn_workspaces`) or
  less than 10 GB of free disk.
- Usage recipes for each agent: claude, codex, opencode, omp, cursor-agent
  and aider (`docs/agents.md`), and disk usage findings (`docs/disk-usage.md`).

### Fixed

- `gc` no longer deletes a workspace folder that has no record if it still
  holds work (uncommitted changes, or commits that no branch, remote branch or
  tag points to). It registers the folder again instead. Folders that Git
  cannot read are left alone.
- Background deletes run in their own process group. `drop` restarts trash
  deletes that are older than 10 minutes.
- The pool size is capped at 8.

## [0.1.0] - 2026-10-15

First release: `spawn`, `list`, `status`, `path`, `land`, `drop`, `gc`, the
pool of ready workspaces and the config file.

[Unreleased]: https://github.com/mendoanjoe/agent-ws/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mendoanjoe/agent-ws/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mendoanjoe/agent-ws/releases/tag/v0.1.0
