# Peer cache over Tailscale

A store can serve other machines, so a laptop gets what a desktop or a build
VPS already compiled:

```sh
# on the machine with the warm store (binds its Tailscale IP, port 7878)
ns cache serve

# on the others, in ~/.config/naspeed/config.toml
remotes = ["http://desktop.tailnet-name.ts.net:7878"]
```

## How lookups work

On a local miss the wrapper asks each peer, in order, for the entry. It
verifies every object's blake3 hash and writes everything into the local store,
then restores as from a local hit. The next build on that machine needs no
network.

A peer that refuses or times out (`NASPEED_REMOTE_CONNECT_MS`, default 800) is
skipped by every wrapper for 60 s, so being off the tailnet costs one timeout,
not one per crate. Keys are path-independent, so peers share entries across
checkout paths and user names. They must have the same toolchain and target,
because a different `rustc` simply misses.

## Shared cache with push

For a shared cache on a VPS, start it with `ns cache serve --allow-push` and a
token, then upload from CI or a desktop with `ns cache push http://vps:7878`.

The token is the first line of `~/.config/naspeed/remote.token`
(`remote_token_file` in the config) or `NASPEED_REMOTE_TOKEN`. When set, every
request must send `Authorization: Bearer <token>`. The same token goes to every
configured remote over plain HTTP, which is fine inside a tailnet
(WireGuard-encrypted) but not across the internet.

## Trust

- Names are checked to be hex, and entries must parse and match their key.
- Output paths must stay inside the out dir and must not pass through the
  entry's own symlinks.
- File modes are limited to plain permission bits, and manifests may only name
  files inside the build.
- Objects are hash-verified. A peer that stalls is cut off after a few seconds
  and treated as down.

No one can check that an rlib really matches its key, though, so only add peers
you would also let build your code: your own machines on your tailnet. Serving
binds the Tailscale address unless `--listen` says otherwise. Avoid `0.0.0.0`
outside a private network.
