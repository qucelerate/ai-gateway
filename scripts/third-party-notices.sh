#!/usr/bin/env bash
# Prints the third-party license list of the gateway's Rust dependencies
# (Markdown), from Cargo.lock, with cargo-about (about.toml, about.hbs):
#
#   scripts/third-party-notices.sh > THIRD_PARTY_NOTICES.md
#
# Needs cargo-about (`cargo install cargo-about --locked --features cli`).
# NOTICE lists the vendored code and data that are not crates.
set -euo pipefail
cd "$(dirname "$0")/.."
if ! cargo about --version >/dev/null 2>&1; then
  echo "cargo-about is not installed: cargo install cargo-about --locked --features cli" >&2
  exit 1
fi
exec cargo about generate --locked --fail about.hbs
