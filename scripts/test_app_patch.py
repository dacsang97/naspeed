#!/usr/bin/env python3
"""Verify Cargo app-patch image reuse, isolation, invalidation, and fallback."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from test_function_codegen import tool


SOURCE = '''\
use std::sync::atomic::{AtomicI32, Ordering};

#[repr(C)]
struct Shared {
    value: AtomicI32,
}

static STATE: Shared = Shared { value: AtomicI32::new(10) };

#[inline(never)]
fn base_step(amount: i32) -> i32 {
    STATE.value.fetch_add(amount, Ordering::SeqCst) + amount
}

#[inline(never)]
fn first(call: fn(i32) -> i32) -> i32 {
    STATE.value.load(Ordering::SeqCst) - 10 + call(3)
}

#[inline(never)]
fn second() -> i32 {
    STATE.value.fetch_add(2, Ordering::SeqCst) + 2
}

#[inline(never)]
fn panic_step(_: i32) -> i32 {
    panic!("unwind through first patch")
}

fn main() {
    let unwound = std::panic::catch_unwind(|| first(panic_step)).is_err();
    let first_value = first(base_step);
    let second_value = second();
    println!(
        "{first_value} {second_value} {} {unwound}",
        STATE.value.load(Ordering::SeqCst),
    );
}
'''

FIRST_EDIT = SOURCE.replace("call(3)", "call(5)")
BASE_EDIT = FIRST_EDIT.replace(
    "struct Shared {\n    value: AtomicI32,\n}",
    "struct Shared {\n    _generation: u64,\n    value: AtomicI32,\n}",
).replace(
    "Shared { value: AtomicI32::new(10) }",
    "Shared { _generation: 1, value: AtomicI32::new(10) }",
)

BASE_MAPPED = "naspeed app patch base mapped: "
PATCH_MAPPED = "naspeed app patch mapped: "
WORKER_STARTED = "naspeed demand compiler: started"


def mapped_images(stderr):
    bases = [
        Path(line.removeprefix(BASE_MAPPED))
        for line in stderr.splitlines()
        if line.startswith(BASE_MAPPED)
    ]
    assert len(bases) == 1, stderr
    base = bases[0]
    assert base.is_absolute() and base.is_file(), base

    patches = {}
    for line in stderr.splitlines():
        if not line.startswith(PATCH_MAPPED):
            continue
        payload = line.removeprefix(PATCH_MAPPED)
        slot, separator, image = payload.partition(" ")
        path = Path(image)
        assert separator and slot and path.is_absolute() and path.is_file(), line
        assert slot not in patches, (slot, stderr)
        patches[slot] = path
    assert len(patches) == 2, (patches, stderr)
    return base, patches


def main(naspeed, driver, toolchain):
    naspeed = Path(naspeed).resolve(strict=True)
    driver = Path(driver).absolute()
    with tempfile.TemporaryDirectory(prefix="naspeed-app-patch-") as directory:
        root = Path(directory).resolve()
        ns = root / "ns"
        ns.symlink_to(naspeed)
        (root / "src").mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname="app_patch_fixture"\nversion="0.0.0"\nedition="2024"\n'
        )
        source = root / "src/main.rs"
        source.write_text(SOURCE)
        runner = root / "runner"
        runner.write_text('#!/bin/sh\nexec "$@"\n')
        runner.chmod(0o755)

        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("CARGO_", "RUSTC_", "NASPEED_"))
            and key not in ("RUSTFLAGS", "RUSTUP_TOOLCHAIN")
        }
        env.update(
            CARGO=tool(toolchain, "cargo"),
            RUSTC=tool(toolchain, "rustc"),
            RUSTC_WRAPPER="",
            CARGO_HOME=str(root / "cargo-home"),
            NASPEED_LAZY_RUN_DRIVER=str(driver),
            NASPEED_LAZY_RUN_REPORT="1",
            NASPEED_LAZY_APP_PATCH="first,second",
            CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=str(runner),
            NO_COLOR="1",
        )

        def invoke():
            return subprocess.run(
                [ns, "run", "--lazy-ir", "--offline", "--bin", "app_patch_fixture", "-vv"],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                timeout=90,
            )

        def run(expected):
            result = invoke()
            assert result.returncode == 0, (result.stdout, result.stderr)
            assert result.stdout.splitlines()[-1:] == [expected], (result.stdout, result.stderr)
            return result

        original_output = "13 15 15 true"
        original = run(original_output)
        original_base, original_patches = mapped_images(original.stderr)
        warm = run(original_output)
        warm_base, warm_patches = mapped_images(warm.stderr)
        assert "Fresh app_patch_fixture" in warm.stderr, warm.stderr
        assert (warm_base, warm_patches) == (original_base, original_patches), warm.stderr
        assert WORKER_STARTED not in warm.stderr, warm.stderr

        executable = root / "target/debug/app_patch_fixture"
        saved = root / "saved-launcher"
        saved.write_bytes(executable.read_bytes())
        saved.chmod(0o755)

        source.write_text(FIRST_EDIT)
        edited_output = "15 17 17 true"
        edited = run(edited_output)
        edited_base, edited_patches = mapped_images(edited.stderr)
        assert edited_base == original_base, edited.stderr
        assert edited_patches.keys() == original_patches.keys(), edited.stderr
        changed_slots = [
            slot for slot in edited_patches if edited_patches[slot] != original_patches[slot]
        ]
        assert len(changed_slots) == 1, (original_patches, edited_patches)
        changed_slot = changed_slots[0]
        unchanged_slot = next(slot for slot in edited_patches if slot != changed_slot)
        assert edited_patches[unchanged_slot] == original_patches[unchanged_slot]
        assert original_patches[changed_slot].is_file()

        edited_warm = run(edited_output)
        assert mapped_images(edited_warm.stderr) == (edited_base, edited_patches)
        assert WORKER_STARTED not in edited_warm.stderr, edited_warm.stderr

        old = subprocess.run(
            [runner, saved], cwd=root, env=env, capture_output=True, text=True, timeout=30
        )
        assert old.returncode == 0 and old.stdout.splitlines()[-1:] == [original_output], old
        assert mapped_images(old.stderr) == (original_base, original_patches)

        source.write_text(BASE_EDIT)
        fresh = run(edited_output)
        fresh_base, fresh_patches = mapped_images(fresh.stderr)
        assert fresh_base != edited_base, fresh.stderr
        assert fresh_base.is_file() and edited_base.is_file()
        assert fresh_patches.keys() == edited_patches.keys(), fresh.stderr

        damaged = fresh_patches[changed_slot]
        damaged.write_bytes(b"damaged app patch")
        fallback = subprocess.run(
            [runner, executable], cwd=root, env=env, capture_output=True, text=True, timeout=45
        )
        assert fallback.returncode == 0, (fallback.stdout, fallback.stderr)
        assert fallback.stdout.splitlines()[-1:] == [edited_output], fallback
        assert "naspeed app patch unavailable; using ORC" in fallback.stderr, fallback.stderr

        print(
            "Cargo app patches: per-function reuse, shared state, unwind, isolation, "
            "base invalidation and ORC fallback verified"
        )


if __name__ == "__main__":
    main(*sys.argv[1:])
