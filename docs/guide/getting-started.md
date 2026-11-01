# Getting started

naspeed sits in cargo's `RUSTC_WRAPPER` slot. It stores each compiled registry,
git or workspace crate once per volume, in a content-addressed store, and
clones it copy-on-write into every `target/` dir that needs it. A machine-wide
slot pool also caps how many `rustc` run at once.

## Requirements

- Rust 1.98 or newer (the repo pins `stable` in `rust-toolchain.toml`).
- macOS, Linux or Windows. Disk savings need a filesystem with clones: APFS,
  btrfs, xfs, or ReFS (a Windows [Dev Drive](https://learn.microsoft.com/windows/dev-drive/)).
  On other filesystems (ext4, NTFS) naspeed still speeds builds up but saves
  no disk (see [Limits](./limits)).

## Install and set up

Install the binary with one of:

```sh
brew install dacsang97/tap/naspeed   # macOS / Linux, prebuilt
cargo install naspeed --locked       # from crates.io (any OS)
cargo build --release                # from a checkout: ./target/release/naspeed
```

Then point cargo at it:

```sh
naspeed setup                        # store in ~/.naspeed
```

The store must be on the same volume as your projects. If they live on another
volume, pass `--store` with a dir on that volume:

```sh
naspeed setup --store /Volumes/Work/.naspeed   # macOS: projects under /Volumes/Work
naspeed setup --store /mnt/data/.naspeed       # Linux: projects under /mnt/data
naspeed setup --store D:\.naspeed              # Windows: projects on the D: Dev Drive
```

Run it once per volume to add more stores; each `setup` keeps the earlier ones.

`setup` copies the binary into `~/.cargo/bin`, so after `brew upgrade naspeed`
or a new `cargo install`, run `naspeed setup` again to refresh that copy.

`setup` does the following:

1. Copies the binary to `~/.cargo/bin/naspeed` on the internal disk, so cargo
   never points at a volume that might be unplugged. It also creates
   `~/.cargo/bin/ns`, a symlink used by the [ns front-end](./ns). On Windows
   these are `naspeed.exe` and `ns.exe` (a hard link, since symlinks need
   Developer Mode) under `%USERPROFILE%\.cargo\bin`.
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
   cross volumes. Without `--store`, setup uses `~/.naspeed`; run from a dir
   on another volume (say `/Volumes/Work`), it uses `<that volume>/.naspeed`
   instead. Run `setup` once per volume to add a store for each.
5. Prints any `RUSTC_WRAPPER` found in your environment or shell rc files. An
   env `RUSTC_WRAPPER` overrides the config and bypasses naspeed, so remove
   those lines yourself; naspeed never edits rc files.

Running `setup` again is safe. After setup, every `cargo` call goes through
naspeed, whether it comes from a shell, an IDE, rust-analyzer or an agent.

### Windows

Install Rust with rustup and the MSVC toolchain first. Then, in PowerShell:

```powershell
irm https://dacsang97.github.io/naspeed/install.ps1 | iex
```

The [script](https://github.com/dacsang97/naspeed/blob/main/docs/public/install.ps1)
downloads the latest release zip for your CPU (x86_64 or ARM64) into a temp
folder, checks its SHA-256, runs `naspeed.exe setup` and removes the folder. To
put the store on the drive that holds your code, set `NASPEED_STORE` on the same
line (and `NASPEED_VERSION = "v0.5.0"` to pin a release):

```powershell
$env:NASPEED_STORE = "D:\.naspeed"; irm https://dacsang97.github.io/naspeed/install.ps1 | iex
```

Paste each command as one line: some consoles run a multi-line paste out of
order.

Without the script: `cargo install naspeed --locked` then `naspeed setup`, or
unzip `naspeed-<tag>-<arch>-pc-windows-msvc.zip` from the
[releases](https://github.com/dacsang97/naspeed/releases) and run
`naspeed-<tag>-<arch>-pc-windows-msvc\naspeed.exe setup`.

`setup` copies `naspeed.exe` and `ns.exe` into `%USERPROFILE%\.cargo\bin`, so
the unzipped folder can be deleted afterwards. Without `--store`, the store goes
to `%USERPROFILE%\.naspeed`, or to `<drive>:\.naspeed` when setup runs from a
folder on another drive. Run setup once per drive that holds projects.

On NTFS naspeed speeds builds up but copies instead of cloning, so it saves no
disk; keep code and store on a [Dev Drive](https://learn.microsoft.com/windows/dev-drive/)
(ReFS) to save disk as well. To upgrade, install the new release and run
`naspeed setup` again. Build-script run caching, `ns test` runtime input
tracking and `setup --cross` are not available on Windows (see [Limits](./limits)).

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
