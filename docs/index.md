---
layout: home

hero:
  name: naspeed
  text: One crate store per volume
  tagline: A transparent RUSTC_WRAPPER for cargo. Compiled crates are stored once and cloned copy-on-write into every target/ dir. No daemon, no server, no -Z flags.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/dacsang97/naspeed

features:
  - title: Disk
    details: On APFS (clonefile), btrfs/xfs (FICLONE) and ReFS / Windows Dev Drive (block cloning) a restored crate takes almost no extra space. Ten worktrees of one repo share their dependencies' blocks.
  - title: Speed
    details: A new worktree or project gets every shared dependency straight from the store, including the results of build scripts that compile C (jemalloc-sys, aws-lc-sys, ring).
  - title: Workspace crates
    details: Your own crates are cached too, keyed on the exact source files rustc read. Another worktree, a branch switch back or another agent at the same commit reuses them.
  - title: Safety
    details: If naspeed is unsure about a call, or anything fails, it runs rustc (or the build script) unchanged.
---
