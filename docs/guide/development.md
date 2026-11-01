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
| `scripts/measure_baseline.sh` | Disk usage and build time before and after naspeed (`env`, `sizes`, `warm`, `cold`, `parallel-ram`). Never runs `cargo clean` on a real project. |
| `scripts/target_components.py` | Splits one `target/` dir into registry deps, workspace deps, incremental, build-script `OUT_DIR` and other. Run with `python3 -I`. |
| `scripts/bench_copies.sh` | Benchmarks naspeed on copies of real projects, never the originals. |
| `scripts/ci_simulate.sh` | Simulates two runs of the `ns ci init` workflow on fresh runners (Linux container, or `host`). |

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
| `ci.yml` | pull requests, called by `release.yml`, run by hand | `ns check` (rustfmt and clippy) and unit tests with this commit's naspeed installed by its own `naspeed setup` (`ns test --lib --bins`, plus the setup and wrapper suites on Windows) on Linux, macOS and Windows · `cargo publish --dry-run` |
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
src/project      ns.toml and workspace graph
src/dev          ns dev
src/testcache    ns test result cache
src/release      ns release
src/image        ns image (OCI)
src/ci           ns ci init
tests/           integration tests
docs/            this site (VitePress)
```
