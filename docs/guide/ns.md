# ns front-end

`setup` also creates `~/.cargo/bin/ns`, a symlink to naspeed. Through `ns`, the
binary works as a project front-end that drives cargo for the dev loop,
releases, container images and CI. It does not replace cargo: `cargo`,
rust-analyzer and IDEs work as before and still go through the cache. Invoked
as `ns`, the binary never enters wrapper mode, so a typo like `ns rustc` errors
out instead of compiling.

## Commands

| Command | What it does |
|---|---|
| `ns dev [--bin B] [--test] [-- args]` | Watches the workspace (respecting `.gitignore`, always skipping `target*` and `.git`), runs `cargo check` on each change, then builds and restarts the binary. A compile error keeps the old process serving. The old process group is stopped (SIGTERM, then SIGKILL after 5 s) and `dev.ports` are free before the new one starts. Changes during a build trigger one more cycle. A change to `.env` only restarts. `--test` runs `cargo test -p` for the changed crates and every workspace crate that depends on them. Ctrl-C cleans up the whole process group. |
| `ns fmt [--check] [cargo fmt args]` | `cargo fmt` over every package of the workspace; `--check` reports unformatted files instead of rewriting them. Packages whose `.rs` files are unchanged since they were last formatted cleanly are skipped (the record is in the store, keyed by rustfmt version and config, shared by all worktrees), so a repeat run takes a fraction of a second. `-p` narrows it (no cache), rustfmt flags go after `--`. |
| `ns check [--fix] [--no-fmt] [clippy args]` | Format, lint and type check in one step: `cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`, both through the cache. Each step is reported and the command fails if either fails. `-p` or target flags narrow the clippy run, flags after `--` replace `-D warnings`, and `--fix` formats in place and applies clippy's automatic fixes. |
| `ns test [--no-cache] [cargo test args] [-- test args]` | Runs `cargo test` with naspeed as the target runner; a test binary whose inputs did not change since it last passed replays that run's output (marked `(cached)` on stderr) instead of running. Failures are never cached. It ends with what the run did: rustc hits and misses, test binaries replayed and run. `ns dev --test` goes through the same cache. See [Test result cache](./test-cache). |
| `ns release [--snapshot] [--publish] [--target T]…` | Runs the `before` hooks, then builds every build variant × target (zig, xwin, universal2 and pinned glibc come from naspeed's cross setup). It writes reproducible archives, a checksum file, one SPDX SBOM per build, and `metadata.json` to `dist/`, which carries its own `.gitignore` so it never shows up in `git status`. Signing uses keyless cosign when it is available. `--publish` checks for a clean tree and a pushed tag, then creates the GitHub Release (draft and pre-release flags, filtered changelog) and uploads the assets. |
| `ns image [--push] [--registry R] [--tag T]…` | Turns the release binaries into a multi-arch OCI image on distroless `:nonroot` and pushes it through the registry HTTP API, with no Docker daemon. Base layers are referenced, or mounted or copied server-side, never rebuilt. The SBOM is attached as `sha256-<digest>.sbom`. Without `--push` it only writes `<dist>/oci` and prints what it would push. |
| `ns ci init [--force]` | Writes `.github/workflows/ns-ci.yml` (fmt, clippy, `ns test` on Linux and macOS), `ns-build.yml` (a snapshot release on main that warms the release cache) and `ns-release.yml` (on `v*` tags: release, then image). The naspeed store is carried in the GitHub Actions cache. Pull requests only restore it; only pushes to main save it. Each workflow gets the minimal permissions it needs. |
| `ns cache export <dir> [--max-gb N]` / `ns cache import <dir>` | Copies the most recently used store entries (and cached test results) to or from a plain directory, which is what `actions/cache` saves. Import re-hashes every object and skips what the store already has. |
| `ns cache serve [--listen ADDR] [--allow-push]` | Serves this machine's store to other machines over HTTP, by default on its Tailscale IPv4 (`tailscale ip -4`), port 7878. Read-only unless `--allow-push`, which needs a token. See [Peer cache over Tailscale](./peer-cache). |
| `ns cache push <url> [--since-hours H]` | Uploads entries used in the last H hours (default 24) to a peer started with `--allow-push`, objects first, entries last. |

Builds see `NS_VERSION`, `NS_TAG`, `NS_COMMIT` and `SOURCE_DATE_EPOCH`, so a
binary can embed its version with `option_env!("NS_VERSION")`.

## `ns.toml`

Optional; without it, defaults come from `cargo metadata`. Unknown keys are
errors.

```toml
[dev]
bin = "api"
args = ["serve", "--port", "8317"]
env_file = ".env"
ports = [8317]
test = true

[release]
before = ["cd web/ui && bun install --frozen-lockfile && bun run build"]
targets = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "universal2-apple-darwin"]
checksum = "checksums.txt"
draft = true
prerelease = "auto"                          # v1.2.0-rc.1 → pre-release
archive = { name = "{binary}_{version}_{os}_{arch}", files = ["README.md", "LICENSE"] }
changelog = { sort = "asc", exclude = ["^docs:", "^test:", "^chore:"] }

[[release.builds]]                           # build variants
bin = "api"
features = ["dashboard-ui"]

[[release.builds]]
id = "noui"
bin = "api"
binary = "api-noui"

[test]                                       # `ns test` result cache
cache = true                                 # false: always run
env = ["DATABASE_URL"]                       # env vars the tests read (part of the key)
inputs = ["testdata"]                        # extra files/dirs read at run time

[image]                                      # OCI image
name = "ghcr.io/owner/api"
build = "api"
cmd = ["serve", "-config", "/data/config.yaml"]
workdir = "/data"
volumes = ["/data"]
ports = ["8317"]
tags = ["{version}", "latest"]               # latest is skipped for snapshots and pre-releases
```

Defaults for `[image]`: the binary goes to `/usr/bin/{binary}`, runs as user
`65532:65532`, and sits on `gcr.io/distroless/static-debian12:nonroot` (musl)
or `cc-debian12:nonroot` (gnu). Labels
`org.opencontainers.image.{title,version,revision,source,created}` are set.
