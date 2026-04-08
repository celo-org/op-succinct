# CI recipes - mirrors GitHub Actions workflows for local execution.

# ─── Lint workflow ───────────────────────────────────────────────────────────

# Check that SP1 version is consistent across Dockerfiles, workflows, and justfile.
check-sp1-version:
    #!/usr/bin/env bash
    set -euo pipefail
    SP1_VERSION=$(grep 'sp1-sdk' Cargo.toml | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
    echo "Expected SP1 version from Cargo.toml: $SP1_VERSION"
    MISMATCHES=$(grep -rn 'sp1up -v' --include='Dockerfile*' | grep -v "$SP1_VERSION" || true)
    MISMATCHES+=$(grep -rn 'sp1up -v' .github/workflows/ --exclude='lint.yml' | grep -v "$SP1_VERSION" || true)
    MISMATCHES+=$(grep -n -- '--tag v' justfile | grep -v "$SP1_VERSION" || true)
    if [ -n "$MISMATCHES" ]; then
      echo "SP1 version mismatch found. Expected $SP1_VERSION (from Cargo.toml):"
      echo "$MISMATCHES"
      exit 1
    fi
    echo "SP1 version consistency check passed"

# Run cargo check with all targets and features.
cargo-check:
    cargo check --all-targets --all-features --tests

# Run cargo fmt check.
cargo-fmt:
    cargo fmt --all -- --check

# Run cargo clippy with warnings as errors.
cargo-clippy:
    cargo clippy --all-features --all-targets -- -D warnings -A incomplete-features

# Run all lint checks (mirrors the Lint workflow).
lint: check-sp1-version cargo-check cargo-fmt cargo-clippy

# ─── Cargo Tests workflow ────────────────────────────────────────────────────

# Run cargo tests (mirrors the Cargo Tests workflow).
cargo-tests: tests

# ─── Cost Estimator workflow ─────────────────────────────────────────────────

# Run cost estimator on recent block range (requires RPC env vars).
cost-estimator-rolling:
    RUST_LOG=info cargo run --bin cost-estimator --release --features eigenda -- --rolling

# ─── ELF workflow ────────────────────────────────────────────────────────────

# Build ELFs and verify they haven't changed (mirrors the ELF workflow).
verify-elfs: build-elfs
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "$(git status --porcelain elf/)" ]; then
      echo "❌ ELF files changed during build!"
      git diff
      exit 1
    else
      echo "✅ ELF files remained unchanged"
    fi

# ─── Foundry Test workflow ───────────────────────────────────────────────────

# Run forge fmt check on contracts.
forge-fmt:
    cd contracts && forge fmt --check src/ test/ script/

# Run forge build with contract sizes.
forge-build-sizes:
    cd contracts && forge build --sizes

# Run forge tests.
forge-test:
    cd contracts && forge test -vvv

# Run all foundry checks (mirrors the Foundry Test workflow).
foundry-test: forge-fmt forge-build-sizes forge-test

# ─── SQLx Query Check workflow ───────────────────────────────────────────────

# Verify SQLx query files are up to date.
sqlx-check:
    cd validity && cargo sqlx prepare --check

# ─── Link Checker workflow ───────────────────────────────────────────────────

# Check markdown links (requires lychee: cargo install lychee).
link-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v lychee &> /dev/null; then
      echo "lychee not found. Install with: cargo install lychee"
      exit 1
    fi
    lychee \
      --verbose \
      --no-progress \
      --accept 200,204,206,301,302,303,307,308,403 \
      --exclude-loopback \
      --exclude "^https://github.com/.*/(pull|issues|compare)/" \
      --exclude "^https://crates.io/crates/" \
      --exclude "^https://docs.rs/" \
      --exclude "localhost" \
      --exclude "127.0.0.1" \
      --exclude-path target/ \
      --exclude-path contracts/lib/ \
      "**/*.md"

# ─── Spell Checker workflow ──────────────────────────────────────────────────

# Check spelling in files (requires misspell: go install github.com/client9/misspell/cmd/misspell@latest).
spell-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v misspell &> /dev/null; then
      echo "misspell not found. Install with: go install github.com/client9/misspell/cmd/misspell@latest"
      exit 1
    fi
    misspell -error .

# ─── Run all CI checks ──────────────────────────────────────────────────────

# Run all CI checks that can be run locally without RPC endpoints.
ci: lint cargo-tests foundry-test sqlx-check link-check spell-check
