#!/usr/bin/env python3
"""verify-release-assets.py <archive> <sha256-sidecar>

Checks a release bottle before it is published: the sidecar digest matches the
archive, the archive holds no path that escapes its root (absolute, "..", or a
symlink), and it actually contains a wally binary. Exits non-zero on any of
these, so a bad bottle fails the release rather than reaching a user.
"""
import hashlib
import os
import sys
import tarfile
import zipfile


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def unsafe(name):
    parts = name.replace("\\", "/").split("/")
    return name.startswith("/") or ".." in parts


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: verify-release-assets.py <archive> <sha256-sidecar>")
    archive, sidecar = sys.argv[1], sys.argv[2]
    if not os.path.isfile(archive):
        sys.exit(f"missing archive: {archive}")
    if not os.path.isfile(sidecar):
        sys.exit(f"missing sidecar: {sidecar}")

    want = open(sidecar).read().split()[0]
    got = sha256(archive)
    if want.lower() != got.lower():
        sys.exit(f"sha256 mismatch for {archive}: sidecar {want} != actual {got}")

    names = []
    if archive.endswith((".tar.gz", ".tgz")):
        with tarfile.open(archive) as t:
            for m in t.getmembers():
                if unsafe(m.name):
                    sys.exit(f"unsafe path in archive: {m.name}")
                if m.issym() or m.islnk():
                    sys.exit(f"archive contains a link that can escape root: {m.name}")
                names.append(m.name)
    elif archive.endswith(".zip"):
        with zipfile.ZipFile(archive) as z:
            for n in z.namelist():
                if unsafe(n):
                    sys.exit(f"unsafe path in archive: {n}")
                names.append(n)
    else:
        sys.exit(f"unknown archive type: {archive}")

    if not any(n.endswith("bin/wally") or n.endswith("bin/wally.exe") for n in names):
        sys.exit(f"archive has no bin/wally: {archive}")

    print(f"ok {os.path.basename(archive)} ({got[:12]}…, {len(names)} entries)")


if __name__ == "__main__":
    main()
