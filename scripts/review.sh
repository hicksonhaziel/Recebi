#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/review.sh RECEIVABLE_ID' \
    '' \
    'Guided local review for one Recebi anomaly. It displays deterministic' \
    'evidence, starts the operator-only ZeroClaw SOP, and applies only a' \
    'durably approved receipt.' \
    '' \
    'Optional environment overrides:' \
    '  RECEBI_MCP_BIN       path to recebi-mcp release binary' \
    '  RECEBI_CONFIG        path to trusted recebi.toml' \
    '  ZEROCLAW_SOP_DB      path to ZeroClaw SOP runs.db' \
    '  ZEROCLAW_DEVICES_DB  path to ZeroClaw devices.db' \
    '  ZEROCLAW_GATEWAY_URL loopback ZeroClaw gateway URL'
}

if [[ ${1-} == "--help" || ${1-} == "-h" ]]; then
  usage
  exit 0
fi
if [[ $# -ne 1 || -z $1 || ${#1} -gt 64 || $1 == *$'\n'* || $1 == *$'\r'* ]]; then
  usage >&2
  exit 2
fi

receivable_id=$1
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
recebi_bin=${RECEBI_MCP_BIN:-"$repo_root/target/release/recebi-mcp"}
recebi_config=${RECEBI_CONFIG:-"$HOME/.zeroclaw/recebi.toml"}
sop_db=${ZEROCLAW_SOP_DB:-"$HOME/.zeroclaw/data/sop/runs.db"}
devices_db=${ZEROCLAW_DEVICES_DB:-"$HOME/.zeroclaw/data/devices.db"}
gateway_url=${ZEROCLAW_GATEWAY_URL:-"http://127.0.0.1:42617"}

if [[ ! $gateway_url =~ ^http://127\.0\.0\.1:[0-9]{1,5}$ ]]; then
  printf '%s\n' 'ZeroClaw gateway must be an explicit IPv4 loopback HTTP URL' >&2
  exit 2
fi
for command in curl jq sqlite3 zeroclaw; do
  command -v "$command" >/dev/null || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  }
done
[[ -x $recebi_bin && -r $recebi_config && -r $sop_db && -r $devices_db ]] || {
  printf '%s\n' \
    'trusted Recebi binary, config, SOP database, or device database is unavailable' >&2
  exit 2
}

mcp_request=$(jq -cn --arg id "$receivable_id" '{
  jsonrpc: "2.0",
  id: 1,
  method: "tools/call",
  params: {
    name: "recebi_check",
    arguments: {receivable_id: $id}
  }
}')
mcp_response=$(printf '%s\n' "$mcp_request" |
  "$recebi_bin" --config "$recebi_config")
candidate=$(jq -cer '
  select(.result.isError == false)
  | .result.content[0].text
  | fromjson
  | select(.status == "needs_review")
' <<<"$mcp_response") || {
  printf '%s\n' 'receivable is unavailable or is not currently in needs_review' >&2
  exit 3
}

fingerprint=$(jq -er .candidate_fingerprint <<<"$candidate")
signature=$(jq -er .signature <<<"$candidate")
reason=$(jq -er .reason <<<"$candidate")
eligible=$(jq -er .variance_eligible <<<"$candidate")

printf '\nReceivable review\n'
printf '  ID:          %s\n' "$receivable_id"
printf '  Reason:      %s\n' "$reason"
printf '  Signature:   %s\n' "$signature"
printf '  Fingerprint: %s\n' "$fingerprint"
if [[ $eligible == true ]]; then
  expected=$(jq -er .expected_amount <<<"$candidate")
  received=$(jq -er .received_amount <<<"$candidate")
  shortfall=$(jq -er .shortfall_amount <<<"$candidate")
  printf '  Expected:    %s USDC\n' "$expected"
  printf '  Received:    %s USDC\n' "$received"
  printf '  Shortfall:   %s USDC\n' "$shortfall"
  printf '  Eligibility: canonical finalized underpayment\n'
else
  printf '  Eligibility: variance acceptance is NOT available\n'
fi

printf '\nChoose one action:\n'
printf '  1) Ignore this candidate and keep waiting\n'
printf '  2) Cancel the receivable unpaid\n'
if [[ $eligible == true ]]; then
  printf '  3) Accept the underpayment with a recorded variance\n'
fi
printf '  q) Exit without changing anything\n'
read -r -p '> ' choice

variance_reason=none
case "$choice" in
  1) action=ignore_candidate_and_reopen ;;
  2) action=cancel_unpaid ;;
  3)
    if [[ $eligible != true ]]; then
      printf '%s\n' 'this candidate is not eligible for variance acceptance' >&2
      exit 3
    fi
    action=accept_underpayment_with_variance
    printf '\nWhy is the merchant accepting the shortfall?\n'
    printf '  1) Rounding adjustment\n'
    printf '  2) Commercial discount\n'
    printf '  3) Merchant write-off\n'
    read -r -p '> ' reason_choice
    case "$reason_choice" in
      1) variance_reason=rounding_adjustment ;;
      2) variance_reason=commercial_discount ;;
      3) variance_reason=merchant_write_off ;;
      *)
        printf '%s\n' 'invalid variance reason; nothing changed' >&2
        exit 2
        ;;
    esac
    ;;
  q|Q)
    printf '%s\n' 'nothing changed'
    exit 0
    ;;
  *)
    printf '%s\n' 'invalid action; nothing changed' >&2
    exit 2
    ;;
esac

printf '\nSelected action: %s\n' "$action"
printf 'Variance reason: %s\n' "$variance_reason"
printf 'Type the full receivable ID to create its approval request:\n'
read -r -p '> ' confirmation
if [[ $confirmation != "$receivable_id" ]]; then
  printf '%s\n' 'confirmation did not match; nothing changed' >&2
  exit 3
fi

curl -fsS "$gateway_url/health" >/dev/null || {
  printf '%s\n' 'local ZeroClaw gateway is unavailable' >&2
  exit 3
}
devices_before=$(sqlite3 -json "$devices_db" \
  'SELECT id FROM devices ORDER BY id;')
if ! jq -e 'type == "array"' >/dev/null <<<"$devices_before"; then
  printf '%s\n' 'could not snapshot existing ZeroClaw devices' >&2
  exit 3
fi
pair_code=$(curl -fsS -X POST "$gateway_url/admin/paircode/new" |
  jq -er .pairing_code)
gateway_token=$(curl -fsS -X POST "$gateway_url/pair" \
  -H "X-Pairing-Code: $pair_code" | jq -er .token)
ephemeral_device_id=$(curl -fsS "$gateway_url/api/devices" \
  -H "Authorization: Bearer $gateway_token" |
  jq -er --argjson before "$devices_before" '
    [.devices[]
      | select(.id as $id | ($before | map(.id) | index($id)) == null)]
    | select(length == 1)
    | .[0].id
  ') || {
  printf '%s\n' \
    'could not uniquely identify the temporary ZeroClaw approval device' >&2
  exit 3
}

cleanup_device() {
  curl -fsS -X DELETE "$gateway_url/api/devices/$ephemeral_device_id" \
    -H "Authorization: Bearer $gateway_token" >/dev/null 2>&1 || true
}
trap cleanup_device EXIT

payload=$(jq -cn \
  --arg id "$receivable_id" \
  --arg fingerprint "$fingerprint" \
  --arg action "$action" \
  --arg variance_reason "$variance_reason" \
  '{
    payload: ({
      receivable_id: $id,
      candidate_fingerprint: $fingerprint,
      action: $action,
      variance_reason: $variance_reason
    } | tojson)
  }')
run_json=$(curl -fsS -X POST \
  "$gateway_url/api/sops/recebi-resolve-review/run" \
  -H "Authorization: Bearer $gateway_token" \
  -H 'Content-Type: application/json' \
  --data "$payload")
run_id=$(jq -er '.run_id // .run.run_id' <<<"$run_json")
if [[ ! $run_id =~ ^run-[A-Za-z0-9-]{1,123}$ ]]; then
  printf '%s\n' 'ZeroClaw returned an invalid SOP run ID' >&2
  exit 3
fi

printf '\nApproval request created: %s\n' "$run_id"
printf 'Type APPROVE to continue, DENY to cancel this request, or press Ctrl+C\n'
printf 'and let the normal ZeroClaw timeout cancel it automatically.\n'
read -r -p '> ' decision
case "$decision" in
  APPROVE)
    zeroclaw sop approve "$run_id"
    ;;
  DENY)
    zeroclaw sop deny "$run_id"
    printf '%s\n' 'review denied; receivable state was not changed'
    exit 0
    ;;
  *)
    printf '%s\n' 'no approval was issued; the SOP will fail closed on timeout'
    exit 0
    ;;
esac

status=running
for _attempt in $(seq 1 180); do
  status=$(sqlite3 "$sop_db" \
    "SELECT json_extract(json,'$.run.status') FROM sop_runs WHERE run_id='$run_id';")
  if [[ $status == completed || $status == cancelled || $status == failed ]]; then
    break
  fi
  sleep 0.5
done
if [[ $status != completed ]]; then
  printf 'approval run did not complete safely (status: %s)\n' "$status" >&2
  exit 3
fi

printf '\nApplying verified durable receipt...\n'
"$script_dir/resolve-review.sh" "$run_id"
