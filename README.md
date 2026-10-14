# agent-ws

Instant, isolated workspaces for coding agents on macOS (APFS).

A workspace is a Git worktree whose files are APFS clones of your checkout,
ignored files included. Agents start with `node_modules` and build caches in
place, so there is no `install` step, and the clones share disk blocks until
something is written.

```sh
agent-ws spawn a1                 # ~1 s on a 2.7k-file repo with 700 packages
cd "$(agent-ws path a1)"          # work there
agent-ws status                   # changed files per workspace + OVERLAP warnings
agent-ws land a1 -m "feat: ..."   # commit to branch agent-ws/a1, remove workspace
agent-ws drop a1                  # remove without landing (instant)
agent-ws list
agent-ws gc                       # clean up after crashes (--dry-run to preview)
```

Every command accepts `--json`.

## What a workspace contains

- Tracked files at the base commit (`--base`, default `HEAD`). Uncommitted
  edits and untracked files in your checkout are not copied.
- Ignored files from your checkout (dependencies, caches, build output).
- Not `.git` (the worktree links to the shared object store) and not
  `.claude/worktrees`. Skip more with `--exclude PATH`.
- Not ignored secret files: `.env`, `.env.*` (except `.env.example`) and
  `.dev.vars`. Keep them with `--keep-secrets`.

## Safety

- Writes in a workspace never change your checkout (copy-on-write).
- Absolute symlinks into your checkout are retargeted into the workspace.
  Absolute symlinks elsewhere are reported with a warning.
- `land` refuses to overwrite an existing branch (`--force-branch` to allow)
  and keeps the workspace when it fails.
- `drop` renames the tree into `.trash` and deletes it in the background.
- Spawning stops below 2 GB of free disk (`--min-free-gb`).

## Many agents at once

- Spawn, land and drop are safe to run in parallel. A spawn claims its name
  first, so two agents asking for the same name get exactly one workspace.
- Clones contend for the same disk: 8 parallel spawns of a 2.7k-file repo
  with 700 packages finish in ~4.4 s wall time (~1 s each when sequential).
- `gc` removes spawns whose process died, records of folders deleted by hand,
  folders with no record and trash older than 10 minutes. It never touches a
  spawn whose process is still alive.

## Storage

State and workspaces live in `~/.agent-ws/<repo>-<hash>/` (override with
`AGENT_WS_HOME`; it must be on the same APFS volume as the repository). The
folder is excluded from Spotlight and Time Machine.

## Library

All logic is in the `agent_ws` library (`Repo::spawn`, `status`, `land`,
`drop`) so other tools can embed it; `src/main.rs` is a thin CLI.
