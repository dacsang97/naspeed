#!/usr/bin/env bash
# Measure Rust build disk usage and build time, before and after naspeed.
#
# Usage:
#   measure_baseline.sh env                     toolchain, cargo config, wrapper env
#   measure_baseline.sh sizes                   df, du of every target/ and caches, component split
#   measure_baseline.sh warm <project> <file>   touch <file>, cargo build, best of 2 (seconds)
#   measure_baseline.sh cold <project>          build a target-less copy next to <project>, best of 2
#   measure_baseline.sh parallel-ram <p1> ...   cold-build copies in parallel, report peak RSS sum
#
# Never runs `cargo clean` on a real project. Cold copies are created as a
# sibling directory so relative path dependencies still resolve, and removed
# after each run.
set -euo pipefail

VOLUME="${NASPEED_MEASURE_VOLUME:-$HOME}"
SEARCH_ROOT="${NASPEED_MEASURE_ROOT:-$HOME}"
# Another cache dir to include in the size report (optional).
EXTRA_CACHE_DIR="${NASPEED_MEASURE_EXTRA_CACHE:-}"
CARGO_HOME_PATH="${CARGO_HOME:-$HOME/.cargo}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

gb() { awk '{printf "%.2f", $1/1048576}'; }

find_target_dirs() {
  # Every dir cargo tagged as its own build dir (CACHEDIR.TAG with cargo's
  # signature), whatever its name (target, target-wdio, …) and depth.
  find "$SEARCH_ROOT" \( -name node_modules -o -name .git \) -prune -o \
    -name CACHEDIR.TAG -type f -print 2>/dev/null |
    while read -r tag; do
      dir=$(dirname "$tag")
      # pytest/venv use the same standard signature; cargo dirs also have
      # .rustc_info.json. Nested tags (target/<triple>) are skipped below.
      grep -q 8a477f597d28d172789f06886806bc55 "$tag" 2>/dev/null &&
        [ -f "$dir/.rustc_info.json" ] && echo "$dir"
    done | grep -vE "${NASPEED_MEASURE_EXCLUDE:-^\$}" | sort | awk 'prev == "" || index($0, prev "/") != 1 { print; prev = $0 }'
}

cmd_env() {
  echo "date: $(date '+%Y-%m-%d %H:%M:%S %Z')"
  rustc -vV
  cargo -V
  echo "--- ~/.cargo/config.toml"
  cat "$CARGO_HOME_PATH/config.toml" 2>/dev/null || echo "(none)"
  echo "--- wrapper env"
  env | grep -E '^(RUSTC_WRAPPER|CARGO_|NASPEED_)' | sort || true
}

cmd_sizes() {
  echo "df_k $(df -k "$VOLUME" | awk 'NR==2 {print $3}') used on $VOLUME"
  echo "df_k $(df -k / | awk 'NR==2 {print $3}') used on /"
  local total=0
  while read -r dir; do
    local k
    k=$(du -sk "$dir" | awk '{print $1}')
    total=$((total + k))
    printf '%8s GB  %s\n' "$(echo "$k" | gb)" "$dir"
  done < <(find_target_dirs)
  printf '%8s GB  %s\n' "$(echo "$total" | gb)" "TOTAL target/"
  for extra in "$EXTRA_CACHE_DIR" "$CARGO_HOME_PATH/registry" "$CARGO_HOME_PATH/git" "$VOLUME/.naspeed"; do
    [ -d "$extra" ] && printf '%8s GB  %s\n' "$(du -sk "$extra" | awk '{print $1}' | gb)" "$extra"
  done
  echo "--- component split (registry deps / workspace deps / incremental / build-script out / other)"
  while read -r dir; do
    python3 -I "$SCRIPT_DIR/target_components.py" "$dir"
  done < <(find_target_dirs)
}

best_of_two() {
  # Runs "$@" twice and prints the faster wall time in seconds.
  local best="" start end elapsed
  for _ in 1 2; do
    start=$(python3 -c 'import time; print(time.time())')
    if ! "$@" >/dev/null 2>&1; then
      echo "FAIL"
      return
    fi
    end=$(python3 -c 'import time; print(time.time())')
    elapsed=$(python3 -c "print(round($end - $start, 1))")
    if [ -z "$best" ] || python3 -c "import sys; sys.exit(0 if $elapsed < $best else 1)"; then
      best=$elapsed
    fi
  done
  echo "$best"
}

warm_build() {
  local project="$1" file="$2"
  touch "$project/$file"
  (cd "$project" && cargo build --offline)
}

cold_copy_build() {
  local project="$1" copy
  copy="$(dirname "$project")/.naspeed-cold-$(basename "$project")"
  rm -rf "$copy"
  # Sources only: skip build output, JS deps and hidden dirs (e.g. a 40 GB
  # .local cache) except .cargo, which can hold project cargo config.
  rsync -a --include '.cargo/' --include '.cargo/**' --exclude '.*' \
    --exclude target --exclude node_modules "$project/" "$copy/"
  local rc=0
  (cd "$copy" && cargo build --offline) || rc=$?
  rm -rf "$copy"
  return $rc
}

cmd_warm() {
  local project="$1" file="$2"
  # First build makes sure the target dir is up to date before timing.
  (cd "$project" && cargo build --offline >/dev/null 2>&1)
  echo "warm_s $(best_of_two warm_build "$project" "$file") $project"
}

cmd_cold() {
  echo "cold_s $(best_of_two cold_copy_build "$1") $1"
}

cmd_parallel_ram() {
  # Sample total RSS of rustc/cc/ld processes every second while building
  # target-less copies of every project in parallel.
  local pids=() log
  log=$(mktemp)
  for project in "$@"; do
    cold_copy_build "$project" >/dev/null 2>&1 &
    pids+=($!)
  done
  local peak=0 swap_start swap_end
  swap_start=$(sysctl -n vm.swapusage 2>/dev/null | awk '{print $6}' || echo "?")
  while kill -0 "${pids[@]}" 2>/dev/null; do
    local rss
    rss=$(ps -axo rss=,comm= | awk '/rustc|cc$|clang|ld|ld64|cargo|build-script/ {s+=$1} END {print s+0}')
    [ "$rss" -gt "$peak" ] && peak=$rss
    sleep 1
  done
  wait "${pids[@]}" || true
  swap_end=$(sysctl -n vm.swapusage 2>/dev/null | awk '{print $6}' || echo "?")
  echo "parallel_builds $# peak_rss_gb $(echo "$peak" | gb) swap_used_before $swap_start swap_used_after $swap_end"
  rm -f "$log"
}

case "${1:-}" in
  env) cmd_env ;;
  sizes) cmd_sizes ;;
  warm) cmd_warm "$2" "$3" ;;
  cold) cmd_cold "$2" ;;
  parallel-ram) shift; cmd_parallel_ram "$@" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
