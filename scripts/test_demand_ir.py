#!/usr/bin/env python3
"""Exercise source-to-IR requests, without permitting native link or bad Rust."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile


MAIN = '''\
mod bodies;

const BOTH: [fn(u32) -> u32; 2] = [bodies::called_body::<2>, bodies::unused_body];

fn main() {
    println!("guest began");
    let selector = usize::from(std::env::args().nth(1).as_deref() == Some("branch"));
    let f = std::hint::black_box(BOTH[selector]);
    println!("result {} {}", f(10), f(20));
    if selector == 1 { std::process::exit(27); }
}
'''


def main(naspeed, driver):
    naspeed = Path(naspeed).resolve(strict=True)
    driver = Path(driver).absolute()
    with tempfile.TemporaryDirectory(prefix="naspeed-demand-ir-") as directory:
        cwd = Path(directory).resolve()
        ns = cwd / "ns"
        ns.symlink_to(naspeed)
        source = cwd / "main.rs"
        source.write_text(MAIN)
        bodies = cwd / "bodies.rs"
        bodies.write_text('''\
static ADD: u32 = 3;
#[inline(never)] pub fn called_body<const N: u32>(x: u32) -> u32 { x * N + ADD }
#[inline(never)] pub fn unused_body(x: u32) -> u32 { if x == 0 { 17 } else { unused_body(x - 1) } }
''')
        bindir = cwd / "bin"
        bindir.mkdir()
        for name in ("cc", "clang", "ld"):
            linker = bindir / name
            linker.write_text('#!/bin/sh\ntouch "$LINK_LOG"\nexit 99\n')
            linker.chmod(0o755)
        log = cwd / "linker.log"
        env = os.environ | {
            "NASPEED_LAZY_RUN_DRIVER": str(driver),
            "NASPEED_LAZY_RUN_REPORT": "1",
            "PATH": f"{bindir}:{os.environ['PATH']}",
            "LINK_LOG": str(log),
        }

        def run(*args, status=0):
            proc = subprocess.run([ns, "run", "--lazy-ir", source, "--", *args], cwd=cwd,
                                  env=env, text=True, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, timeout=45)
            assert proc.returncode == status, proc.stdout
            assert not log.exists(), "demand mode invoked the system linker"
            return proc.stdout

        def trace(output, name):
            return [line for line in output.splitlines()
                    if line.startswith("naspeed demand IR: _R") and name in line]

        sparse = run()
        assert "result 23 43" in sparse, sparse
        assert not trace(sparse, "unused_body"), sparse
        requested, = trace(sparse, "called_body")
        assert sparse.index("guest began") < sparse.index(requested) < sparse.index("result "), sparse
        dense = run("branch", status=27)
        assert "result 17 17" in dense, dense
        assert not trace(dense, "called_body"), dense
        requested, = trace(dense, "unused_body")
        assert dense.index("guest began") < dense.index(requested) < dense.index("result "), dense

        source.write_text(MAIN + '\nfn invalid_never_called() { let _: u8 = "type error"; }\n')
        invalid = run(status=1)
        assert "mismatched types" in invalid and "guest began" not in invalid, invalid
        source.write_text(MAIN + '\n#[used] static REGISTERED: u32 = 1;\n')
        unsupported = run(status=125)
        assert "retained or custom-section" in unsupported and "guest began" not in unsupported, unsupported
        print("demand IR: first-call MIR/IR, recursion, generics, static, no linker, and eager Rust diagnostics verified")


if __name__ == "__main__":
    main(*sys.argv[1:])
