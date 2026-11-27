#!/usr/bin/env python3
"""Verify cooperative live Cargo app patches in a long-running process."""
import os
from pathlib import Path
import queue
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time

from test_function_codegen import tool


SOURCE = '''\
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::{self, JoinHandle};

static RUN_STATE: AtomicI32 = AtomicI32::new(0);
static BLOCK_ENTERED: AtomicBool = AtomicBool::new(false);
static BLOCK_RELEASE: AtomicBool = AtomicBool::new(false);
static PANIC_ENTERED: AtomicBool = AtomicBool::new(false);
static PANIC_RELEASE: AtomicBool = AtomicBool::new(false);

#[inline(never)]
fn selected_value() -> i32 {
    10
}

#[inline(never)]
fn blocked_value() -> i32 {
    BLOCK_ENTERED.store(true, Ordering::SeqCst);
    while !BLOCK_RELEASE.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    11
}

#[inline(never)]
fn blocked_panic() {
    PANIC_ENTERED.store(true, Ordering::SeqCst);
    while !PANIC_RELEASE.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    panic!("panic from selected frame: {}", std::hint::black_box(1));
}

#[inline(never)]
fn unselected_value() -> i32 {
    7
}

fn main() {
    let pid = std::process::id();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut blocked: Option<JoinHandle<i32>> = None;
    let mut panicking: Option<JoinHandle<bool>> = None;

    writeln!(out, "READY {pid}").unwrap();
    out.flush().unwrap();
    for line in stdin.lock().lines() {
        match line.unwrap().as_str() {
            "probe" => {
                let state = RUN_STATE.fetch_add(1, Ordering::SeqCst) + 1;
                writeln!(out, "PROBE {pid} {state} {} {}", selected_value(), unselected_value()).unwrap();
            }
            "block" => {
                BLOCK_RELEASE.store(false, Ordering::SeqCst);
                BLOCK_ENTERED.store(false, Ordering::SeqCst);
                blocked = Some(thread::spawn(blocked_value));
                while !BLOCK_ENTERED.load(Ordering::SeqCst) {
                    thread::yield_now();
                }
                writeln!(out, "BLOCKED {pid}").unwrap();
            }
            "release" => {
                BLOCK_RELEASE.store(true, Ordering::SeqCst);
                let value = blocked.take().unwrap().join().unwrap();
                writeln!(out, "RELEASED {pid} {value}").unwrap();
            }
            "block-panic" => {
                PANIC_RELEASE.store(false, Ordering::SeqCst);
                PANIC_ENTERED.store(false, Ordering::SeqCst);
                panicking = Some(thread::spawn(|| {
                    std::panic::catch_unwind(blocked_panic).is_err()
                }));
                while !PANIC_ENTERED.load(Ordering::SeqCst) {
                    thread::yield_now();
                }
                writeln!(out, "PANIC-BLOCKED {pid}").unwrap();
            }
            "release-panic" => {
                PANIC_RELEASE.store(true, Ordering::SeqCst);
                let unwound = panicking.take().unwrap().join().unwrap();
                writeln!(out, "PANIC-CAUGHT {pid} {unwound}").unwrap();
            }
            "quit" => {
                writeln!(out, "BYE {pid}").unwrap();
                out.flush().unwrap();
                break;
            }
            command => writeln!(out, "UNKNOWN {command}").unwrap(),
        }
        out.flush().unwrap();
    }
}
'''

EDITED = (
    SOURCE.replace("    10\n}", "    20\n}")
    .replace("    11\n}", "    21\n}")
    .replace("black_box(1)", "black_box(2)")
)
REEDITED = (
    EDITED.replace("    20\n}", "    30\n}")
    .replace("    21\n}", "    31\n}")
    .replace("black_box(2)", "black_box(3)")
)
INCOMPATIBLE = REEDITED.replace("    7\n}", "    700\n}")
SELECTED = "selected_value,blocked_value,blocked_panic"
WATCHING = "naspeed app live watching: "
APPLIED = re.compile(r"naspeed app live applied: ([0-9]+) us")
REJECTED = "naspeed app live rejected: "
RECLAIMED = re.compile(r"naspeed app live reclaimed: ([0-9]+) revisions")


class Lines:
    def __init__(self, stream):
        self._queue = queue.Queue()
        self.seen = []
        self._thread = threading.Thread(target=self._read, args=(stream,), daemon=True)
        self._thread.start()

    def _read(self, stream):
        for line in stream:
            line = line.rstrip("\n")
            self.seen.append(line)
            self._queue.put(line)
        self._queue.put(None)

    def wait(self, predicate, timeout=90):
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"timed out waiting for output; saw {self.seen!r}")
            try:
                line = self._queue.get(timeout=remaining)
            except queue.Empty as error:
                raise AssertionError(f"timed out waiting for output; saw {self.seen!r}") from error
            if line is None:
                raise AssertionError(f"output closed while waiting; saw {self.seen!r}")
            if predicate(line):
                return line


def main(naspeed, driver, toolchain):
    naspeed = Path(naspeed).resolve(strict=True)
    driver = Path(driver).absolute()
    with tempfile.TemporaryDirectory(prefix="naspeed-app-live-") as directory:
        root = Path(directory).resolve()
        ns = root / "ns"
        ns.symlink_to(naspeed)
        (root / "src").mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname="app_live_fixture"\nversion="0.0.0"\nedition="2024"\n'
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
            NASPEED_LAZY_APP_PATCH=SELECTED,
            NASPEED_LAZY_APP_LIVE="1",
            CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=str(runner),
            NO_COLOR="1",
        )
        process = subprocess.Popen(
            [ns, "run", "--lazy-ir", "--offline", "--bin", "app_live_fixture", "-vv"],
            cwd=root,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            start_new_session=True,
        )
        assert process.stdin is not None and process.stdout is not None and process.stderr is not None
        stdout = Lines(process.stdout)
        stderr = Lines(process.stderr)

        def send(command, expected):
            assert process.poll() is None, (process.returncode, stdout.seen, stderr.seen)
            process.stdin.write(command + "\n")
            process.stdin.flush()
            return stdout.wait(lambda line: line == expected)

        def wait_stderr_events(events):
            remaining = dict(events)
            matched = {}
            while remaining:
                line = stderr.wait(
                    lambda candidate: any(
                        predicate(candidate) for predicate in remaining.values()
                    )
                )
                name = next(
                    name for name, predicate in remaining.items() if predicate(line)
                )
                matched[name] = line
                del remaining[name]
            return matched

        def loaded_app_images():
            result = subprocess.run(
                ["vmmap", str(pid)],
                capture_output=True,
                text=True,
                timeout=15,
            )
            assert result.returncode == 0, (result.stdout, result.stderr)
            return [
                line for line in result.stdout.splitlines()
                if line.startswith("__TEXT")
                and "app.dylib" in line
                and re.search(r"\br-x/", line)
            ]

        def assert_mapped_images(count):
            images = loaded_app_images()
            assert len(images) == count, images

        build_env = env.copy()
        build_env.pop("NASPEED_LAZY_APP_LIVE")

        def publish(text, succeeds=True):
            source.write_text(text)
            result = subprocess.run(
                [
                    ns,
                    "build",
                    "--lazy-ir",
                    "--offline",
                    "-p",
                    "app_live_fixture",
                    "--bin",
                    "app_live_fixture",
                    "-vv",
                ],
                cwd=root,
                env=build_env,
                capture_output=True,
                text=True,
                timeout=90,
            )
            if succeeds:
                assert result.returncode == 0, (result.stdout, result.stderr)
            else:
                assert result.returncode != 0, (result.stdout, result.stderr)
            return result

        try:
            ready = stdout.wait(lambda line: line.startswith("READY "))
            pid = int(ready.split()[1])
            watching = stderr.wait(lambda line: line.startswith(WATCHING))
            feed = Path(watching.removeprefix(WATCHING))
            assert feed.is_absolute(), watching
            original_plan = feed.read_bytes()

            send("probe", f"PROBE {pid} 1 10 7")
            send("block", f"BLOCKED {pid}")
            send("block-panic", f"PANIC-BLOCKED {pid}")
            slots = len(SELECTED.split(","))
            initial_images = 1 + slots  # Base and one dylib per slot.
            assert_mapped_images(initial_images)

            publish(EDITED)
            applied = stderr.wait(lambda line: APPLIED.fullmatch(line) is not None)
            assert int(APPLIED.fullmatch(applied).group(1)) >= 0
            assert_mapped_images(initial_images + slots)

            publish(REEDITED)
            events = wait_stderr_events(
                {
                    "applied": lambda line: APPLIED.fullmatch(line) is not None,
                    "reclaimed": lambda line: RECLAIMED.fullmatch(line) is not None,
                }
            )
            assert int(RECLAIMED.fullmatch(events["reclaimed"]).group(1)) == 1
            assert_mapped_images(initial_images + slots)
            send("probe", f"PROBE {pid} 2 30 7")

            send("release", f"RELEASED {pid} 11")
            assert_mapped_images(initial_images + slots)
            send("release-panic", f"PANIC-CAUGHT {pid} true")
            events = wait_stderr_events(
                {
                    "panic": lambda line: line == "panic from selected frame: 1",
                    "reclaimed": lambda line: RECLAIMED.fullmatch(line) is not None,
                }
            )
            assert int(RECLAIMED.fullmatch(events["reclaimed"]).group(1)) == 1
            assert_mapped_images(initial_images)
            send("probe", f"PROBE {pid} 3 30 7")

            publish(INCOMPATIBLE)
            events = wait_stderr_events(
                {
                    "applied": lambda line: APPLIED.fullmatch(line) is not None,
                    "reclaimed": lambda line: RECLAIMED.fullmatch(line) is not None,
                }
            )
            # A rebuilt base is mapped alongside the old one; the old main
            # loop still reads its own globals, but selected calls land on
            # the new image. One extra base stays mapped for the process.
            assert_mapped_images(initial_images + 1)
            send("probe", f"PROBE {pid} 4 30 7")

            publish(REEDITED + "\nthis is not valid Rust\n", succeeds=False)
            send("probe", f"PROBE {pid} 5 30 7")

            # A valid first candidate followed by a missing second image
            # must not change any slot in the running app.
            candidate = original_plan.split(b"\0")
            candidate[4] = os.fsencode(root / "missing.dylib")
            stage = root / "damaged-publication"
            stage.write_bytes(b"\0".join(candidate))
            stage.replace(feed)
            stderr.wait(lambda line: line == REJECTED + "function image missing, damaged or not loadable")
            send("probe", f"PROBE {pid} 6 30 7")

            publish(SOURCE)
            stderr.wait(lambda line: APPLIED.fullmatch(line) is not None)
            send("probe", f"PROBE {pid} 7 10 7")

            publish(EDITED)
            stderr.wait(lambda line: APPLIED.fullmatch(line) is not None)
            send("probe", f"PROBE {pid} 8 20 7")

            send("quit", f"BYE {pid}")
            assert process.wait(timeout=15) == 0, (stdout.seen, stderr.seen)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)

        print(
            "Cargo app live patches: same process, preserved state, pinned old frames, "
            "panic unwind, eager reclamation, base rebase, damaged publication, failed build, rollback and re-edit verified"
        )


if __name__ == "__main__":
    main(*sys.argv[1:])
