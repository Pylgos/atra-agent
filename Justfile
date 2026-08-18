set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

# Build every distributable artifact and run all automated tests.
check: install-web build test-rust test-web

# Build the Dioxus application and the self-contained native Web executable.
build:
    ./tools/build-atra-web.sh

# Run all automated tests. The Web test builds its required executable first.
test: test-rust test-web

# Run the complete Rust workspace test suite.
test-rust:
    cargo test --workspace

# Build and smoke-test the Web Client in Chromium.
test-web: install-web build
    ./tools/test-atra-web.sh

# Install the pinned Playwright dependencies without changing the lockfile.
install-web:
    pnpm --dir web install --frozen-lockfile
