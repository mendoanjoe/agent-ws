# agent-ws

Instant, isolated workspaces for coding agents on macOS and Linux.

Running several coding agents on one repository means one checkout per agent.
`git worktree add` gives you that, but every worktree needs its own dependency
install, takes seconds to create and remove, and costs a full copy on disk.

agent-ws clones your checkout copy-on-write instead (APFS clones on macOS,
reflinks on Btrfs and XFS on Linux): dependencies and build caches come along,
nothing is copied until it is written, and a workspace is ready in about a
second, or in ~40 ms from a pool. When the agent is done, `land` turns its work
into a branch or a pull request. It can also be the worktree backend of Claude
Code, so `claude --worktree` and subagents start with dependencies installed.

| | `git worktree` + install | agent-ws |
| --- | --- | --- |
| New workspace, ready to build | 2.5 – 4.8 s | 1.2 – 1.4 s, 41 ms from the pool |
| Remove it | 2.2 – 2.8 s | 35 ms |
| Disk per workspace | full copy | ~15 MB |

Measured on a 2.7k-file TypeScript monorepo with 26k files in `node_modules`;
see [Benchmarks](#benchmarks).

## Requirements

- macOS with an APFS volume (the default on every modern Mac), or Linux with
  the repository on Btrfs or XFS (`reflink=1`, the default since xfsprogs
  5.1). ext4 has no reflinks.
- Git 2.31 or later (2.38 or later for conflict detection in `status`)
- Rust 1.88 or later, to build

## Install

```sh
brew install mendoanjoe/tap/agent-ws   # macOS, prebuilt, with shell completion
cargo binstall agent-ws                # macOS and Linux, prebuilt
cargo install agent-ws                 # build from source (Rust 1.88+)
```

Prebuilt binaries for Apple silicon, Intel Macs and Linux (x86_64 and arm64,
static) are also attached to each
[GitHub release](https://github.com/mendoanjoe/agent-ws/releases). Shell
completion for other installs: `agent-ws completions zsh > ~/.zfunc/_agent-ws`
(or `bash`, `fish`).
For the latest unreleased version: `cargo install --git https://github.com/mendoanjoe/agent-ws`,
or from a clone: `cargo install --path .`

## Quick start

Run these inside your repository:

```sh
agent-ws spawn fix-login             # create a workspace (base: HEAD)
cd "$(agent-ws path fix-login)"      # it is a normal Git worktree; deps included
# ... edit, build, test, commit if you like ...
agent-ws status                      # what each workspace changed
agent-ws diff fix-login              # the full patch, new files included
agent-ws land fix-login -m "fix: login redirect"   # -> branch agent-ws/fix-login
```

`land` commits whatever is left, creates the local branch `agent-ws/<name>` in
your repository and removes the workspace. Add `--pr` to push it and open a
pull request (with the GitHub CLI), or `--squash` for a single commit. To
throw the work away instead: `agent-ws drop fix-login`.

Your own checkout is never changed: its uncommitted edits are not copied into
workspaces (unless you hand them over with `spawn --carry`), and nothing
written in a workspace reaches it.

Need checkpoints before a risky change, or want to branch off an in-progress
workspace?

```sh
agent-ws snap fix-login pre-refactor # instant CoW checkpoint (tree + index + HEAD)
agent-ws restore fix-login           # restore latest checkpoint if an attempt goes wrong
agent-ws spawn fix-v2 --from fix-login # fork edits, staged index, untracked files & caches
agent-ws sync fix-login --resync-deps  # rebase onto updated HEAD & CoW-refresh deps
```

## Using it with coding agents

An agent cannot move into a workspace in the middle of a session, so the
coordinator (you, a script, or an orchestrating agent) creates the workspace
first and starts the agent inside it:

```sh
agent-ws exec task-42 --spawn -- claude   # or codex, opencode, omp, aider, ...
agent-ws land task-42 -m "feat: task 42"
```

`exec` runs any command inside the workspace (`--spawn` creates it first) and
passes on its exit code. Each workspace has its own port: commands get
`PORT=4100` in the first workspace, `4110` in the next, and so on, plus
`AGENT_WS_NAME`, `AGENT_WS_PATH` and friends, so the dev servers of parallel
agents do not collide. `agent-ws list --long` shows each workspace's age,
port, changes, the processes inside and whether its last `exec` is still
running.

Each agent starts a little differently (`codex -C`, `omp --cwd`, …).
[docs/agents.md](docs/agents.md) has a verified, copy-paste recipe for Claude
Code, Codex, opencode, omp, Cursor Agent and aider — interactive and
headless — plus a parallel-fleet playbook, idempotent orchestration flags,
checkpoints, and structured JSON error handling.

Several agents in parallel:

```sh
agent-ws pool set 4                  # keep 4 workspaces ready
for t in a b c d; do
  agent-ws exec "$t" --spawn --log -- my-agent --task "$t" &
done
wait
agent-ws status                      # OVERLAP: two agents touched one file;
                                     # CONFLICT: their changes do not merge
for t in a b c d; do agent-ws land "$t" -m "agent: $t"; done
```

### Agent ergonomics & structured JSON

- **Self-contained guide**: `agent-ws skill` prints a `SKILL.md` guide with
  YAML frontmatter that coding agents can load directly.
- **Context discovery**: `agent-ws whoami --json` tells an agent whether it is
  inside a workspace (`inside_workspace`, `name`, `base`, `branch`, `port`,
  `origin`), and `whoami --status --json` adds file `overlaps` and `conflicts`.
- **Idempotent scripts**: `spawn --ensure` reuses an existing ready workspace
  (`reused: true`), `spawn --auto` picks the first free name (`NAME`, `NAME-2`,
  …, `NAME-100`), and `drop --if-exists` exits `0` with `dropped: false` when
  the workspace is already gone.
- **Self-action guard**: `land` and `drop` refuse to run when your current
  working directory is inside the target workspace (`SELF_ACTION_BLOCKED`),
  preventing an agent from deleting its own cwd; pass `--allow-self` to
  override.
- **Structured JSON errors**: Passing `--json` to any command prints failures to
  `stderr` as `{"ok":false,"code":"<CODE>","error":"..."}` and exits `1`.
  Codes: `ALREADY_EXISTS`, `NOT_FOUND`, `CONFLICT`, `DIRTY`, `UNTRUSTED_HOOKS`,
  `LOW_DISK`, `INVALID_INPUT`, `SELF_ACTION_BLOCKED`, `UNSUPPORTED_FS`,
  `GIT_ERROR`, `INTERNAL`.

### Claude Code

Claude Code makes its own git worktrees for `claude --worktree`, for subagents
with `isolation: "worktree"`, for workflows and for background sessions,
without your dependencies. Let agent-ws make them instead:

```sh
agent-ws claude install              # this repository, only you
agent-ws claude install --scope user # every repository
```

This adds `agent-ws claude hook` as Claude Code's `WorktreeCreate` and
`WorktreeRemove` hook (in `.claude/settings.local.json`, or
`~/.claude/settings.json` for `--scope user`; `--scope project` writes the
shared `.claude/settings.json`). Each Claude worktree is then a workspace on a
branch `agent-ws/<name>`, from the pool when one is ready. Its commits stay on
that branch when Claude removes the worktree. Claude Code never removes the
worktrees of subagents; the background prune drops unused ones after a day.
`agent-ws claude uninstall` undoes it. `agent-ws` must be on Claude Code's
`PATH`.

A line worth putting in every agent prompt:

> You work in an isolated workspace. Stay in it and do not switch branches.
> Commit if you like; the coordinator lands your work.

Every command accepts `--json` for scripts.

## Commands

| Command | What it does |
| --- | --- |
| `spawn NAME` | Create a workspace. `--base REV`, `--exclude PATH` (exact or glob), `--keep-secrets`, `--no-pool`, `--min-free-gb N`, `--carry` (bring your uncommitted edits), `--branch B` (check out a new branch), `--no-hooks`, `--from WS` (fork edits, index, untracked files & caches from `WS`), `--ensure` (reuse if ready), `--auto` (`NAME`, `NAME-2`, …, `NAME-100`) |
| `exec NAME -- CMD` | Run a command in the workspace with `PORT` and `AGENT_WS_*` set; exits with its code. `--spawn`, `--base REV`, `--log` (keep the output; on by default without a terminal) |
| `log NAME` | Print the output of the last `exec --log` (`--json` returns `{"name","log"}`) |
| `path NAME` | Print the workspace folder |
| `list` | List workspaces. `--long`: age, port, changes, processes inside, last `exec`, real disk use (`--no-disk` skips the last, which reads every file) |
| `repos` | Every repository agent-ws keeps workspaces for |
| `status [NAME]` | Changed files per workspace, files changed in more than one (`OVERLAP`), and pairs whose changes conflict (`CONFLICT`) |
| `diff NAME [PATH…]` | What the workspace changed since its base, new files included. `--stat`; `--json` returns per-file status and line counts (with or without `--stat`) |
| `land NAME -m MSG` | Commit, create branch `agent-ws/NAME`, remove the workspace. `--branch B`, `--keep`, `--no-verify`, `--force-branch`, `--squash`, `--pr` (`--pr-base B`, `--draft`), `--allow-self` |
| `drop NAME` | Remove a workspace without landing. `--if-exists`, `--allow-self` |
| `sync NAME` | Rebase (or `--merge`) a clean workspace onto `--base REV` (default `HEAD`), updating its recorded base; `--resync-deps` also CoW-refreshes ignored deps |
| `resync-deps NAME [PATH…]` | Atomically CoW-refresh git-ignored dependency directories from the source checkout (auto-detected if omitted) |
| `snap NAME [SNAP]` | Create an instant CoW checkpoint of a workspace (auto-named if `SNAP` omitted); `--drop` deletes `SNAP` |
| `snaps NAME` | List checkpoints of a workspace |
| `restore NAME [SNAP]` | Restore a workspace's tree, index, `HEAD`, and base from `SNAP` (latest if omitted) |
| `whoami` | Identify the workspace containing `cwd`; `--status` adds `overlaps` and `conflicts` |
| `skill` | Print a self-contained `SKILL.md` guide for coding agents |
| `gc [--dry-run]` | Clean up after crashed or killed processes |
| `prune` | Drop workspaces whose work is already merged. `--dry-run`, `--base BRANCH`, `--gh`, `--min-age-min N` (default `60`), `--target-free-gb N` |
| `du [NAME]` | Real disk use per workspace (bytes no other file shares) next to `du`, cohort shared/reclaimable bytes, and `--depth N` per-directory breakdown |
| `pool set N` / `fill` / `status` / `clear` | Keep N workspaces ready (at most 8); `fill [--size N]` refreshes commit-stale entries in place |
| `trust` | Allow the hooks in `.agent-ws.toml` to run (again after they change) |
| `claude install` / `uninstall` | Make Claude Code create its worktrees with agent-ws. `--scope local\|project\|user` |
| `completions SHELL` | Print a completion script for bash, zsh, fish, elvish or powershell |

## Desktop app

`desktop/` holds a small app (Tauri) for the same workspaces, on macOS and
Linux: every repository in a sidebar, its workspaces in a table that updates
by itself (age, port, last `exec`, changes, processes inside, real disk use),
`CONFLICT` and `OVERLAP` lines, and for the selected workspace its diff and
`exec` log. It opens a workspace in your editor, a terminal, the file manager
or the browser at its `PORT`, and lands (squash, pull request), drops and
prunes. A menu-bar icon counts the agents running and a notification says when
one finishes or fails. It uses the agent-ws library directly, so it shows
exactly what the CLI shows.

Build it from a clone (Node 24+ and the [Tauri prerequisites](https://tauri.app/start/prerequisites/)):

```sh
cd desktop
npm install
npm run tauri dev                       # run it
npm run tauri build                     # .app/.dmg on macOS, AppImage/.deb on Linux
```

Releases tagged `desktop-v*` carry a built app. It is not signed yet: on macOS,
right-click it and choose Open the first time.

## What a workspace contains

- Your tracked files at the base commit.
- With `--carry`: your uncommitted edits and untracked files too, as
  uncommitted changes on `HEAD`.
- With `--from WS`: the tree, staged index, untracked files, caches, and
  submodules of existing workspace `WS` (inheriting its base commit).
- Your ignored files: `node_modules`, build caches, generated files (minus
  `exclude` paths or globs).
- Your submodules, each with its own copy of its Git data.
- Not: your uncommitted edits, untracked files (unless `--carry` or `--from`),
  or untracked secret files (`.env`, `.env.*`, `.dev.vars`, `*.pem`, `*.key`,
  `*.p12`, `id_rsa`, `id_ed25519`; `.env.example` and friends stay). Removed
  secrets are listed in the `spawn` output; `--keep-secrets` keeps them. Files
  holding only certificates, such as the `cacert.pem` CA bundle in a Python
  `.venv`, are public and stay.

## Disk use and cleanup

`du` counts a cloned file in full in every workspace, so `du -sh ~/.agent-ws`
shows about the size of your checkout per workspace. The real cost is
`agent-ws du`: the bytes no other file shares, usually 10 – 50 MB per
workspace. Pass `--depth 1` (or higher) to see which directories inside each
workspace hold private bytes, and watch `cohort shared` / `reclaimable` to see
blocks shared across workspaces/pool/trash that the source checkout no longer
owns.

That cost grows when a workspace is kept around. When the source checkout
rewrites files (a reinstall, a rebuild), every old workspace becomes the only
owner of its copy (or shares an orphaned cohort with other old workspaces).
Refresh dependencies in live workspaces with `agent-ws resync-deps NAME` (or
`agent-ws sync NAME --resync-deps`), and drop workspaces when their branch is
merged:

```sh
git fetch                             # prune compares against origin's branch
agent-ws prune --dry-run              # what would go, and the space it frees
agent-ws prune                        # drop them
agent-ws prune --target-free-gb 20    # drop largest eligible until 20 GB is free
```

`prune` drops a workspace only when its tree is clean (ignored files such as
`node_modules` do not count) and one of these holds:

- its HEAD is in the base branch (`merged`);
- every file it changed is the same in the base, as after a squash or rebase
  merge (`squash-merged`);
- with `--gh`, GitHub has a merged PR from a branch at exactly its HEAD, such
  as the `agent-ws/NAME` branch a `land --keep` created (`pr-merged`);
- it has no commits and no changes since spawn (`unused`).

It never touches a workspace with uncommitted changes or unmerged commits,
one created less than an hour ago (`--min-age-min`), or one that a running
process (an agent, a shell) has as its current directory. The base is
`--base`, else `prune_base` from the config, else `origin/HEAD`,
`origin/main`, `origin/master`, `main` or `master`, the first that exists.
With `--target-free-gb N`, `prune` drops the largest eligible workspaces first
(and clears the pool if needed) until at least `N` GB is free on disk.

If you forget: every `spawn` also starts a background prune, at most once
per 10 minutes. It drops merged workspaces only and leaves never-used ones,
since a coordinator may have spawned those for later. The exception is
Claude Code's worktrees, which Claude never removes for subagents: unused
ones go once they are a day old. Turn it off with `auto_prune = false`.

`spawn` warns from 20 workspaces (`warn_workspaces`), below 10 GB free disk,
and when total private disk use exceeds `max_private_gb` (also shown as
`budget` in `agent-ws du`). See [docs/disk-usage.md](docs/disk-usage.md) for
measurements.

`gc` never deletes a workspace folder that has lost its record but still
holds work (uncommitted changes, or commits no branch contains). It
registers the folder again, so you can `land` or `drop` it.

## Pool

`agent-ws pool set N` keeps N workspaces ready, so `spawn` takes ~40 ms and
refills the pool in the background. A pooled workspace's freshness key tracks
both `HEAD` and a dependency/config key (`exclude`, `secrets`, `secrets_keep`,
`strict_symlinks`, `scan_skip`, and lockfile/stamp timestamps). When `HEAD`
moves without changing settings or stamp files, pooled entries stay
`refreshable`: `spawn` and `pool fill` update them incrementally in place with
`git checkout -f --detach` and `git clean -fdq` instead of re-cloning. When
settings or stamp files change, pooled entries become `stale` and are replaced.
Spawns with `--from`, `--exclude`, `--keep-secrets`, `--carry` or `--no-pool`,
or a `--base` other than `HEAD`, always clone fresh. `pool set 0` turns it
off; the maximum is 8.

## Configuration

Optional `.agent-ws.toml` at the repository root:

```toml
exclude = ["tmp/huge", "**/dist", "*.log"] # exact paths or globs never copied
scan_skip = ["target", "**/node_modules"]  # subtrees skipped during secret/symlink scan
secrets = [".env", "*.pem"]                # replaces the default secret patterns
secrets_keep = [".env.example"]            # replaces the default exceptions
strict_symlinks = true                     # fail spawn if a symlink points outside the workspace
min_free_gb = 5                            # refuse to spawn below this (default 2)
max_private_gb = 25                        # warn on spawn if private disk exceeds this; shown in `du`
pool = 4                                   # default pool size; `pool set` overrides it
pool_stamp = ["pnpm-lock.yaml"]            # files whose change makes pooled workspaces stale
prune_base = "origin/develop"              # branch `prune` compares against
warn_workspaces = 30                       # spawn warns from this many workspaces (0 = never)
auto_prune = false                         # no background prune after spawn (default on)
port_base = 5000                           # PORT of the first workspace (default 4100)
port_step = 20                             # ports per workspace (default 10)

[hooks]
post_spawn = ["op inject -i .env.tpl -o .env"] # run in each new workspace
pre_land = ["pnpm test"]                       # must pass before `land` commits
```

Unknown keys are an error. Workspaces and state live in
`~/.agent-ws/<repo>-<hash>/`; set `AGENT_WS_HOME` to move them (same volume as
the repository).

### Hooks

Hooks are shell commands run inside the workspace, with the same environment
as `exec` (`PORT`, `AGENT_WS_NAME`, …). `post_spawn` runs after every spawn,
also from the pool; if it fails, the workspace is removed again. `pre_land`
runs before `land` commits; if it fails, nothing is committed. Their output
goes to stderr.

Because `.agent-ws.toml` comes with the repository, hooks run only after you
have read them and run `agent-ws trust`; after any change to them, `spawn` and
`land` stop and list them until you trust them again. Skip them once with
`spawn --no-hooks` or `land --no-verify`.

A typical use: secret files never reach workspaces, so pull development
secrets from your secret manager in `post_spawn` instead of copying yours.

## Troubleshooting

| Message or symptom | What to do |
| --- | --- |
| `... are on different volumes` | Put `AGENT_WS_HOME` on the volume that holds the repository. |
| `branch agent-ws/NAME already exists` | Land with `--branch other-name`, or `--force-branch` if you mean to overwrite. |
| `refusing to drop/land the workspace you are currently inside` (`SELF_ACTION_BLOCKED`) | `cd` back to the repository root first, or pass `--allow-self`. |
| `still being created` / leftovers after a crash | `agent-ws gc --dry-run`, then `agent-ws gc`. |
| `corrupt record` | A state file was edited by hand; `agent-ws gc` forgets it safely. |
| `WARNING symlink ... points outside the workspace` | Writes through that link leave the workspace. Fix the link, or set `strict_symlinks`. |
| First build in a workspace is slow | Expected: files are read cold once; later builds are warm. |
| `du` shows huge numbers for `~/.agent-ws` | Shared clone blocks are counted once per workspace. `agent-ws du` shows real/reclaimable use; `resync-deps` refreshes stale deps and `prune` frees merged ones. |
| `WARNING N workspaces exist` / `exceeds max_private_gb` | `agent-ws prune --dry-run`, then `agent-ws prune` (or `--target-free-gb N`); drop the rest you no longer need. |
| `.agent-ws.toml has hooks that run commands …` | Read the listed commands, then `agent-ws trust`; or `--no-hooks` / `--no-verify` once. |
| `reflink …: Operation not supported` (Linux) | The repository is on a filesystem without reflinks (ext4). Use Btrfs or XFS for the repository and `AGENT_WS_HOME`. |
| `(conflict check needs Git 2.38 or later)` | `status` still lists overlaps; update Git to see which ones conflict. |
| Claude Code: `WorktreeCreate hook failed` | `agent-ws` must be on Claude Code's `PATH`. The hook's error is in Claude Code's debug log. |

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
best case for `git worktree`. On Linux (Btrfs/XFS), fresh clones use parallel
per-file `FICLONE` ioctls instead of macOS's single-syscall directory
`clonefile(2)`, so fresh spawn times scale with file count while pooled spawns
remain ~40 ms. `agent-ws drop` returns after an atomic rename into `.trash` and
deletes in the background (use `bench/bench.sh` to see both async rename and
full settle times). Reproduce on a throwaway APFS/Btrfs/XFS copy of your
repository:

```sh
cp -c -R my-repo /tmp/my-repo-copy
bench/bench.sh /tmp/my-repo-copy "pnpm install --frozen-lockfile --offline"
```

## How it works

[docs/how-it-works.md](docs/how-it-works.md) has the diagrams, the pool and
`gc` rules, the safety guarantees and a code map.

## Contributing

Contributions are welcome: bug reports, fixes, more agents in
[docs/agents.md](docs/agents.md), docs. See [CONTRIBUTING.md](CONTRIBUTING.md)
for setup, tests and the review checklist.

## License

[MIT](LICENSE)
