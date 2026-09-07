#!/usr/bin/env python3
"""Validate that a cask bump changes only version and verified checksums."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


VERSION_LINE = re.compile(r'(?m)^(\s*version\s+)"([^"\n]+)"(\s*)$')
SHA256 = re.compile(r"(?<![0-9a-f])[0-9a-f]{64}(?![0-9a-f])")


def normalize(cask: str) -> tuple[str, list[str]]:
    normalized, version_count = VERSION_LINE.subn(r'\1"__VERSION__"\3', cask)
    if version_count != 1:
        raise ValueError(f"expected one literal version line, found {version_count}")
    hashes = SHA256.findall(normalized)
    if len(hashes) != 3:
        raise ValueError(f"expected three SHA-256 values, found {len(hashes)}")
    return SHA256.sub("__SHA256__", normalized), hashes


def validate(base: str, candidate: str, version: str, hashes: list[str]) -> None:
    versions = [match.group(2) for match in VERSION_LINE.finditer(candidate)]
    if versions != [version]:
        raise ValueError(f"candidate version is {versions!r}, expected {version!r}")
    normalized_base, _ = normalize(base)
    normalized_candidate, candidate_hashes = normalize(candidate)
    if normalized_candidate != normalized_base:
        raise ValueError("candidate changes content other than version and SHA-256 values")
    if candidate_hashes != hashes:
        raise ValueError("candidate SHA-256 values do not match the published release assets")


def render(base: str, version: str, hashes: list[str]) -> str:
    if not version or any(character in version for character in ['"', "\n", "\r"]):
        raise ValueError("version is not safe for a literal cask version")
    if len(hashes) != 3 or any(re.fullmatch(r"[0-9a-f]{64}", value) is None for value in hashes):
        raise ValueError("expected exactly three lowercase SHA-256 values")

    normalize(base)
    candidate, version_count = VERSION_LINE.subn(
        lambda match: f'{match.group(1)}"{version}"{match.group(3)}',
        base,
    )
    if version_count != 1:
        raise ValueError(f"expected one literal version line, found {version_count}")
    replacements = iter(hashes)
    candidate = SHA256.sub(lambda _match: next(replacements), candidate)
    validate(base, candidate, version, hashes)
    return candidate


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--render", action="store_true")
    parser.add_argument("base", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("version")
    parser.add_argument("sha256", nargs=3)
    args = parser.parse_args()
    base = args.base.read_text()
    if args.render:
        args.candidate.write_text(render(base, args.version, args.sha256))
    else:
        validate(base, args.candidate.read_text(), args.version, args.sha256)


if __name__ == "__main__":
    main()
