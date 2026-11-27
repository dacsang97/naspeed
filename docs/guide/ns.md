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
| `ns build [cargo build args]` | Runs Cargo directly, reusing its target directory and, after `naspeed setup`, the configured build cache. For universal2 and pinned glibc, use [`naspeed build`](./commands). |
| `ns run [cargo run args] [-- program args]` | Builds only what Cargo needs, then runs the selected binary or example with Cargo's working directory, environment, and target runner. |
| `ns dev [--bin B]… [--all] [--no-tui] [--test] [-- args]` | Detects Cargo or Go, watches build inputs, builds once, then restarts only affected application process groups. Failed builds leave old processes serving; changes during builds are queued. Interactive terminals get the searchable log TUI; pipes and `--no-tui` get prefixed plain output. See [`ns dev`](#ns-dev). |
| `ns fmt [--check] [cargo fmt args]` | `cargo fmt` over every package of the workspace; `--check` reports unformatted files instead of rewriting them. Packages whose `.rs` files are unchanged since they were last formatted cleanly are skipped (the record is in the store, keyed by rustfmt version and config, shared by all worktrees), so a repeat run takes a fraction of a second. `-p` narrows it (no cache), rustfmt flags go after `--`. |
| `ns check [--fix] [--no-fmt] [clippy args]` | Format, lint and type check in one step: `cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`, both through the cache. Each step is reported and the command fails if either fails. `-p` or target flags narrow the clippy run, flags after `--` replace `-D warnings`, and `--fix` formats in place and applies clippy's automatic fixes. |
| `ns patch [--bin B] [--target-dir D]` | Rebuilds only the patched function bodies of the last `ns build --lazy-ir` plan without rustc: `nasc` re-emits each selector body against the signature recorded at build time, `naspeed-lazy-launch --emit-obj` compiles it, `rust-lld` links a new patch dylib, and the updated plan is staged to `live/<app>` so a running process flips in under a second. It refuses — telling you to run `ns build --lazy-ir` — when any selector's signature changed, a body uses constructs outside `nasc`'s recorded-callset, or any other function in the crate was added, removed, or edited (checked against the per-build `items.map` ledger). The running process applies the result itself when launched with `NASPEED_LAZY_APP_LIVE=1` or under `ns dev` with `live`. |

When a `[dev.apps.<bin>].live` app is configured, `ns dev` tries this fast
path itself on every rebuild: if every changed Rust input belongs to the
live app's package, it runs `ns patch` first and skips `cargo build`
entirely — the running process installs the new bodies and the cycle
reports in under a second. Anything outside that scope (signature
changes, new or removed functions, non-selector edits, other crates)
falls through to the normal build automatically.
| `ns test [--no-cache] [cargo test args] [-- test args]` | Replays passing test binaries whose inputs are unchanged (marked `(cached)` on stderr). Failures are never cached, and every binary runs even after one fails (`--fail-fast` stops at the first, like plain Cargo). `--no-cache` skips test-cache planning and counters without disabling the rustc build cache. `ns dev --test` uses the same test cache. See [Test result cache](./test-cache). |
| `ns release [--snapshot] [--publish] [--target T]…` | Runs the `before` hooks, then builds every build variant × target (zig, xwin, universal2 and pinned glibc come from naspeed's cross setup). It writes reproducible archives, a checksum file, one SPDX SBOM per build, and `metadata.json` to `dist/`, which carries its own `.gitignore` so it never shows up in `git status`. Signing uses keyless cosign when it is available. `--publish` checks for a clean tree and a pushed tag, then creates the GitHub Release (draft and pre-release flags, filtered changelog) and uploads the assets. |
| `ns image [--push] [--registry R] [--tag T]…` | Turns the release binaries into a multi-arch OCI image on distroless `:nonroot` and pushes it through the registry HTTP API, with no Docker daemon. Base layers are referenced, or mounted or copied server-side, never rebuilt. The SBOM is attached as `sha256-<digest>.sbom`. Without `--push` it only writes `<dist>/oci` and prints what it would push. |
| `ns ci init [--force]` | Writes `.github/workflows/ns-ci.yml` (fmt, clippy, `ns test` on Linux and macOS), `ns-build.yml` (a snapshot release on main that warms the release cache) and `ns-release.yml` (on `v*` tags: release, then image). The naspeed store is carried in the GitHub Actions cache. Pull requests only restore it; only pushes to main save it. Each workflow gets the minimal permissions it needs. |
| `ns cache export <dir> [--max-gb N]` / `ns cache import <dir>` | Copies complete native entries plus newest whole memo, test-result, Go build-cache, and image build-cache files to a transfer directory. `--max-gb` caps every transferred regular file; `0` exports no payload bytes. Imports merge auxiliary caches without deleting local files. |
| `ns cache serve [--listen ADDR] [--allow-push]` | Serves this machine's store to other machines over HTTP, by default on its Tailscale IPv4 (`tailscale ip -4`), port 7878. Read-only unless `--allow-push`, which needs a token. See [Peer cache over Tailscale](./peer-cache). |
| `ns update [--check]` | Checks the channel naspeed was installed with (Homebrew tap, `cargo install`, or a downloaded archive) and updates to the latest release, finishing with `naspeed setup` so the `~/.cargo/bin` copy and `ns` alias refresh too. `--check` only reports. |
| `ns cache push <url> [--since-hours H]` | Uploads entries used in the last H hours (default 24) to a peer started with `--allow-push`, objects first, entries last. |

Builds see `NS_VERSION`, `NS_TAG`, `NS_COMMIT` and `SOURCE_DATE_EPOCH`, so a
binary can embed its version with `option_env!("NS_VERSION")`.

## Function-level Rust rebuilds

The opt-in native driver keeps small groups and expensive monomorphized
functions at function granularity. Larger groups of cheap functions are
partitioned into stable shards to avoid flooding LLVM and the linker with
small object files. Rustc reuses unchanged units when another part of the
same crate changes. Cargo still type-checks and links; this mode does not
patch running programs or share incremental objects between worktrees.

From the naspeed checkout, build the driver against its pinned rustc SDK:

```sh
rustup toolchain install 1.99.1 --profile minimal --component rustc-dev
make function-driver FUNCTION_TOOLCHAIN=1.99.1
export RUSTC="$(rustup which --toolchain 1.99.1 rustc)"
export RUSTC_WORKSPACE_WRAPPER="$PWD/target/function-tools/naspeed-rustc-functions"
export CARGO_INCREMENTAL=1
```

In the Rust project, run `ns build`, `ns run`, or `ns test` normally. The
driver only splits eligible native, unoptimized, incremental compile units
without LTO. The driver uses rustc's monomorphized dependency graph to put
the required local inlining copies in each shard. Units with retained or
custom-section statics stay grouped. Release, cross, coverage, and other
unsupported compiler configurations retain native partitioning. If the
`RUSTC` path does not match the driver SDK, it delegates unchanged.

Cargo can nest this workspace wrapper inside naspeed's regular rustc
wrapper. Workspace compiler actions handled by the workspace wrapper use
rustc's local incremental cache, not naspeed's shared crate cache; registry
crates still use the regular wrapper. To turn it off, unset
`RUSTC_WORKSPACE_WRAPPER`, `RUSTC`, and `CARGO_INCREMENTAL` in this shell.

Use `NASPEED_FUNCTION_CODEGEN_REPORT=1 ns build -vv` for compiler-level
diagnostics. `NASPEED_FUNCTION` maps a function to a unit; rustc's
`CGU_REUSE` reports whether its object was emitted (`No`) or reused
(`PostLto`). `make function-driver-test FUNCTION_TOOLCHAIN=1.99.1` verifies
the edit/build/run/test loop against a real compiler.

## Split dependency linking on macOS

With the pinned driver built above, point `NASPEED_SPLIT_LINK_DRIVER` at
the second wrapper from `make function-driver`:

```sh
export NASPEED_SPLIT_LINK_DRIVER="$PWD/target/function-tools/naspeed-rustc-split"
# From the Rust project:
ns run --split-link --bin app -- arg1
```

Put `--split-link` immediately after `run`. Cargo still chooses, builds,
and starts the target with its ordinary arguments, environment, and runner.
For a host, unoptimized Cargo executable with Rust dependencies, naspeed
links the Rust dependency archives into a dylib the first time. After
an edit in the binary crate, that image stays unchanged and only the app
links again. Changes to dependencies can require a new image. Cargo
rebuilds if a referenced image is missing. `NASPEED_SPLIT_LINK_REPORT=1`
reports `built` or `reused` when rustc rebuilds the app.
This mode keeps rustc's native codegen units; it does not enable function
sharding.

`ns dev` picks the driver up automatically for its build runs when it sits
next to the `naspeed` binary, in `~/.config/naspeed/bin/naspeed-rustc-split`,
or in `NASPEED_SPLIT_LINK_DRIVER`; the `.toolchain` file `make
function-driver` writes next to the driver supplies `RUSTC` when the shell
has none.

This is a development mode, not a standalone or distributable binary:
it uses the matching toolchain's dynamic `std` and keeps dependency
images in `.naspeed-split/` under Cargo's build output. Older images remain
available to any binaries that still reference them until `cargo clean`.
Switching between plain and split runs can also leave both variants of
the workspace's Cargo artifacts in the same target directory.
Custom linker flags, optimized profiles, and cross targets use native
linking. Use plain `ns run` for projects that depend on static archive
membership or constructor/link-section ordering. `ns build` and `ns test`
do not select split linking. Unlike lazy execution, this mode still
uses the system linker for the binary and does not patch running processes.

## Lazy Rust execution on Apple Silicon

Build with the pinned Rust SDK above and an LLVM 23 installation with Clang:

```sh
make lazy-driver LLVM_CONFIG=/path/to/llvm-23/bin/llvm-config
export NASPEED_LAZY_RUN_DRIVER="$PWD/target/function-tools/naspeed-rustc-lazy"
# From the Rust project:
ns run --lazy --bin app -- arg1
```

Put `--lazy` immediately after `run`. Cargo still manages dependencies and
invokes its configured runner. The selected dev executable is a launcher:
rustc emits LLVM bitcode, and ORC creates machine code for a function when
it is first called (or reuses the cached object), without `cc`/`ld` producing
an application executable. `NASPEED_LAZY_RUN_REPORT=1` traces materializations
and object cache hits. `make lazy-driver-test` checks actual execution with a
system linker that is deliberately made to fail.

Rust analysis and bitcode emission are not lazy, and Rust library
dependencies are loaded from Cargo's precompiled archives. Building the launcher
or dependencies such as proc macros and build scripts can still use the system
linker. This is not a standalone executable: `current_exe()` identifies the
launcher, and relocatable distribution, native link flags, custom allocators,
Mach-O constructors, optimized profiles, and cross targets are not supported.
Use normal `ns run` if the program needs those semantics. Run generations and
reusable objects are local to Cargo's output tree and removed by `cargo clean`.

### Generate LLVM IR on first call

In a Cargo workspace, put `--lazy-ir` immediately after `run`:

```sh
NASPEED_LAZY_RUN_REPORT=1 ns run --lazy-ir -p app --bin app -- arg1
```

Unlike `--lazy`, this mode does not emit the executable's LLVM IR up front.
A repeat run with all its functions cached starts no rustc process for the
selected executable. New paths request IR as needed. Cargo owns dependency
builds and invalidation. After an edit, rustc reuses a function's existing IR
only if its incremental graph verifies the codegen inputs. Changed Cargo
dependencies or compiler flags use a separate IR cache. Cargo's pre-run
check also reuses rustc's per-body type results where dependencies remain
valid; it still rejects errors in uncalled bodies. `NASPEED_LAZY_RUN_REPORT=1`
and Cargo's `-vv` show `typeck disk hit (cargo)` for results loaded from disk,
`IR reused` for validated IR across compiler sessions, and `IR cache hit`
for IR loaded directly by the launcher. `demand object hit` means the
launcher skips parsing and re-encoding that function's LLVM IR and hands
its cached Mach-O object directly to ORC. `cargo clean` removes these caches.
The selected executable still needs no final system link.

To reuse a prelinked image of Cargo's dependency archives:

```sh
NASPEED_LAZY_DEP_IMAGE=1 NASPEED_LAZY_RUN_REPORT=1 ns run --lazy-ir -vv -p app -- arg1
```

This opt-in uses the pinned `rust-lld` and the macOS SDK on a dependency
cache miss. Changes to the executable alone reuse the image. `dyld` maps
and rebases it; the executable's own functions still use demand ORC.
If the image cannot be prepared or admitted, the launcher uses the
original archives. The image adds disk usage and is removed by `cargo clean`.

To share read-only application code pages between runs of the same build:

```sh
NASPEED_LAZY_APP_IMAGE=1 NASPEED_LAZY_RUN_REPORT=1 ns run --lazy-ir -vv -p app -- arg1
```

This mode takes precedence over the dependency-only image. On a Cargo
rebuild it emits a native object and links a new application dylib, so
it trades first-run and edit cost for fast subsequent launches. `dyld`
maps its file-backed `__TEXT` with ASLR; mutable data stays per process.
This is not live patching: different builds do not share one code arena.
A missing or rejected image uses the original ORC path. `cargo clean`
removes retained generations.

To reuse the same application code pages **across edits** to selected functions:

```sh
NASPEED_LAZY_APP_PATCH=main,handle_request NASPEED_LAZY_RUN_REPORT=1 \
  ns run --lazy-ir -p app -- arg1
```

Select functions by unique Rust names in the selected executable (not
dependencies). Use non-generic functions and `#[inline(never)]` on helpers
you want to keep independently patchable. This experimental dev mode emits
application IR without DWARF. If the base IR, native dependencies, and
imports still match, naspeed links only changed function images and uses the
previous base and unchanged function images. A change to globals, remaining
code, or ABI can require a new base. Unsupported or rejected images use ORC.

By default the launcher binds a process-private dispatch table before `main`.
Old launchers keep their own function revisions until `cargo clean`.

To apply compatible edits to a **running** app, start it with:

```sh
export NASPEED_LAZY_APP_PATCH=handle_request
NASPEED_LAZY_APP_LIVE=1 NASPEED_LAZY_RUN_REPORT=1 ns run --lazy-ir -p app --bin app
```

In another terminal, after each edit:

```sh
NASPEED_LAZY_APP_PATCH=handle_request ns build --lazy-ir -p app --bin app
```

Use the same workspace, target directory, profile, binary and patch list.
The running app watches the build feed for that binary. It maps verified
function images and swaps the whole dispatch table atomically; new calls
use the new code, while old stack frames can finish and unwind. It does not
modify executable pages or jump an active frame into a different body.
Patch a function the app calls repeatedly, not a `main` that never returns.

A compile error, rejected image, or a changed slot layout leaves the
process unchanged. A changed base image is not a rejection: the launcher
maps the rebuilt base alongside the old one and republishes the dispatch
through every table, so selected calls in old and new frames land on the
same revision. Only an ABI or slot-layout change requires a restart.
Each call pins its dispatch revision through return or Rust unwind.
The watcher unloads an older image once no active or current revision
needs it, even if a different old revision is still running. Each base
change leaves one base image mapped for the life of the process; patches
and old revisions are released.
After upgrading the lazy driver or launcher, start a new live session.

Only use live mode with a trusted local Cargo target directory.
`NASPEED_LAZY_RUN_REPORT=1` prints update timing and reclaimed revisions.

To reclaim the corresponding cache files on disk as well:

```sh
NASPEED_LAZY_APP_PATCH=handle_request NASPEED_LAZY_APP_CACHE_KEEP=2 \
  ns build --lazy-ir -p app --bin app
```

On successful builds naspeed keeps the 8 most recent managed patch
generations for the binary plus any generation leased by a running
process, and removes patch images no retained generation or live feed
references. `NASPEED_LAZY_APP_CACHE_KEEP=N` keeps N generations instead;
`off` disables disk collection. If a build or reader already holds the
cache, collection waits for a later build.

Copied launchers outside the retention window can expire. Building the
old source again recreates missing images. Generations written by older
drivers are left alone, as are the IR caches and Cargo's own artifacts.
`cargo clean` still removes the whole target cache.

For a standalone Rust 2024 crate using only the pinned SDK:

```sh
NASPEED_LAZY_RUN_REPORT=1 ns run --lazy-ir src/main.rs -- arg1
```

Modules can live in separate files. Standalone mode has no Cargo target
selection, dependency build, or persistent IR cache.

`--lazy-ir` checks types and borrows before starting the program, so errors
in uncalled functions still prevent execution. The live compiler
monomorphizes and emits IR for an instance at its first call. Aside from
the entry unit and referenced data, it does not generate IR ahead of calls.
Errors that require a concrete monomorphization can surface on demand,
after the program has started.
Native exports, custom linking or allocators, retained items, and static
pointer initializers/TLS are outside this mode's supported contract.

## `ns dev`

`--bin` is repeatable and overrides configured selections. `--all` selects
every project binary and cannot be combined with `--bin`. If neither is
provided, `[dev].bins` is used; legacy `[dev].bin` remains supported. Arguments
after `--` replace configured arguments and require exactly one selected bin.

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

`[dev.apps.<bin>]` also accepts `live`, a list of 1-64 Rust function names to
hot-patch in the running process instead of restarting it:

```toml
[dev.apps.api]
live = ["handle_request"]
```

This uses the same launcher machinery as `ns run --lazy-ir` with
`NASPEED_LAZY_APP_PATCH`/`NASPEED_LAZY_APP_LIVE`: when a build finishes, the
running process applies the new function images itself, without a process
restart, so an eligible edit lands in milliseconds rather than seconds. A
rebuilt base is rebased in place; only a slot-layout or ABI change prints
`naspeed app live rejected` on the app's stderr and `ns dev` falls back to a normal restart.
At most one app per `ns dev` may set `live`, and the demand driver
(`make lazy-driver`) must be available.

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
The same thread then replaces byte-identical files in the profile dirs
with clones (APFS/ReFS/XFS/Btrfs/ZFS volumes only; skipped otherwise).
Beyond `ns dev`, the rustc wrapper itself spawns a detached sweep+dedup
of a profile dir at most once every four hours after cached compilations,
so plain `cargo build` under the wrapper also reclaims space.

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
