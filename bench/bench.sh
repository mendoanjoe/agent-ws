#!/usr/bin/env bash
# Compare `git worktree add` (+ dependency install) with agent-ws.
#
# Usage: bench/bench.sh <repo> ["<install command>"]
#   e.g. bench/bench.sh ../copy-of-my-repo "pnpm install --frozen-lockfile --offline"
#
# Run it on a throwaway APFS/Btrfs/XFS copy of a repository (`cp -c -R repo copy`):
# it creates and removes worktrees, workspaces and branches there. Scratch state
# goes next to the repository so clones stay on the same volume.
set -euo pipefail

if [ "$#" -lt 1 ] || [ -z "${1:-}" ]; then
  echo "Usage: $0 <repo> [\"<install command>\"]" >&2
  exit 2
fi

for dep in git perl jq; do
  if ! command -v "$dep" >/dev/null 2>&1; then
    echo "Missing required dependency: $dep" >&2
    exit 1
  fi
done

REPO=$(cd "$1" && pwd)
INSTALL=${2:-}
BIN=${AGENT_WS_BIN:-agent-ws}
if ! command -v "$BIN" >/dev/null 2>&1 && [ ! -x "$BIN" ]; then
  echo "Missing agent-ws binary: $BIN" >&2
  exit 1
fi
RUNS=${RUNS:-3}
PAR=${PAR:-8}
WORK=$(mktemp -d "$(dirname "$REPO")/agent-ws-bench.XXXXXX")
export AGENT_WS_HOME=$WORK/home AGENT_WS_NO_REFILL=1 AGENT_WS_NO_AUTO_PRUNE=1

cleanup() {
  if cd "$REPO" 2>/dev/null; then
    "$BIN" gc >/dev/null 2>&1 || true
    git worktree prune >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

now() { perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'; }
median() { sort -n | awk '{ a[NR] = $1 } END { print a[int((NR + 1) / 2)] }'; }
row() { printf '| %s | %s |\n' "$1" "$2"; }
# `drop` deletes in the background; wait so deletes do not skew the next step.
settle() { while [ -n "$(ls -A "$AGENT_WS_HOME"/*/.trash 2>/dev/null)" ]; do sleep 0.2; done; }

cd "$REPO"
tracked=$(git ls-files | wc -l | tr -d ' ')
deps=$( [ -d node_modules ] && find node_modules -type f | wc -l | tr -d ' ' || echo 0)

# Warmup pass (discarded) so filesystem and Git caches are warm across steps.
git worktree add -q --detach "$WORK/wt-warm" HEAD
if [ -n "$INSTALL" ]; then (cd "$WORK/wt-warm" && sh -c "$INSTALL" >/dev/null 2>&1); fi
git worktree remove --force "$WORK/wt-warm"
"$BIN" spawn --no-pool "warm" >/dev/null
"$BIN" drop "warm" >/dev/null
settle

# 1. git worktree add (+ install), and removing it again.
add=(); rm_wt=()
for i in $(seq "$RUNS"); do
  t=$(now)
  git worktree add -q --detach "$WORK/wt$i" HEAD
  if [ -n "$INSTALL" ]; then (cd "$WORK/wt$i" && sh -c "$INSTALL" >/dev/null 2>&1); fi
  add+=($(( $(now) - t )))
  t=$(now); git worktree remove --force "$WORK/wt$i"; rm_wt+=($(( $(now) - t )))
done

# 2. agent-ws fresh clone, drop (async rename), and full settle (delete).
fresh=(); drop=(); drop_full=()
for i in $(seq "$RUNS"); do
  t=$(now); "$BIN" spawn --no-pool "f$i" >/dev/null; fresh+=($(( $(now) - t )))
  t=$(now); "$BIN" drop "f$i" >/dev/null; drop+=($(( $(now) - t )))
  settle
  drop_full+=($(( $(now) - t )))
done

# 3. agent-ws from the pool.
"$BIN" pool fill --size "$RUNS" >/dev/null
pool=()
for i in $(seq "$RUNS"); do
  t=$(now); "$BIN" spawn "p$i" >/dev/null; pool+=($(( $(now) - t )))
done
while IFS= read -r n; do
  [ -n "$n" ] && "$BIN" drop "$n" >/dev/null
done < <("$BIN" list --json | jq -r '.[].name')
settle

# 4. PAR spawns at once, fresh and from the pool (wall time for all).
pids=()
t=$(now)
for i in $(seq "$PAR"); do
  "$BIN" spawn --no-pool "pf$i" >/dev/null &
  pids+=("$!")
done
for pid in "${pids[@]}"; do wait "$pid"; done
par_fresh=$(( $(now) - t ))

"$BIN" pool fill --size "$PAR" >/dev/null
pids=()
t=$(now)
for i in $(seq "$PAR"); do
  "$BIN" spawn "pp$i" >/dev/null &
  pids+=("$!")
done
for pid in "${pids[@]}"; do wait "$pid"; done
par_pool=$(( $(now) - t ))

while IFS= read -r n; do
  [ -n "$n" ] && "$BIN" drop "$n" >/dev/null
done < <("$BIN" list --json | jq -r '.[].name')
settle

# 5. Disk: real private vs allocated bytes across 5 fresh workspaces via `agent-ws du`.
for i in 1 2 3 4 5; do "$BIN" spawn --no-pool "d$i" >/dev/null; done
du_json=$("$BIN" du --json)
private_mib=$(printf '%s' "$du_json" | jq -r '(.total.private / 5 / 1048576) | floor')
allocated_mib=$(printf '%s' "$du_json" | jq -r '(.entries[0].allocated / 1048576) | floor')

ms() { printf '%s\n' "$@" | median; }
echo
echo "Repository: $tracked tracked files, $deps files in node_modules; median of $RUNS runs."
echo
row "Step" "Time"
row "---" "---"
row "git worktree add${INSTALL:+ + install}" "$(ms "${add[@]}") ms"
row "agent-ws spawn (fresh clone)" "$(ms "${fresh[@]}") ms"
row "agent-ws spawn (from pool)" "$(ms "${pool[@]}") ms"
row "$PAR x fresh spawns at once (wall)" "$par_fresh ms"
row "$PAR x spawns from the pool at once (wall)" "$par_pool ms"
row "git worktree remove (with deps)" "$(ms "${rm_wt[@]}") ms"
row "agent-ws drop (async rename)" "$(ms "${drop[@]}") ms"
row "agent-ws drop + settle (full delete)" "$(ms "${drop_full[@]}") ms"
row "Disk per workspace (private / allocated)" "$private_mib MiB / $allocated_mib MiB"
