# Using agent-ws with coding agents

agent-ws does not launch agents for you. The **coordinator** — you, a script, or
an orchestrating agent — owns the workspace lifecycle:

```sh
agent-ws spawn NAME --ensure --json   # idempotent: creates or reuses NAME
agent-ws path NAME                    # get its folder, start the agent there
# ... the agent works in that folder (with optional `snap`/`restore` checkpoints) ...
agent-ws land NAME -m MSG             # or: agent-ws drop NAME --if-exists
```

An agent cannot move into a workspace mid-session, so it must be *started* with
its working directory inside `$(agent-ws path NAME)`. Every agent below shows two
forms:

- **Interactive** — start the agent's TUI/REPL in the workspace and drive it by
  hand.
- **Headless** — one shot, prompt in, exit. This is what you script for parallel
  fleets.

Where an agent has a native working-directory flag we use it; otherwise we use the
universal `(cd "$W" && agent ...)` subshell, which works for every agent. The
shortest universal form is `agent-ws exec NAME -- agent ...`: it starts the agent
inside the workspace with `PORT` and `AGENT_WS_*` set, and `--spawn` creates the
workspace first.

> Print a ready-to-paste `SKILL.md` guide for any agent with:
>
> ```sh
> agent-ws skill
> ```
>
> And paste this line into the prompt you give *any* worker agent:
>
> > You work in an isolated agent-ws workspace. Stay in it and do not switch
> > branches. Run `agent-ws whoami --json` if you need your workspace name or
> > port, and `agent-ws snap <NAME>` before risky edits. Commit if you like;
> > the coordinator lands your work. Do not run `agent-ws land` or
> > `agent-ws drop` yourself.

---

## Agent-native primitives

### 1. Built-in `SKILL.md` and `whoami`

Agents running inside a workspace can discover their own environment without
guessing paths or parsing git state:

```sh
agent-ws skill                    # self-contained SKILL.md with YAML frontmatter
agent-ws whoami --json            # inside_workspace, name, base, branch, port, origin
agent-ws whoami --status --json   # adds file overlaps and merge conflicts with peers
```

When run outside any workspace (e.g. in the main checkout), `whoami --json`
returns `"inside_workspace": false` and `repo_root`.

### 2. Idempotent spawns, auto-naming, and drops

Orchestrators and retry loops do not need custom existence checks:

```sh
# Reuse NAME if ready (returns "reused": true), or spawn fresh if missing:
agent-ws spawn auth-fix --ensure --json

# Pick the first unused name in NAME, NAME-2, ..., NAME-100:
agent-ws spawn worker --auto --json

# Succeed with {"name":"auth-fix","dropped":false} if already removed:
agent-ws drop auth-fix --if-exists --json
```

`spawn --ensure` validates `--branch` and `--base` against the existing workspace
when passed, returning `INVALID_INPUT` if they differ. `--ensure` and `--auto`
cannot be combined.

### 3. Workspace forking (`spawn --from`)

Start a follow-up agent or branch off an experiment from an existing workspace
without committing first:

```sh
agent-ws spawn auth-v2 --from auth-fix --json
```

`--from` clones `auth-fix`'s working tree (uncommitted edits, untracked files,
build caches, `node_modules`), staged Git index, and submodules while keeping
`auth-fix`'s original `base` commit (`--from` cannot be combined with `--base` or
`--carry`).

### 4. Instant CoW checkpoints (`snap`, `snaps`, `restore`)

Before an agent runs a risky refactor or migration, take a copy-on-write
checkpoint of its tree, index, `HEAD`, and `base`:

```sh
agent-ws snap auth-fix before-rewrite --json   # or omit snap name to auto-name
agent-ws snaps auth-fix --json                 # list checkpoints oldest -> newest
agent-ws restore auth-fix --json               # restore latest (or name a snap)
agent-ws snap auth-fix before-rewrite --drop --json
```

Restores are repeatable and leave the workspace's Git worktree linkage intact.

### 5. Syncing base commits & dependencies (`sync`, `resync-deps`)

Keep long-lived workspaces up to date as `main` advances or lockfiles change:

```sh
# Rebase (default) or merge onto repository HEAD (or --base REV):
agent-ws sync auth-fix --json
agent-ws sync auth-fix --base origin/main --merge --resync-deps --json

# CoW-refresh git-ignored dependency dirs (auto-detected or explicit paths):
agent-ws resync-deps auth-fix --json
agent-ws resync-deps auth-fix node_modules apps/web/dist --json
```

`sync` refuses dirty trees (`DIRTY`) and aborts cleanly on merge/rebase
conflicts (`CONFLICT`) without leaving the workspace mid-rebase.

### 6. Mechanical self-action guard (`--allow-self`)

If an agent attempts to `agent-ws land` or `agent-ws drop` the workspace whose
directory contains its own `cwd`, agent-ws blocks the call with error code
`SELF_ACTION_BLOCKED` so the agent cannot delete or unlink the directory it is
sitting in. A coordinator running from outside the workspace succeeds normally;
pass `--allow-self` if an automated caller explicitly intends to override it.

### 7. Structured `--json` error codes

With `--json`, every command prints errors to `stderr` as a single JSON line and
exits `1`:

```json
{"ok":false,"code":"CONFLICT","error":"rebase conflict syncing workspace \"auth-fix\" onto abcdef123456"}
```

Codes: `ALREADY_EXISTS`, `NOT_FOUND`, `CONFLICT`, `DIRTY`, `UNTRUSTED_HOOKS`,
`LOW_DISK`, `INVALID_INPUT`, `SELF_ACTION_BLOCKED`, `UNSUPPORTED_FS`,
`GIT_ERROR`, `INTERNAL`.

---

## Claude Code (`claude`)

`claude` has no working-directory flag, so start it from inside the workspace.
`-p/--print` runs non-interactively (skips the trust dialog); the prompt is a
positional argument.

```sh
agent-ws spawn fix-login --ensure
W=$(agent-ws path fix-login)

# Interactive
(cd "$W" && claude)

# Headless
(cd "$W" && claude -p "Fix the login redirect and run the tests")

agent-ws land fix-login -m "fix: login redirect"
```

`--add-dir DIR` can grant the agent access to a second directory if a task needs
it; by default the workspace alone is writable.

Claude Code can also create the workspaces itself: after `agent-ws claude
install`, every worktree Claude Code makes (`claude --worktree`, subagents with
`isolation: "worktree"`, workflows, background sessions) is an agent-ws
workspace on a branch `agent-ws/<name>`, dependencies included. See the
[README](../README.md#claude-code).

## Codex (`codex`)

`codex -C/--cd DIR` sets the working root directly, so no `cd` is needed.
`codex` starts an interactive session; `codex exec` runs non-interactively.
`-s/--sandbox workspace-write` keeps generated shell commands inside the
workspace (ignore Codex's own `--worktree` — agent-ws replaces it).

```sh
agent-ws spawn fix-login --ensure

# Interactive
codex -C "$(agent-ws path fix-login)"

# Headless
codex exec -C "$(agent-ws path fix-login)" \
  -s workspace-write "Fix the login redirect and run the tests"

agent-ws land fix-login -m "fix: login redirect"
```

## opencode (`opencode`)

The TUI takes the project path as a positional argument; `opencode run --dir DIR`
runs a single message headless.

```sh
agent-ws spawn fix-login --ensure

# Interactive
opencode "$(agent-ws path fix-login)"

# Headless
opencode run --dir "$(agent-ws path fix-login)" "Fix the login redirect"

agent-ws land fix-login -m "fix: login redirect"
```

## omp (`omp`)

`omp --cwd=DIR` sets the launch directory; `-p/--print` runs one turn and exits;
the prompt is positional.

```sh
agent-ws spawn fix-login --ensure

# Interactive
omp --cwd="$(agent-ws path fix-login)"

# Headless
omp --cwd="$(agent-ws path fix-login)" -p "Fix the login redirect"

agent-ws land fix-login -m "fix: login redirect"
```

## Cursor Agent (`cursor-agent`)

No working-directory flag, so use the `cd` subshell. `-p/--print` runs
non-interactively with full tool access (write + shell); the prompt is
positional. `--mode plan` starts read-only if you only want analysis.

```sh
agent-ws spawn fix-login --ensure
W=$(agent-ws path fix-login)

# Interactive
(cd "$W" && cursor-agent)

# Headless
(cd "$W" && cursor-agent -p "Fix the login redirect")

agent-ws land fix-login -m "fix: login redirect"
```

## aider (`aider`)

aider has no cwd flag; run it from inside the workspace. The prompt flag is
`--message` (`-m`).

```sh
agent-ws spawn fix-login --ensure
(cd "$(agent-ws path fix-login)" && aider -m "Fix the login redirect")
agent-ws land fix-login -m "fix: login redirect"
```

---

## Parallel fleet (headless)

Keep a pool warm so each spawn is ~40 ms, then fan out one headless agent per
task. Each agent gets its own workspace; nothing they do touches your checkout
until you `land`.

```sh
agent-ws pool set 4

for t in auth ui perf docs; do
  agent-ws exec "$t" --spawn --log -- my-agent -p "Task $t: ..." &
done
wait

agent-ws list --long            # exit code of each agent, changes, disk use
agent-ws log perf --json        # what one of them printed (or drop --json for raw text)
agent-ws status                 # OVERLAP: two agents touched one file
                                # CONFLICT: their changes do not merge
agent-ws diff auth --stat --json
for t in auth ui perf docs; do agent-ws land "$t" -m "agent: $t"; done
```

`exec` starts the agent inside the workspace and gives each one its own `PORT`
(4100, 4110, …), so agents that start dev servers do not collide. Read the
`CONFLICT` lines before landing: those pairs need an order, or a merge by
hand; an `OVERLAP` without a `CONFLICT` merges cleanly.

## After a crash

A killed agent leaves its workspace and record behind. Clean up with:

```sh
agent-ws gc --dry-run    # show what would be removed
agent-ws gc              # remove workspaces whose process died
```

`gc` never touches a workspace whose process is still alive, and registers a
folder that lost its record but still holds work instead of deleting it.

## After a merge

```sh
git fetch && agent-ws prune --dry-run && agent-ws prune
agent-ws prune --target-free-gb 20   # also free space up to 20 GB if disk runs low
```

`spawn` also prunes merged workspaces in the background, so forgotten ones
do not pile up.

## Notes

- `land` creates a **local** branch `agent-ws/NAME`. With `--pr` it also pushes
  it to `origin` and opens a pull request with the GitHub CLI (`gh`);
  otherwise pushing follows your repo's usual rules.
- Every agent-ws command takes `--json` if you need to parse it in a script,
  with structured error codes on `stderr` when exiting non-zero.
- Command flags above were verified against each CLI's `--help`. Agents change;
  if a flag differs, `agent -h` is the source of truth — the agent-ws side
  (`spawn` / `path` / `snap` / `restore` / `sync` / `land` / `drop` / `gc`) does not.
