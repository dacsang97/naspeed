# Test result cache

`ns test` runs `cargo test` with naspeed as the target runner. A test binary
whose inputs did not change since it last passed replays that run's output
(marked `(cached)` on stderr) instead of running. Failures are never cached.

## What the key covers

Each test-binary run is keyed on:

- the binary's content, its arguments and the cwd;
- `CARGO_PKG_*`, `RUST_TEST_*`, `RUST_MIN_STACK` and the `[test].env` variables;
- the files of the package and of every workspace member it depends on
  (respecting `.gitignore`, without `.git` and the target dir);
- the package's own binaries (what `CARGO_BIN_EXE_*` runs);
- `Cargo.lock`, `.cargo/config.toml` and `.env` files (root and per member,
  even when gitignored);
- `[test].inputs`.

Symlinked fixture dirs are hashed by content. Editing a library re-runs its
tests and those of every crate depending on it; editing only a binary leaves
the library's result cached. Results live in `<store>/tests/` and are trimmed
by `gc` with the same age limit as entries.

## What it cannot see

Reads naspeed cannot see are not tracked: the network, files outside those
inputs, the clock. Use `[test].cache = false` in [`ns.toml`](./ns#ns-toml) for
such a workspace, or `ns test --no-cache` for one run.

- Doctests run under rustdoc rather than the runner, so they always run.
- `--list` and `--bench` runs are never cached.
- If the project already sets a runner for a target (wasm, qemu, a wrapper
  script), naspeed leaves it in place and that target's tests run uncached.
- A test that runs another member's binary without depending on that member is
  not covered: list the binary's crate in `[test].inputs`.
