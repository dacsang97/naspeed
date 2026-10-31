# Configuration

naspeed reads `~/.config/naspeed/config.toml`, which `setup` creates:

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

Project-level settings for `ns` live in [`ns.toml`](./ns#ns-toml).

## Environment variables

These override the file:

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
| `NO_COLOR=1` | Plain log lines, no colors |
| `CLICOLOR_FORCE=1` | Colored log lines even when stderr is not a terminal (GitHub Actions is colored by default) |
| `NASPEED_UI_INDENT=<n>` | Start log lines `n` levels deep, so a wrapper's own steps can nest naspeed's (naspeed-action sets it) |

Command output is a tree of log lines on stderr: `• step` at the top, nested
`• detail` lines, `key=value` fields in one column and `• took: 12s` under
slow steps. Program output stays on stdout: test binaries,
`naspeed stats --json`, the image digest of `ns image`.
| `NASPEED_LOG_FILE=<path>` | Write logs to this file |

`NASPEED_DISABLE=1` affects only later builds; see [Limits](./limits).
