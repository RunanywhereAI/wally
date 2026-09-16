#!/usr/bin/env python3
"""stamp-formula.py <version> <platform=sha256> [...]

Stamps Formula/wally.rb with the released version and bottle digests. The
formula is committed with placeholder values; the release job runs this from
the published archives' sidecars so the tap points at real, verified bottles.
Only macos-arm64 is tapped today, so one digest is expected.
"""
import os
import re
import sys


def main():
    if len(sys.argv) < 3:
        sys.exit("usage: stamp-formula.py <version> <platform=sha256> [...]")
    version = sys.argv[1].lstrip("v")
    digests = {}
    for pair in sys.argv[2:]:
        if "=" not in pair:
            sys.exit(f"expected platform=sha256, got: {pair}")
        platform, digest = pair.split("=", 1)
        if not re.fullmatch(r"[0-9a-fA-F]{64}", digest):
            sys.exit(f"not a sha256: {digest}")
        digests[platform] = digest.lower()

    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    path = os.path.join(root, "Formula", "wally.rb")
    text = open(path).read()

    text, n = re.subn(r'version "[^"]*"', f'version "{version}"', text, count=1)
    if n != 1:
        sys.exit("could not find a version line in Formula/wally.rb")

    if "macos-arm64" in digests:
        text, n = re.subn(
            r'sha256 "[0-9a-fA-F]{64}"',
            f'sha256 "{digests["macos-arm64"]}"',
            text,
            count=1,
        )
        if n != 1:
            sys.exit("could not find a sha256 line in Formula/wally.rb")

    open(path, "w").write(text)
    print(f"stamped Formula/wally.rb -> version {version}, {', '.join(digests)}")


if __name__ == "__main__":
    main()
