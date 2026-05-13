#!/bin/bash
# SessionStart hook for Claude Code on the web.
#
# Prepares a Rust toolchain (rustfmt + clippy) and pre-fetches / pre-builds
# Cargo dependencies so `cargo fmt`, `cargo clippy`, and `cargo test` are
# ready to use immediately in the session.
set -euo pipefail

# Only run in remote (Claude Code on the web) environments.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

echo "[session-start] preparing Rust toolchain & cargo deps..." >&2

# Ensure ~/.cargo/bin is on PATH for the rest of the session.
if [ -d "$HOME/.cargo/bin" ]; then
  echo "export PATH=\"$HOME/.cargo/bin:\$PATH\"" >> "${CLAUDE_ENV_FILE:-/dev/null}"
  export PATH="$HOME/.cargo/bin:$PATH"
fi

# Install rustup + stable toolchain if cargo is missing.
if ! command -v cargo >/dev/null 2>&1; then
  echo "[session-start] installing rustup (stable toolchain)..." >&2
  curl -sSf --proto '=https' --tlsv1.2 https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
fi

# rustfmt and clippy are needed by CI (.github/workflows/ci.yml).
if command -v rustup >/dev/null 2>&1; then
  rustup component add rustfmt clippy >/dev/null 2>&1 || true
fi

# Prefetch dependencies (network) so later cargo invocations are offline-friendly.
cargo fetch --locked

# Warm the build cache so `cargo clippy` / `cargo test` are fast.
# `cargo test --no-run` compiles tests but does not execute them.
cargo build --all-targets --locked
cargo test --all-features --no-run --locked

echo "[session-start] done." >&2
