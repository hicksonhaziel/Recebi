# Reconcile open receivables

Recebi uses two bounded ZeroClaw jobs:

- a lightweight hot watchdog; its worker exits after one no-op pass or checks
  a recent invoice at five-second deadlines for at most 3 minutes;
- a permanent background pass every 5 minutes for all remaining open invoices.

The hot job is a shell job pinned to `scripts/run-reconcile-job.sh hot`. The
runner invokes `recebi_hot_reconcile` directly and sends preformatted Telegram
alerts. No LLM participates in the five-second path.

Create and harden the hot job:

```bash
zeroclaw cron add-every 1000 \
  '/absolute/path/to/Recebi/scripts/run-reconcile-job.sh hot TELEGRAM_CHAT_ID' \
  --agent hickson

scripts/install-hot-reconcile-cron.sh JOB_ID TELEGRAM_CHAT_ID
```

The background job calls only `recebi__recebi_reconcile_open` with
`{"max_count":10}`. Create and harden it:

```bash
zeroclaw cron add '*/5 * * * *' \
  'Run exactly one Recebi background reconciliation pass.' \
  --agent hickson \
  --prompt \
  --allowed-tool recebi__recebi_reconcile_open \
  --uses-memory false \
  --tz Africa/Lagos

scripts/install-reconcile-cron.sh JOB_ID TELEGRAM_CHAT_ID
```

Both installers create mode-0600 SQLite backups and force the jobs enabled.
The hot installer verifies the exact shell command and bounded 5-second loop;
the background installer
verifies the exact tool allowlist and fail-closed delivery. Raw signatures are
never printed in Telegram.
