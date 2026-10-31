# Test result cache

`ns test` runs `cargo test` with naspeed as the target runner. A test binary
whose inputs did not change since it last passed replays that run's output
(marked `(cached)` on stderr) instead of running. Failures are never cached.

A cached pass has to be trustworthy, so naspeed tracks a test's inputs in
two layers: what it can see before the run, and what the test actually reads
while it runs.

## The key: what is known before the run

Each test-binary run is keyed on:

- the binary's content, its arguments and the cwd;
- `CARGO_PKG_*`, `RUST_TEST_*` and the `[test].env` variables;
- every environment variable the sources name: `env::var("X")`, `var_os`,
  `option_env!`, `env!` and `dotenvy::var` in the package and the workspace
  members it depends on (Cargo's own build-time variables excepted);
- the files of the package and of every workspace member it depends on
  (respecting `.gitignore`, without `.git` and the target dir);
- every binary of the workspace in the profile dir, since an integration test
  may run any of them (`CARGO_BIN_EXE_*`, or a path into `target/`);
- `Cargo.lock`, `.cargo/config.toml` and `.env` files (root and per member,
  even when gitignored);
- `[test].inputs`.

Symlinked fixture dirs are hashed by content. Editing a library re-runs its
tests and those of every crate depending on it; editing only a binary leaves
the library's result cached. Results live in `<store>/tests/` and are trimmed
by `gc` with the same age limit as entries.

## What the test reads: recorded at run time

Like `go test`, which learns from the `os` package which variables and files
a test looked at, naspeed records a test's runtime inputs. A small tracer
library (shipped inside naspeed) is inserted into the test binary
(`DYLD_INSERT_LIBRARIES` on macOS, `LD_PRELOAD` on Linux) and logs every:

- environment variable read (`getenv`);
- file opened for reading, path stat'ed or probed (`open`, `stat`,
  `statx`, `access`), directory listed (`opendir`);
- program started (`execve`, `posix_spawn`), including the `PATH` entries
  searched for a bare name.

After a passing run naspeed snapshots those inputs: content hashes of files,
existence of probed paths, the names in listed directories, and hashes of
variable values (never the values themselves). The pass replays only while
every recorded input is unchanged, so a test that reads `shared/config.toml`
from the workspace root, a `$HOME` file or `TEST_DATABASE_URL` re-runs when
that changes, and a test that reads none of them is not disturbed by edits
to them.

Paths that cannot change a verdict or are already covered are left out:
system libraries, temp dirs, the target dir, the cargo registry and git
checkouts (pinned by `Cargo.lock`), the rustup toolchains and terminal
capability lookups.

When a run cannot be traced (a statically linked test binary), or a test
starts a program the tracer cannot follow (macOS strips `DYLD_*` from
SIP-protected programs such as `/bin/sh`), the pass is also keyed on every
workspace file outside the packages, minus `[test].exclude`. That stays
correct but re-runs on any such edit.
`NASPEED_TEST_TRACE=0` or `[test].trace = false` turns tracing off.

## Every failure in one run

Plain `cargo test` stops after the first test binary that fails, so the
binaries after it never run. `ns test` passes `--no-fail-fast`: binaries that
passed before replay almost for free, so one run reports every failure.
`ns test --fail-fast` keeps cargo's behaviour.

The closing summary also names the most common reasons binaries ran, for
example `why ran  reasons=env:DATABASE_URL (50), no earlier run (3)`. A run
started from a Makefile that exports `.env` and a run from a bare shell see
different environments, and this line makes that visible without
`--explain`.

## Why a test ran: `--explain`

```sh
ns test --explain -p app
```

prints, for every test binary, why it replayed (`checked=7 recorded inputs`)
or ran:

```text
• running app-0e1f2c…   why=key changed: env:PROBE_MODE
• running app-0e1f2c…   why=file /repo/shared/value.txt changed
• running app-0e1f2c…   why=key changed: binary, dir:core
```

## Run summaries: `--summarize`

```sh
ns test --summarize
```

writes `<target>/naspeed/test-runs/<id>.json`: totals (replayed, passed,
failed, time saved) and, for every test binary, its package, key, the hashed
parts of the key, whether it replayed, passed or failed and why, and the
inputs recorded for it. Two summaries can be diffed to see what changed
between runs, like `turbo run --summarize`.

## Hashing cost

File hashes are remembered by stat, the way git's index works:
`(path, size, mtime, inode) → hash` in `<store>/memo/test-file-hashes.tsv`.
A file whose stat is unchanged is not read again, so keying a test binary
costs a walk and a `stat` per file. A file modified within the last two
seconds is hashed but not remembered, since a second edit could keep its
mtime. On naspeed's own unit tests a replay went from 1.4 s to 0.2 s.

## Configuration

```toml
[test]
cache = true                 # false: always run
env = ["DATABASE_URL"]       # variables to key on beyond those found in the sources
inputs = ["testdata"]        # extra files/dirs (relative to the root) to key on
trace = true                 # record runtime inputs
exclude = ["docs/**"]        # left out of the workspace files of untraced runs
```

## What it cannot see

- The network, databases and other services, and the clock. A test whose
  verdict depends on them should run with `ns test --no-cache`, or the
  workspace can set `[test].cache = false`.
- Reads by programs that cannot be traced (see above) outside the workspace,
  and by statically linked programs a test starts (on Linux).
- Doctests run under rustdoc rather than the runner, so they always run.
- `--list` and `--bench` runs are never cached.
- If the project already sets a runner for a target (wasm, qemu, a wrapper
  script), naspeed leaves it in place and that target's tests run uncached.
