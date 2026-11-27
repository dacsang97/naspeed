#!/usr/bin/env python3
"""Benchmark harness for the naspeed action engine (BenchmarkRun schema_version "1").

Subcommands:
  collect   run a command N times under a scenario and emit one BenchmarkRun JSON record
  floor     measure a Go workspace `go test` runtime floor (build vs test time)
  compare   diff two BenchmarkRun records per metric (PASS/FAIL/UNSUPPORTED)

Scenarios (collect --scenario):
  no-op                  command run with no source edit; output_digest must be identical
  leaf-edit              append a unique marker to a leaf file before each measured run
  core-edit              append a unique marker to a widely-imported file before each run
  warm-cache-cold-runner warm target/cache dir but no persistent runner state
  three-worktree         build N sibling worktree/copies, record per-root allocated bytes
  go-test-floor          go build vs go test floor split (see also the `floor` subcommand)
  env                    environment snapshot only (also emitted by measure_baseline.sh env)

Every metric that cannot be measured on this platform is emitted as
{"status": "unsupported", "reason": "..."} rather than dropped.
"""
import argparse
import copy
import glob
import hashlib
import json
import os
import platform
import re
import resource
import shutil
import socket
import subprocess
import sys
import tempfile
import time

SCHEMA_VERSION = "1"
SCENARIOS = [
    "no-op", "leaf-edit", "core-edit",
    "warm-cache-cold-runner", "three-worktree",
    "go-test-floor", "env",
]
ENV_ALLOWLIST_PREFIXES = ("RUSTC_WRAPPER", "CARGO_", "NASPEED_", "GOWORK")
DEFAULT_DB_URL = "postgres://postgres:postgres@127.0.0.1:5432/postgres"

EDIT_SCENARIOS = {"leaf-edit", "core-edit"}


# ---------------------------------------------------------------- helpers

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def hash16(value):
    return hashlib.sha256(value.encode("utf-8", "surrogateescape")).hexdigest()[:16]


def unsupported(reason):
    return {"status": "unsupported", "reason": reason}


def run_cmd(argv, cwd=None, env=None, capture=True):
    kw = {"cwd": cwd, "env": env}
    if capture:
        kw.update(stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                  errors="replace")
    return subprocess.run(argv, **kw)


def tool_version(argv, cwd=None):
    try:
        p = run_cmd(argv, cwd=cwd)
        return (p.stdout or "").strip().splitlines()[0] if p.returncode == 0 else None
    except OSError:
        return None


def git_info(repo):
    rev = run_cmd(["git", "-C", repo, "rev-parse", "HEAD"])
    dirty = run_cmd(["git", "-C", repo, "status", "--porcelain"])
    return {
        "path": os.path.abspath(repo),
        "revision": rev.stdout.strip() if rev.returncode == 0 else "unknown",
        "dirty": bool(dirty.stdout and dirty.stdout.strip()) if dirty.returncode == 0 else True,
    }


def filesystem_of(path):
    sysname = platform.system()
    try:
        if sysname == "Darwin":
            p = run_cmd(["diskutil", "info", path])
            m = re.search(r"Type \(Bundle\):\s*(\S+)", p.stdout or "")
            if m:
                return m.group(1).lower()  # apfs, hfs, ...
            st = run_cmd(["stat", "-f", "%T", path])
            if st.returncode == 0 and st.stdout.strip():
                return st.stdout.strip().lower()
        else:
            p = run_cmd(["df", "-T", path])
            lines = (p.stdout or "").splitlines()
            if len(lines) >= 2:
                return lines[1].split()[1].lower()
            p = run_cmd(["stat", "-f", "-c", "%T", path])
            if p.returncode == 0 and p.stdout.strip():
                return p.stdout.strip().lower()
    except OSError:
        pass
    return "unknown"


def env_allowlist():
    out = {}
    for k, v in sorted(os.environ.items()):
        if any(k == p or k.startswith(p) for p in ENV_ALLOWLIST_PREFIXES):
            out[k] = hash16(v)
    return out


def toolchains(repo=None):
    return {
        "rustc": tool_version(["rustc", "-V"]) or "absent",
        "cargo": tool_version(["cargo", "-V"]) or "absent",
        "go": tool_version(["go", "version"]) or "absent",
        "ns": tool_version(["ns", "--version"]) or "absent",
    }


def environment_block(repo):
    sysname = platform.system().lower()
    osname = {"darwin": "macos", "linux": "linux", "windows": "windows"}.get(sysname, sysname)
    return {
        "os": osname,
        "os_version": platform.release(),
        "arch": platform.machine().lower() or "unknown",
        "filesystem": filesystem_of(repo or os.getcwd()),
        "toolchains": toolchains(repo),
        "env_allowlist": env_allowlist(),
    }


def lockfile_digest(repo):
    h = hashlib.sha256()
    found = False
    for name in ("Cargo.lock", "go.sum", "go.mod", "rust-toolchain.toml"):
        p = os.path.join(repo, name)
        if os.path.isfile(p):
            found = True
            h.update(name.encode())
            h.update(sha256_file(p).encode())
    return h.hexdigest() if found else "none"


# ---------------------------------------------------------------- output dir

def git_ignored(repo, rel):
    p = subprocess.run(["git", "-C", repo, "check-ignore", "-q", rel])
    return p.returncode == 0


def filesystem_of(path):
    sysname = platform.system()
    try:
        # mount point for path, then its type via diskutil or `mount`
        mp = run_cmd(["df", path])
        lines = (mp.stdout or "").splitlines()
        mountpoint = lines[1].split()[-1] if len(lines) >= 2 else path
        if sysname == "Darwin":
            p = run_cmd(["diskutil", "info", mountpoint])
            m = re.search(r"Type \(Bundle\):\s*(\S+)", p.stdout or "")
            if m:
                return m.group(1).lower()  # apfs, hfs, ...
            mo = run_cmd(["mount"])
            for line in (mo.stdout or "").splitlines():
                if f" on {mountpoint} (" in line:
                    m = re.search(r"\((\w+)", line)
                    if m:
                        return m.group(1).lower()
        else:
            p = run_cmd(["df", "-T", path])
            lines = (p.stdout or "").splitlines()
            if len(lines) >= 2:
                return lines[1].split()[1].lower()
            p = run_cmd(["stat", "-f", "-c", "%T", path])
            if p.returncode == 0 and p.stdout.strip():
                return p.stdout.strip().lower()
    except OSError:
        pass
    return "unknown"


def resolve_outdir(repo, explicit):
    """Default <repo>/.bench only if gitignored; refuse non-ignored in-repo dirs."""
    if explicit:
        d = os.path.abspath(explicit)
        in_repo = os.path.commonpath([d, os.path.abspath(repo)]) == os.path.abspath(repo)
        if in_repo and os.path.isdir(os.path.join(repo, ".git")):
            rel = os.path.relpath(d, repo)
            if not git_ignored(repo, rel):
                raise SystemExit(
                    f"refusing to write reports into non-ignored in-repo dir: {d}\n"
                    f"add '{rel}/' to {repo}/.gitignore or point --out elsewhere")
        os.makedirs(d, exist_ok=True)
        return d
    cand = os.path.join(repo, ".bench")
    if os.path.isdir(os.path.join(repo, ".git")) and git_ignored(repo, ".bench"):
        os.makedirs(cand, exist_ok=True)
        return cand
    env = os.environ.get("NASPEED_BENCH_OUT")
    if env:
        os.makedirs(env, exist_ok=True)
        return os.path.abspath(env)
    return tempfile.mkdtemp(prefix="naspeed-bench-")


# ---------------------------------------------------------------- metrics

def tree_pids(root):
    """Descendants of root pid (inclusive) via ps pid/ppid table."""
    try:
        p = run_cmd(["ps", "-axo", "pid=,ppid="])
    except OSError:
        return {root}
    kids = {}
    for line in (p.stdout or "").splitlines():
        parts = line.split()
        if len(parts) == 2:
            try:
                pid, ppid = int(parts[0]), int(parts[1])
            except ValueError:
                continue
            kids.setdefault(ppid, []).append(pid)
    out, stack = set(), [root]
    while stack:
        pid = stack.pop()
        if pid in out:
            continue
        out.add(pid)
        stack.extend(kids.get(pid, []))
    return out


def rss_bytes(pid):
    try:
        p = run_cmd(["ps", "-o", "rss=", "-p", str(pid)])
        return int(p.stdout.strip() or 0) * 1024
    except (OSError, ValueError):
        return 0


def measure_run(argv, cwd, env):
    """Run argv once; return (wall_s, cpu_s, peak_rss_bytes, output_text, returncode)."""
    ru0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.monotonic()
    proc = subprocess.Popen(argv, cwd=cwd, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, errors="replace",
                            start_new_session=True)
    peak = 0
    while proc.poll() is None:
        pids = tree_pids(proc.pid)
        total = sum(rss_bytes(p) for p in pids)
        peak = max(peak, total)
        time.sleep(0.05)
    out = proc.stdout.read() if proc.stdout else ""
    wall = time.monotonic() - t0
    ru1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime)
    return wall, cpu, peak, out or "", proc.returncode


def artifacts_for(repo, patterns, tmp_bin=None):
    files = []
    if tmp_bin:
        files.append(tmp_bin)
    pats = patterns or []
    if not pats:
        # auto: executables directly under target/debug (top 2 levels)
        for pat in ("target/debug/*", "target/debug/deps/*"):
            for p in sorted(glob.glob(os.path.join(repo, pat))):
                if (os.path.isfile(p) and os.access(p, os.X_OK)
                        and not p.endswith((".d", ".rlib", ".rmeta"))):
                    pats.append(os.path.relpath(p, repo))
                    break  # only first match per glob level keeps it cheap
    for pat in pats:
        for p in sorted(glob.glob(os.path.join(repo, pat))):
            if os.path.isfile(p):
                files.append(p)
    seen, uniq = set(), []
    for f in files:
        f = os.path.abspath(f)
        if f not in seen:
            seen.add(f)
            uniq.append(f)
    return uniq


def artifact_bytes(files):
    logical = physical = 0
    for f in files:
        try:
            st = os.stat(f)
        except OSError:
            continue
        logical += st.st_size
        physical += st.st_blocks * 512  # APFS-accurate allocation; du is not
    return logical, physical


def output_digest(files):
    h = hashlib.sha256()
    for f in files:
        h.update(os.path.basename(f).encode())
        h.update(sha256_file(f).encode())
    return h.hexdigest()


def parse_test_summary(output):
    """Serialize go test per-package outcomes; else unsupported."""
    results = {}
    for line in output.splitlines():
        m = re.match(r"^(ok|FAIL|---)\s+(\S+)", line)
        if m and "/" in m.group(2) or (m and "." in m.group(2)):
            results[m.group(2)] = m.group(1)
        m2 = re.match(r"^(ok|FAIL)\s+(\S+)", line)
        if m2:
            results[m2.group(2)] = m2.group(1)
    if not results:
        return {"status": "unsupported",
                "reason": "no go-test-style package result lines in output",
                "digest": None}
    blob = json.dumps(results, sort_keys=True)
    status = "pass" if all(v == "ok" for v in results.values()) else "fail"
    return {"status": status, "digest": hashlib.sha256(blob.encode()).hexdigest(),
            "packages": results}


def parse_cache_counters(output):
    hits = re.findall(r"cache hits?[:\s]+(\d+)", output, re.I)
    misses = re.findall(r"cache miss(?:es)?[:\s]+(\d+)", output, re.I)
    return (int(hits[-1]) if hits else None, int(misses[-1]) if misses else None)


def median_metrics(samples):
    keys = ["wall_s", "cpu_s", "peak_rss_bytes", "logical_bytes",
            "allocated_physical_bytes", "targets_restarted", "targets_tested"]
    valid = [s["metrics"] for s in samples if s["valid"] and not s["warmup"]]
    if not valid:
        valid = [s["metrics"] for s in samples if s["valid"]]

    def med(vals):
        vals = sorted(vals)
        n = len(vals)
        return vals[n // 2] if n % 2 else (vals[n // 2 - 1] + vals[n // 2]) / 2

    m = {}
    for k in keys:
        vals = [v[k] for v in valid if isinstance(v.get(k), (int, float))]
        m[k] = med(vals) if vals else unsupported("no valid numeric samples")
    for k in ("network_bytes", "cache_hits", "cache_misses"):
        m[k] = unsupported("not measurable per-process on this platform")
    return m


# ---------------------------------------------------------------- collect

def apply_edit(repo, edit_file, marker):
    path = os.path.join(repo, edit_file)
    with open(path, "a") as f:
        f.write(marker)


def restore_file(repo, edit_file, original):
    path = os.path.join(repo, edit_file)
    with open(path, "wb") as f:
        f.write(original)


def make_worktrees(repo, n, outdir):
    """N sibling copies at same revision; git worktrees when possible."""
    roots = []
    base = os.path.join(outdir, "worktrees")
    os.makedirs(base, exist_ok=True)
    is_git = os.path.isdir(os.path.join(repo, ".git"))
    rev = git_info(repo)["revision"]
    for i in range(n):
        dest = os.path.join(base, f"wt{i}")
        if os.path.exists(dest):
            shutil.rmtree(dest, ignore_errors=True)
            subprocess.run(["git", "-C", repo, "worktree", "remove", "--force", dest],
                           capture_output=True)
        if is_git:
            p = subprocess.run(["git", "-C", repo, "worktree", "add", "--detach", dest, rev],
                               capture_output=True, text=True)
            if p.returncode != 0:
                shutil.copytree(repo, dest, symlinks=True,
                                ignore=shutil.ignore_patterns(".git", "target", "node_modules"))
        else:
            shutil.copytree(repo, dest, symlinks=True,
                            ignore=shutil.ignore_patterns(".git", "target", "node_modules"))
        roots.append(dest)
    return roots


def dir_physical_bytes(root):
    total = 0
    for dirpath, _dirs, files in os.walk(root):
        for name in files:
            try:
                total += os.lstat(os.path.join(dirpath, name)).st_blocks * 512
            except OSError:
                pass
    return total


def cmd_collect(a):
    repo = os.path.abspath(a.repo)
    if not os.path.isdir(repo):
        raise SystemExit(f"not a directory: {repo}")
    outdir = resolve_outdir(repo, a.out)
    print(f"output_dir {outdir}", file=sys.stderr)

    edit_file = a.edit_file
    if a.scenario == "leaf-edit" and not edit_file:
        edit_file = a.rust_leaf if os.path.isfile(os.path.join(repo, a.rust_leaf)) else a.go_leaf
    if a.scenario == "core-edit" and not edit_file:
        edit_file = a.rust_core if os.path.isfile(os.path.join(repo, a.rust_core)) else a.go_core
    original = None
    if edit_file:
        p = os.path.join(repo, edit_file)
        if not os.path.isfile(p):
            raise SystemExit(f"edit file not found: {p}")
        with open(p, "rb") as f:
            original = f.read()

    last_out = ""

    def snapshot_tag():
        return json.dumps({"toolchains": toolchains(repo),
                           "lockfile": lockfile_digest(repo),
                           "command": a.command}, sort_keys=True)

    tag0 = snapshot_tag()
    samples = []
    tmp_bin = None
    worktree_roots = []
    try:
        if a.scenario == "three-worktree" and a.worktrees == 0:
            a.worktrees = 3
        if a.worktrees:
            worktree_roots = make_worktrees(repo, a.worktrees, outdir)

        total = a.warmup + a.samples
        for i in range(total):
            warmup = i < a.warmup
            valid, reason = True, None
            if snapshot_tag() != tag0:
                valid = False
                reason = "toolchain/lockfile/command changed mid-run"
            if edit_file and not warmup:
                marker = ("//" if edit_file.endswith((".rs", ".go", ".c", ".cc",
                          ".cpp", ".h", ".js", ".ts")) else "#")
                apply_edit(repo, edit_file,
                           f"\n{marker} {a.edit_marker} sample {i} {time.time_ns()}\n")
            wall, cpu, peak, out, rc = measure_run(a.command, repo, os.environ.copy())
            if rc != 0:
                valid = False
                reason = f"command exited {rc}"
            hits, misses = parse_cache_counters(out)
            files = artifacts_for(repo, a.artifacts, tmp_bin)
            logical, physical = artifact_bytes(files)
            metrics = {
                "wall_s": round(wall, 4),
                "cpu_s": round(cpu, 4),
                "peak_rss_bytes": peak,
                "network_bytes": unsupported("no per-process network counter without root"),
                "logical_bytes": logical,
                "allocated_physical_bytes": physical,
                "cache_hits": hits if hits is not None else unsupported("no cache counters in tool output"),
                "cache_misses": misses if misses is not None else unsupported("no cache counters in tool output"),
                "targets_restarted": unsupported("not reported by tool output"),
                "targets_tested": unsupported("not reported by tool output"),
            }
            samples.append({"index": i, "warmup": warmup, "valid": valid,
                            "invalid_reason": reason, "metrics": metrics})
            last_out = out
            if edit_file and not warmup:
                restore_file(repo, edit_file, original)
    finally:
        if edit_file and original is not None:
            restore_file(repo, edit_file, original)
        wt_alloc = [{"path": r,
                     "allocated_physical_bytes": dir_physical_bytes(r)}
                    for r in worktree_roots]
        for r in worktree_roots:
            subprocess.run(["git", "-C", repo, "worktree", "remove", "--force", r],
                           capture_output=True)
            shutil.rmtree(r, ignore_errors=True)

    files = artifacts_for(repo, a.artifacts, tmp_bin)
    logical, physical = artifact_bytes(files)

    record = {
        "schema_version": SCHEMA_VERSION,
        "scenario": a.scenario,
        "repository": git_info(repo),
        "environment": environment_block(repo),
        "cache_state": {"kind": a.cache_state, "detail": a.cache_detail or ""},
        "command": a.command,
        "samples": samples,
        "median": median_metrics(samples),
        "output_digest": output_digest(files) if files else unsupported("no artifacts found"),
        "test_summary": parse_test_summary(last_out),
        "logical_bytes": logical,
        "allocated_bytes": physical,
        "canonical_objects": unsupported("not yet implemented"),
    }
    if wt_alloc:
        record["worktrees"] = wt_alloc

    out_path = os.path.join(outdir, f"{a.scenario}-{int(time.time())}.json")
    with open(out_path, "w") as f:
        json.dump(record, f, indent=2)
    print(json.dumps(record))
    print(f"wrote {out_path}", file=sys.stderr)
    return 0


# ---------------------------------------------------------------- floor

def probe_postgres(url):
    m = re.match(r"postgres(?:ql)?://([^@/]+)@([^:/]+):(\d+)", url or "")
    host, port = (m.group(2), int(m.group(3))) if m else ("127.0.0.1", 5432)
    try:
        with socket.create_connection((host, port), timeout=3):
            return True, f"{host}:{port}"
    except OSError as e:
        return False, f"{host}:{port} ({e})"


def cmd_floor(a):
    repo = os.path.abspath(a.repo)
    outdir = resolve_outdir(repo, a.out)
    print(f"output_dir {outdir}", file=sys.stderr)
    env = os.environ.copy()
    env["GOWORK"] = "off"
    db_url = os.environ.get("TEST_ADMIN_DATABASE_URL") or DEFAULT_DB_URL
    env["TEST_ADMIN_DATABASE_URL"] = db_url  # in-process only, never persisted
    reachable, where = probe_postgres(db_url)

    t0 = time.monotonic()
    b = subprocess.run(["go", "build", "./..."], cwd=repo, env=env,
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    build_s = time.monotonic() - t0

    test_metrics = {}
    test_summary = {"status": "unsupported",
                    "reason": f"postgres unreachable at {where}", "digest": None}
    test_s = 0.0
    if reachable:
        t0 = time.monotonic()
        t = subprocess.run(["go", "test", "./...", "-race", "-count=1"],
                           cwd=repo, env=env,
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        test_s = time.monotonic() - t0
        test_metrics = {
            "wall_s": round(test_s, 4),
            "output_tail": "\n".join((t.stdout or "").splitlines()[-20:]),
        }
        test_summary = parse_test_summary(t.stdout or "")
        test_summary["status"] = test_summary.get("status") or (
            "pass" if t.returncode == 0 else "fail")
    else:
        test_metrics = unsupported(f"postgres unreachable at {where}")

    total_s = build_s + test_s
    # The mandatory floor is the uncacheable test runtime: `go test -race`
    # against real PostgreSQL cannot be replayed or skipped, so test_s is the
    # floor CI wall time can never go below. build_s is the compressible
    # compile/setup headroom reported separately for the 50% target check.
    floor_s = test_s
    floor = {
        "build_s": round(build_s, 4),
        "test_s": round(test_s, 4) if reachable else test_metrics,
        "total_s": round(total_s, 4),
        "floor_s": round(floor_s, 4) if reachable else test_metrics,
        "floor_share_pct": round(100 * floor_s / total_s, 2) if (total_s and reachable) else 0,
        "compressible_s": round(build_s, 4),
        "postgres_reachable": reachable,
        "build_returncode": b.returncode,
        "build_output_tail": "\n".join((b.stdout or "").splitlines()[-20:]),
    }
    record = {
        "schema_version": SCHEMA_VERSION,
        "scenario": "go-test-floor",
        "repository": git_info(repo),
        "environment": environment_block(repo),
        "cache_state": {"kind": "warm", "detail": "go build run before go test"},
        "command": ["go", "test", "./...", "-race", "-count=1"],
        "samples": [],
        "median": {},
        "floor": floor,
        "test_summary": test_summary,
        "output_digest": unsupported("go test produces no persisted binary to digest"),
        "logical_bytes": 0,
        "allocated_bytes": 0,
        "canonical_objects": unsupported("not yet implemented"),
    }
    out_path = os.path.join(outdir, f"go-test-floor-{int(time.time())}.json")
    with open(out_path, "w") as f:
        json.dump(record, f, indent=2)
    print(json.dumps(record))
    print(f"wrote {out_path}", file=sys.stderr)
    return 0


# ---------------------------------------------------------------- compare

def cmd_compare(a):
    with open(a.baseline) as f:
        base = json.load(f)
    with open(a.candidate) as f:
        cand = json.load(f)
    same_repo = base.get("repository", {}).get("path") == cand.get("repository", {}).get("path")
    same_scn = base.get("scenario") == cand.get("scenario")
    if (not same_repo or not same_scn) and not a.force:
        raise SystemExit(
            "refusing to compare records with different repository.path or scenario "
            f"({base.get('repository', {}).get('path')}/{base.get('scenario')} vs "
            f"{cand.get('repository', {}).get('path')}/{cand.get('scenario')}); "
            "pass --force to override")

    lines, results = [], {}
    for mname in a.metrics.split(","):
        mname = mname.strip()
        b = base.get("median", {}).get(mname)
        c = cand.get("median", {}).get(mname)
        if not isinstance(b, (int, float)) or not isinstance(c, (int, float)):
            status = "UNSUPPORTED"
            detail = {"baseline": b, "candidate": c}
        else:
            delta = c - b
            pct = (100 * delta / b) if b else None
            status = "PASS" if delta <= (a.tolerance_pct / 100.0) * abs(b) else "FAIL"
            detail = {"baseline": b, "candidate": c,
                      "delta": round(delta, 4),
                      "delta_pct": round(pct, 2) if pct is not None else None}
        results[mname] = {"status": status, **detail}
        lines.append(f"{mname}: {status} {json.dumps(detail)}")
    for l in lines:
        print(l)
    print(json.dumps({"schema_version": SCHEMA_VERSION,
                      "baseline": os.path.abspath(a.baseline),
                      "candidate": os.path.abspath(a.candidate),
                      "results": results}))
    return 0 if all(r["status"] in ("PASS", "UNSUPPORTED") for r in results.values()) else 1


# ---------------------------------------------------------------- argparse

def build_parser():
    p = argparse.ArgumentParser(
        prog="measure_action_engine.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="subcommand", required=True)

    c = sub.add_parser("collect", help="run a command under a scenario, emit BenchmarkRun JSON",
                       description=f"Scenarios: {', '.join(SCENARIOS)}. "
                                   "leaf-edit/core-edit append a unique marker to the edit "
                                   "file and restore it afterwards.")
    c.add_argument("--repo", required=True)
    c.add_argument("--scenario", required=True, choices=SCENARIOS)
    c.add_argument("--command", nargs=argparse.REMAINDER, required=True,
                   help="command to measure, e.g. --command cargo build")
    c.add_argument("--samples", type=int, default=5)
    c.add_argument("--warmup", type=int, default=1)
    c.add_argument("--edit-file", help="repo-relative file to edit for *-edit scenarios")
    c.add_argument("--edit-marker", default="bench-marker")
    c.add_argument("--rust-leaf", default="src/main.rs")
    c.add_argument("--rust-core", default="src/lib.rs")
    c.add_argument("--go-leaf", default="pkg/scrub/scrub.go")
    c.add_argument("--go-core", default="internal/config/config.go")
    c.add_argument("--worktrees", type=int, default=0,
                   help="N sibling worktree copies (three-worktree defaults to 3)")
    c.add_argument("--artifacts", nargs="*", default=None,
                   help="glob(s) of output artifacts to digest; default: auto")
    c.add_argument("--cache-state", default="warm",
                   choices=["warm", "cold", "restored", "none"])
    c.add_argument("--cache-detail")
    c.add_argument("--out", help="output dir; default <repo>/.bench if gitignored, "
                               "else $NASPEED_BENCH_OUT, else a temp dir")
    c.set_defaults(func=cmd_collect)

    f = sub.add_parser("floor", help="Go workspace go-test floor: go build vs go test split",
                       description="Measures `go build ./...` (setup floor) separately from "
                                   "`go test ./... -race -count=1` with GOWORK=off. "
                                   "TEST_ADMIN_DATABASE_URL is read from the caller env only.")
    f.add_argument("--repo", required=True)
    f.add_argument("--out")
    f.set_defaults(func=cmd_floor)

    cmp_ = sub.add_parser("compare", help="per-metric PASS/FAIL/UNSUPPORTED diff of two runs")
    cmp_.add_argument("--baseline", required=True)
    cmp_.add_argument("--candidate", required=True)
    cmp_.add_argument("--metrics",
                      default="wall_s,cpu_s,peak_rss_bytes,logical_bytes,allocated_physical_bytes")
    cmp_.add_argument("--tolerance-pct", type=float, default=5.0)
    cmp_.add_argument("--force", action="store_true")
    cmp_.set_defaults(func=cmd_compare)
    return p


def main():
    args = build_parser().parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
