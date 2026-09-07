#!/usr/bin/env python3
"""Map changed repository paths to the CI matrix and auxiliary checks."""

from __future__ import annotations

import json
import re
import sys


FULL_MATRIX = [
    {"name": "Lint", "check": "lint", "runner": "ubuntu-24.04"},
    {"name": "Test (Linux)", "check": "test", "runner": "ubuntu-24.04"},
    {"name": "Test (Linux ARM)", "check": "test", "runner": "ubuntu-24.04-arm"},
    {"name": "Test (macOS)", "check": "test", "runner": "macos-26"},
    {"name": "Coverage", "check": "coverage", "runner": "ubuntu-24.04"},
    {"name": "MSRV", "check": "msrv", "runner": "ubuntu-24.04"},
]
LINT_MATRIX = [{"name": "Lint", "check": "lint", "runner": "ubuntu-24.04"}]

RUST_OR_BUILD = re.compile(
    r"(?:\.rs$|^Cargo\.(?:toml|lock)$|^\.cargo/config(?:\.toml)?$|"
    r"^rust-toolchain\.toml$|^clippy\.toml$|^rustfmt\.toml$|"
    r"^\.config/nextest\.toml$|^tests/|"
    r"^\.github/workflows/(?:ci|cargo-deny|release)\.yml$)"
)
LINT_ONLY = re.compile(
    r"(?:^deny\.toml$|^_typos\.toml$|^justfile$|"
    r"^scripts/(?:format-release-notes|ci-path-routing|validate-cask-bump)\.py$|"
    r"^scripts/tests/)"
)
LINKS = re.compile(
    r"(?:^(?:README|SECURITY)\.md$|^lychee\.toml$|"
    r"^\.github/workflows/(?:ci|link-check)\.yml$)"
)


def route(paths: list[str]) -> dict[str, object]:
    if any(RUST_OR_BUILD.search(path) for path in paths):
        matrix = FULL_MATRIX
    elif any(LINT_ONLY.search(path) for path in paths):
        matrix = LINT_MATRIX
    else:
        matrix = []
    return {
        "matrix": matrix,
        "links": any(LINKS.search(path) for path in paths),
        "zizmor": any(path.startswith(".github/workflows/") for path in paths),
        "coverage": matrix == FULL_MATRIX,
    }


def main() -> None:
    result = route([line.strip() for line in sys.stdin if line.strip()])
    for key in ("matrix", "links", "zizmor", "coverage"):
        print(f"{key}={json.dumps(result[key], separators=(',', ':'))}")


if __name__ == "__main__":
    main()
