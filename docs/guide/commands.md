# Commands

| Command | What it does |
|---|---|
| `naspeed status` | Wrapper in effect, slot use, store entries, logical vs unshared bytes, 7-day hit rate, registered target dirs |
| `naspeed status --check` | Also builds a tiny crate with `cargo build -v` and confirms naspeed wraps `rustc` |
| `naspeed stats [--days N] [--json]` | Time saved: compile time of every rustc/build-script hit minus its restore time, test runs replayed by `ns test` and their run time, entries fetched from peers, and the crates that saved the most |
| `naspeed gc [--max-size-gb N] [--max-age-days D]` | Evicts store entries least-recently-used first until the store is under the cap (default 20 GB), and drops entries unused for D days (default 14). Runs on its own in the background at most once a day; `NASPEED_NO_AUTO_GC=1` turns that off |
| `naspeed gc --targets` | Removes stale `-<hash>` variants and old incremental sessions from registered target dirs |
| `naspeed gc --dedup` | Replaces byte-identical files of 64 KiB or more across target dirs with clones |
| `naspeed purge --target <dir>` | Deletes the artifacts naspeed restored into a target dir, plus their fingerprints |
| `naspeed build --target universal2-apple-darwin` | Builds both macOS architectures, then runs `lipo -create` |
| `naspeed build --target x86_64-unknown-linux-gnu.2.17` | Links against glibc 2.17 through zig, in a separate target dir |
| `naspeed setup --cross` | Installs zig linker shims for Linux gnu/musl and windows-gnu, and the msvc SDK if `--accept-msvc-license` is given |
| `naspeed uninstall [--purge]` | Undoes exactly the keys setup changed; `--purge` also deletes the store |

Every `gc` mode takes `--dry-run`, and `--exclude <dir>` (repeatable) to leave
a project you are actively developing alone.

The project-level commands (`ns dev`, `ns test`, `ns release`, `ns image`,
`ns ci init`, `ns cache …`) are described in [ns front-end](./ns).
