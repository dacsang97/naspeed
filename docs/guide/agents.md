# AI agents

Coding agents (Claude Code, Codex, Cursor, Copilot…) run `cargo` like anyone
else, so on a machine set up with naspeed they already go through the cache.
Two habits still cost them time, though:

- they try to "fix" a build by turning the cache off (`RUSTC_WRAPPER=`,
  `NASPEED_DISABLE=1`) or by running `cargo clean`;
- they skip formatting, linting and tests before saying they are done, unless
  the project says so explicitly.

A short block in the project's agent instructions fixes both. Agents read
`AGENTS.md` (most tools) or `CLAUDE.md` (Claude Code; a one-line
`@AGENTS.md` there pulls in the shared file).

## Instructions block

Paste this into `AGENTS.md` or `CLAUDE.md`. It is deliberately short: every
line is an instruction an agent would otherwise get wrong.

```md
<!-- naspeed:begin -->
## Rust builds (naspeed)

Rust builds on this machine go through naspeed, a transparent `RUSTC_WRAPPER`
cache; `ns` is its project front-end. Docs: https://dacsang97.github.io/naspeed/

- Use `cargo` as usual: it already goes through the cache. Never set
  `RUSTC_WRAPPER=` or `NASPEED_DISABLE=1`, and never run `cargo clean` to work
  around a build problem. Run `naspeed status --check` and report its output.
- Fresh worktrees and target dirs are cheap (crates come from the store), so
  each task can use its own.
- Prefer the cached `ns` commands:
  - `ns fmt` formats the workspace (`ns fmt --check` to only report);
  - `ns check` runs rustfmt --check and clippy -D warnings on every target;
  - `ns test` runs `cargo test` and replays test binaries unchanged since they
    last passed (`ns test --no-cache` forces a full run);
  - `ns dev` runs the app and rebuilds it on change (settings in `ns.toml`).
- Before finishing a task, run `ns check` and `ns test`, and fix what they report.
<!-- naspeed:end -->
```

The markers let a later update replace the block in place. If the project's
`Makefile` or `justfile` already wraps these commands (`make check`,
`make test`), say so in the block instead, so agents use the project's entry
points.

## Prompt: set a project up with naspeed

Give this prompt to an agent in a Rust repository to adopt naspeed. It covers
what is usually needed in an existing project, and asks the agent to prove
the result rather than claim it.

```md
Set this Rust repository up to build through naspeed
(https://dacsang97.github.io/naspeed/). Work on a branch; do not commit
unrelated changes.

1. Check that naspeed is installed and active: `naspeed --version`,
   `naspeed status --check`. If it is missing, stop and tell me how to
   install it (`brew install dacsang97/tap/naspeed` or
   `cargo install naspeed --locked`, then `naspeed setup`).
2. Find settings that bypass the cache and remove them: `RUSTC_WRAPPER=`
   or `RUSTC_WRAPPER=sccache` in Makefiles, scripts, `.cargo/config.toml`,
   `.env` files and CI; `CARGO_INCREMENTAL=0` meant for sccache. Keep
   anything unrelated to caching.
3. If the project runs a server or CLI during development, add an `ns.toml`
   with a `[dev]` section (`bin`, `env_file`, `ports` the process listens
   on) so `ns dev` can rebuild and restart it.
4. Point the project's own entry points at `ns`, with a plain cargo
   fallback for machines without naspeed, e.g. in a Makefile:
   `NS := $(shell command -v ns 2>/dev/null)`, then
   `check` → `ns check` (else `cargo fmt --all -- --check` and
   `cargo clippy --workspace --all-targets -- -D warnings`),
   `test` → `ns test --workspace`, `fmt` → `ns fmt`, `run` → `ns dev`.
5. In GitHub Actions, replace other Rust build caches (`rust-cache`,
   sccache) with `dacsang97/naspeed-action@v1`, one `cache-key` per kind
   of job (see https://dacsang97.github.io/naspeed/guide/github-actions).
6. Add the naspeed block from https://dacsang97.github.io/naspeed/guide/agents
   to AGENTS.md (or CLAUDE.md).
7. Verify and report the numbers: `ns check`; `ns test` twice (the second
   run should show `(cached)` binaries); `naspeed stats`. If a test fails,
   report it; do not hide it with `--no-cache` or by skipping it.
```

## Notes for agent workflows

- **Parallel jobs.** Each job can work in its own git worktree with its own
  target dir; dependencies and unchanged workspace crates are restored from
  the store, and a machine-wide slot pool keeps parallel builds from
  exhausting memory. There is no need to share one target dir between agents.
- **Reading results.** `ns test` and `ns check` end with one summary line
  per step (`ns: clippy ok (778ms)`), and `naspeed stats --json` gives
  machine-readable totals.
- **When something looks wrong.** `naspeed status --check` builds a tiny
  crate and confirms naspeed wraps `rustc`; its output is what to include in
  a bug report. `ns test --no-cache` and `NASPEED_DISABLE=1 cargo build`
  are for diagnosing, not for everyday use.
