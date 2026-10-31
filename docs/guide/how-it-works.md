# How it works

## Which calls are cached

On each call, cargo runs `naspeed rustc <args>`. For registry and git crates, a
call is cached only when all of the following hold:

- the input is under `$CARGO_HOME/registry/src` or `$CARGO_HOME/git/checkouts`;
- the crate type is `lib`, `rlib` or `proc-macro`, or the call compiles the
  crate's build script (`build_script_build`, `bin`);
- `--emit` contains only `link`, `metadata` or `dep-info`;
- there is no `-C incremental`, `--test`, `-Z`, `--print`, stdin input or
  unknown flag;
- `CARGO_PRIMARY_PACKAGE` is not set and no unusual `RUSTC_*` variable is set.

Workspace crates and path dependencies follow their own rules,
[below](#workspace-crates-and-path-dependencies). Every other call is passed
through to `rustc`.

## The cache key

The key is built in two tiers:

- **Tier 1** is a blake3 hash of: the toolchain (`rustc -vV` plus the sysroot
  resolved behind the rustup proxy); the normalized arguments; the package
  source tree; each `--extern` artifact's identity; the build script's
  `OUT_DIR` and native libs; `CARGO_*` variables; `RUSTFLAGS`; and variables
  that change object files without showing up in dep-info, such as
  `MACOSX_DEPLOYMENT_TARGET` and `SDKROOT`. `CC*`/`CFLAGS*` are not part of it:
  rustc does not read them, and C code reaches an rlib only through `OUT_DIR`.
  Paths under the profile dir and `$CARGO_HOME` are replaced with placeholders,
  so two target dirs produce the same key.
- **Tier 2** adds the current values of the `# env-dep:` variables that rustc
  reported the first time it compiled the crate.

An `--extern` artifact's identity is the blake3 hash of its content, memoized
in the xattr `user.naspeed.id`. Every restored artifact is a clone of one store
object, so a dependency has the same identity in every target dir.

## Hit and miss

On a **hit**, naspeed clones the outputs into `--out-dir` and makes them
writable (`0644`), so rustc can overwrite them on a later rebuild. It then sets
their mtime to now, rewrites the `.d` file and replays rustc's output,
path-rewritten. No `rustc` runs and no slot is taken.

On a **miss**, naspeed holds a per-key lock, so a second build of the same
crate waits and then hits. It takes a slot and runs `rustc`, forwarding stdout
and stderr line by line so cargo's pipelining still sees the `rmeta`
notification right away. It then clones the outputs into the store.

## Workspace crates and path dependencies

These are cached too (turn this off with `workspace_cache = false` or
`NASPEED_WORKSPACE_CACHE=0`). Their binaries, tests and build scripts are never
cached. Local sources change, so the tier-1 key leaves them out. Tier 2 instead
hashes every file listed in the `.d` file from the last compile:

- the set of files rustc reads is decided by the content of those files, so an
  edit that pulls in a new file (`mod x;`, `include!`) changes a listed file and
  misses;
- a file edited while rustc runs is not stored;
- `-C incremental` still runs on a miss; a hit leaves the incremental dir
  untouched.

To let other checkouts share these artifacts, naspeed adds
`--remap-path-prefix <workspace>=/naspeed/workspace` and
`<profile dir>=/naspeed/target` for these crates. The trade-offs:

- a debugger shows those prefixes for workspace sources (lldb:
  `settings set target.source-map /naspeed/workspace <checkout>`);
- panic messages are unaffected, because cargo passes relative paths;
- a proc-macro that reads files rustc does not list (for example sqlx offline
  data) is not tracked, just as cargo itself does not rebuild for them.

### Incremental warm-up

A restored crate has no `-C incremental` session, so without help the first
edit in a fresh worktree recompiles the edited crate and its dependents in
full. After a hit on a workspace crate built with incremental, naspeed starts
the same rustc call detached, at low priority and within the machine slots,
with `--out-dir` pointed at a throwaway dir. It leaves a finalized session
behind, and cargo never sees it.

On a 12-crate workspace the first edit of a low-level crate after a fresh
worktree went from 29 s to about 15 s (cargo without naspeed: 10.5 s), and
later edits match cargo. `warm_incremental = false` turns this off.

## Build-script runs

Cargo runs build scripts itself, not through `RUSTC_WRAPPER`. After naspeed
compiles (or restores) a registry crate's build script, it moves the binary
aside to `<name>.naspeed-real` and puts a short shell launcher in its place.
Cargo copies and runs the launcher, which calls `naspeed __build-script`.

The run's key covers:

- the script binary;
- the package sources;
- the toolchain;
- the host C toolchain (`cc --version` and the identity of `cc`, `cmake`,
  `make` and similar tools on `PATH`);
- cargo's build-script environment: `TARGET`, `PROFILE`, features,
  `CARGO_CFG_*` and `DEP_*`;
- `CC*`, `CFLAGS*`, `PKG_CONFIG*` and `CMAKE*`;
- the content of every `DEP_*` directory in the profile;
- in a second tier, every `rerun-if-env-changed` variable and
  `rerun-if-changed` path that the script declared on its first run.

On a hit, naspeed empties `OUT_DIR`, restores the stored tree as clones,
rewrites small text files that name the target dir, and replays the output. On
a miss, the script runs in an empty `OUT_DIR`, exactly as in a fresh target
dir. To turn this off, set `build_script_cache = false` in the config or
`NASPEED_BUILD_SCRIPT_CACHE=0`.

A script that reads a variable or file without declaring it is not re-run by
cargo either. Across target dirs, naspeed reuses the result just as cargo would
within one target dir.

## Machine-wide slots

Each `rustc` that really runs, whether passed through or a miss, holds one of N
`flock`ed slot files in `~/.config/naspeed/slots/`. The default N is the number
of logical cores. A killed process releases its slot automatically. Before
starting, naspeed waits up to 30 seconds while available memory is below
`min_free_mem_mb` (default 1536). Available memory means free + inactive +
purgeable pages on macOS, and `MemAvailable` on Linux.

rustc still honors cargo's own jobserver. naspeed's slots are an extra,
machine-level limit. Build scripts (for example the `cc` crate compiling C) and
rustc's codegen threads take tokens from cargo's jobserver, not from naspeed
slots. Keep `build.jobs` moderate for that reason.
