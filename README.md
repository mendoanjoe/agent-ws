# agent-ws

Instant, isolated workspaces for coding agents on macOS (APFS).

A workspace is a Git worktree whose files are APFS clones of your checkout,
ignored files included. Agents start with `node_modules` and build caches in
place, so there is no `install` step, and the clones share disk blocks until
something is written.

```sh
agent-ws pool set 4               # keep 4 workspaces ready (optional)
agent-ws spawn a1                 # ~40 ms from the pool, ~1 s cloning fresh
cd "$(agent-ws path a1)"          # work there
agent-ws status                   # changed files per workspace + OVERLAP warnings
agent-ws land a1 -m "feat: ..."   # commit to branch agent-ws/a1, remove workspace
agent-ws drop a1                  # remove without landing (instant)
agent-ws list
agent-ws gc                       # clean up after crashes (--dry-run to preview)
```

Every command accepts `--json`. Timings are from a 2.7k-file repository with
700 npm packages.

## What a workspace contains

- Tracked files at the base commit (`--base`, default `HEAD`). Uncommitted
  edits and untracked files in your checkout are not copied.
- Ignored files from your checkout (dependencies, caches, build output).
- Not `.git` (the worktree links to the shared object store) and not
  `.claude/worktrees`. Skip more with `--exclude PATH` or `exclude` in the
  config.
- Not untracked secret files, wherever they are (also inside ignored
  folders): `.env`, `.env.*`, `.dev.vars`, `*.pem`, `*.key`, `*.p12`,
  `id_rsa`, `id_ed25519`, except `.env.example`, `.env.sample` and
  `.env.template`. Tracked files are never removed. Keep secrets with
  `--keep-secrets`.

## Pool

`agent-ws pool set N` keeps N workspaces ready. A spawn takes one with an
atomic rename (8 parallel spawns: 80 ms wall time, ~40 ms each) and starts a
background `pool fill` to replace it. Without a pool, parallel spawns contend
for the disk (8 at once: ~4.4 s).

A pooled workspace is used only while it still matches: same `HEAD`, same
config, and unchanged lockfiles and `node_modules` stamp files
(`pool_stamp`). Spawns with `--base`, `--exclude` or `--keep-secrets`, or
`--no-pool`, always clone fresh. `pool status` shows fresh and stale entries,
`pool fill` replaces stale ones now, `pool clear` empties the pool.

The first build or typecheck in any new workspace reads files cold (the page
cache is per file, not per disk block), so it is slower than in your checkout;
later runs are warm.

## Configuration

Optional `.agent-ws.toml` at the repository root:

```toml
exclude = ["tmp/huge"]          # never cloned
secrets = [".env", "*.pem"]     # replaces the default secret patterns
secrets_keep = [".env.example"] # replaces the default exceptions
strict_symlinks = true          # fail a spawn on symlinks leaving the workspace
min_free_gb = 5                 # default 2
pool = 4                        # default pool size; `pool set` overrides it
pool_stamp = ["pnpm-lock.yaml"] # files whose change makes the pool stale
```

Unknown keys are an error.

## Safety

- Writes in a workspace never change your checkout (copy-on-write).
- Absolute symlinks into your checkout are retargeted into the workspace.
  Relative symlinks that climb out of the repository are rewritten to the
  absolute path they meant in your checkout. Both kinds of links that leave
  the workspace are reported, and fail the spawn with `strict_symlinks`.
- `land` refuses to overwrite an existing branch (`--force-branch` to allow),
  keeps the workspace when it fails and reports files other workspaces also
  changed.
- `drop` renames the tree into `.trash` and deletes it in the background.
- Spawning stops below 2 GB of free disk.

## Many agents at once

- Spawn, land, drop and pool fills are safe to run in parallel. A spawn
  claims its name first, so two agents asking for the same name get exactly
  one workspace, and a pooled workspace is never handed out twice.
- `gc` removes spawns and pool fills whose process died, records of folders
  deleted by hand, dead pool claims, folders with no record and trash older
  than 10 minutes. It never touches work whose process is still alive.

## Storage

State and workspaces live in `~/.agent-ws/<repo>-<hash>/` (override with
`AGENT_WS_HOME`; it must be on the same APFS volume as the repository). The
folder is excluded from Spotlight and Time Machine.

## Library

All logic is in the `agent_ws` library (`Repo::spawn`, `status`, `land`,
`drop`, `gc`, `pool_fill`) so other tools can embed it; `src/main.rs` is a
thin CLI.
