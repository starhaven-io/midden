# Build

# Build the project
build:
    cargo build

# Build in release mode
build-release:
    cargo build --release

# Clean build artifacts
clean:
    cargo clean

# Test

# Run tests
test:
    cargo test

# Lint

# Audit GitHub Actions workflows
audit:
    zizmor --persona auditor .github/workflows/

# Run clippy
clippy:
    cargo clippy --all-targets -- -D warnings

# Check formatting
fmt-check:
    cargo fmt -- --check

# Format code
fmt:
    cargo fmt

# Check for typos
typos:
    typos

# Check dependency licenses, advisories, and sources
deny:
    cargo deny check

# Check for broken links in README
lychee:
    lychee --config lychee.toml README.md

# Check

# Run all checks
check:
    #!/usr/bin/env bash
    set -euo pipefail
    failed=0
    skipped=()
    run() {
        echo "--- $1 ---"
        if ! "$@"; then
            failed=1
        fi
    }
    skip() {
        echo "--- $1 --- skipped ($2 not found)"
        skipped+=("$2 (brew install $3)")
    }
    run cargo clippy --all-targets -- -D warnings
    run cargo fmt -- --check
    if command -v typos &>/dev/null; then
        run typos
    else
        skip typos typos typos-cli
    fi
    if command -v cargo-deny &>/dev/null; then
        run cargo deny check
    else
        skip cargo-deny cargo-deny cargo-deny
    fi
    if command -v zizmor &>/dev/null; then
        run zizmor --persona auditor .github/workflows/
    else
        skip audit zizmor zizmor
    fi
    if command -v lychee &>/dev/null; then
        run lychee --config lychee.toml README.md
    else
        skip lychee lychee lychee
    fi
    run cargo test
    if [ ${#skipped[@]} -gt 0 ]; then
        echo ""
        echo "Checks skipped due to missing tools:"
        for tool in "${skipped[@]}"; do
            echo "  - $tool"
        done
        failed=1
    fi
    exit $failed

# Install git hooks (DCO sign-off + pre-push checks) — run once per clone
install-hooks:
    git config core.hooksPath .githooks
