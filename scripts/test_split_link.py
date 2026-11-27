#!/usr/bin/env python3
"""Verify the dependency image, Cargo freshness, and execution semantics on Darwin."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile

from test_function_codegen import tool


SOURCE = """\
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    println!("{} {} {:?}", dep::value() + 1, std::env::var("VIA_RUNNER").unwrap(), args);
    if args.first().is_some_and(|arg| arg == "fail") { std::process::exit(23); }
}
"""


def main(naspeed, driver, toolchain):
    if sys.platform != "darwin":
        return
    naspeed = Path(naspeed).resolve(strict=True)
    driver = Path(driver).resolve(strict=True).with_name("naspeed-rustc-split")
    with tempfile.TemporaryDirectory(prefix="naspeed-split-link-") as tmp:
        root = Path(tmp).resolve()
        ns = root / "ns"
        ns.symlink_to(naspeed)
        (root / "src").mkdir()
        (root / "dep/src").mkdir(parents=True)
        (root / "Cargo.toml").write_text(
            '[package]\nname="split_app"\nversion="0.0.0"\nedition="2024"\n'
            '[dependencies]\ndep={path="dep"}\n',
            encoding="utf-8",
        )
        (root / "dep/Cargo.toml").write_text(
            '[package]\nname="dep"\nversion="0.0.0"\nedition="2024"\n', encoding="utf-8",
        )
        source = root / "src/main.rs"
        dep = root / "dep/src/lib.rs"
        source.write_text(SOURCE, encoding="utf-8")
        dep.write_text("pub fn value() -> u32 { 10 }\n", encoding="utf-8")
        runner = root / "runner"
        runner.write_text('#!/bin/sh\nVIA_RUNNER=yes exec "$@"\n', encoding="utf-8")
        runner.chmod(0o755)
        env = {
            name: value for name, value in os.environ.items()
            if not name.startswith(("CARGO_", "RUSTC_", "NASPEED_"))
            and name not in ("RUSTFLAGS", "RUSTUP_TOOLCHAIN")
        }
        rustc = tool(toolchain, "rustc")
        host = next(line[6:] for line in subprocess.check_output([rustc, "-vV"], text=True).splitlines()
                    if line.startswith("host: "))
        env.update(CARGO=tool(toolchain, "cargo"), RUSTC=rustc,
                   CARGO_HOME=str(root / "cargo-home"), CARGO_INCREMENTAL="1",
                   NASPEED_SPLIT_LINK_DRIVER=str(driver), NASPEED_SPLIT_LINK_REPORT="1",
                   NO_COLOR="1")
        env[f"CARGO_TARGET_{host.upper().replace('-', '_')}_RUNNER"] = str(runner)

        def run(*flags, status=0):
            result = subprocess.run([ns, *flags], cwd=root, env=env, text=True,
                                    capture_output=True, timeout=90)
            assert result.returncode == status, (result.stdout, result.stderr)
            return result

        def links(profile="debug"):
            return subprocess.check_output(["xcrun", "otool", "-L", root / f"target/{profile}/split_app"],
                                           text=True)

        def images():
            return set(root.glob("target/*/deps/.naspeed-split/*.dylib"))

        split = ("run", "--split-link", "--offline", "-vv")
        cold = run(*split, "--", "a b", "--split-link")
        assert cold.stdout.strip() == '11 yes ["a b", "--split-link"]', cold.stdout
        image, = images()
        inode = image.stat().st_ino
        assert image.name in links()
        unchanged = run(*split)
        assert "Fresh split_app" in unchanged.stderr, unchanged.stderr

        source.write_text(SOURCE.replace("+ 1", "+ 2"), encoding="utf-8")
        warm = run(*split, "--", "fail", status=23)
        assert warm.stdout.strip() == '12 yes ["fail"]', warm.stdout
        assert images() == {image} and image.stat().st_ino == inode
        assert image.name in links()

        dep.write_text("pub fn value() -> u32 { 30 }\n", encoding="utf-8")
        assert run(*split).stdout.strip() == "32 yes []"
        updated, = images() - {image}
        assert updated.name in links() and image.is_file()

        updated.unlink()
        assert run(*split).stdout.strip() == "32 yes []"
        assert updated.is_file()

        native = run("run", "--offline", "--", "--split-link")
        assert native.stdout.strip() == '32 yes ["--split-link"]'
        assert "lib__naspeed_run_dependencies" not in links()
        assert run(*split, "--release").stdout.strip() == "32 yes []"
        assert "lib__naspeed_run_dependencies" not in links("release")
        print("split link reuse, dependency invalidation, missing-image recovery, Cargo runner and native run verified")


if __name__ == "__main__":
    main(*sys.argv[1:])
