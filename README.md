# naspeed

A transparent `RUSTC_WRAPPER` for cargo. naspeed stores each compiled crate
once per volume, in a content-addressed store, and clones it copy-on-write into
every `target/` dir that needs it. A machine-wide slot pool also caps how many
`rustc` run at once. No daemon, no server, no `-Z` flags. If naspeed is unsure
about a call, or anything fails, it runs `rustc` unchanged.

Repository: <https://github.com/dacsang97/naspeed>
Docs: <https://dacsang97.github.io/naspeed/>

## Quick start

Requires Rust 1.98+ on macOS or Linux. Disk savings need a filesystem with
clones (APFS, btrfs, xfs); the store must be on the same volume as your
projects.

```sh
brew install dacsang97/tap/naspeed   # or: cargo install naspeed --locked
naspeed setup                        # store in ~/.naspeed
```

If your projects live on another volume, put the store there instead:

```sh
naspeed setup --store /Volumes/Work/.naspeed   # projects under /Volumes/Work
```

`setup` installs the binary to `~/.cargo/bin/naspeed` (plus the `ns` symlink),
backs up `~/.cargo/config.toml` and points `build.rustc-wrapper` at naspeed.
From then on every `cargo` call, from a shell, an IDE, rust-analyzer or an
agent, goes through the cache.

```sh
naspeed status --check   # confirm naspeed wraps rustc; store size and hit rate
naspeed uninstall        # restore the config keys setup changed
```

## Documentation

Everything else lives in the [docs site](https://dacsang97.github.io/naspeed/)
(sources in [`docs/`](./docs), local preview with `pnpm --dir docs run dev`):

- [Getting started](https://dacsang97.github.io/naspeed/guide/getting-started) —
  what `setup` changes, cross builds, uninstall
- [Commands](https://dacsang97.github.io/naspeed/guide/commands) —
  `status`, `stats`, `gc`, `purge`, `build`
- [ns front-end](https://dacsang97.github.io/naspeed/guide/ns) —
  `ns dev`, `ns test`, `ns check`, `ns fmt`, `ns release`, `ns image`, `ns ci init`, `ns.toml`
- [Test result cache](https://dacsang97.github.io/naspeed/guide/test-cache) and
  [Peer cache over Tailscale](https://dacsang97.github.io/naspeed/guide/peer-cache)
- [GitHub Actions](https://dacsang97.github.io/naspeed/guide/github-actions) —
  `dacsang97/naspeed-action`: install, store cache, job summary
- [AI agents](https://dacsang97.github.io/naspeed/guide/agents) —
  instructions block for `AGENTS.md` / `CLAUDE.md`, and a prompt to set a
  project up with naspeed
- [How it works](https://dacsang97.github.io/naspeed/guide/how-it-works) —
  cache key, workspace crates, build scripts, slots
- [Configuration](https://dacsang97.github.io/naspeed/guide/configuration) —
  `config.toml` and `NASPEED_*` variables
- [Limits](https://dacsang97.github.io/naspeed/guide/limits) and
  [Development](https://dacsang97.github.io/naspeed/guide/development)

## License

Apache-2.0. Third-party notices are in [NOTICE](NOTICE).
