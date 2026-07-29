#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/resolve-review.sh RUN_ID' \
    '' \
    'Verifies a completed recebi-resolve-review ZeroClaw run, derives the exact' \
    'receivable/fingerprint/action from its durable receipt, and applies it' \
    'through the non-discoverable local Recebi MCP operation.' \
    '' \
    'Optional environment overrides:' \
    '  RECEBI_MCP_BIN       path to recebi-mcp release binary' \
    '  RECEBI_CONFIG        path to trusted recebi.toml' \
    '  ZEROCLAW_SOP_DB      path to ZeroClaw SOP runs.db'
}

if [[ ${1-} == "--help" || ${1-} == "-h" ]]; then
  usage
  exit 0
fi
if [[ $# -ne 1 || ! $1 =~ ^run-[A-Za-z0-9-]{1,123}$ ]]; then
  usage >&2
  exit 2
fi

run_id=$1
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
recebi_bin=${RECEBI_MCP_BIN:-"$repo_root/target/release/recebi-mcp"}
recebi_config=${RECEBI_CONFIG:-"$HOME/.zeroclaw/recebi.toml"}
sop_db=${ZEROCLAW_SOP_DB:-"$HOME/.zeroclaw/data/sop/runs.db"}

for command in jq sqlite3; do
  command -v "$command" >/dev/null || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  }
done
[[ -x $recebi_bin ]] || {
  printf 'recebi binary is not executable: %s\n' "$recebi_bin" >&2
  exit 2
}
[[ -r $recebi_config && -r $sop_db ]] || {
  printf '%s\n' 'trusted Recebi config or ZeroClaw SOP database is unreadable' >&2
  exit 2
}

run_json=$(sqlite3 "$sop_db" \
  "SELECT json FROM sop_runs WHERE run_id='$run_id' AND terminal=1;")
[[ -n $run_json ]] || {
  printf '%s\n' 'run is missing, non-terminal, or not durably completed' >&2
  exit 3
}

receipt=$(jq -cer --arg run_id "$run_id" '
  .run as $run
  | select(
      $run.run_id == $run_id
      and $run.sop_name == "recebi-resolve-review"
      and $run.status == "completed"
      and ($run.step_results | length) == 1
      and $run.step_results[0].status == "completed"
    )
  | ($run.trigger_event.payload | fromjson?) as $trigger
  | ($run.step_results[0].output | fromjson?) as $approval
  | select(
      $approval.receivable_id == $trigger.receivable_id
      and $approval.fingerprint == $trigger.candidate_fingerprint
      and $approval.requested_action == $trigger.action
      and $approval.approval_checkpoint == "cleared"
      and ($trigger.candidate_fingerprint | test("^[0-9a-f]{64}$"))
      and ($trigger.action == "ignore_candidate_and_reopen"
           or $trigger.action == "cancel_unpaid")
    )
  | {
      receivable_id: $trigger.receivable_id,
      candidate_fingerprint: $trigger.candidate_fingerprint,
      action: $trigger.action,
      approval_run_id: $run_id
    }
' <<<"$run_json") || {
  printf '%s\n' 'durable SOP receipt failed closed validation' >&2
  exit 3
}

request=$(jq -cn --argjson arguments "$receipt" '{
  jsonrpc: "2.0",
  id: 1,
  method: "tools/call",
  params: {name: "recebi_resolve_review", arguments: $arguments}
}')
response=$(printf '%s\n' "$request" |
  "$recebi_bin" --config "$recebi_config")

jq -er '
  select(.result.isError == false)
  | .result.content[0].text
  | fromjson
' <<<"$response"
