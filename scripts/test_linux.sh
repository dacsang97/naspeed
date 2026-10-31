#!/usr/bin/env bash
# Run naspeed's test suite on Linux in a container.
#
#   scripts/test_linux.sh copy    # store + target dirs on the bind mount (no reflink → copy mode)
#   scripts/test_linux.sh btrfs   # same tests on a loop-mounted btrfs (reflink mode, needs --privileged)
#
# Uses the small debian:bookworm-slim image; the Rust toolchain (rustup) and
# the container's target dirs live under target/linux-* on the bind mount,
# so the internal disk holding Docker's images barely grows. The host
# registry is mounted so tests run --offline like on macOS.
set -euo pipefail

MODE="${1:-copy}"
IMAGE="${NASPEED_LINUX_IMAGE:-debian:bookworm-slim}"
TOOLCHAIN="${NASPEED_LINUX_TOOLCHAIN:-1.98.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_HOME_HOST="${CARGO_HOME:-$HOME/.cargo}"
mkdir -p "$ROOT/target/linux-rustup"

prelude='
set -euo pipefail
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends ca-certificates curl gcc libc6-dev attr btrfs-progs >/dev/null
export RUSTUP_HOME=/src/target/linux-rustup CARGO_HOME=/cargo PATH=/cargo/bin:$PATH
curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain '"$TOOLCHAIN"' >/dev/null
export CARGO_TERM_COLOR=never
uname -sm; rustc -V
'

common=(
  --rm
  -v "$ROOT:/src"
  -v "$CARGO_HOME_HOST/registry:/cargo/registry"
  -v "$CARGO_HOME_HOST/git:/cargo/git"
  -w /src
)

case "$MODE" in
  copy)
    docker run "${common[@]}" "$IMAGE" bash -c "$prelude"'
      export CARGO_TARGET_DIR=/src/target/linux-copy
      cargo test --offline --no-fail-fast 2>&1 | grep -E "^(test |test result|error|failures)|panicked"
    '
    ;;
  btrfs)
    docker run --privileged "${common[@]}" "$IMAGE" bash -c "$prelude"'
      img=/src/target/linux-btrfs.img
      truncate -s 6G "$img"
      mkfs.btrfs -q -f "$img"
      mkdir -p /mnt/btrfs && mount -o loop "$img" /mnt/btrfs
      trap "umount /mnt/btrfs; rm -f $img" EXIT
      mkdir -p /mnt/btrfs/naspeed-src
      tar -C /src --exclude=./target -cf - . | tar -C /mnt/btrfs/naspeed-src -xf -
      cd /mnt/btrfs/naspeed-src
      export CARGO_TARGET_DIR=/mnt/btrfs/target
      cargo test --offline --no-fail-fast 2>&1 | grep -E "^(test |test result|error|failures)|panicked"
      # Disk cost of a second target dir served from the store (reflinks).
      cargo build --offline --release -q
      ws=/mnt/btrfs/df-check; rm -rf $ws; mkdir -p $ws/store $ws/cfg
      cp -r tests/fixtures/sample_workspace $ws/ws
      used() { sync; btrfs filesystem sync /mnt/btrfs; df -k --output=used /mnt/btrfs | tail -1; }
      b() { (cd $ws/ws && env -u RUSTC_WRAPPER RUSTC_WRAPPER=$CARGO_TARGET_DIR/release/naspeed \
            NASPEED_STORE=$ws/store NASPEED_CONFIG_DIR=$ws/cfg CARGO_TARGET_DIR=$ws/$1 cargo build --offline -q); }
      u0=$(used); b t1; u1=$(used); b t2; u2=$(used)
      echo "df-check: first build +$((u1-u0)) KiB, second target dir +$((u2-u1)) KiB"
    '
    ;;
  *)
    echo "usage: $0 copy|btrfs" >&2
    exit 2
    ;;
esac
