#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/run-reconcile-job.sh MODE TELEGRAM_CHAT_ID' \
    '' \
    'MODE is hot or background.' \
    'Hot mode exits immediately when no recent invoice is open.' \
    'Otherwise it runs at five-second deadlines for at most three minutes.' \
    'This runner invokes deterministic Recebi code and sends structured' \
    'Telegram alerts without an LLM.'
}

if [[ ${1-} == '--help' || ${1-} == '-h' ]]; then
  usage
  exit 0
fi
if [[ $# -ne 2 ]]; then
  usage >&2
  exit 2
fi

mode=$1
telegram_chat_id=$2
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
mcp_binary=${RECEBI_MCP_BINARY:-"$repo_dir/target/release/recebi-mcp"}
recebi_config=${RECEBI_CONFIG:-"$HOME/.zeroclaw/recebi.toml"}
hot_passes=${RECEBI_HOT_PASSES:-36}
hot_interval_ms=${RECEBI_HOT_INTERVAL_MS:-5000}
if [[ -n ${ZEROCLAW_BIN:-} ]]; then
  zeroclaw_bin=$ZEROCLAW_BIN
elif command -v zeroclaw >/dev/null; then
  zeroclaw_bin=$(command -v zeroclaw)
else
  zeroclaw_bin="$HOME/.cargo/bin/zeroclaw"
fi

if [[ $mode != hot && $mode != background ]]; then
  printf '%s\n' 'MODE must be hot or background' >&2
  exit 2
fi
if [[ ! $hot_passes =~ ^[0-9]+$ ]] || (( hot_passes < 1 || hot_passes > 60 )); then
  printf '%s\n' 'RECEBI_HOT_PASSES must be an integer from 1 to 60' >&2
  exit 2
fi
if [[ ! $hot_interval_ms =~ ^[0-9]+$ ]] ||
   (( hot_interval_ms < 1 || hot_interval_ms > 60000 )); then
  printf '%s\n' 'RECEBI_HOT_INTERVAL_MS must be an integer from 1 to 60000' >&2
  exit 2
fi
if [[ ! $telegram_chat_id =~ ^-?[0-9]+$ ]]; then
  printf '%s\n' 'Telegram chat ID must be numeric' >&2
  exit 2
fi
if [[ -z $zeroclaw_bin || ! -x $zeroclaw_bin ]]; then
  printf '%s\n' 'missing required command: zeroclaw' >&2
  exit 2
fi
for command in date jq sleep; do
  command -v "$command" >/dev/null || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  }
done
[[ -x $mcp_binary ]] || {
  printf 'Recebi MCP binary is not executable: %s\n' "$mcp_binary" >&2
  exit 2
}
[[ -f $recebi_config && -r $recebi_config ]] || {
  printf 'Recebi config is not readable: %s\n' "$recebi_config" >&2
  exit 2
}
if [[ $mode == hot ]]; then
  tool_name='recebi_hot_reconcile'
  arguments='{}'
  pass_count=$hot_passes
else
  tool_name='recebi_reconcile_open'
  arguments='{"max_count":10}'
  pass_count=1
fi

send_message() {
  "$zeroclaw_bin" channel send "$1" \
    --channel-id telegram \
    --recipient "$telegram_chat_id"
}

acknowledge_notification() {
  local notification_id=$1 delivery_receipt=$2 request response
  request=$(jq -cn \
    --argjson notification_id "$notification_id" \
    --arg delivery_receipt "$delivery_receipt" \
    '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"recebi_acknowledge_notification",arguments:{notification_id:$notification_id,delivery_receipt:$delivery_receipt}}}')
  response=$(printf '%s\n' "$request" | "$mcp_binary" --config "$recebi_config")
  jq -e '.result.content[0].text and (.result.isError == false)' \
    >/dev/null <<<"$response" || {
    printf '%s\n' 'Recebi notification acknowledgement failed closed' >&2
    return 3
  }
}

run_pass() {
  local request response record status message notification_id delivery_receipt
  local -a terminal_records

  request=$(jq -cn \
    --arg tool "$tool_name" \
    --argjson arguments "$arguments" \
    '{jsonrpc:"2.0",id:1,method:"tools/call",params:{name:$tool,arguments:$arguments}}')
  response=$(printf '%s\n' "$request" | "$mcp_binary" --config "$recebi_config")
  jq -e '.result.content[0].text and (.result.isError == false)' \
    >/dev/null <<<"$response" || {
    printf '%s\n' 'Recebi reconciliation job failed closed' >&2
    return 3
  }
  result=$(jq -cer '.result.content[0].text | fromjson' <<<"$response")
  jq -e '
    (.checked | type == "number") and
    (.checked >= 0) and
    (.terminal | type == "array") and
    (.incomplete | type == "number") and
    (.incomplete_samples | type == "array")
  ' >/dev/null <<<"$result"

  mapfile -t terminal_records < <(jq -c '.terminal[]' <<<"$result")
  for record in "${terminal_records[@]}"; do
    notification_id=$(jq -r '.notification_id' <<<"$record")
    status=$(jq -r '.status' <<<"$record")
    case $status in
      payment_verified)
        message=$(jq -r '
          "✅ Payment verified\n\n" +
          "• Invoice: `" + .receivable_id + "`\n" +
          "• Amount: `" + (.received_amount // .expected_amount // "unknown") + " USDC`\n" +
          "• Status: Exact payment recorded\n" +
          "• Official PTAX: Pending monthly close\n" +
          "• Signature: [View in Solana Explorer](" + .explorer_url + ")"
        ' <<<"$record")
        ;;
      needs_review)
        message=$(jq -r '
          "⚠️ Payment needs review\n\n" +
          "• Invoice: `" + .receivable_id + "`\n" +
          "• Status: Unpaid\n" +
          "• Reason: `" + (.reason // "verification_failed") + "`" +
          (if .expected_amount then "\n• Expected: `" + .expected_amount + " USDC`" else "" end) +
          (if .received_amount then "\n• Received: `" + .received_amount + " USDC`" else "" end) +
          "\n• Official PTAX: Not available — invoice unpaid" +
          "\n• Signature: [View in Solana Explorer](" + .explorer_url + ")"
        ' <<<"$record")
        ;;
      *)
        printf 'unexpected terminal status: %s\n' "$status" >&2
        return 3
        ;;
    esac
    send_message "$message"
    delivery_receipt="telegram:${telegram_chat_id}:${notification_id}"
    acknowledge_notification "$notification_id" "$delivery_receipt"
  done
}

result='{"terminal":[],"incomplete":0,"incomplete_samples":[]}'
deadline_ns=$(date +%s%N)
for (( pass = 1; pass <= pass_count; pass++ )); do
  run_pass
  if [[ $mode == hot ]] && (( $(jq -r '.checked' <<<"$result") == 0 )); then
    break
  fi
  if (( pass < pass_count )); then
    deadline_ns=$(( deadline_ns + hot_interval_ms * 1000000 ))
    now_ns=$(date +%s%N)
    remaining_ns=$(( deadline_ns - now_ns ))
    if (( remaining_ns > 0 )); then
      printf -v delay_seconds '%d.%09d' \
        "$(( remaining_ns / 1000000000 ))" \
        "$(( remaining_ns % 1000000000 ))"
      sleep "$delay_seconds"
    fi
  fi
done

# A terminal record is sent immediately. Transient RPC failures are reported
# at most once per bounded hot worker, using only the final pass, to avoid
# flooding Telegram during a short outage.
incomplete=$(jq -r '.incomplete' <<<"$result")
if (( incomplete > 0 )); then
  ids=$(jq -r '.incomplete_samples | map("`" + . + "`") | join(", ")' <<<"$result")
  send_message "⚠️ Reconciliation incomplete

• Status: Chain evidence unavailable
• Invoices: $ids

No payment state was inferred."
fi
