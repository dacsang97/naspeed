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
- On a volume without clone support (ext4, other filesystems), naspeed still
  speeds builds up but saves no disk.
- The msvc target needs LLVM (`clang-cl`, `lld-link`, `llvm-lib`) and `xwin`.
  The Microsoft CRT and SDK are only downloaded after `--accept-msvc-license`,
  into `<store>/xwin`.
