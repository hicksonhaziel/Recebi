#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/restore-drill.sh [--keep]' \
    '' \
    'Non-destructive Recebi backup and restore drill. It takes a consistent' \
    'SQLite backup of the live ledger, restores it into an isolated private' \
    'directory, verifies the restored event chain and checkpoint chain with the' \
    'trusted binary, and proves the restored material-ledger root equals the' \
    'live root. The live data directory is only ever opened read-only.' \
    '' \
    'No network call, scheduler, or state mutation is performed.' \
    '' \
    'Options:' \
    '  --keep   retain the restore directory and print its path' \
    '' \
    'Optional environment overrides:' \
    '  RECEBI_MCP_BIN  path to recebi-mcp release binary' \
    '  RECEBI_CONFIG   path to trusted recebi.toml'
}

keep=false
case "${1-}" in
  --help | -h)
    usage
    exit 0
    ;;
  --keep) keep=true ;;
  '') ;;
  *)
    usage >&2
    exit 2
    ;;
esac

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
recebi_bin=${RECEBI_MCP_BIN:-"$repo_root/target/release/recebi-mcp"}
recebi_config=${RECEBI_CONFIG:-"$HOME/.zeroclaw/recebi.toml"}

for command in jq sha256sum sqlite3; do
  command -v "$command" >/dev/null || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  }
done
[[ -x $recebi_bin && -r $recebi_config ]] || {
  printf '%s\n' 'trusted Recebi binary or configuration is unavailable' >&2
  exit 2
}

live_data_dir=$(sed -n -E \
  's/^[[:space:]]*data_dir[[:space:]]*=[[:space:]]*"(.*)"[[:space:]]*$/\1/p' \
  "$recebi_config" | tail -n 1)
[[ -n $live_data_dir ]] || {
  printf '%s\n' 'could not read data_dir from the trusted configuration' >&2
  exit 2
}
live_database="$live_data_dir/recebi.sqlite3"
[[ -r $live_database ]] || {
  printf '%s\n' 'live Recebi database is unreadable' >&2
  exit 2
}

restore_dir=$(mktemp -d "${TMPDIR:-/tmp}/recebi-restore-drill.XXXXXX")
chmod 700 "$restore_dir"
cleanup() {
  if [[ $keep == true ]]; then
    printf '\nRestore directory retained: %s\n' "$restore_dir"
  else
    rm -rf -- "$restore_dir"
  fi
}
trap cleanup EXIT

restored_data_dir="$restore_dir/data"
install -d -m 700 "$restored_data_dir"
restored_database="$restored_data_dir/recebi.sqlite3"

# Consistent snapshot through the SQLite backup API from a read-only source
# connection. WAL content is included and the live database is never written.
sqlite3 -readonly "$live_database" ".backup '$restored_database'"
chmod 600 "$restored_database"
sha256sum "$restored_database" | tee "$restored_database.sha256" >/dev/null
chmod 600 "$restored_database.sha256"

drill_config="$restore_dir/recebi.drill.toml"
: >"$drill_config"
chmod 600 "$drill_config"
sed -E "s|^([[:space:]]*data_dir[[:space:]]*=[[:space:]]*).*$|\1\"$restored_data_dir\"|" \
  "$recebi_config" >"$drill_config"

checkpoint_query='SELECT sequence AS s, lower(hex(ledger_root)) AS root,
  lower(hex(checkpoint_hash)) AS checkpoint
  FROM ledger_checkpoints ORDER BY sequence DESC LIMIT 1;'
counts_query='SELECT
  (SELECT count(*) FROM receivables) AS receivables,
  (SELECT count(*) FROM settlements) AS settlements,
  (SELECT count(*) FROM receivable_events) AS events;'

live_checkpoint=$(sqlite3 -readonly -json "$live_database" "$checkpoint_query")
live_counts=$(sqlite3 -readonly -json "$live_database" "$counts_query")
restored_checkpoint=$(sqlite3 -readonly -json "$restored_database" "$checkpoint_query")
restored_counts=$(sqlite3 -readonly -json "$restored_database" "$counts_query")

verification=$("$recebi_bin" --config "$drill_config" --verify-ledger) || {
  printf '%s\n' 'restored ledger failed trusted verification' >&2
  exit 3
}

live_root=$(jq -er '.[0].root' <<<"$live_checkpoint")
live_sequence=$(jq -er '.[0].s' <<<"$live_checkpoint")
live_checkpoint_hash=$(jq -er '.[0].checkpoint' <<<"$live_checkpoint")
restored_root=$(jq -er '.[0].root' <<<"$restored_checkpoint")
restored_sequence=$(jq -er '.[0].s' <<<"$restored_checkpoint")
restored_checkpoint_hash=$(jq -er '.[0].checkpoint' <<<"$restored_checkpoint")
verified_root=$(jq -er '.material_ledger_root' <<<"$verification")

failures=0
compare() {
  if [[ $2 == "$3" ]]; then
    printf '  ok        %s\n' "$1"
  else
    printf '  MISMATCH  %s\n' "$1" >&2
    failures=$((failures + 1))
  fi
}

printf '\nRecebi restore drill\n'
printf '  Live database:     %s\n' "$live_database"
printf '  Restored database: %s\n' "$restored_database"
printf '  Checkpoint:        sequence %s\n' "$restored_sequence"
printf '  Material root:     %s\n' "$verified_root"
printf '  Checkpoint hash:   %s\n' "$restored_checkpoint_hash"
printf '\nVerification\n'
printf '  ok        restored event chain verified by trusted binary\n'
printf '  ok        restored checkpoint chain verified by trusted binary\n'
compare 'checkpoint sequence matches live' "$live_sequence" "$restored_sequence"
compare 'stored ledger root matches live' "$live_root" "$restored_root"
compare 'checkpoint hash matches live' "$live_checkpoint_hash" "$restored_checkpoint_hash"
compare 'recomputed root matches stored checkpoint' "$verified_root" "$restored_root"
compare 'material row counts match live' "$live_counts" "$restored_counts"

if ((failures > 0)); then
  printf '\nDrill FAILED with %s mismatch(es)\n' "$failures" >&2
  exit 3
fi
printf '\nDrill passed. Restored copy is byte-verifiable against the live ledger.\n'
printf 'Backup digest: %s\n' "$(cut -d' ' -f1 <"$restored_database.sha256")"
