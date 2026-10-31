"""Split one cargo target/ dir into registry deps, workspace deps, incremental,
build-script OUT_DIR and other, so the 25 GB goal can be checked against the
part naspeed cannot share (workspace crates and incremental state).

Usage: python3 -I target_components.py <target-dir>
"""

import json
import os
import re
import subprocess
import sys

HASHED = re.compile(r"^(?:lib)?(?P<name>.+?)-[0-9a-f]{16}(?:\..*)?$")


def blocks(path):
    total = 0
    for root, dirs, files in os.walk(path):
        for name in files + dirs:
            try:
                total += os.lstat(os.path.join(root, name)).st_blocks * 512
            except OSError:
                pass
    return total


def workspace_names(workspace_root):
    try:
        out = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"],
            cwd=workspace_root, capture_output=True, text=True, check=True,
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return set()
    names = set()
    for pkg in json.loads(out)["packages"]:
        names.add(pkg["name"].replace("-", "_"))
        for target in pkg["targets"]:
            names.add(target["name"].replace("-", "_"))
    return names


def entry_size(path):
    st = os.lstat(path)
    return blocks(path) if os.path.isdir(path) and not os.path.islink(path) else st.st_blocks * 512


def main(target):
    target = os.path.abspath(target)
    ws = workspace_names(os.path.dirname(target))
    split = {"registry": 0, "workspace": 0, "incremental": 0, "build_out": 0, "other": 0}

    for root, dirs, _files in os.walk(target):
        # A profile dir is any dir that directly holds deps/ (target/<profile> or target/<triple>/<profile>).
        if "deps" not in dirs:
            continue
        for sub in list(dirs):
            path = os.path.join(root, sub)
            if sub == "incremental":
                split["incremental"] += blocks(path)
            elif sub in ("deps", ".fingerprint"):
                for entry in os.listdir(path):
                    m = HASHED.match(entry)
                    name = m.group("name").replace("-", "_") if m else None
                    bucket = "workspace" if name in ws else "registry"
                    split[bucket] += entry_size(os.path.join(path, entry))
            elif sub == "build":
                for entry in os.listdir(path):
                    pkg_dir = os.path.join(path, entry)
                    out_dir = os.path.join(pkg_dir, "out")
                    out = blocks(out_dir) if os.path.isdir(out_dir) else 0
                    split["build_out"] += out
                    m = HASHED.match(entry)
                    name = m.group("name").replace("-", "_") if m else None
                    split["workspace" if name in ws else "registry"] += blocks(pkg_dir) - out
            else:
                split["other"] += blocks(path)
        dirs[:] = []  # profile handled; do not descend again

    # Anything outside profile dirs (e.g. doc/, debug/ top-level binaries) counts as other.
    accounted = sum(split.values())
    split["other"] += max(0, blocks(target) - accounted)
    gb = {k: round(v / 2**30, 2) for k, v in split.items()}
    print(f"{target}: " + " ".join(f"{k}={v}GB" for k, v in gb.items())
          + f" workspace_crates={len(ws)}")


if __name__ == "__main__":
    main(sys.argv[1])
