# Cron architecture

## Hot dispatcher

- ZeroClaw schedule: fixed interval, `every_ms = 1000`, used as a lightweight
  watchdog.
- Job type: shell, pinned to `scripts/run-reconcile-job.sh hot CHAT_ID`.
- With no recent open invoice, the worker makes one bounded call and exits.
- With a recent open invoice, the worker invokes `recebi_hot_reconcile` at
  monotonic five-second deadlines for at most 3 minutes. No LLM is involved.
- Active invoice window: 180 seconds, enforced inside Recebi.
- No terminal records: the runner exits quietly.
- Terminal record: the runner sends one structured Telegram alert with an
  Explorer link.

The watchdog stays installed, but the shell worker exists only for one no-op
pass or one bounded hot window. An invoice leaves the hot set after three
minutes. This avoids per-invoice scheduler mutation and orphaned jobs.

## Background reconciliation

- ZeroClaw schedule: `*/5 * * * *` in `Africa/Lagos`.
- Allowed tool: `recebi__recebi_reconcile_open` only.
- Maximum records per pass: 10.
- Purpose: recovery and monitoring after the hot window.

## Installation

Use [install-hot-reconcile-cron.sh](../scripts/install-hot-reconcile-cron.sh)
and [install-reconcile-cron.sh](../scripts/install-reconcile-cron.sh). Both
helpers back up the scheduler database and force the job enabled. The hot
installer verifies its exact command; the background installer verifies the
schedule, tool allowlist, memory, and Telegram delivery before restart.

ZeroClaw 0.8.3 calculates the next fixed interval only after a shell worker
finishes. A nominal 5-second job whose pass takes about 5 seconds therefore
starts roughly every 10 seconds. The 1-second watchdog minimizes idle pickup
latency. Once a recent invoice is present, timing moves inside the deterministic
bounded worker, which provides the real 5-second cadence without a perpetual
process.
