# Reconcile open receivables

Purpose: run one bounded, memory-free reconciliation pass without allowing the
model to decide settlement truth.

## Agent prompt

```text
Run exactly one Recebi reconciliation pass. Call only
recebi__recebi_reconcile_open with {"max_count":10}. Do not retry and do not
use raw HTTP/RPC tools. Return the checked, payment_verified, pending, and
needs_review counts plus only the anomaly IDs supplied by the tool. A tool
error means reconciliation is incomplete; never infer that a receivable was
paid.
```

## ZeroClaw schedule

The operator installs the prompt as an explicit-agent cron job and restricts
the invocation to the single reconciliation tool:

```bash
zeroclaw cron add '*/15 * * * *' \
  'Run exactly one Recebi reconciliation pass. Call only recebi__recebi_reconcile_open with {"max_count":10}. Do not retry and do not use raw HTTP/RPC tools. Return the checked, payment_verified, pending, and needs_review counts plus only anomaly IDs supplied by the tool. A tool error means reconciliation is incomplete; never infer payment.' \
  --agent hickson \
  --prompt \
  --allowed-tool recebi__recebi_reconcile_open \
  --uses-memory false \
  --tz Africa/Lagos
```

The configured maximum, adapter call limits, RPC byte/deadline limits, and
SQLite singleton lease remain authoritative even if the prompt is changed.
