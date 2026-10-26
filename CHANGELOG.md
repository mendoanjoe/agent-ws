# Changelog

All notable changes to agent-ws are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/mendoanjoe/agent-ws/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mendoanjoe/agent-ws/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mendoanjoe/agent-ws/releases/tag/v0.1.0
