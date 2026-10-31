#!/usr/bin/env bash
# Cargo rejects package/bin names starting with a digit, so the binary is
# built as threel-cli and installed under the name 3l-cli.
set -e
cd "$(dirname "$0")"
cargo build --release
mkdir -p ~/.local/bin
install -m755 target/release/threel-cli ~/.local/bin/3l-cli
echo "installed: ~/.local/bin/3l-cli"
