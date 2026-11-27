# Disk usage of agent-ws workspaces

Measured on 2026-10-22 after the disk filled up: `~/.agent-ws` showed 147 GB
in `du` with about 120 pixel-office workspaces, and the Mac had 1.9 GB free.
The measurements are read-only, on what was left of
`~/.agent-ws/pixel-office-0d67b230` (15 workspaces, later 10).

## How to measure

`du` counts every allocated block of every file, so a cloned block counts
once per clone. APFS knows how much of a file no other file shares
(`getattrlist` with `ATTR_CMNEXT_PRIVATESIZE`); deleting the file frees at
least that much. `agent-ws du` adds that up per workspace. On Linux it reads
the same from the extents `FIEMAP` does not flag as shared (Btrfs, XFS); the
measurements below are from a Mac.

Use `agent-ws du --depth 1` (or `--depth 2`) to see which subdirectories inside
each workspace hold the private bytes without running `du` by hand. `agent-ws
du` also inspects APFS clone IDs (`ATTR_CMNEXT_CLONEID`) or Linux `FIEMAP`
shared extent IDs across the source checkout, workspaces, pool, and `.trash` to
report `cohort shared` (blocks still shared among workspaces/pool/trash that the
source checkout no longer holds, such as after a reinstall) and `reclaimable`
bytes (`total.private + cohort_shared_bytes`).

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
is why the pool is capped, why `agent-ws resync-deps` can CoW-refresh
dependency directories in long-lived workspaces, and why merged workspaces
should go.

`private` is a lower bound for a single workspace: blocks two workspaces share
with each other, but not with the source, are private to neither. `cohort
shared` and `reclaimable` in `agent-ws du` account for that shared cohort
explicitly so you see the true space freed by cleaning up the whole cohort.

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

- `agent-ws du [--depth N]` shows the real (private) size next to the `du` size,
  per workspace, pooled workspace and trash entry, free disk, optional
  per-directory breakdown (`--depth`), and `cohort shared` / `reclaimable`
  bytes (`ATTR_CMNEXT_CLONEID` on macOS, `FIEMAP` shared extent IDs on Linux).
- `agent-ws resync-deps NAME [PATH…]` (and `agent-ws sync --resync-deps`)
  reclaims orphaned `node_modules` / build-cache bytes in existing workspaces
  by atomically CoW-swapping them with the source checkout's current copy.
- `agent-ws prune` drops workspaces whose work is already merged (see the
  README), and `prune --target-free-gb N` drops the largest eligible workspaces
  (and clears the pool if needed) until at least `N` GB is free. On the real
  home on 2026-10-22 the dry run found nothing to drop: of 10 workspaces, 5 had
  staged changes and 5 had 1 – 9 unmerged commits with no PR on GitHub; 6 more
  records pointed at deleted folders (`gc`).
- The background `rm` of `drop` runs in its own process group, so killing
  the agent's terminal no longer leaves trees in `.trash`; the next `drop`
  restarts deletes older than 10 minutes.
- The pool is capped at 8, and `pool fill` refreshes commit-stale pooled
  workspaces in place when only `HEAD` moved instead of discarding and
  re-cloning them.
- `spawn` warns from 20 workspaces (`warn_workspaces`), below 10 GB free, and
  when total private disk usage exceeds `max_private_gb` (also shown as `budget`
  in `agent-ws du`).
- `spawn` prunes merged workspaces in the background (`auto_prune`), so a
  forgotten `prune` no longer lets them pile up.
- `gc` no longer deletes record-less folders that hold work. On the real
  home it would have deleted `s0803f-it` (247 uncommitted changes, one
  commit not in `origin/main`); now it registers it again.

If a repository has large build outputs or caches you never want copied into
workspaces, list exact paths or glob patterns (such as `**/node_modules`,
`apps/*/dist`, or `*.log`) in `exclude` in `.agent-ws.toml`.
