# Development

## Build and test

```sh
cargo build --release
ns test
ns check   # cargo fmt --check, then clippy -D warnings on every target
```

Integration tests live in `tests/` (setup/uninstall, materialize, gc, peer
cache, test cache, release snapshot, local OCI registry, and others), with
shared helpers in `tests/common` and fixture crates in `tests/fixtures`.

## Try the development build locally

Install the debug build into `~/.local/bin` without replacing a Homebrew
installation:

```sh
make dev-install
hash -r
make dev-status
```

Both `naspeed` and its `ns` alias are installed. If `~/.local/bin` precedes
Homebrew in `PATH`, the development build is selected while the Homebrew files
remain untouched. Remove only the development copy and return to Homebrew with:

```sh
make dev-uninstall
hash -r
make dev-status
```

Set `DEV_BINDIR=/another/bin` on either install or uninstall to use a different
local directory. The Makefile records ownership in that directory and refuses
to replace or remove an existing `naspeed`/`ns` pair it does not own. After
checking a pre-existing pair, `DEV_FORCE=1 make dev-install` explicitly replaces
it and records this checkout as the owner.

## Linux

```sh
scripts/test_linux.sh copy    # store + target dirs on the bind mount (no reflink → copy mode)
scripts/test_linux.sh btrfs   # same tests on a loop-mounted btrfs (reflink mode, needs --privileged)
```

Both run the suite in a `debian:bookworm-slim` container, with the toolchain
and the container's target dirs under `target/linux-*`.

## Measurement scripts

| Script | Purpose |
|---|---|
| `scripts/measure_baseline.sh` | Disk usage and build time before and after naspeed (`env`, `sizes`, `warm`, `cold`, `parallel-ram`). `env` now also emits a versioned machine-readable BenchmarkRun JSON record (`schema_version` 1). Never runs `cargo clean` on a real project. |
| `scripts/measure_action_engine.py` | Collects, floors and compares BenchmarkRun records (`collect`, `floor`, `compare`) for the action-engine benchmarks. |
| `scripts/target_components.py` | Splits one `target/` dir into registry deps, workspace deps, incremental, build-script `OUT_DIR` and other. Run with `python3 -I`. |
| `scripts/bench_copies.sh` | Benchmarks naspeed on copies of real projects, never the originals. |
| `scripts/ci_simulate.sh` | Simulates two runs of the `ns ci init` workflow on fresh runners (Linux container, or `host`). |

### BenchmarkRun records

```sh
bash scripts/measure_baseline.sh env
python3 scripts/measure_action_engine.py collect --repo <path> \
    --scenario <no-op|leaf-edit|core-edit|warm-cache-cold-runner|three-worktree> \
    --command <cmd>
python3 scripts/measure_action_engine.py floor --repo ../go-workspace
python3 scripts/measure_action_engine.py compare --baseline a.json --candidate b.json
```

`collect` reports the median of five valid samples; the warm-up run is
recorded but excluded. A sample is invalid when the toolchain, lockfile
digest, requested targets or cache generation change. Metrics that cannot be
measured appear as explicit `{"status":"unsupported","reason":...}` objects —
collection fails loudly rather than silently dropping a key. Environment
values are redacted to names plus hashes. Reports go under a gitignored
output dir (`.bench/` when it is ignored, else `NASPEED_BENCH_OUT`, else a
temp dir) and are never committed.

`floor` measures a Go workspace's mandatory `go test ./... -race -count=1`
PostgreSQL floor, timed separately from compile/setup: `test_s`/`floor_s` is
the uncacheable test runtime CI can never go below, `build_s`/`compressible_s`
is the headroom left for optimization. `floor_share_pct` near 100 means a
wall-clock CI speedup target is impossible without changing the test suite
itself. An unreachable postgres yields an explicit unsupported/blocked
metric, never a faked number.

## Docs

This site is built with VitePress from `docs/`:

```sh
pnpm --dir docs install
pnpm --dir docs run dev     # local preview
pnpm --dir docs run build   # static site in docs/.vitepress/
```

## CI

| Workflow | Trigger | Jobs |
|---|---|---|
| `ci.yml` | pull requests, called by `release.yml`, run by hand | `ns check` (rustfmt and clippy) and `ns test --lib --bins` (plus the setup and wrapper suites on Windows) on Linux, macOS and Windows · `cargo publish --dry-run` |
| `release.yml` | push to `main` | `ci.yml`, then release-plz: open or update the Release PR; after it merges, publish to crates.io, tag `v<version>`, create the GitHub release and call `release-binaries.yml` |
| `release-binaries.yml` | called by `release.yml`, or run by hand with an existing tag | build `naspeed-<tag>-<target>.tar.gz` for macOS (arm64, x86_64) and Linux musl (arm64, x86_64), attach them to the release, update `naspeed.rb` in `dacsang97/homebrew-tap` |
| `docs.yml` | push to `main` touching `docs/**` | VitePress build → GitHub Pages |

The check, test and package jobs build through the released naspeed itself,
installed by [naspeed-action](https://github.com/dacsang97/naspeed-action),
which carries the store in the GitHub Actions cache (one `cache-key` per job).
Only pushes to `main` save it; pull requests restore it read-only.

## Release

Releases go through [release-plz](https://release-plz.dev/). Every push to
`main` refreshes a Release PR that bumps the version in `Cargo.toml` and
updates `CHANGELOG.md` from conventional commits. Merging that PR publishes the
release; an ordinary commit never does. If the binaries or the Homebrew step
fail, rerun `release-binaries.yml` from the Actions tab with the tag (for
example `v0.1.0`).

One-time repository setup:

| Setting | Where | Purpose |
|---|---|---|
| `CRATES_IO_PUBLISH_ENABLED` = `true` | Actions variable | turns on the publish job |
| `crates-io` | Environment | runs the publish job (add required reviewers here to gate releases) |
| `CARGO_REGISTRY_TOKEN` | secret in the `crates-io` environment | crates.io token with `publish-new` and `publish-update`. The first publish needs it; afterwards you can set up crates.io trusted publishing for `release.yml` and delete it |
| `HOMEBREW_TAP_TOKEN` | repository secret | fine-grained token with contents read/write on `dacsang97/homebrew-tap` |
| Allow GitHub Actions to create and approve pull requests | Settings → Actions → General | lets release-plz open the Release PR |

`scripts/homebrew_formula.sh <tag> <checksums-dir>` prints the formula the
workflow commits, so a release's formula can be inspected locally.

## Project layout

```
src/wrapper      RUSTC_WRAPPER: cacheability, cache key, hit/miss, build-script runs, incremental warm-up
src/store        content-addressed store, copy-on-write copies, key locks, usage log
src/jobserver    machine-wide rustc slots and memory guard
src/gc           store LRU, target sweep, dedup, daily auto gc
src/setup        ~/.cargo/config.toml edits and shell env check
src/commands     naspeed subcommands (status, stats, gc, purge, build, cache …)
src/cross        zig linker shims and msvc SDK
src/remote       peer cache client and server
src/project      ns.toml and Cargo workspace graph
src/dev          ns dev toolchain selection, Cargo/Go graphs, watcher, build/test cycle, process supervision and TUI
src/testcache    ns test result cache (Cargo)
src/release      ns release
src/image        ns image (OCI)
src/ci           ns ci init
tests/           integration tests
docs/            this site (VitePress)
```
