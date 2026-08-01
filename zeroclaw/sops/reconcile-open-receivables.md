# Reconcile open receivables

Purpose: provide an optional low-frequency recovery scan without allowing the
model to decide settlement truth. The default live-payment path is the bounded
on-demand `recebi_watch_payment` tool, so this schedule should remain disabled
unless an operator explicitly wants background recovery scans.

## Agent prompt

```text
Run exactly one Recebi reconciliation pass. Call only
recebi__recebi_reconcile_open with {"max_count":10}. Do not retry and do not
use raw HTTP/RPC tools. Return the checked, payment_verified, pending,
needs_review, and incomplete counts plus only the bounded anomaly and
incomplete IDs supplied by the tool. An incomplete record means no payment
state was inferred for that record. If payment_verified, needs_review, or
incomplete is non-zero, send a compact operator alert. If all three are zero,
return exactly `NO_REPLY[INFO]: no new Recebi activity`. Never infer that a
receivable was paid from an error.
```

## ZeroClaw schedule

If explicitly required, the operator installs the prompt as an explicit-agent
cron job and restricts the invocation to the single reconciliation tool. This
is not the ten-second demo path and must not be enabled merely because an open
receivable exists. The MCP batch remains capped at ten records and uses a
SQLite singleton lease, so it cannot become an unbounded RPC loop:

```bash
zeroclaw cron add '*/5 * * * *' \
  'Run exactly one Recebi reconciliation pass. Call only recebi__recebi_reconcile_open with {"max_count":10}. Do not retry and do not use raw HTTP/RPC tools. Return the checked, payment_verified, pending, needs_review, and incomplete counts plus only bounded anomaly and incomplete IDs supplied by the tool. If payment_verified, needs_review, or incomplete is non-zero, send a compact operator alert. If all three are zero, return exactly NO_REPLY[INFO]: no new Recebi activity. Never infer payment from an error.' \
  --agent hickson \
  --prompt \
  --allowed-tool recebi__recebi_reconcile_open \
  --uses-memory false \
  --tz Africa/Lagos
```

The 0.8.3 CLI does not expose cron delivery flags. After the command prints the
job ID, configure its authorized Telegram peer and restart the daemon with the
repository helper (the helper creates a mode-0600 backup first):

```bash
scripts/install-reconcile-cron.sh JOB_ID TELEGRAM_CHAT_ID
```

The resulting delivery is `announce` to that explicit Telegram peer with
`best_effort=false`; a delivery failure is therefore visible as a failed cron
run. A normal no-change run returns `NO_REPLY[INFO]` and is suppressed by the
channel, while a payment, anomaly, or incomplete scan is announced.

The configured maximum, adapter call limits, RPC byte/deadline limits, and
SQLite singleton lease remain authoritative even if the prompt is changed.
