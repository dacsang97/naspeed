#!/usr/bin/env bash
# Simulate two runs of the `ns ci init` workflow on fresh runners.
#
#   scripts/ci_simulate.sh          # Linux container (debian:bookworm-slim)
#   scripts/ci_simulate.sh host     # natively on this machine, in target/
#
# Run 1: empty store → `naspeed setup` → `cargo test --locked` (cold) →
#        `ns cache export "$HOME/.naspeed-ci" --max-gb 5` → "save" (tar, like
#        actions/cache).
# Run 2: another runner: new HOME, CARGO_HOME, workspace path and a copy of
#        the toolchain under another RUSTUP_HOME → "restore" the tar →
#        `naspeed setup` → `ns cache import` → `cargo test --locked`.
# Prints both runs' hits/misses and fails when run 2's hit rate is below
# NASPEED_CI_MIN_HIT_RATE (default 90%).
#
# Container mode uses the scripts/test_linux.sh recipe (toolchain cached
# under target/linux-rustup, host registry mounted so cargo runs --offline)
# and removes its container on exit. Host mode never touches ~/.cargo or
# ~/.rustup beyond reading them: every run has its own HOME and CARGO_HOME.
set -euo pipefail

MODE="${1:-linux}"
IMAGE="${NASPEED_LINUX_IMAGE:-debian:bookworm-slim}"
TOOLCHAIN="${NASPEED_LINUX_TOOLCHAIN:-1.98.0}"
MIN_RATE="${NASPEED_CI_MIN_HIT_RATE:-90}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_HOME_HOST="${CARGO_HOME:-$HOME/.cargo}"

# The two CI runs. Inputs (env): SRC (naspeed checkout), RUNS (scratch dir),
# CARGO_BASE (bin/ proxies, registry/, git/), BASE_RUSTUP, TC (toolchain
# name), NS_BIN (naspeed binary), MIN_RATE.
runs='
set -euo pipefail
counts() { # hits misses in a store usage log
  local log=$1/usage.log
  echo "$(grep -c "^H" "$log" || true) $(grep -c "^M" "$log" || true)"
}

# One CI run on a fresh "runner" rooted at $RUNS/$1, toolchain under $2.
ci_run() {
  local run=$RUNS/$1
  mkdir -p "$run/home" "$run/cargo/bin" "$run/work"
  for tool in cargo rustc rustup rustdoc; do
    if [ -e "$CARGO_BASE/bin/$tool" ]; then cp "$CARGO_BASE/bin/$tool" "$run/cargo/bin/"; fi
  done
  ln -s "$CARGO_BASE/registry" "$run/cargo/registry"
  ln -s "$CARGO_BASE/git" "$run/cargo/git"
  cp -R "$SRC/tests/fixtures/sample_workspace" "$run/work/sample"
  (
    export HOME=$run/home CARGO_HOME=$run/cargo RUSTUP_HOME=$2 RUSTUP_TOOLCHAIN=$TC
    export PATH=$CARGO_HOME/bin:/usr/local/bin:/usr/bin:/bin NASPEED_NO_AUTO_GC=1
    unset NASPEED_STORE NASPEED_CONFIG_DIR CARGO_TARGET_DIR RUSTC_WRAPPER
    cd "$run/work/sample"
    # actions/cache/restore
    if [ -f "$RUNS/cache.tar" ]; then tar -C "$HOME" -xf "$RUNS/cache.tar"; fi
    "$NS_BIN" setup --store "$HOME/.naspeed" >/dev/null
    if [ -d "$HOME/.naspeed-ci/entries" ]; then ns cache import "$HOME/.naspeed-ci"; fi
    start=$(date +%s)
    cargo test --locked --offline > "$run/cargo.log" 2>&1 || { cat "$run/cargo.log"; exit 1; }
    echo "$1: cargo test took $(( $(date +%s) - start ))s"
    read -r hits misses < <(counts "$HOME/.naspeed")
    echo "$1: $hits hits, $misses misses"
    echo "$hits $misses" > "$run/counts"
    ns cache export "$HOME/.naspeed-ci" --max-gb 5
    # actions/cache/save (the generated workflow does this on main pushes only)
    tar -C "$HOME" -cf "$RUNS/cache.tar" .naspeed-ci
    echo "$1: cache archive $(du -h "$RUNS/cache.tar" | cut -f1)"
  )
}

ci_run run1 "$BASE_RUSTUP"
# Second runner: the same toolchain installed under another path.
mkdir -p "$RUNS/rustup-elsewhere/toolchains"
cp -R "$BASE_RUSTUP/toolchains/$TC" "$RUNS/rustup-elsewhere/toolchains/"
ci_run run2 "$RUNS/rustup-elsewhere"

read -r hits misses < "$RUNS/run2/counts"
total=$((hits + misses))
rate=$(( total > 0 ? hits * 100 / total : 0 ))
echo "run 2 hit rate: ${rate}% ($hits/$total)"
[ "$rate" -ge "$MIN_RATE" ] || { echo "below ${MIN_RATE}%" >&2; exit 1; }
'

case "$MODE" in
  linux)
    NAME="naspeed-ci-simulate-$$"
    mkdir -p "$ROOT/target/linux-rustup"
    if command -v orbctl >/dev/null && ! orbctl status 2>/dev/null | grep -q Running; then
      orbctl start
    fi
    docker info >/dev/null
    cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
    trap cleanup EXIT INT TERM
    prelude='
set -euo pipefail
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends ca-certificates curl gcc libc6-dev >/dev/null
export RUSTUP_HOME=/src/target/linux-rustup CARGO_HOME=/cargo PATH=/cargo/bin:$PATH
curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain "$TOOLCHAIN" >/dev/null
export CARGO_TERM_COLOR=never
uname -sm; rustc -V
CARGO_TARGET_DIR=/src/target/linux-ci-sim cargo build --release --offline -q
export SRC=/src RUNS=/runs CARGO_BASE=/cargo BASE_RUSTUP=/src/target/linux-rustup
export TC="$(rustup show active-toolchain | cut -d" " -f1)"
export NS_BIN=/src/target/linux-ci-sim/release/naspeed
'
    docker run --name "$NAME" --rm \
      -e TOOLCHAIN="$TOOLCHAIN" -e MIN_RATE="$MIN_RATE" \
      -v "$ROOT:/src" \
      -v "$CARGO_HOME_HOST/registry:/cargo/registry" \
      -v "$CARGO_HOME_HOST/git:/cargo/git" \
      -w /src "$IMAGE" bash -c "$prelude$runs"
    ;;
  host)
    (cd "$ROOT" && cargo build --release -q)
    export SRC="$ROOT" RUNS="$ROOT/target/ci-sim-host" CARGO_BASE="$CARGO_HOME_HOST"
    export BASE_RUSTUP="${RUSTUP_HOME:-$HOME/.rustup}" MIN_RATE
    export TC="${NASPEED_HOST_TOOLCHAIN:-$TOOLCHAIN-$(rustc -vV | sed -n "s/^host: //p")}"
    export NS_BIN="$ROOT/target/release/naspeed"
    rm -rf "$RUNS"
    # NASPEED_CI_SIM_KEEP=1 keeps the run dirs for inspection.
    [ -n "${NASPEED_CI_SIM_KEEP:-}" ] || trap 'rm -rf "$RUNS"' EXIT
    uname -sm; echo "toolchain $TC"
    bash -c "$runs"
    ;;
  *)
    echo "usage: $0 [linux|host]" >&2
    exit 2
    ;;
esac
