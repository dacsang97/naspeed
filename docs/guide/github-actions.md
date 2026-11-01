# GitHub Actions

[`dacsang97/naspeed-action`](https://github.com/dacsang97/naspeed-action)
brings naspeed to a workflow in one step:

1. downloads the naspeed release for the runner (Linux, macOS and Windows,
   x64 and arm64), checks its checksum and keeps it in the runner tool cache;
2. runs `naspeed setup`, so every later `cargo` call in the job goes through
   naspeed (`ns` is on `PATH` too);
3. restores the naspeed store from the GitHub Actions cache, and at the end of
   a successful job exports it and saves it again;
4. optionally runs `naspeed <args>`;
5. writes a **naspeed Report** to the job summary: rustc hits and misses,
   test binaries replayed, time saved and what happened to the cache.

It replaces `Swatinem/rust-cache`, sccache setups and hand-written
`actions/cache` steps for the build. Install Rust before it.

## Check and test

```yaml
name: ci

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - uses: dacsang97/naspeed-action@v1
        with:
          cache-key: check
          args: check --locked # rustfmt --check, then clippy -D warnings

  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
      - uses: dacsang97/naspeed-action@v1
        with:
          cache-key: test
          args: test --workspace --locked # cargo test + test result cache
```

`args` runs `naspeed <args>` after setup and the cache restore, so
`args: test --locked` is `ns test --locked`. Leave it empty to only install
and set up; later `run:` steps that call `cargo` or `ns` still go through
naspeed, and the report counts them too:

```yaml
- uses: dacsang97/naspeed-action@v1
  with:
    cache-key: clippy
- run: cargo clippy --locked --all-targets -- -D warnings
- run: cargo build --locked --release
```

The test cache carries over between runs: a test binary identical to one
that passed in the run that saved the store replays its output instead of
running. Pushes to `main` refresh it.

## Cache keys and who may write the cache

Give each kind of job its own `cache-key` (`check`, `test`, `release`…), so a
release build does not evict the store a test job relies on. Entries are
saved as

```text
<cache-key>-<os>-<arch>-<rustc version>-<hash of **/Cargo.lock>-<run id>-<attempt>-<job>
```

A restore takes the newest entry with the same lockfile, then falls back to
any entry for the same compiler, so a dependency bump still starts from a warm
store. GitHub evicts the least recently used entries past 10 GB per
repository; `cache-max-gb` (default 3) caps each exported store.

With `cache-save: auto` (the default) only pushes to the default branch save
the store. Pull requests, forks included, restore it but never write it, so a
pull request cannot plant compiled artifacts that `main` builds or releases
would pick up later. Use `cache-save: false` on jobs that should only read.

## Cross builds and releases

```yaml
- uses: dacsang97/naspeed-action@v1
  with:
    cache-key: release
    setup-args: --cross --install-targets
    args: release --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl
```

`setup-args` go to `naspeed setup`: `--cross` installs the zig linker shims
for Linux and windows-gnu targets, `--install-targets` adds the missing
`rustup` targets. See [ns front-end](./ns) for `ns release` and `ns image`.

## Only the binary

```yaml
- uses: dacsang97/naspeed-action@v1
  with:
    install-only: true
```

puts naspeed on `PATH` without running `setup` or touching the cache.

## Inputs

| Name | Default | Description |
|---|---|---|
| `version` | `latest` | `latest`, an exact version (`0.2.0` or `v0.2.0`) or a semver range (`~0.2`) |
| `args` | | Arguments for naspeed, run after setup and restore, e.g. `test --locked` |
| `workdir` | `.` | Working directory for `args`, relative to the repository root |
| `summary` | `true` | Write the naspeed report to the job summary |
| `store` | `~/.naspeed` | naspeed store directory; on Windows the default follows the workspace drive (`D:\.naspeed` on hosted runners) |
| `setup-args` | | Extra arguments for `naspeed setup`, e.g. `--cross --install-targets` |
| `install-only` | `false` | Only install naspeed; no `naspeed setup`, no cache |
| `cache` | `true` | Restore the store from the Actions cache (and save it, see `cache-save`) |
| `cache-key` | `naspeed` | Cache key prefix; use one per kind of job |
| `cache-save` | `auto` | `auto` (pushes to the default branch only), `true` or `false` |
| `cache-max-gb` | `3` | Size cap of the exported store |
| `token` | the job's `GITHUB_TOKEN` | Token used to look up naspeed releases |

Outputs: `version` (installed naspeed version), `path` (binary path),
`cache-hit` (`true` when a saved store was restored) and `cache-matched-key`.

## Report

With `summary: true` the job summary gets a report such as:

> **naspeed Report**
>
> - **Command:** ✅ `naspeed test --locked`
> - **Build:** ✅ **346 hits** · 2 misses · 99.4% hit rate · saved 4m01s
> - **Tests:** ✅ **1 replayed** · 1 ran · saved 4s
> - **Store:** ♻️ restored `test-Linux-X64-1.99.1-ab12…-41-1-test`

The numbers cover every `cargo` and `ns` call of the job after the action
step.

## Pinning and updates

`@v1` follows the latest 1.x release of the action. For stricter
reproducibility pin an exact tag (`@v1.1.1`) or a commit SHA with the tag as a
comment, and let Dependabot or Renovate bump it:

```yaml
# .github/dependabot.yml
version: 2
updates:
  - package-ecosystem: github-actions
    directory: /
    schedule:
      interval: weekly
```

The naspeed binary itself follows `version` (`latest` by default); pin it
(`version: 0.2.0`) when CI must not pick up a new release unannounced.

## Generated workflows

`ns ci init` writes a complete set of workflows for a project (`ns-ci.yml`,
`ns-build.yml`, `ns-release.yml`) with the same cache rules; it is the quickest
start for a repository that has no CI yet. See [ns front-end](./ns).
