#!/usr/bin/env python3
"""Reserve a patch release tag, or stamp that version into a build checkout.

Tags point at the pushed source commit. Stamping only changes the build
checkout, so releases never create recursive version-bump commits on main.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def parse_version(value):
    if not SEMVER.fullmatch(value):
        raise ValueError(f"expected a stable major.minor.patch version, got {value!r}")
    return tuple(map(int, value.split(".")))


def next_version(base, tags):
    latest = max((parse_version(tag) for tag in tags), default=None)
    candidate = parse_version(base)
    if latest is not None:
        candidate = max(candidate, (latest[0], latest[1], latest[2] + 1))
    return ".".join(map(str, candidate))


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def reserve(base):
    sha = git("rev-parse", "HEAD")
    for _ in range(5):
        refs = dict(line.split()[::-1] for line in git("ls-remote", "--tags", "origin").splitlines())
        tags = {}
        for ref, target in refs.items():
            version = ref.removeprefix("refs/tags/v")
            if ref.startswith("refs/tags/v") and SEMVER.fullmatch(version):
                tags[version] = refs.get(ref + "^{}", target)
        # Re-running a failed workflow uses its existing version.
        existing = [v for v, target in tags.items() if target == sha]
        if existing:
            return max(existing, key=parse_version)
        version = next_version(base, tags)
        result = subprocess.run(["git", "push", "origin", f"{sha}:refs/tags/v{version}"], cwd=ROOT)
        if result.returncode == 0:
            return version
    raise RuntimeError("could not reserve a release tag after five attempts")


def stamp(root, version):
    parse_version(version)
    manifest = root / "Cargo.toml"
    text, count = re.subn(r'(?m)^version = "[^"]+"$', f'version = "{version}"', manifest.read_text(), count=1)
    if count != 1:
        raise ValueError("package version missing from Cargo.toml")
    manifest.write_text(text)
    lock = root / "Cargo.lock"
    text, count = re.subn(r'(\[\[package\]\]\nname = "oxide-api"\nversion = ")[^"]+("\n)',
                          lambda m: m[1] + version + m[2], lock.read_text())
    if count != 1:
        raise ValueError("oxide-api package missing from Cargo.lock")
    lock.write_text(text)
    spec = root / "openapi.yaml"
    spec.write_text(re.sub(r"(?m)^  version: .+$", f"  version: {version}", spec.read_text(), count=1))
    spec = root / "openapi.json"
    data = json.loads(spec.read_text())
    data["info"]["version"] = version
    spec.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--reserve", action="store_true", help="create the next release tag on origin")
    group.add_argument("--set", metavar="VERSION", help="stamp a version into this build checkout")
    args = parser.parse_args()
    if args.reserve:
        base = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
        version = reserve(base)
        output = f"version={version}\ntag=v{version}\n"
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a") as handle:
                handle.write(output)
        print(output, end="")
    else:
        stamp(ROOT, args.set)


if __name__ == "__main__":
    main()
