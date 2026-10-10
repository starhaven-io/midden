# Build

# Build the project
build:
    cargo build --locked

# Build in release mode
build-release:
    cargo build --locked --release

# Clean build artifacts
clean:
    cargo clean

# Test

# Run tests
test:
    cargo test --locked

# Run Ruby script and workflow contract tests
script-tests:
    bundle exec ruby scripts/test.rb

# Lint

# fleet:block audit
audit:
    zizmor --strict-collection --persona auditor .github/workflows/
# fleet:end

# Run clippy
clippy:
    cargo clippy --locked --all-targets -- -D warnings

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

# Check for broken links in public documentation
lychee:
    lychee --config lychee.toml README.md SECURITY.md

# Check

# Run all checks
check:
    ruby scripts/check.rb

# fleet:block install-hooks
# Install git hooks (AI trailer guard + DCO sign-off + pre-push checks). Run once per clone.
install-hooks:
    git config core.hooksPath .githooks
# fleet:end

# fleet:block pinprick-audit
pinprick-audit:
    pinprick audit .
# fleet:end
