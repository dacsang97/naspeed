# Getting started

naspeed sits in cargo's `RUSTC_WRAPPER` slot. It stores each compiled registry,
git or workspace crate once per volume, in a content-addressed store, and
clones it copy-on-write into every `target/` dir that needs it. A machine-wide
slot pool also caps how many `rustc` run at once.

## Requirements

- Rust 1.98 or newer (the repo pins `stable` in `rust-toolchain.toml`).
- macOS or Linux. Disk savings need a filesystem with clones: APFS, btrfs or
  xfs. On other filesystems naspeed still speeds builds up but saves no disk
  (see [Limits](./limits)).

## Install and set up

Install the binary with one of:

```sh
brew install dacsang97/tap/naspeed   # macOS / Linux, prebuilt
cargo install naspeed --locked       # from crates.io
cargo build --release                # from a checkout: ./target/release/naspeed
```

Then point cargo at it:

```sh
naspeed setup --store /Volumes/Work/.naspeed
```

`setup` copies the binary into `~/.cargo/bin`, so after `brew upgrade naspeed`
or a new `cargo install`, run `naspeed setup` again to refresh that copy.

`setup` does the following:

1. Copies the binary to `~/.cargo/bin/naspeed` on the internal disk, so cargo
   never points at a volume that might be unplugged. It also creates
   `~/.cargo/bin/ns`, a symlink used by the [ns front-end](./ns).
2. Backs up `~/.cargo/config.toml` into `~/.config/naspeed/backups/`. The first
   backup, `config.toml.before-naspeed`, is never overwritten.
3. Edits the config with comments kept: sets `build.rustc-wrapper` to the
   installed binary and `build.jobs = 4`, and removes `SCCACHE_*` from `[env]`.
   `build.incremental = false` stays unless you pass `--enable-incremental`.
   `--deps-debug line-tables-only` writes
   `[profile.dev.package."*"] debug = "line-tables-only"`, which makes
   dependencies smaller and faster to compile. Backtraces into dependencies keep
   `file:line`, but a debugger can no longer inspect their local variables.
4. Creates the store and records it in `~/.config/naspeed/config.toml`. The
   store must be on the same volume as your projects, because clones cannot
   cross volumes. Without `--store`, setup uses `<volume of cwd>/.naspeed`.
5. Prints any `RUSTC_WRAPPER` found in your environment or shell rc files. An
   env `RUSTC_WRAPPER` overrides the config and bypasses naspeed, so remove
   those lines yourself; naspeed never edits rc files.

Running `setup` again is safe. After setup, every `cargo` call goes through
naspeed, whether it comes from a shell, an IDE, rust-analyzer or an agent.

## Check that it works

```sh
naspeed status          # wrapper in effect, store entries, hit rate
naspeed status --check  # also builds a tiny crate and confirms naspeed wraps rustc
naspeed stats           # time saved by cache hits
```

To get the full disk saving on existing projects, rebuild them through naspeed
(`cargo clean`, then build): artifacts built before naspeed differ between
target dirs.

## Cross builds

```sh
naspeed setup --cross                         # zig shims for Linux gnu/musl and windows-gnu
naspeed setup --cross --accept-msvc-license   # also the msvc SDK
```

Ordinary targets stay transparent through `cargo build --target`. Only
universal2 and pinned glibc go through `naspeed build`; see
[Commands](./commands).

## Uninstall

```sh
naspeed uninstall          # restores each config key it changed; keeps later edits of yours
naspeed uninstall --purge  # also deletes the store
```
