#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
tool_dir="$repo_dir/devtools"

if [[ ! -d "$tool_dir/node_modules" ]]; then
    printf 'Dependencies are missing. Run: npm ci --prefix %q\n' "$tool_dir" >&2
    exit 1
fi

exec node --no-deprecation "$tool_dir/devnet-wallet.mjs" "$@"
