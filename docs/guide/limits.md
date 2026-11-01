# Limits

- `du` counts every clone at full size. Use `naspeed status` (unshared bytes)
  or `df` instead.
- Artifacts built before naspeed differ between target dirs, because rmeta
  embeds `OUT_DIR`. The real disk saving comes from rebuilding through naspeed
  (`cargo clean`, then build). `gc --dedup` mainly recovers identical
  build-script output.
- `NASPEED_DISABLE=1` affects only later builds. Cargo does not fingerprint the
  wrapper, so restored artifacts stay `Fresh`. To drop them, use
  `naspeed purge --target <dir>` or `cargo clean`.
- If the store volume is missing or read-only, every call passes through.
- On a volume without clone support (ext4, NTFS, other filesystems), naspeed
  still speeds builds up but saves no disk. On Windows, put projects and the
  store on a Dev Drive (ReFS) for block clones.
- Windows: build-script *runs* are not cached (their compilation is). The run
  cache swaps the script binary for a shell launcher, and Windows only runs
  `.exe` build scripts. `ns test` has no runtime input tracer there, so test
  results are keyed on declared inputs only (as with `NASPEED_TEST_TRACE=0`).
  `ns dev` stops the app with Ctrl-Break and then ends its job object instead
  of SIGTERM/SIGKILL. `setup --cross` and `naspeed build <triple>.<glibc>`
  need POSIX shell shims, so they are not available on Windows hosts, and
  `ns release` `before` hooks run with `sh` (Git for Windows provides it).
  The xattr memos live in NTFS/ReFS alternate data streams.
- The msvc target needs LLVM (`clang-cl`, `lld-link`, `llvm-lib`) and `xwin`.
  The Microsoft CRT and SDK are only downloaded after `--accept-msvc-license`,
  into `<store>/xwin`.
