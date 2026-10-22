# Disk usage of agent-ws workspaces

Measured on 2026-10-22 after the disk filled up: `~/.agent-ws` showed 147 GB
in `du` with about 120 pixel-office workspaces, and the Mac had 1.9 GB free.
The measurements are read-only, on what was left of
`~/.agent-ws/pixel-office-0d67b230` (15 workspaces, later 10).

## How to measure

`du` counts every allocated block of every file, so a cloned block counts
once per clone. APFS knows how much of a file no other file shares
(`getattrlist` with `ATTR_CMNEXT_PRIVATESIZE`); deleting the file frees at
least that much. `agent-ws du` adds that up per workspace.

| | `du -A` (logical) | `du` (allocated) | private (real) |
| --- | --- | --- | --- |
| one workspace | 814 MB | 943 MB | 10 – 22 MB |
| 15 workspaces | 12.5 GB | 14.2 GB | 280 MB |
| the source checkout | 3.8 GB | 4.4 GB | 1.1 GB |

So the 147 GB was almost all double counting: 120 × ~1.2 GB of shared
blocks. The real cost of a workspace that was spawned and worked in is
10 – 50 MB.

## Where the private bytes are

Per directory, the private bytes are tracked source files (`apps/*/src`,
`modules/*`), about 1 – 2 MB per folder. `git checkout` writes them when the
base commit differs from what the source checkout has on disk. Rewritten
`node_modules` and build caches were not a factor: `node_modules/.pnpm`
(27,000 files, 824 MB) held under 1 MB of private data, and no workspace had
`.turbo`, `dist` or vitest caches of any size.

Blocks can also become real when the source checkout rewrites its copy (a
reinstall or rebuild): the workspaces' clones then keep the old blocks alone.
A check of APFS clone ids (`ATTR_CMNEXT_CLONEID`) found 4 KB held only by
workspaces and no longer by the source. That did not happen here, but after
a `pnpm install` in the source every old workspace would own its
`node_modules` (about 800 MB each), and that would fill a disk quickly. This
is why the pool is capped and why merged workspaces should go.

`private` is a lower bound for a group of workspaces: blocks two workspaces
share with each other, but not with the source, are private to neither.

## What was not the cause

- `.trash` was empty and there were no pooled workspaces (`pool.json` absent).
- No local Time Machine snapshots held deleted blocks
  (`tmutil listlocalsnapshots /` empty).
- The shared `.git` of the source is 0.9 GB (objects 0.8 GB, worktree
  metadata 49 MB).

Other agent checkouts under `~/.pixel-coord` (`wt`, `wt-*`,
`agent-ws-review`, `agent-ws-trial`, `s815r`) show 17 GB in `du` and hold
about 2.7 GB private (`s815r` alone 1.3 GB: 25 plain `git worktree`
checkouts with no clone sharing). The rest of the disk is outside agent-ws: `~/work`
57 GB, `~/Downloads` 9 GB, `~/Library/Caches` 7 GB, the pnpm store 7 GB,
swap (VM volume) 18 GB.

The ~105 workspaces deleted before the measurement could not be measured.
Free space grew from 1.9 GB to 10 – 13 GB while they and other things went;
even if all of it came from those workspaces, that is about 100 MB each.

## What changed

- `agent-ws du` shows the real (private) size next to the `du` size, per
  workspace, pooled workspace and trash entry, and free disk.
- `agent-ws prune` drops workspaces whose work is already merged (see the
  README). On the real home on 2026-10-22 the dry run found nothing to drop:
  of 10 workspaces, 5 had staged changes and 5 had 1 – 9 unmerged commits
  with no PR on GitHub; 6 more records pointed at deleted folders (`gc`).
- The background `rm` of `drop` runs in its own process group, so killing
  the agent's terminal no longer leaves trees in `.trash`; the next `drop`
  restarts deletes older than 10 minutes.
- The pool is capped at 8.
- `spawn` warns from 20 workspaces (`warn_workspaces`) and below 10 GB free.
- `spawn` prunes merged workspaces in the background (`auto_prune`), so a
  forgotten `prune` no longer lets them pile up.
- `gc` no longer deletes record-less folders that hold work. On the real
  home it would have deleted `s0803f-it` (247 uncommitted changes, one
  commit not in `origin/main`); now it registers it again.

No cache exclude list was added: nothing in the measurement was a re-copied
cache. If a repository has one, list it in `exclude` in `.agent-ws.toml`.
