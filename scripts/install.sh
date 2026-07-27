#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cargo build --locked --release --manifest-path "$repo_dir/Cargo.toml" -p recebi-mcp

printf 'Built %s\n' "$repo_dir/target/release/recebi-mcp"
