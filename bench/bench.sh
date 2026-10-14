#!/usr/bin/env bash
# Compare `git worktree add` (+ dependency install) with agent-ws.
#
# Usage: bench/bench.sh <repo> ["<install command>"]
#   e.g. bench/bench.sh ../copy-of-my-repo "pnpm install --frozen-lockfile --offline"
#
# Run it on a throwaway APFS copy of a repository (`cp -c -R repo copy`): it
# creates and removes worktrees, workspaces and branches there. Scratch state
# goes next to the repository so clones stay on the same volume.
set -euo pipefail

REPO=$(cd "$1" && pwd)
INSTALL=${2:-}
BIN=${AGENT_WS_BIN:-agent-ws}
RUNS=${RUNS:-3}
PAR=${PAR:-8}
WORK=$(mktemp -d "$(dirname "$REPO")/agent-ws-bench.XXXXXX")
export AGENT_WS_HOME=$WORK/home AGENT_WS_NO_REFILL=1
trap 'cd "$REPO"; "$BIN" gc >/dev/null 2>&1 || true; git worktree prune; rm -rf "$WORK"' EXIT

now() { perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'; }
median() { sort -n | awk '{ a[NR] = $1 } END { print a[int((NR + 1) / 2)] }'; }
free_kb() { df -k "$WORK" | awk 'NR == 2 { print $4 }'; }
row() { printf '| %s | %s |\n' "$1" "$2"; }
# `drop` deletes in the background; wait so deletes do not skew the next step.
settle() { while [ -n "$(ls -A "$AGENT_WS_HOME"/*/.trash 2>/dev/null)" ]; do sleep 0.5; done; }

cd "$REPO"
tracked=$(git ls-files | wc -l | tr -d ' ')
deps=$( [ -d node_modules ] && find node_modules -type f | wc -l | tr -d ' ' || echo 0)

# 1. git worktree add (+ install), and removing it again.
add=(); rm_wt=()
for i in $(seq "$RUNS"); do
  t=$(now)
  git worktree add -q --detach "$WORK/wt$i" HEAD
  if [ -n "$INSTALL" ]; then (cd "$WORK/wt$i" && eval "$INSTALL" >/dev/null 2>&1); fi
  add+=($(( $(now) - t )))
  t=$(now); git worktree remove --force "$WORK/wt$i"; rm_wt+=($(( $(now) - t )))
done

# 2. agent-ws fresh clone, and drop.
fresh=(); drop=()
for i in $(seq "$RUNS"); do
  t=$(now); "$BIN" spawn --no-pool "f$i" >/dev/null; fresh+=($(( $(now) - t )))
  t=$(now); "$BIN" drop "f$i" >/dev/null; drop+=($(( $(now) - t )))
  settle
done

# 3. agent-ws from the pool.
"$BIN" pool fill --size "$RUNS" >/dev/null
pool=()
for i in $(seq "$RUNS"); do
  t=$(now); "$BIN" spawn "p$i" >/dev/null; pool+=($(( $(now) - t )))
done

# 4. PAR spawns at once, fresh and from the pool (wall time for all).
t=$(now)
for i in $(seq "$PAR"); do "$BIN" spawn --no-pool "pf$i" >/dev/null & done; wait
par_fresh=$(( $(now) - t ))
"$BIN" pool fill --size "$PAR" >/dev/null
t=$(now)
for i in $(seq "$PAR"); do "$BIN" spawn "pp$i" >/dev/null & done; wait
par_pool=$(( $(now) - t ))
for n in $("$BIN" list --json | jq -r '.[].name'); do "$BIN" drop "$n" >/dev/null; done
settle; sync; sleep 2

# 5. Disk: real space used by 5 fresh workspaces vs their logical size.
before=$(free_kb)
for i in 1 2 3 4 5; do "$BIN" spawn --no-pool "d$i" >/dev/null; done
sync; sleep 2
used_mb=$(( (before - $(free_kb)) / 1024 / 5 ))
logical_mb=$(( $(du -sk "$("$BIN" path d1)" | cut -f1) / 1024 ))

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
row "agent-ws drop" "$(ms "${drop[@]}") ms"
row "Disk per workspace (real / logical)" "$used_mb MB / $logical_mb MB"
