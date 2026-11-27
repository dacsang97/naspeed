#!/usr/bin/env python3
"""Use Cargo, the real Rust compiler, the ORC launcher, and a poisoned linker."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from test_function_codegen import tool


SOURCE = '''\
use std::cell::Cell;
use std::io::{self, Write};

thread_local! {static THREAD_VALUE: Cell<u32> = const { Cell::new(41) };}
static OFFSET: u32 = 2;

#[inline(never)] fn demanded_once() -> u32 { dep::value() + OFFSET }
#[inline(never)] fn conditional_body() { println!("conditional ran"); }

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(std::env::var("VIA_RUNNER").unwrap(), "yes");
    println!("guest started, argv={args:?}");
    io::stdout().flush().unwrap();
    let f = std::hint::black_box(demanded_once as fn() -> u32);
    println!("value {} {}", f(), f());
    if args.first().is_some_and(|arg| arg == "conditional") { conditional_body(); }
    if args.first().is_some_and(|arg| arg == "unwind") {
        let child = std::thread::spawn(|| THREAD_VALUE.with(|value| value.get()));
        println!("thread {}", child.join().unwrap());
        let caught = std::panic::catch_unwind(|| panic!("caught panic"));
        assert!(caught.is_err());
        println!("caught");
    }
    if args.first().is_some_and(|arg| arg == "exit") { std::process::exit(23); }
}
'''


def main(naspeed, driver, toolchain):
    naspeed = Path(naspeed).resolve(strict=True)
    driver = Path(driver).absolute()
    cargo = tool(toolchain, "cargo")
    rustc = tool(toolchain, "rustc")
    with tempfile.TemporaryDirectory(prefix="naspeed lazy '") as tmp:
        root = Path(tmp).resolve()
        ns = root / "ns"
        ns.symlink_to(naspeed)
        (root / "src").mkdir()
        (root / "dep/src").mkdir(parents=True)
        (root / "leaf/src").mkdir(parents=True)
        (root / "bin").mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname="lazy_app"\nversion="0.0.0"\nedition="2024"\n'
            '[dependencies]\ndep={path="dep"}\n'
        )
        (root / "dep/Cargo.toml").write_text(
            '[package]\nname="dep"\nversion="0.0.0"\nedition="2024"\n'
            '[dependencies]\nleaf={path="../leaf"}\n'
        )
        (root / "leaf/Cargo.toml").write_text(
            '[package]\nname="leaf"\nversion="0.0.0"\nedition="2024"\n'
        )
        (root / "dep/src/lib.rs").write_text("pub fn value() -> u32 { leaf::value() }\n")
        leaf = root / "leaf/src/lib.rs"
        leaf.write_text("pub fn value() -> u32 { 10 }\n")
        source = root / "src/main.rs"
        source.write_text(SOURCE)
        (root / "examples").mkdir()
        (root / "examples/demo.rs").write_text('include!("../src/main.rs");\n')
        runner = root / "runner"
        runner.write_text('#!/bin/sh\nVIA_RUNNER=yes exec "$@"\n')
        runner.chmod(0o755)
        for name in ("cc", "clang", "ld"):
            poison = root / "bin" / name
            poison.write_text('#!/bin/sh\nprintf "linker invoked\\n" >> "$LINK_LOG"\nexit 99\n')
            poison.chmod(0o755)
        env = {
            name: value for name, value in os.environ.items()
            if not name.startswith(("CARGO_", "RUSTC_", "NASPEED_"))
            and name not in ("RUSTFLAGS", "RUSTUP_TOOLCHAIN")
        }
        env.update(CARGO=cargo, RUSTC=rustc, RUSTC_WRAPPER="", CARGO_HOME=str(root / "cargo-home"),
                   NASPEED_LAZY_RUN_DRIVER=str(driver), NASPEED_LAZY_RUN_REPORT="1", NO_COLOR="1",
                   PATH=f'{root / "bin"}:{env["PATH"]}', LINK_LOG=str(root / "link.log"))
        # Cargo splits an env runner on spaces. A TOML array preserves paths.
        (root / ".cargo").mkdir()
        (root / ".cargo/config.toml").write_text(
            '[target.aarch64-apple-darwin]\nrunner=[' + json.dumps(str(runner)) + ']\n'
        )

        def run(*flags, status=0):
            result = subprocess.run([ns, "run", *flags], cwd=root, env=env, text=True,
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=90)
            assert result.returncode == status, result.stdout
            return result.stdout

        assert "linking with" in run("--offline", status=101)
        log = root / "link.log"
        assert log.is_file(), "native control did not try the poisoned linker"
        log.unlink()
        lazy = ("--lazy", "--offline", "-vv")
        first = run(*lazy, "--", "a b", "--lazy")
        trace = [line for line in first.splitlines() if line.startswith("naspeed lazy materialized: ")]
        main = "guest started, argv=[\"a b\", \"--lazy\"]"
        demanded = [line for line in trace if "demanded_once" in line]
        assert len(demanded) == 1 and first.index(main) < first.index(demanded[0]), first
        assert not any("conditional_body" in line for line in trace), first
        assert "naspeed lazy cache hit:" not in first, first
        assert "value 12 12" in first and not log.exists(), first
        exe = root / "target/debug/lazy_app"
        assert exe.read_text().startswith("#!/bin/sh\n"), "unexpected AOT executable"

        conditional = run(*lazy, "--", "conditional")
        assert "Fresh lazy_app" in conditional, conditional
        assert any(line.startswith("naspeed lazy cache hit: ") and "demanded_once" in line
                   for line in conditional.splitlines()), conditional
        body = next(line for line in conditional.splitlines()
                    if line.startswith("naspeed lazy materialized: ") and "conditional_body" in line)
        assert conditional.index("guest started") < conditional.index(body) < conditional.index("conditional ran")
        unwind = run(*lazy, "--", "unwind")
        assert "thread 41" in unwind and "\ncaught\n" in unwind, unwind
        assert "value 12 12" in run(*lazy, "--", "exit", status=23)

        old_bitcode, = root.glob("target/debug/deps/.naspeed-lazy/*/app.bc")
        leaf.write_text("pub fn value() -> u32 { 29 }\n")
        assert "value 31 31" in run(*lazy)
        new_bitcode, = set(root.glob("target/debug/deps/.naspeed-lazy/*/app.bc")) - {old_bitcode}
        new_bitcode.unlink()
        assert "value 31 31" in run(*lazy)
        assert "value 31 31" in run(*lazy, "--example", "demo")
        assert not log.exists(), "lazy mode invoked the system linker"

        assert "unoptimized Darwin arm64" in run(*lazy, "--release", status=101)
        assert not log.exists(), "lazy mode silently fell back to the system linker"
        print("lazy function materialization and object reuse, no system linker, Cargo freshness, transitive archives, argv, TLS, unwind and exit verified")


if __name__ == "__main__":
    main(*sys.argv[1:])
