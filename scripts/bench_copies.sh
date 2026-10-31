#!/usr/bin/env bash
# Benchmark naspeed on copies of real projects (never the originals).
#   bench.sh <name> <project-dir> <deep-file> <leaf-file>
set -uo pipefail
NAME=$1; SRC=$2; DEEP=$3; LEAF=$4
# Copies go under $NASPEED_BENCH_ROOT (same volume as the store, so clones work).
ROOT="${NASPEED_BENCH_ROOT:?set NASPEED_BENCH_ROOT}/$NAME"
LOG=$ROOT/results.txt
mkdir -p "$ROOT"; rm -f "$ROOT"/*.out; : > "$LOG"
RUN=$(date +%s)
now() { python3 -c 'import time; print(time.time())'; }
log() { echo "$*" | tee -a "$LOG"; }
timed() { # label dir mode(none|ns) cmd...
  local label=$1 dir=$2 mode=$3; shift 3
  local s e rc
  s=$(now)
  if [ "$mode" = none ]; then
    (cd "$dir" && RUSTC_WRAPPER= "$@") >>"$ROOT/$label.out" 2>&1; rc=$?
  else
    (cd "$dir" && "$@") >>"$ROOT/$label.out" 2>&1; rc=$?
  fi
  e=$(now)
  log "$label $(python3 -c "print(round($e-$s,1))")s rc=$rc"
}
counts() { local l="${NASPEED_STORE:?set NASPEED_STORE}/usage.log"; echo "$(grep -c '^H' $l 2>/dev/null)/$(grep -c '^M' $l 2>/dev/null)"; }
copy() { rm -rf "$1"; rsync -a --exclude target --exclude .git --exclude node_modules "$SRC/" "$1/"; }
# Unique per run, so an edit never hits an entry stored by an earlier run.
edit() { echo "pub fn naspeed_bench_marker_${RUN}_$2() {}" >> "$1"; }

for w in A B C D; do copy "$ROOT/$w"; done
log "== $NAME $(date '+%F %T')  load: $(uptime | sed 's/.*averages: //')"
timed base_cold "$ROOT/A" none cargo build --workspace --offline
c0=$(counts); timed ns_first "$ROOT/B" ns cargo build --workspace --offline; c1=$(counts)
log "   usage.log H/M: $c0 -> $c1"
timed ns_worktree2 "$ROOT/C" ns cargo build --workspace --offline; c2=$(counts)
log "   usage.log H/M: $c1 -> $c2"
ws=$(now); while pgrep -f __warm-incremental >/dev/null; do sleep 0.5; done
log "   background incremental warm-up finished $(python3 -c "print(round($(now)-$ws,1))")s after ns_worktree2"
edit "$ROOT/A/$DEEP" 1; timed base_edit_deep "$ROOT/A" none cargo build --workspace --offline
edit "$ROOT/C/$DEEP" 1; timed ns_edit_deep "$ROOT/C" ns cargo build --workspace --offline
edit "$ROOT/A/$LEAF" 2; timed base_edit_leaf "$ROOT/A" none cargo build --workspace --offline
edit "$ROOT/C/$LEAF" 2; timed ns_edit_leaf "$ROOT/C" ns cargo build --workspace --offline
edit "$ROOT/D/$DEEP" 1; edit "$ROOT/D/$LEAF" 2
timed ns_worktree3_same_edits "$ROOT/D" ns cargo build --workspace --offline
timed base_test1 "$ROOT/A" none cargo test --workspace --offline
timed base_test2 "$ROOT/A" none cargo test --workspace --offline
timed ns_test1 "$ROOT/C" ns ns test --workspace --offline
timed ns_test2 "$ROOT/C" ns ns test --workspace --offline
log "   cached test binaries on ns_test2: $(grep -c '(cached)' $ROOT/ns_test2.out)"
pass() { grep -E '^test result' "$1" | awk '{p+=$4; f+=$6} END {print p" passed, "f" failed"}'; }
log "   base_test1: $(pass $ROOT/base_test1.out) | ns_test1: $(pass $ROOT/ns_test1.out) | ns_test2: $(pass $ROOT/ns_test2.out)"
log "== done $(date '+%T')"
