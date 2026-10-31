# naspeed

A transparent `RUSTC_WRAPPER` for cargo. naspeed stores each compiled registry or git crate once per volume, in a content-addressed store, and clones it copy-on-write into every `target/` dir that needs it. A machine-wide slot pool also caps how many `rustc` run at once. No daemon, no server, no `-Z` flags.

- **Disk:** on APFS (`clonefile`) and btrfs/xfs (`FICLONE`), a restored crate takes almost no extra disk space. Ten worktrees of one repo share their dependencies' blocks.
- **Speed:** a new worktree or project gets every dependency it shares with an earlier build straight from the store, including the *results* of their build scripts (the C that jemalloc-sys, aws-lc-sys or ring compile).
- **Workspace crates:** your own crates are cached too, keyed on the exact source files rustc read. A second worktree, a branch switch back, or another agent at the same commit reuses them.
- **Safety:** if naspeed is unsure about a call, or anything fails, it runs `rustc` (or the build script) unchanged.

## Install and set up

```sh
cargo build --release
./target/release/naspeed setup --store /Volumes/Work/.naspeed
```

`setup` does the following:

1. Copies the binary to `~/.cargo/bin/naspeed` on the internal disk, so cargo never points at a volume that might be unplugged.
2. Backs up `~/.cargo/config.toml` into `~/.config/naspeed/backups/`. The first backup, `config.toml.before-naspeed`, is never overwritten.
3. Edits the config with comments kept: sets `build.rustc-wrapper` to the installed binary and `build.jobs = 4`, and removes `SCCACHE_*` from `[env]`. `build.incremental = false` stays unless you pass `--enable-incremental`. `--deps-debug line-tables-only` writes `[profile.dev.package."*"] debug = "line-tables-only"`, which makes dependencies smaller and faster to compile. Backtraces into dependencies keep `file:line`, but a debugger can no longer inspect their local variables.
4. Creates the store and records it in `~/.config/naspeed/config.toml`. The store must be on the same volume as your projects, because clones cannot cross volumes. Without `--store`, setup uses `<volume of cwd>/.naspeed`.
5. Prints any `RUSTC_WRAPPER` found in your environment or shell rc files. An env `RUSTC_WRAPPER` overrides the config and bypasses naspeed, so remove those lines yourself; naspeed never edits rc files.

Running `setup` again is safe. After setup, every `cargo` call goes through naspeed, whether it comes from a shell, an IDE, rust-analyzer or an agent.

## Commands

| Command | What it does |
|---|---|
| `naspeed status` | Wrapper in effect, slot use, store entries, logical vs unshared bytes, 7-day hit rate, registered target dirs |
| `naspeed stats [--days N] [--json]` | Time saved: compile time of every rustc/build-script hit minus its restore time, test runs replayed by `ns test` and their run time, entries fetched from peers, and the crates that saved the most |
| `naspeed status --check` | Also builds a tiny crate with `cargo build -v` and confirms naspeed wraps `rustc` |
| `naspeed gc [--max-size-gb N] [--max-age-days D]` | Evicts store entries least-recently-used first until the store is under the cap (default 20 GB), and drops entries unused for D days (default 14). Runs on its own in the background at most once a day; `NASPEED_NO_AUTO_GC=1` turns that off |
| `naspeed gc --targets` | Removes stale `-<hash>` variants and old incremental sessions from registered target dirs |
| `naspeed gc --dedup` | Replaces byte-identical files of 64 KiB or more across target dirs with clones |
| `naspeed purge --target <dir>` | Deletes the artifacts naspeed restored into a target dir, plus their fingerprints |
| `naspeed build --target universal2-apple-darwin` | Builds both macOS architectures, then runs `lipo -create` |
| `naspeed build --target x86_64-unknown-linux-gnu.2.17` | Links against glibc 2.17 through zig, in a separate target dir |
| `naspeed setup --cross` | Installs zig linker shims for Linux gnu/musl and windows-gnu, and the msvc SDK if `--accept-msvc-license` is given |
| `naspeed uninstall [--purge]` | Undoes exactly the keys setup changed; `--purge` also deletes the store |

Every `gc` mode takes `--dry-run`, and `--exclude <dir>` (repeatable) to leave a project you are actively developing alone.

## `ns`: dev loop, release, images and CI

`setup` also creates `~/.cargo/bin/ns`, a symlink to naspeed. Through `ns`, the binary works as a project front-end that drives cargo for the dev loop, releases, container images and CI. It does not replace cargo: `cargo`, rust-analyzer and IDEs work as before and still go through the cache. Invoked as `ns`, the binary never enters wrapper mode, so a typo like `ns rustc` errors out instead of compiling.

| Command | What it does |
|---|---|
| `ns dev [--bin B] [--test] [-- args]` | Watches the workspace (respecting `.gitignore`, always skipping `target*` and `.git`), runs `cargo check` on each change, then builds and restarts the binary. A compile error keeps the old process serving. The old process group is stopped (SIGTERM, then SIGKILL after 5 s) and `dev.ports` are free before the new one starts. Changes during a build trigger one more cycle. A change to `.env` only restarts. `--test` runs `cargo test -p` for the changed crates and every workspace crate that depends on them. Ctrl-C cleans up the whole process group. |
| `ns release [--snapshot] [--publish] [--target T]…` | Runs the `before` hooks, then builds every build variant × target (zig, xwin, universal2 and pinned glibc come from naspeed's cross setup). It writes reproducible archives, a checksum file, one SPDX SBOM per build, and `metadata.json`. Signing uses keyless cosign when it is available. `--publish` checks for a clean tree and a pushed tag, then creates the GitHub Release (draft and pre-release flags, filtered changelog) and uploads the assets. |
| `ns image [--push] [--registry R] [--tag T]…` | Turns the release binaries into a multi-arch OCI image on distroless `:nonroot` and pushes it through the registry HTTP API, with no Docker daemon. Base layers are referenced, or mounted or copied server-side, never rebuilt. The SBOM is attached as `sha256-<digest>.sbom`. Without `--push` it only writes `<dist>/oci` and prints what it would push. |
| `ns ci init [--force]` | Writes `.github/workflows/ns-ci.yml` (fmt, clippy, `ns test` on Linux and macOS), `ns-build.yml` (a snapshot release on main that warms the release cache) and `ns-release.yml` (on `v*` tags: release, then image). The naspeed store is carried in the GitHub Actions cache. Pull requests only restore it; only pushes to main save it. Each workflow gets the minimal permissions it needs. |
| `ns test [--no-cache] [cargo test args] [-- test args]` | Runs `cargo test` with naspeed as the target runner; a test binary whose inputs did not change since it last passed replays that run's output (marked `(cached)` on stderr) instead of running. Failures are never cached. `ns dev --test` goes through the same cache. See [Test result cache](#test-result-cache). |
| `ns cache export <dir> [--max-gb N]` / `ns cache import <dir>` | Copies the most recently used store entries (and cached test results) to or from a plain directory, which is what `actions/cache` saves. Import re-hashes every object and skips what the store already has. |
| `ns cache serve [--listen ADDR] [--allow-push]` | Serves this machine's store to other machines over HTTP, by default on its Tailscale IPv4 (`tailscale ip -4`), port 7878. Read-only unless `--allow-push`, which needs a token. See [Peer cache over Tailscale](#peer-cache-over-tailscale). |
| `ns cache push <url> [--since-hours H]` | Uploads entries used in the last H hours (default 24) to a peer started with `--allow-push`, objects first, entries last. |

Builds see `NS_VERSION`, `NS_TAG`, `NS_COMMIT` and `SOURCE_DATE_EPOCH`, so a binary can embed its version with `option_env!("NS_VERSION")`.

### `ns.toml`

Optional; without it, defaults come from `cargo metadata`. Unknown keys are errors.

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

Defaults for `[image]`: the binary goes to `/usr/bin/{binary}`, runs as user `65532:65532`, and sits on `gcr.io/distroless/static-debian12:nonroot` (musl) or `cc-debian12:nonroot` (gnu). Labels `org.opencontainers.image.{title,version,revision,source,created}` are set.

### Test result cache

`ns test` keys each test-binary run on: the binary's content, its arguments, the cwd, `CARGO_PKG_*`, `RUST_TEST_*`, `RUST_MIN_STACK` and the `[test].env` variables, the files of the package and of every workspace member it depends on (respecting `.gitignore`, without `.git` and the target dir), the package's own binaries (what `CARGO_BIN_EXE_*` runs), `Cargo.lock`, `.cargo/config.toml` and `.env` files (root and per member, even when gitignored) and `[test].inputs`. Symlinked fixture dirs are hashed by content. Editing a library re-runs its tests and those of every crate depending on it; editing only a binary leaves the library's result cached. Results live in `<store>/tests/` and are trimmed by `gc` with the same age limit as entries.

Reads naspeed cannot see are not tracked: the network, files outside those inputs, the clock. Use `[test].cache = false` for such a workspace, or `ns test --no-cache` for one run. Doctests run under rustdoc rather than the runner, so they always run. `--list` and `--bench` runs are never cached. If the project already sets a runner for a target (wasm, qemu, a wrapper script), naspeed leaves it in place and that target's tests run uncached. A test that runs another member's binary without depending on that member is not covered: list the binary's crate in `[test].inputs`.

### Peer cache over Tailscale

A store can serve other machines, so a laptop gets what a desktop or a build VPS already compiled:

```sh
# on the machine with the warm store (binds its Tailscale IP, port 7878)
ns cache serve

# on the others, in ~/.config/naspeed/config.toml
remotes = ["http://desktop.tailnet-name.ts.net:7878"]
```

On a local miss the wrapper asks each peer, in order, for the entry. It verifies every object's blake3 hash and writes everything into the local store, then restores as from a local hit. The next build on that machine needs no network. A peer that refuses or times out (`NASPEED_REMOTE_CONNECT_MS`, default 800) is skipped by every wrapper for 60 s, so being off the tailnet costs one timeout, not one per crate. Keys are path-independent, so peers share entries across checkout paths and user names. They must have the same toolchain and target, because a different `rustc` simply misses.

For a shared cache on a VPS, start it with `ns cache serve --allow-push` and a token, then upload from CI or a desktop with `ns cache push http://vps:7878`. The token is the first line of `~/.config/naspeed/remote.token` (`remote_token_file` in the config) or `NASPEED_REMOTE_TOKEN`. When set, every request must send `Authorization: Bearer <token>`. The same token goes to every configured remote over plain HTTP, which is fine inside a tailnet (WireGuard-encrypted) but not across the internet.

Trust: names are checked to be hex, entries must parse and match their key, output paths must stay inside the out dir and must not pass through the entry's own symlinks, file modes are limited to plain permission bits, manifests may only name files inside the build, and objects are hash-verified. A peer that stalls is cut off after a few seconds and treated as down. No one can check that an rlib really matches its key, though, so only add peers you would also let build your code: your own machines on your tailnet. Serving binds the Tailscale address unless `--listen` says otherwise. Avoid `0.0.0.0` outside a private network.

## How it works

On each call, cargo runs `naspeed rustc <args>`. A call is cached only when all of the following hold:

- the input is under `$CARGO_HOME/registry/src` or `$CARGO_HOME/git/checkouts`;
- the crate type is `lib`, `rlib` or `proc-macro`, or the call compiles the crate's build script (`build_script_build`, `bin`);
- `--emit` contains only `link`, `metadata` or `dep-info`;
- there is no `-C incremental`, `--test`, `-Z`, `--print`, stdin input or unknown flag;
- `CARGO_PRIMARY_PACKAGE` is not set and no unusual `RUSTC_*` variable is set.

Every other call is passed through to `rustc`.

The key is built in two tiers:

- **Tier 1** is a blake3 hash of: the toolchain (`rustc -vV` plus the sysroot resolved behind the rustup proxy); the normalized arguments; the package source tree; each `--extern` artifact's identity; the build script's `OUT_DIR` and native libs; `CARGO_*` variables; `RUSTFLAGS`; and variables that change object files without showing up in dep-info, such as `MACOSX_DEPLOYMENT_TARGET` and `SDKROOT`. `CC*`/`CFLAGS*` are not part of it: rustc does not read them, and C code reaches an rlib only through `OUT_DIR`. Paths under the profile dir and `$CARGO_HOME` are replaced with placeholders, so two target dirs produce the same key.
- **Tier 2** adds the current values of the `# env-dep:` variables that rustc reported the first time it compiled the crate.

### Workspace crates and path dependencies

A restored crate has no `-C incremental` session, so without help the first edit in a fresh worktree recompiles the edited crate and its dependents in full. After a hit on a workspace crate built with incremental, naspeed starts the same rustc call detached, at low priority and within the machine slots, with `--out-dir` pointed at a throwaway dir. It leaves a finalized session behind, and cargo never sees it. On a 12-crate workspace the first edit of a low-level crate after a fresh worktree went from 29 s to about 15 s (cargo without naspeed: 10.5 s), and later edits match cargo. `warm_incremental = false` turns this off.


These are cached too (turn this off with `workspace_cache = false` or `NASPEED_WORKSPACE_CACHE=0`). Their binaries, tests and build scripts are never cached. Local sources change, so the tier-1 key leaves them out. Tier 2 instead hashes every file listed in the `.d` file from the last compile:

- the set of files rustc reads is decided by the content of those files, so an edit that pulls in a new file (`mod x;`, `include!`) changes a listed file and misses;
- a file edited while rustc runs is not stored;
- `-C incremental` still runs on a miss; a hit leaves the incremental dir untouched.

To let other checkouts share these artifacts, naspeed adds `--remap-path-prefix <workspace>=/naspeed/workspace` and `<profile dir>=/naspeed/target` for these crates. The trade-offs:

- a debugger shows those prefixes for workspace sources (lldb: `settings set target.source-map /naspeed/workspace <checkout>`);
- panic messages are unaffected, because cargo passes relative paths;
- a proc-macro that reads files rustc does not list (for example sqlx offline data) is not tracked, just as cargo itself does not rebuild for them.

An `--extern` artifact's identity is the blake3 hash of its content, memoized in the xattr `user.naspeed.id`. Every restored artifact is a clone of one store object, so a dependency has the same identity in every target dir.

On a **hit**, naspeed clones the outputs into `--out-dir` and makes them writable (`0644`), so rustc can overwrite them on a later rebuild. It then sets their mtime to now, rewrites the `.d` file and replays rustc's output, path-rewritten. No `rustc` runs and no slot is taken.

On a **miss**, naspeed holds a per-key lock, so a second build of the same crate waits and then hits. It takes a slot and runs `rustc`, forwarding stdout and stderr line by line so cargo's pipelining still sees the `rmeta` notification right away. It then clones the outputs into the store.

## Build-script runs

Cargo runs build scripts itself, not through `RUSTC_WRAPPER`. After naspeed compiles (or restores) a registry crate's build script, it moves the binary aside to `<name>.naspeed-real` and puts a short shell launcher in its place. Cargo copies and runs the launcher, which calls `naspeed __build-script`.

The run's key covers:

- the script binary;
- the package sources;
- the toolchain;
- the host C toolchain (`cc --version` and the identity of `cc`, `cmake`, `make` and similar tools on `PATH`);
- cargo's build-script environment: `TARGET`, `PROFILE`, features, `CARGO_CFG_*` and `DEP_*`;
- `CC*`, `CFLAGS*`, `PKG_CONFIG*` and `CMAKE*`;
- the content of every `DEP_*` directory in the profile;
- in a second tier, every `rerun-if-env-changed` variable and `rerun-if-changed` path that the script declared on its first run.

On a hit, naspeed empties `OUT_DIR`, restores the stored tree as clones, rewrites small text files that name the target dir, and replays the output. On a miss, the script runs in an empty `OUT_DIR`, exactly as in a fresh target dir. To turn this off, set `build_script_cache = false` in the config or `NASPEED_BUILD_SCRIPT_CACHE=0`.

A script that reads a variable or file without declaring it is not re-run by cargo either. Across target dirs, naspeed reuses the result just as cargo would within one target dir.

## Machine-wide slots

Each `rustc` that really runs, whether passed through or a miss, holds one of N `flock`ed slot files in `~/.config/naspeed/slots/`. The default N is the number of logical cores. A killed process releases its slot automatically. Before starting, naspeed waits up to 30 seconds while available memory is below `min_free_mem_mb` (default 1536). Available memory means free + inactive + purgeable pages on macOS, and `MemAvailable` on Linux.

rustc still honors cargo's own jobserver. naspeed's slots are an extra, machine-level limit. Build scripts (for example the `cc` crate compiling C) and rustc's codegen threads take tokens from cargo's jobserver, not from naspeed slots. Keep `build.jobs` moderate for that reason.

## Configuration

`~/.config/naspeed/config.toml`:

```toml
stores = ["/Volumes/Work/.naspeed"]
slots = 8
min_free_mem_mb = 1536
store_cap_gb = 20
store_max_age_days = 14
build_script_cache = true
workspace_cache = true
cow_mode = "auto"   # auto | copy | hardlink (hardlink: opt-in, objects stay read-only)
warm_incremental = true  # rebuild incremental state of restored workspace crates in the background
remotes = []        # peer stores, e.g. ["http://desktop.tailnet-name.ts.net:7878"]
remote_token_file = "~/.config/naspeed/remote.token"
```

These environment variables override the file:

| Variable | Effect |
|---|---|
| `NASPEED_STORE` | Use this store |
| `NASPEED_SLOTS` | Number of slots |
| `NASPEED_MIN_FREE_MEM_MB` | Memory threshold |
| `NASPEED_COW_MODE` | Copy mode |
| `NASPEED_CONFIG_DIR` | Config directory |
| `NASPEED_DISABLE=1` | Pass every call through |
| `NASPEED_BUILD_SCRIPT_CACHE=0` | Run build scripts uncached |
| `NASPEED_WORKSPACE_CACHE=0` | Do not cache workspace crates |
| `NASPEED_NO_AUTO_GC=1` | No daily background trim |
| `NASPEED_WARM_INCREMENTAL=0` | No background incremental warm-up after a hit |
| `NASPEED_REMOTES=url,url` | Peer stores for this shell (empty: none) |
| `NASPEED_REMOTE_TOKEN` | Token for peers and `serve` |
| `NASPEED_REMOTE_CONNECT_MS` | Connect timeout per peer (default 800) |
| `NASPEED_LOG=naspeed=debug` | Turn on logging |
| `NASPEED_LOG_FILE=<path>` | Write logs to this file |

## Limits

- `du` counts every clone at full size. Use `naspeed status` (unshared bytes) or `df` instead.
- Artifacts built before naspeed differ between target dirs, because rmeta embeds `OUT_DIR`. The real disk saving comes from rebuilding through naspeed (`cargo clean`, then build). `gc --dedup` mainly recovers identical build-script output.
- `NASPEED_DISABLE=1` affects only later builds. Cargo does not fingerprint the wrapper, so restored artifacts stay `Fresh`. To drop them, use `naspeed purge --target <dir>` or `cargo clean`.
- If the store volume is missing or read-only, every call passes through.
- On a volume without clone support (ext4, other filesystems), naspeed still speeds builds up but saves no disk.
- The msvc target needs LLVM (`clang-cl`, `lld-link`, `llvm-lib`) and `xwin`. The Microsoft CRT and SDK are only downloaded after `--accept-msvc-license`, into `<store>/xwin`.

## Uninstall

```sh
naspeed uninstall          # restores each config key it changed; keeps later edits of yours
naspeed uninstall --purge  # also deletes the store
```

## License

Apache-2.0. Third-party notices are in [NOTICE](NOTICE).
