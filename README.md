# agent-ws

Instant, isolated workspaces for coding agents on macOS.

Running several coding agents on one repository means one checkout per agent.
`git worktree add` gives you that, but every worktree needs its own dependency
install, takes seconds to create and remove, and costs a full copy on disk.

agent-ws clones your checkout with APFS copy-on-write instead: dependencies and
build caches come along, nothing is copied until it is written, and a workspace
is ready in about a second, or in ~40 ms from a pool. When the agent is done,
`land` turns its work into a branch.

| | `git worktree` + install | agent-ws |
| --- | --- | --- |
| New workspace, ready to build | 2.5 – 4.8 s | 1.2 – 1.4 s, 41 ms from the pool |
| Remove it | 2.2 – 2.8 s | 35 ms |
| Disk per workspace | full copy | ~15 MB |

Measured on a 2.7k-file TypeScript monorepo with 26k files in `node_modules`;
see [Benchmarks](#benchmarks).

## Requirements

- macOS with an APFS volume (the default on every modern Mac)
- Git 2.31 or later
- Rust 1.88 or later, to build

## Install

```sh
cargo install --git https://github.com/mendoanjoe/agent-ws
```

or from a clone: `cargo install --path .`

## Quick start

Run these inside your repository:

```sh
agent-ws spawn fix-login             # create a workspace (base: HEAD)
cd "$(agent-ws path fix-login)"      # it is a normal Git worktree; deps included
# ... edit, build, test, commit if you like ...
agent-ws status                      # what each workspace changed
agent-ws land fix-login -m "fix: login redirect"   # -> branch agent-ws/fix-login
```

`land` commits whatever is left, creates the local branch `agent-ws/<name>` in
your repository and removes the workspace. Push it or open a PR as usual. To
throw the work away instead: `agent-ws drop fix-login`.

Your own checkout is never changed: its uncommitted edits are not copied into
workspaces, and nothing written in a workspace reaches it.

## Using it with coding agents

An agent cannot move into a workspace in the middle of a session, so the
coordinator (you, a script, or an orchestrating agent) creates the workspace
first and starts the agent inside it:

```sh
agent-ws spawn task-42
W=$(agent-ws path task-42)
(cd "$W" && claude)                  # or codex, opencode, omp, aider, ...
agent-ws land task-42 -m "feat: task 42"
```

Each agent starts a little differently (`codex -C`, `omp --cwd`, the `cd`
subshell for `claude` and `cursor-agent`, …). [docs/agents.md](docs/agents.md)
has a verified, copy-paste recipe for Claude Code, Codex, opencode, omp, Cursor
Agent and aider — interactive and headless — plus a parallel-fleet script.

Several agents in parallel:

```sh
agent-ws pool set 4                  # keep 4 workspaces ready
for t in a b c d; do
  agent-ws spawn "$t"
  (cd "$(agent-ws path "$t")" && my-agent --task "$t") &
done
wait
agent-ws status                      # OVERLAP lines: two agents touched one file
for t in a b c d; do agent-ws land "$t" -m "agent: $t"; done
```

A line worth putting in every agent prompt:

> You work in an isolated workspace. Stay in it and do not switch branches.
> Commit if you like; the coordinator lands your work.

Every command accepts `--json` for scripts.

## Commands

| Command | What it does |
| --- | --- |
| `spawn NAME` | Create a workspace. `--base REV`, `--exclude PATH`, `--keep-secrets`, `--no-pool`, `--min-free-gb N` |
| `path NAME` | Print the workspace folder |
| `list` | List workspaces |
| `status [NAME]` | Changed files per workspace and files changed in more than one (`OVERLAP`) |
| `land NAME -m MSG` | Commit, create branch `agent-ws/NAME`, remove the workspace. `--branch B`, `--keep`, `--no-verify`, `--force-branch` |
| `drop NAME` | Remove a workspace without landing |
| `pool set N` / `fill` / `status` / `clear` | Keep N workspaces ready |
| `gc [--dry-run]` | Clean up after crashed or killed processes |

## What a workspace contains

- Your tracked files at the base commit.
- Your ignored files: `node_modules`, build caches, generated files.
- Your submodules, each with its own copy of its Git data.
- Not: your uncommitted edits, untracked files, or untracked secret files
  (`.env`, `.env.*`, `.dev.vars`, `*.pem`, `*.key`, `*.p12`, `id_rsa`,
  `id_ed25519`; `.env.example` and friends stay). Removed secrets are listed in
  the `spawn` output; `--keep-secrets` keeps them.

## Pool

`agent-ws pool set N` keeps N workspaces ready, so `spawn` takes ~40 ms and
refills the pool in the background. A pooled workspace is only handed out
while it still matches your repository: same `HEAD`, same settings, same
lockfiles. Spawns with `--base`, `--exclude`, `--keep-secrets` or `--no-pool`
always clone fresh. `pool set 0` turns it off.

## Configuration

Optional `.agent-ws.toml` at the repository root:

```toml
exclude = ["tmp/huge"]          # never copied into workspaces
secrets = [".env", "*.pem"]     # replaces the default secret patterns
secrets_keep = [".env.example"] # replaces the default exceptions
strict_symlinks = true          # fail spawn if a symlink points outside the workspace
min_free_gb = 5                 # refuse to spawn below this (default 2)
pool = 4                        # default pool size; `pool set` overrides it
pool_stamp = ["pnpm-lock.yaml"] # files whose change makes pooled workspaces stale
```

Unknown keys are an error. Workspaces and state live in
`~/.agent-ws/<repo>-<hash>/`; set `AGENT_WS_HOME` to move them (same APFS
volume as the repository).

## Troubleshooting

| Message or symptom | What to do |
| --- | --- |
| `... are on different volumes` | Put `AGENT_WS_HOME` on the volume that holds the repository. |
| `branch agent-ws/NAME already exists` | Land with `--branch other-name`, or `--force-branch` if you mean to overwrite. |
| `still being created` / leftovers after a crash | `agent-ws gc --dry-run`, then `agent-ws gc`. |
| `corrupt record` | A state file was edited by hand; `agent-ws gc` forgets it safely. |
| `WARNING symlink ... points outside the workspace` | Writes through that link leave the workspace. Fix the link, or set `strict_symlinks`. |
| First build in a workspace is slow | Expected: files are read cold once; later builds are warm. |

## Benchmarks

Apple M4 (10 cores, 16 GB), macOS 26.6, git 2.39. A private TypeScript monorepo
with 2,693 tracked files and 26,411 files in `node_modules` (pnpm). Ranges over
two runs of `bench/bench.sh`, each value the median of 3 tries.

| Step | `git worktree` | agent-ws |
| --- | --- | --- |
| New workspace, ready to build | 2.5 – 4.8 s (add + `pnpm install`) | 1.16 – 1.38 s |
| New workspace from the pool | — | 41 – 43 ms |
| 8 new workspaces at once (wall time) | — | 4.6 – 5.1 s fresh, 75 – 81 ms from the pool |
| Remove a workspace | 2.2 – 2.8 s | 33 – 36 ms |
| Disk per workspace | full copy | 13 – 15 MB real (820 MB logical) |

The install used a warm pnpm store, `--offline` and `--ignore-scripts`, the
best case for `git worktree`. Reproduce on a throwaway APFS copy of your
repository:

```sh
cp -c -R my-repo /tmp/my-repo-copy
bench/bench.sh /tmp/my-repo-copy "pnpm install --frozen-lockfile --offline"
```

## How it works

[docs/how-it-works.md](docs/how-it-works.md) has the diagrams, the pool and
`gc` rules, the safety guarantees and a code map.

## Contributing

Contributions are welcome: bug reports, fixes, Linux support (reflink),
docs. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, tests and the review
checklist.

## License

[MIT](LICENSE)
