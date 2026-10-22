# Using agent-ws with coding agents

agent-ws does not launch agents for you. The **coordinator** — you, a script, or
an orchestrating agent — owns the whole lifecycle:

```sh
agent-ws spawn NAME          # create the workspace first
agent-ws path NAME           # get its folder, start the agent there
# ... the agent works in that folder ...
agent-ws land NAME -m MSG    # or: agent-ws drop NAME
```

An agent cannot move into a workspace mid-session, so it must be *started* with
its working directory inside `$(agent-ws path NAME)`. Every agent below shows two
forms:

- **Interactive** — start the agent's TUI/REPL in the workspace and drive it by
  hand.
- **Headless** — one shot, prompt in, exit. This is what you script for parallel
  fleets.

Where an agent has a native working-directory flag we use it; otherwise we use the
universal `(cd "$W" && agent ...)` subshell, which works for every agent.

> Paste this line into the prompt you give *any* agent:
>
> > You work in an isolated agent-ws workspace. Stay in it and do not switch
> > branches. Commit if you like; the coordinator lands your work. Do not run
> > `agent-ws land` or `agent-ws drop` yourself.

---

## Claude Code (`claude`)

`claude` has no working-directory flag, so start it from inside the workspace.
`-p/--print` runs non-interactively (skips the trust dialog); the prompt is a
positional argument.

```sh
agent-ws spawn fix-login
W=$(agent-ws path fix-login)

# Interactive
(cd "$W" && claude)

# Headless
(cd "$W" && claude -p "Fix the login redirect and run the tests")

agent-ws land fix-login -m "fix: login redirect"
```

`--add-dir DIR` can grant the agent access to a second directory if a task needs
it; by default the workspace alone is writable.

## Codex (`codex`)

`codex -C/--cd DIR` sets the working root directly, so no `cd` is needed.
`codex` starts an interactive session; `codex exec` runs non-interactively.
`-s/--sandbox workspace-write` keeps generated shell commands inside the
workspace (ignore Codex's own `--worktree` — agent-ws replaces it).

```sh
agent-ws spawn fix-login

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
agent-ws spawn fix-login

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
agent-ws spawn fix-login

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
agent-ws spawn fix-login
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
agent-ws spawn fix-login
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
  agent-ws spawn "$t" >/dev/null
  W=$(agent-ws path "$t")
  (cd "$W" && my-agent -p "Task $t: ...") &
done
wait

agent-ws status                 # OVERLAP lines = two agents touched one file
for t in auth ui perf docs; do agent-ws land "$t" -m "agent: $t"; done
```

Read the `OVERLAP` lines before landing: if two workspaces changed the same file,
land them in order (or merge by hand) rather than force-overwriting a branch.

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
```

`spawn` also prunes merged workspaces in the background, so forgotten ones
do not pile up.

## Notes

- `land` creates a **local** branch `agent-ws/NAME` only. Pushing and opening a
  PR follow your repo's usual rules.
- Every agent-ws command takes `--json` if you need to parse it in a script.
- Command flags above were verified against each CLI's `--help`. Agents change;
  if a flag differs, `agent -h` is the source of truth — the agent-ws side
  (`spawn` / `path` / `land` / `drop` / `gc`) does not.
