# ns front-end

`setup` also creates `~/.cargo/bin/ns`, a symlink to naspeed. Through `ns`, the
binary works as a project front-end. `ns dev` supports Cargo workspaces, Go
modules and `go.work` workspaces; release, image, CI, check, format and cached
test commands remain Cargo-only. It does not replace cargo or Go tooling.
Invoked as `ns`, the binary never enters wrapper mode, so a typo like
`ns rustc` errors out instead of compiling.

## Commands

| Command | What it does |
|---|---|
| `ns dev [--bin B]… [--all] [--no-tui] [--test] [-- args]` | Detects Cargo or Go, watches build inputs, builds once, then restarts only affected application process groups. Failed builds leave old processes serving; changes during builds are queued. Interactive terminals get the searchable log TUI; pipes and `--no-tui` get prefixed plain output. See [`ns dev`](#ns-dev). |
| `ns dev --no-tui --exec -- <command> [args]` | Runs an external foreground command with this naspeed binary as `RUSTC_WRAPPER`. Streams stay attached to the terminal; naspeed does not watch files, launch a TUI, or change global Cargo config. |
| `ns fmt [--check] [cargo fmt args]` | `cargo fmt` over every package of the workspace; `--check` reports unformatted files instead of rewriting them. Packages whose `.rs` files are unchanged since they were last formatted cleanly are skipped (the record is in the store, keyed by rustfmt version and config, shared by all worktrees), so a repeat run takes a fraction of a second. `-p` narrows it (no cache), rustfmt flags go after `--`. |
| `ns check [--fix] [--no-fmt] [clippy args]` | Format, lint and type check in one step: `cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`, both through the cache. Each step is reported and the command fails if either fails. `-p` or target flags narrow the clippy run, flags after `--` replace `-D warnings`, and `--fix` formats in place and applies clippy's automatic fixes. |
| `ns test [--no-cache] [cargo test args] [-- test args]` | Runs `cargo test` with naspeed as the target runner; a test binary whose inputs did not change since it last passed replays that run's output (marked `(cached)` on stderr) instead of running. Failures are never cached, and every binary runs even after one fails (`--fail-fast` stops at the first, like plain cargo). It ends with what the run did: rustc hits and misses, test binaries replayed and run. `ns dev --test` goes through the same cache. See [Test result cache](./test-cache). |
| `ns release [--snapshot] [--publish] [--target T]…` | Runs the `before` hooks, then builds every build variant × target (zig, xwin, universal2 and pinned glibc come from naspeed's cross setup). It writes reproducible archives, a checksum file, one SPDX SBOM per build, and `metadata.json` to `dist/`, which carries its own `.gitignore` so it never shows up in `git status`. Signing uses keyless cosign when it is available. `--publish` checks for a clean tree and a pushed tag, then creates the GitHub Release (draft and pre-release flags, filtered changelog) and uploads the assets. |
| `ns image [--push] [--registry R] [--tag T]…` | Turns the release binaries into a multi-arch OCI image on distroless `:nonroot` and pushes it through the registry HTTP API, with no Docker daemon. Base layers are referenced, or mounted or copied server-side, never rebuilt. The SBOM is attached as `sha256-<digest>.sbom`. Without `--push` it only writes `<dist>/oci` and prints what it would push. |
| `ns ci init [--force]` | Writes `.github/workflows/ns-ci.yml` (fmt, clippy, `ns test` on Linux and macOS), `ns-build.yml` (a snapshot release on main that warms the release cache) and `ns-release.yml` (on `v*` tags: release, then image). The naspeed store is carried in the GitHub Actions cache. Pull requests only restore it; only pushes to main save it. Each workflow gets the minimal permissions it needs. |
| `ns cache export <dir> [--max-gb N]` / `ns cache import <dir>` | Copies complete native entries plus newest whole memo, test-result, Go build-cache, and image build-cache files to a transfer directory. `--max-gb` caps every transferred regular file; `0` exports no payload bytes. Imports merge auxiliary caches without deleting local files. |
| `ns cache serve [--listen ADDR] [--allow-push]` | Serves this machine's store to other machines over HTTP, by default on its Tailscale IPv4 (`tailscale ip -4`), port 7878. Read-only unless `--allow-push`, which needs a token. See [Peer cache over Tailscale](./peer-cache). |
| `ns update [--check]` | Checks the channel naspeed was installed with (Homebrew tap, `cargo install`, or a downloaded archive) and updates to the latest release, finishing with `naspeed setup` so the `~/.cargo/bin` copy and `ns` alias refresh too. `--check` only reports. |
| `ns cache push <url> [--since-hours H]` | Uploads entries used in the last H hours (default 24) to a peer started with `--allow-push`, objects first, entries last. |

Builds see `NS_VERSION`, `NS_TAG`, `NS_COMMIT` and `SOURCE_DATE_EPOCH`, so a
binary can embed its version with `option_env!("NS_VERSION")`.

## `ns dev`

`--bin` is repeatable and overrides configured selections. `--all` selects
every project binary and cannot be combined with `--bin`. If neither is
provided, `[dev].bins` is used; legacy `[dev].bin` remains supported. Arguments
after `--` replace configured arguments and require exactly one selected bin.

Use `--exec` when the supplied command owns the development loop. It implies
plain output and leaves stdout and stderr untouched; add `2>&1` to combine
them. `--bin`, `--all`, and `--test` apply only to the managed loop.

```toml
[dev]
bins = ["api", "worker"]
args = ["--verbose"]
env_file = ".env"
log_dir = true

[dev.apps.api]
args = ["serve", "--port", "8317"]
env_file = ".env.api"
ports = [8317]

[dev.apps.worker]
args = ["worker"]
env_file = ".env.worker"
```

Global `args`, `env_file` and `ports` are defaults; `[dev.apps.<bin>]` overrides
them per binary. Source changes build the selected binaries once, then restart
only apps whose package or dependency changed. Manifest changes affect every
selected app. An app's own environment file restarts only that app; another
watched runtime file restarts all selected apps. A failed build keeps existing
processes alive. Files observed during the cycle run in the next cycle.

### Go projects

At startup `ns dev` uses `go.work` when `go env GOWORK` names one, otherwise
the module containing `go.mod`. A `go.work` workspace requires Go 1.25 or newer.
Main-package names come from the last import-path element (`cmd/api` is `api`).
When two main packages have the same name, disambiguate the selected app:

```toml
[dev]
toolchain = "go"
bins = ["api"]

[dev.apps.api]
package = "example.com/service/cmd/api"
```

Go source, cgo and embedded-file changes rebuild only mains that import the
changed package. `_test.go` changes run affected tests with `--test` but do not
restart apps. `go.mod`, `go.sum`, `go.work`, `go.work.sum`, and a new package
reload the package graph. Binaries and logs live under `.ns-dev/`, which should
be gitignored. `vendor`, `testdata`, and `.ns-dev` are never watched.

If Cargo and Go marker files coexist at the same project root, set
`[dev].toolchain = "cargo"` or `"go"`. All commands except `ns dev` remain
Cargo-only and return a direct error in a Go-only directory.

For Cargo projects, `ns dev` keeps `target/` from growing without bound: cargo never
deletes old artifacts on its own. Every cargo check and build it starts marks
the units cargo reports, fresh ones included, as used. Five minutes after
start and then every 30 minutes, a background thread runs the
`naspeed gc --targets` sweep on the project's profile dirs and prints what it
freed as a status line. A profile dir cargo is using at that moment is left
for the next round. Units that `ns check`, `ns test` or a plain `cargo` reuse
without rebuilding are not marked, so for them "used" means last rebuilt; the
newest variant of each crate is always kept, so at worst a variant that only
those commands use and that has not been rebuilt for `store_max_age_days`
(default 14) is rebuilt once. `NASPEED_NO_AUTO_GC=1` turns the thread off.

Session logs default to `target/ns-dev` for Cargo and `.ns-dev/logs` for Go; a
string selects another directory and `false` disables files. Each session
truncates its files, strips ANSI controls, timestamps lines, and keeps one `.1`
rotation after a log exceeds 50 MiB. `build.log` is reserved for the build
pipeline; if an app name would collide with it or another assigned filename,
`app-` is prefixed repeatedly until that session's name is unique.

An interactive stdin/stdout starts the TUI; `--no-tui` or redirected streams
use plain output, with app lines prefixed by binary on stdout and status/build
lines on stderr. With a single app its lines carry no `[bin]` prefix, in plain
output, the TUI and copied text alike. Cargo progress streams as build lines
while a build runs. Go diagnostics are reported together when a build fails,
so multi-line compiler and module hints remain intact.

The TUI's header holds the tabs (All, one per bin, and build, which shows the
compiler's and `--test` output and turns red when a build fails; click one, or
use Tab and Shift-Tab) with each process's state and the last build on the
right. Wrapped rows of untagged lines hang two columns in.
The mouse wheel scrolls the logs; scrolling back to the bottom (or `End`,
`Enter`) follows new lines again. Drag over the logs to select text as in a
terminal; while holding the left button, use the wheel to extend the selection
beyond the current viewport. A double-click picks the word under the pointer,
including one that wraps across rows. Releasing the button copies it, with wrapped rows
rejoined into their line. While a selection is shown the log view holds still;
the next key or click drops it, as does scrolling after release. The composer
at the bottom is the filter, on a
surface shaded from the terminal's background (asked with OSC 11; terminals
that do not answer get none): typing filters the logs as you go (smart-case
literal, or a `re:` regex; an incomplete regex keeps the last valid filter),
the hits are highlighted and counted on the right, Backspace edits it and
`Esc` or Ctrl-U clears it.
The footer only says `? for shortcuts`: `?` on an empty filter lists them
(again, `Esc` or any key closes the list). Commands sit on control keys: Ctrl-E cycles the severity filter, Ctrl-R
restarts the active tab's binary (every binary from All or build), Ctrl-L clears the active view
and Ctrl-C or Ctrl-D quits; `Up`/`Down`, PgUp/PgDn and `Home` still scroll.
Copy sends OSC 52 and also tries the platform clipboard command; the TUI
reports which methods succeeded.
Terminal modes are restored on exit, Ctrl-C, SIGTERM and panic unwinding.

## `ns.toml`

Optional; without it, defaults come from the selected toolchain's project
graph. Unknown keys are errors.

```toml
[dev]
toolchain = "cargo"                           # or "go"; required when both markers exist
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
trace = true                                 # record what tests read at run time
bins = "scoped"                              # key only on own-package bins (trace covers executed ones)
exclude = ["docs/**"]                        # left out of untraced runs' workspace files

[image]                                      # OCI image
name = "ghcr.io/owner/api"
build = "api"
cmd = ["serve", "-config", "/data/config.yaml"]
workdir = "/data"
volumes = ["/data"]
ports = ["8317"]
tags = ["{version}", "latest"]               # latest is skipped for snapshots and pre-releases
```

For a Go service, configure the release and image behavior explicitly while
keeping the intended public binary and image names:

```toml
[release]
toolchain = "go"
bins = ["lineart_api_svc"]
go_main = "./cmd"
go_ldflags = ["-s", "-w"]
go_cgo = false
targets = ["linux/amd64"]
sbom = "none"
changelog = { sort = "asc", exclude = ["^docs:", "^test:"] }

[image]
name = "docker.io/example/ght-lineart-api-svc"
bin = "lineart_api_svc"
platforms = ["linux/amd64"]
dockerfile = "build/Dockerfile"
extra_files = ["configs/casbin/rbac_model.conf", "assets"]
tags = ["{version}", "latest"]
```

`ns release --snapshot` and `ns release --publish` point both Go caches at the
naspeed store: the content-addressed compiled build cache (`go/build`) and the
module cache (`go/mod`), so the GitHub Action's store-directory cache carries
them between CI runs and a cold runner stops re-downloading dependencies.
Only `go/mod/cache/download` — checksummed public artifacts — is transferred;
`go/mod/cache/vcs`, which holds the git clones and their private-module
credentials, never leaves the machine. Go targets use `GOOS/GOARCH` spelling.
A tagged Go release takes its version from the single `vX.Y.Z` tag on `HEAD`;
snapshots use `0.0.0-SNAPSHOT-<sha>`.

When `image.dockerfile` is set, `ns image` stages the selected release binary
and `extra_files` in an isolated context and runs Docker Buildx. `--push`
selects `--push`; a local build selects `--load`. BuildKit's `mode=max` local
cache lives under the naspeed OCI store and is reused on the next build.
Dockerfile images currently require exactly one configured platform. Without
`dockerfile`, the daemonless multi-platform OCI path described below is used.

Defaults for `[image]`: the binary goes to `/usr/bin/{binary}`, runs as user
`65532:65532`, and sits on `gcr.io/distroless/static-debian12:nonroot` (musl)
or `cc-debian12:nonroot` (gnu). Labels
`org.opencontainers.image.{title,version,revision,source,created}` are set.
