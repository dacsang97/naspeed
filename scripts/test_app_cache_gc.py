#!/usr/bin/env python3
"""Exercise live native-image disk GC via actual Cargo builds and launchers."""
import fcntl
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile

from test_app_live import APPLIED, EDITED, Lines, REEDITED, SELECTED, SOURCE, WATCHING
from test_function_codegen import tool


def main(naspeed, driver, toolchain):
    with tempfile.TemporaryDirectory(prefix="naspeed-app-gc-") as temp:
        root = Path(temp).resolve()
        ns = root / "ns"
        ns.symlink_to(Path(naspeed).resolve(strict=True))
        (root / "Cargo.toml").write_text(
            '[package]\nname="app_gc_fixture"\nversion="0.0.0"\nedition="2024"\n'
        )
        (root / "src").mkdir()
        source = root / "src/main.rs"
        secondary = root / "src/bin/secondary.rs"
        secondary.parent.mkdir()
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("CARGO_", "RUSTC_", "NASPEED_"))
               and k not in ("RUSTFLAGS", "RUSTUP_TOOLCHAIN")}
        env.update(CARGO=tool(toolchain, "cargo"), RUSTC=tool(toolchain, "rustc"),
                   CARGO_HOME=str(root / "cargo-home"), RUSTC_WRAPPER="",
                   NASPEED_LAZY_RUN_DRIVER=str(Path(driver).absolute()),
                   NASPEED_LAZY_APP_PATCH=SELECTED, NASPEED_LAZY_RUN_REPORT="1",
                   NASPEED_LAZY_APP_CACHE_KEEP="1")
        target = root / "target/debug/app_gc_fixture"

        def build(text, name="app_gc_fixture"):
            (secondary if name == "secondary" else source).write_text(text)
            result = subprocess.run(
                [ns, "build", "--lazy-ir", "--offline", "--bin", name, "-vv"],
                cwd=root,
                env=env,
                text=True, capture_output=True, timeout=90,
            )
            assert result.returncode == 0, result.stderr
            launcher = target.with_name(name)
            manifest = Path(shlex.split(launcher.read_text().splitlines()[-1])[2])
            plan = (manifest.parent / "patch-plan").read_bytes().split(b"\0")
            return manifest, tuple(Path(os.fsdecode(path)) for path in plan[0:-1:2])

        def copy_launcher(name):
            launcher = root / name
            launcher.write_bytes(target.read_bytes())
            launcher.chmod(0o755)
            return launcher

        def probe(launcher):
            return subprocess.run([launcher], cwd=root, env=env, input="probe\nquit\n",
                                  text=True, capture_output=True, timeout=20)

        first, original = build(SOURCE)
        other, other_images = build(EDITED, "secondary")
        saved = copy_launcher("first")
        guest = subprocess.Popen([target], cwd=root, env=env | {"NASPEED_LAZY_APP_LIVE": "1"},
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, bufsize=1, start_new_session=True)
        stdout, stderr = Lines(guest.stdout), Lines(guest.stderr)
        try:
            pid = stdout.wait(lambda t: t.startswith("READY ")).split()[1]
            stderr.wait(lambda t: t.startswith(WATCHING))
            second, old = build(EDITED)
            stderr.wait(lambda t: APPLIED.fullmatch(t) is not None)
            stale = copy_launcher("second")

            # Simulate another publisher holding the shared cache transaction.
            lock = first.parent.parent / "patch-gc.lock"
            with lock.open("rb") as concurrent:
                fcntl.flock(concurrent, fcntl.LOCK_SH)
                third, new = build(REEDITED)
                stderr.wait(lambda t: APPLIED.fullmatch(t) is not None)
                assert second.is_file() and all(p.is_file() for p in old)

            # The next successful build sweeps, while the live guest still
            # pins the first generation. Source rollback reuses its image.
            newest, rolled_back = build(SOURCE)
            stderr.wait(lambda t: APPLIED.fullmatch(t) is not None)
            assert rolled_back == original and newest.is_file() and first.is_file()
            assert not second.exists() and not third.exists()
            assert original[0] == old[0] == new[0]
            assert all(not p.exists() for p in (*old[1:], *new[1:])), (old, new)
            assert all(p.is_file() for p in original)
            expired = probe(stale)
            assert expired.returncode != 0 and "expired" in expired.stderr, expired
            still_pinned = probe(saved)
            assert still_pinned.returncode == 0 and " 1 10 7" in still_pinned.stdout, still_pinned
            guest.stdin.write("probe\nquit\n")
            guest.stdin.flush()
            stdout.wait(lambda t: t == f"PROBE {pid} 1 10 7")
            assert guest.wait(timeout=10) == 0

            # The next edit makes the rolled-back revision unreachable.
            _, rebuilt = build(REEDITED)
            assert rebuilt == new and all(p.is_file() for p in rebuilt)
            assert not first.exists() and all(not p.exists() for p in original[1:])
            assert other.is_file() and all(p.is_file() for p in other_images)
            second_bin = probe(target.with_name("secondary"))
            assert second_bin.returncode == 0 and " 1 20 7" in second_bin.stdout, second_bin
            assert probe(saved).returncode != 0
            now = probe(target)
            assert now.returncode == 0 and " 1 30 7" in now.stdout, now
        finally:
            if guest.poll() is None:
                os.killpg(guest.pid, signal.SIGKILL)
                guest.wait(timeout=10)
        print("Cargo app disk GC: lease, concurrent writer, rollback, rebuild and expired launcher verified")


if __name__ == "__main__":
    main(*sys.argv[1:])
