#!/usr/bin/env python3
"""Cargo-selected bins, exact IR hits/misses, and declared build inputs."""
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from test_function_codegen import tool


SOURCE = '''\
const OFFSET: u32 = 0;
type Word = u32;
#[inline(never)] fn width() -> u32 { std::mem::size_of::<Word>() as u32 }
#[inline(never)] fn shared(x: u32) -> u32 { dep::value() + x + OFFSET }
#[inline(never)] fn first() -> u32 { shared(1) }
#[inline(never)] fn second() -> u32 { shared(2) }
fn main() {
    let branch = std::env::args().nth(1);
    let value = if branch.as_deref() == Some("second") { second() } else { first() };
    let value = value + width() - 4;
    println!("{value} {} {}", env!("BUILD_VALUE"), std::env::var("FROM_RUNNER").unwrap());
}
type CheckNumber = u32;
#[allow(dead_code)] fn never_called() -> CheckNumber { 1u32 }
'''


def main(naspeed, driver, toolchain):
    naspeed = Path(naspeed).resolve(strict=True)
    driver = Path(driver).absolute()
    with tempfile.TemporaryDirectory(prefix="naspeed-cargo-demand-") as directory:
        root = Path(directory).resolve()
        ns = root / "ns"
        ns.symlink_to(naspeed)
        (root / "src").mkdir()
        (root / "dep/src").mkdir(parents=True)
        (root / "Cargo.toml").write_text(
            '[package]\nname="demand_fixture"\nversion="0.0.0"\nedition="2024"\n'
            '[dependencies]\ndep={path="dep"}\n'
        )
        (root / "dep/Cargo.toml").write_text('[package]\nname="dep"\nversion="0.0.0"\nedition="2024"\nlinks="native_dep"\n')
        (root / "dep/build.rs").write_text('''\
use std::process::Command;
fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(cc)
        .args(["-arch", "arm64", "-O2", "-c", "native.c", "-o"])
        .arg(format!("{out}/native.o"))
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("ar")
        .args(["rcs", &format!("{out}/libnative_dep.a"), &format!("{out}/native.o")])
        .status()
        .unwrap();
    assert!(status.success());
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=native_dep");
    println!("cargo:rerun-if-changed=native.c");
}
''')
        (root / "dep/native.c").write_text('int native_dep_value(void) { return 5; }\n')
        (root / "build.rs").write_text('''\
fn main() {
    println!("cargo:rerun-if-changed=build-input");
    println!("cargo:rustc-env=BUILD_VALUE={}", std::fs::read_to_string("build-input").unwrap().trim());
}
''')
        (root / "build-input").write_text("build-a")
        dep = root / "dep/src/lib.rs"
        dep.write_text('''\
unsafe extern "C" { fn native_dep_value() -> i32; }
pub fn value() -> u32 { unsafe { native_dep_value() as u32 + 15 } }
''')
        source = root / "src/main.rs"
        source.write_text(SOURCE)
        runner = root / "runner"
        runner.write_text('#!/bin/sh\nFROM_RUNNER=yes exec "$@"\n')
        runner.chmod(0o755)
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("CARGO_", "RUSTC_", "NASPEED_")) and k not in ("RUSTFLAGS", "RUSTUP_TOOLCHAIN")}
        env.update(CARGO=tool(toolchain, "cargo"), RUSTC=tool(toolchain, "rustc"), RUSTC_WRAPPER="",
                   CARGO_HOME=str(root / "cargo-home"), NASPEED_LAZY_RUN_DRIVER=str(driver),
                   NASPEED_LAZY_RUN_REPORT="1", CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=str(runner))

        def invoke(*args):
            cmd = [ns, "run", "--lazy-ir", "--offline", "--bin", "demand_fixture", "-vv", "--", *args]
            return subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True, timeout=80)

        def run(expected, *args):
            out = invoke(*args)
            assert out.returncode == 0 and out.stdout.splitlines()[-1:] == [expected], (out.stdout, out.stderr)
            return out.stderr

        first = run("21 build-a yes")
        assert "naspeed demand compiler: started" in first
        assert "naspeed demand IR cache hit: main" not in first
        assert "naspeed demand typeck disk hit (cargo):" not in first, first
        warm = run("21 build-a yes")
        assert "Fresh demand_fixture" in warm and "naspeed demand IR cache hit: main" in warm, warm
        assert "naspeed demand compiler: started" not in warm, warm
        assert "naspeed demand object hit: main\n" in warm, warm
        object_dir = root / "target/debug/deps/.naspeed-lazy/demand-objects-v1"
        object_file = next(object_dir.iterdir())
        original_object = object_file.read_bytes()
        damaged = original_object[:-1] + bytes([original_object[-1] ^ 1])
        object_file.write_bytes(damaged)
        repaired = run("21 build-a yes")
        assert "naspeed demand compiler: started" not in repaired, repaired
        assert object_file.read_bytes() == original_object
        branch = run("22 build-a yes", "second")
        assert "naspeed demand IR cache hit: main" in branch, branch
        assert "naspeed demand compiler: started" in branch, branch
        assert any(s.startswith("naspeed demand IR cache hit: ") and "shared" in s for s in branch.splitlines()), branch
        assert "naspeed demand compiler: started" not in run("22 build-a yes", "second")

        caches = list(root.glob("target/debug/deps/.naspeed-lazy/ir/*"))
        assert len(caches) == 1, caches
        record = caches[0] / hashlib.sha256(b"main").hexdigest()
        record.write_bytes(b"corrupt record")
        assert "naspeed demand compiler: started" in run("21 build-a yes")
        assert "naspeed demand compiler: started" not in run("21 build-a yes")

        edited = SOURCE.replace("shared(1)", "shared(3)")
        source.write_text(edited)
        changed = run("23 build-a yes")
        assert "naspeed demand IR cache hit: main" not in changed, changed
        assert any("5first" in s for s in changed.splitlines() if s.startswith("naspeed demand IR: ")), changed
        assert any("6shared" in s for s in changed.splitlines() if s.startswith("naspeed demand IR reused: ")), changed
        assert not any("6shared" in s for s in changed.splitlines() if s.startswith("naspeed demand IR: ")), changed
        assert any("6shared" in s for s in changed.splitlines() if s.startswith("naspeed demand object hit: ")), changed
        hits = changed.splitlines()
        assert "naspeed demand typeck disk hit (cargo): shared" in hits, changed
        assert "naspeed demand typeck disk hit (cargo): first" not in hits, changed
        assert "naspeed demand compiler: started" not in run("23 build-a yes")

        # The unchanged text of `shared` does not make its old constant valid.
        edited = edited.replace("OFFSET: u32 = 0", "OFFSET: u32 = 5")
        source.write_text(edited)
        changed = run("28 build-a yes")
        assert any("6shared" in s for s in changed.splitlines() if s.startswith("naspeed demand IR: ")), changed

        source.write_text(edited.replace("type Word = u32", "type Word = u64"))
        changed = run("32 build-a yes")
        assert any("5width" in s for s in changed.splitlines() if s.startswith("naspeed demand IR: ")), changed

        dep.write_text("pub fn value() -> u32 { 39 }\n")
        changed = run("51 build-a yes")
        assert "naspeed demand IR cache hit: main" not in changed, changed
        (root / "build-input").write_text("build-b")
        changed = run("51 build-b yes")
        assert "naspeed demand IR cache hit: main" not in changed, changed
        assert "naspeed demand compiler: started" not in run("51 build-b yes")

        valid = source.read_text()
        for invalid, diagnostic in (
            (valid.replace("type CheckNumber = u32", "type CheckNumber = u64"), "E0308"),
            (valid.replace("{ 1u32 }", '{ let s = String::from("owned"); drop(s); s.len() as u32 }'), "E0382"),
        ):
            source.write_text(invalid)
            out = invoke()
            assert out.returncode != 0 and diagnostic in out.stderr and "never_called" in out.stderr, out.stderr
            assert "naspeed demand compiler: started" not in out.stderr, out.stderr
            assert not out.stdout.rstrip().endswith("51 build-b yes"), out.stdout
        source.write_text(valid)
        run("51 build-b yes")

        env["NASPEED_LAZY_DEP_IMAGE"] = "1"
        mapped = run("51 build-b yes")
        assert "naspeed dependency image built:" in mapped, mapped
        assert "naspeed dependency image mapped:" in mapped, mapped

        source.write_text(valid.replace("shared(3)", "shared(4)"))
        reused = run("52 build-b yes")
        assert "naspeed dependency image hit:" in reused, reused
        assert "naspeed dependency image mapped:" in reused, reused
        image_root = root / "target/debug/deps/.naspeed-lazy/dep-images-v1"
        images = set(image_root.glob("*/image.dylib"))
        assert len(images) == 1, images

        dep.write_text('''\
use std::sync::atomic::{AtomicU32, Ordering};
static VALUE: AtomicU32 = AtomicU32::new(40);
pub fn value() -> u32 { VALUE.fetch_add(1, Ordering::Relaxed) }
#[inline(never)]
pub fn call(f: fn()) { f(); }
''')
        replaced = run("53 build-b yes")
        assert "naspeed dependency image built:" in replaced, replaced
        assert "naspeed dependency image mapped:" in replaced, replaced
        assert "naspeed dependency image mapped:" in run("53 build-b yes")
        source.write_text(source.read_text().replace(
            '    println!("{value}',
            '    if branch.as_deref() == Some("unwind") {\n'
            '        assert!(std::panic::catch_unwind(|| dep::call(|| panic!("through dylib"))).is_err());\n'
            '    }\n    println!("{value}',
        ))
        assert "naspeed dependency image mapped:" in run("53 build-b yes", "unwind")
        (image,) = set(image_root.glob("*/image.dylib")) - images
        image.write_bytes(b"damaged dylib")
        assert "naspeed dependency image rejected; using archives:" in run("53 build-b yes")
        env.pop("NASPEED_LAZY_DEP_IMAGE")
        assert "naspeed dependency image mapped:" not in run("53 build-b yes")

        env["NASPEED_LAZY_APP_IMAGE"] = "1"
        app = run("53 build-b yes", "unwind")
        mapped = "naspeed app image mapped: "
        assert mapped in app and "naspeed demand compiler: started" not in app, app
        first_image = next(s.removeprefix(mapped) for s in app.splitlines() if s.startswith(mapped))
        warm = run("53 build-b yes", "unwind")
        assert "Fresh demand_fixture" in warm and mapped + first_image in warm, warm
        executable = root / "target/debug/demand_fixture"
        saved = root / "old-launch"
        saved.write_bytes(executable.read_bytes())
        saved.chmod(0o755)

        source.write_text(source.read_text().replace("shared(4)", "shared(6)"))
        edited = run("55 build-b yes")
        new_image = next(s.removeprefix(mapped) for s in edited.splitlines() if s.startswith(mapped))
        assert new_image != first_image and Path(first_image).is_file(), edited
        old = subprocess.run([runner, saved], cwd=root, env=env, capture_output=True, text=True, timeout=15)
        assert old.returncode == 0 and old.stdout.strip() == "53 build-b yes", old
        Path(new_image).write_bytes(b"damaged app image")
        fallback = subprocess.run([runner, executable], cwd=root, env=env, capture_output=True, text=True, timeout=30)
        assert fallback.returncode == 0 and fallback.stdout.strip() == "55 build-b yes", fallback
        assert "naspeed app image missing or invalid; using ORC" in fallback.stderr and mapped not in fallback.stderr, fallback.stderr
        env.pop("NASPEED_LAZY_APP_IMAGE")
        assert mapped not in run("55 build-b yes")
        print("Cargo demand IR: typeck, native objects, dependency and app images, state, unwind, invalidation and fallback verified")


if __name__ == "__main__":
    main(*sys.argv[1:])
