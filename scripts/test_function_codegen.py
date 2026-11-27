#!/usr/bin/env python3
"""Exercise naspeed's real rustc-dev driver on a disposable Cargo project."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile

SOURCE = """\
static BASE: u32 = 7;
#[inline(never)] fn changed() -> u32 { 13 }
#[inline(always)] fn offset(x: u32) -> u32 { x + 7 }
#[inline(never)] fn kept() -> u32 { offset(BASE) + 10 }
fn main() { println!("{}", changed() + kept()); }
#[test] fn real_test() { assert!(changed() + kept() >= 36); }
"""


def tool(toolchain, program):
    return subprocess.check_output(
        ["rustup", "which", "--toolchain", toolchain, program], text=True
    ).strip()


def reuse(output, name):
    rows = output.splitlines()
    prefix = f"NASPEED_FUNCTION fn {name} "
    units = [row.removeprefix(prefix) for row in rows if row.startswith(prefix)]
    assert len(units) == 1, (name, units, output)
    prefix = f"CGU_REUSE {units[0]} "
    hits = [row.removeprefix(prefix) for row in rows if row.startswith(prefix)]
    assert len(hits) == 1, (name, hits, output)
    return units[0], hits[0]


def main(naspeed, driver, toolchain):
    naspeed = Path(naspeed).resolve(strict=True)
    driver = Path(driver).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="naspeed-function-codegen-") as tmp:
        root = Path(tmp).resolve()
        ns = root / "ns"
        ns.symlink_to(naspeed)
        (root / "src").mkdir()
        (root / "store").mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname="function_probe"\nversion="0.0.0"\nedition="2024"\n',
            encoding="utf-8",
        )
        source = root / "src/main.rs"
        source.write_text(SOURCE, encoding="utf-8")
        env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("CARGO_", "RUSTC_", "NASPEED_"))
            and key not in ("RUSTFLAGS", "RUSTUP_TOOLCHAIN")
        }
        env.update(
            CARGO=tool(toolchain, "cargo"),
            RUSTC=tool(toolchain, "rustc"),
            CARGO_HOME=str(root / "cargo-home"),
            CARGO_INCREMENTAL="1",
            RUSTC_WRAPPER=str(naspeed),
            RUSTC_WORKSPACE_WRAPPER=str(driver),
            NASPEED_FUNCTION_CODEGEN_REPORT="1",
            NASPEED_CONFIG_DIR=str(root / "config"),
            NASPEED_STORE=str(root / "store"),
            NASPEED_MIN_FREE_MEM_MB="0",
            NASPEED_NO_AUTO_GC="1",
            NO_COLOR="1",
        )

        for probe in ("-vV", "--print=sysroot", "--print=target-libdir"):
            native = subprocess.check_output([env["RUSTC"], probe], cwd=root, env=env)
            wrapped = subprocess.check_output([driver, env["RUSTC"], probe], cwd=root, env=env)
            assert wrapped == native, (probe, wrapped, native)

        def run(verb, *args, ok=True):
            result = subprocess.run(
                [ns, verb, *args, "--offline"], cwd=root, env=env,
                text=True, capture_output=True, timeout=90,
            )
            assert (result.returncode == 0) == ok, (result.stdout, result.stderr)
            return result.stdout

        cold_build = run("build", "-vv")
        cold_test = run("test", "--no-cache", "-vv")
        assert run("run").strip() == "37"
        assert "test real_test ... ok" in cold_test

        source.write_text(SOURCE.replace("{ 13 }", "{ 15 }"), encoding="utf-8")
        for verb, cold, warm in (
            ("build", cold_build, run("build", "-vv")),
            ("test", cold_test, run("test", "--no-cache", "-vv")),
        ):
            for name, expected in (("changed", "No"), ("kept", "PostLto"), ("main", "PostLto")):
                unit, state = reuse(cold, name)
                assert state == "No", (verb, name, state)
                assert reuse(warm, name) == (unit, expected), (verb, name, warm)
            assert reuse(warm, "changed")[0] != reuse(warm, "kept")[0]
        assert run("run").strip() == "39"

        inline_edit = SOURCE.replace("{ 13 }", "{ 15 }").replace("{ x + 7 }", "{ x + 8 }")
        source.write_text(inline_edit, encoding="utf-8")
        rebuilt = run("build", "-vv")
        assert reuse(rebuilt, "kept")[1] == "No", rebuilt
        assert reuse(rebuilt, "changed")[1] == "PostLto", rebuilt
        assert run("run").strip() == "40"

        release = run("run", "--release", "-vv")
        assert release.strip() == "40", release

        source.write_text(SOURCE.replace("{ 13 }", "{ 01 }"), encoding="utf-8")
        failed = run("test", "--no-cache", ok=False)
        assert "test real_test ... FAILED" in failed, failed

        # wrapping_mul brings rustc's local inline copies into a large native
        # unit. The old driver left that unit whole.
        count = 96
        grouped = "".join(
            f"#[inline(never)] fn f{i}(x: u32) -> u32 {{ x.wrapping_mul({i + 10}) }}\n"
            for i in range(count)
        )
        grouped += 'fn main() { println!("{}", ' + " + ".join(f"f{i}(3)" for i in range(count)) + "); }\n"
        source.write_text(grouped, encoding="utf-8")
        cold = run("build", "-vv")
        units = {reuse(cold, f"f{i}")[0] for i in range(count)}
        assert 1 < len(units) < count // 2, units
        source.write_text(grouped.replace("wrapping_mul(10)", "wrapping_mul(13)"), encoding="utf-8")
        warm = run("build", "-vv")
        changed, state = reuse(warm, "f0")
        assert state == "No", warm
        for i in range(1, count):
            unit, state = reuse(warm, f"f{i}")
            assert state == ("No" if unit == changed else "PostLto"), (i, unit, state)
        assert run("run").strip() == str(3 * (sum(range(10, count + 10)) + 3))

        source.write_text("#[used] static REGISTERED: u32 = 41;\n" + grouped, encoding="utf-8")
        retained = run("build", "-vv")
        assert "NASPEED_FUNCTION fn f0 " not in retained, retained
        assert run("run").strip() == str(3 * sum(range(10, count + 10)))
        print("rustc probe parity, function and shard reuse, inline invalidation, CLI output, release and failing test verified")


if __name__ == "__main__":
    main(*sys.argv[1:])
