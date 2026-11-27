# Changelog

All notable changes to agent-ws are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Incremental Pool Refresh**: Pooled workspaces now key on `HEAD:<deps_key>`, where `<deps_key>` hashes workspace-shaping config (`exclude`, `secrets`, `secrets_keep`, `strict_symlinks`, `scan_skip`) and dependency stamp files. When only `HEAD` moves, `pool status` reports those entries as `refreshable` and `spawn` / `pool fill` refreshes them in place (`git checkout -f --detach` + `git clean -fdq`) instead of re-cloning.
- **Glob `exclude` and `scan_skip`**: `exclude` and the new `scan_skip` option in `.agent-ws.toml` support `*`, `?`, and `**` path globs (for example `**/node_modules`, `apps/*/dist`, `*.log`). `clone_tree` still preserves single-syscall `clonefile(2)` on macOS for unaffected subtrees, and `scan_skip` prunes heavy directories from secret/symlink scans.
- **Workspace forking (`spawn --from`)**: Clone a new workspace from an existing workspace instead of the main checkout, preserving its base commit, uncommitted edits, staged index state, untracked files, caches, and submodules.
- **Idempotent & auto-named spawns (`spawn --ensure`, `spawn --auto`)**: `--ensure` reuses an existing ready workspace (`reused: true`) instead of failing, validating `--branch`/`--base` if provided; `--auto` picks the first free name from `NAME`, `NAME-2`, ..., `NAME-100`.
- **Instant CoW checkpoints (`snap`, `snaps`, `restore`)**: Create named or auto-named CoW snapshots of a workspace tree, index, and `HEAD`, list them, delete them (`snap NAME SNAP --drop`), or restore a workspace to a snapshot (repeatable, latest by default).
- **Sync and dependency refresh (`sync`, `resync-deps`)**: `agent-ws sync` rebases (or `--merge`s) a clean workspace onto an updated base commit (default `HEAD`), aborting cleanly on conflict and optionally refreshing dependencies with `--resync-deps`. `agent-ws resync-deps` atomically CoW-swaps git-ignored dependency directories (auto-detected `node_modules`, `.venv`, `venv`, `target`, `.next`, `.turbo`, `.nuxt`, `vendor`, `dist`, `build`, or explicit paths) from the source checkout.
- **Cohort disk accounting, `du --depth`, `max_private_gb`, and `prune --target-free-gb`**:
  - `agent-ws du --depth N` adds a per-directory usage breakdown to depth `N`.
  - `agent-ws du` computes `cohort_shared_bytes` and `reclaimable_bytes` across workspaces, pool, and `.trash` using APFS `ATTR_CMNEXT_CLONEID` on macOS and Linux `FIEMAP` shared extent IDs, catching blocks orphaned when the source checkout reinstalls dependencies.
  - `max_private_gb` in `.agent-ws.toml` warns during `spawn` when total private disk usage exceeds the budget and surfaces `budget` in `du`.
  - `agent-ws prune --target-free-gb N` drops the largest eligible merged/clean workspaces (and clears the pool if needed) until at least `N` GB of disk space is free.
- **Agent-native CLI & structured JSON errors**:
  - `agent-ws whoami [--status]` identifies whether `cwd` is inside a workspace (and its `name`, `base`, `branch`, `port`, `overlaps`, `conflicts`).
  - `agent-ws skill` prints a self-contained `SKILL.md` workflow guide for coding agents.
  - `agent-ws drop --if-exists` succeeds with `dropped: false` when a workspace does not exist.
  - Self-action guard: `land` and `drop` refuse to act on the workspace containing `cwd` (`SELF_ACTION_BLOCKED`) unless `--allow-self` is passed.
  - `--json` errors print structured `{"ok":false,"code":"<CODE>","error":"..."}` payloads to `stderr` (`ALREADY_EXISTS`, `NOT_FOUND`, `CONFLICT`, `DIRTY`, `UNTRUSTED_HOOKS`, `LOW_DISK`, `INVALID_INPUT`, `SELF_ACTION_BLOCKED`, `UNSUPPORTED_FS`, `GIT_ERROR`, `INTERNAL`).
  - `agent-ws diff --stat --json` and `agent-ws log --json` are now supported.

### Changed

- **Parallel filesystem & Git operations**: Linux `FICLONE` cloning now batches files across worker threads; workspace secret/symlink scanning (`scan_with_skip`) runs across top-level subdirectories in parallel; `changed_paths_status` combines `git status` and `git diff` in a single pass; clean workspaces skip `snapshot_tree` during `diff`/`status` and `conflicts()` snapshots and merges in parallel.
- **Desktop app**: Switched `diff` IPC to single-snapshot `diff_bundle` with a 4,000-line patch cap (`truncated`), disabled unused `cli` crate features in `agent-ws-desktop`, added dedicated `disk_usage` IPC, cached `Repo` discovery in the background monitor with adaptive active/idle polling, and debounced filesystem change events.
- **CI, benchmarks & hygiene**: Pinned GitHub Actions to commit SHAs, added build provenance attestation (`actions/attest-build-provenance`), enabled `Swatinem/rust-cache`, enforced `--locked` builds, added Dependabot config, and hardened `bench/bench.sh`.

### Fixed

- **Core safety & correctness**: Hardened `isolate_submodules` against `gitdir:` path escapes, symlinked `.git`, and untrusted `core.fsmonitor`/hooks; enforced strict `owns()`/`meta()` path binding and symlink checks; made `write_record(..., true)` atomic via temp file + `hard_link`; fixed `.ends_with(".json")` filtering so workspace names containing `.json` do not collide with temp files; preserved non-UTF-8 paths via raw `-z` byte parsing; kept `state = "creating"` until `--branch` and `post_spawn` hooks succeed; added pre-mutation conflict and checked-out-branch checks in `land`; fixed `pid_alive` zombie and `EPERM` handling on macOS and Linux.
- **Filesystem, Git, Config & Hooks**: Normalized symlink targets in `scan` to block `strip_prefix` traversal and check `is_secret` on symlinks; tightened streaming PEM certificate validation in `only_certificates`; made `relink` atomic; fixed `clone_tree` permission/mtime ordering on read-only directories; removed `FIEMAP_FLAG_SYNC` and guarded `FIEMAP` extent overflow/zero-length loops; added `check_rev` and `--end-of-options` guards across `git.rs` and `main.rs`; added 1 MiB cap and semantic validation to `.agent-ws.toml`; made Claude Code settings reads/writes symlink-safe and atomic while preserving sibling hooks.
- **Desktop app & UI**: Tightened CSP and `open_with` allowlist/path validation; made tray initialization non-fatal; fixed `refresh()`, `loadDiff()`, and `loadLog()` async staleness races, modal re-entry/double-submit bugs, and keyboard/ARIA accessibility.

## [0.4.0] - 2026-11-06

### Added

- **Desktop app** in `desktop/` (Tauri, macOS and Linux): every repository's
  workspaces in one window that updates by itself, `CONFLICT`/`OVERLAP` lines,
  the diff and `exec` log of a workspace, open in editor/terminal/files/browser
  at its `PORT`, land (squash, pull request), drop and prune, a menu-bar count
  of running agents and a notification when one finishes. Released separately,
  with tags `desktop-v*`.
- `agent-ws repos` lists every repository agent-ws keeps workspaces for (the
  state folder now notes its checkout in `repo.json`).
- `exec` keeps the command's output in a log, read with `agent-ws log NAME`:
  with `--log`, and by default when stdout is not a terminal.
- `list --long --no-disk` skips the real disk use, which reads every file.

### Fixed

- `gc` removed the run records of workspaces with a dot in their name (such as
  `v1.2`), taking the name only up to the first dot.

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

[Unreleased]: https://github.com/mendoanjoe/agent-ws/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/mendoanjoe/agent-ws/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mendoanjoe/agent-ws/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mendoanjoe/agent-ws/releases/tag/v0.1.0
