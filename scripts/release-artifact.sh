#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/release-artifact.sh' \
    '' \
    'Builds the pinned Linux release binary for the current commit and writes it' \
    'with a SHA-256 checksum into dist/. Source paths are remapped so no builder' \
    'home directory appears in the artifact, and the result is verified.' \
    '' \
    'Refuses to run on a dirty working tree, because a published checksum must' \
    'correspond to an exact public commit.'
}

if [[ ${1-} == "--help" || ${1-} == "-h" ]]; then
  usage
  exit 0
fi
if (($# > 0)); then
  usage >&2
  exit 2
fi

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

for command in cargo git sha256sum; do
  command -v "$command" >/dev/null || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  }
done

if [[ -n $(git status --porcelain) ]]; then
  printf '%s\n' 'working tree is dirty; commit before building a release artifact' >&2
  exit 3
fi

commit=$(git rev-parse HEAD)
version=$(sed -n -E 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*$/\1/p' Cargo.toml | head -n 1)
[[ -n $version ]] || {
  printf '%s\n' 'could not read workspace version from Cargo.toml' >&2
  exit 3
}

target_dir="$repo_dir/target/release-artifact"
# Remap both the repository and the Cargo registry so panic metadata cannot
# disclose the builder's home directory or username.
export RUSTFLAGS="--remap-path-prefix=$repo_dir=/recebi --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
CARGO_TARGET_DIR="$target_dir" cargo build --locked --release -p recebi-mcp

binary="$target_dir/release/recebi-mcp"
[[ -x $binary ]] || {
  printf '%s\n' 'release binary was not produced' >&2
  exit 3
}

leaked=$(strings "$binary" | grep -c "$HOME" || true)
if [[ $leaked -ne 0 ]]; then
  printf 'refusing to publish: %s builder path strings remain in the artifact\n' "$leaked" >&2
  exit 3
fi

install -d -m 755 "$repo_dir/dist"
artifact="$repo_dir/dist/recebi-mcp-v$version-x86_64-unknown-linux-gnu"
install -m 755 "$binary" "$artifact"
(cd "$repo_dir/dist" && sha256sum "$(basename "$artifact")" > "$(basename "$artifact").sha256")

printf '\nRelease artifact\n'
printf '  Version:  %s\n' "$version"
printf '  Commit:   %s\n' "$commit"
printf '  Artifact: %s\n' "$artifact"
printf '  SHA-256:  %s\n' "$(cut -d' ' -f1 <"$artifact.sha256")"
printf '  Size:     %s bytes\n' "$(stat -c '%s' "$artifact")"
printf '  Builder paths in artifact: 0\n'
