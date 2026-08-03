# Operations

## Deploy

```bash
scripts/check.sh
scripts/install.sh
zeroclaw service restart
zeroclaw doctor
```

Keep `README.md` minimal. Operational documentation lives in `docs/`.

## Verify scheduler state

```bash
zeroclaw cron list
sqlite3 ~/.zeroclaw/data/cron/jobs.db \
  'SELECT id,name,enabled,schedule,allowed_tools,uses_memory,last_status FROM cron_jobs;'
```

Expected state:

- one enabled lightweight watchdog shell job pinned to
  `scripts/run-reconcile-job.sh hot TELEGRAM_CHAT_ID`;
- with no recent invoice the shell worker exits after one pass; with a recent
  invoice it checks at five-second deadlines for at most three minutes;
- one enabled five-minute job restricted to
  `recebi__recebi_reconcile_open`;
- the background agent job uses an isolated session, memory disabled, and
  explicit Telegram delivery; the hot runner sends directly without an LLM.

## Failure handling

- `incomplete`: RPC evidence was unavailable; no payment state was inferred.
- `needs_review`: run `scripts/review.sh RECEIVABLE_ID` locally.
- hot job unavailable: the five-minute background job remains authoritative.
- Telegram delivery failure: job delivery is fail-closed and the failed run is
  visible in ZeroClaw history.

## Devnet proof

Use the isolated payer only through `scripts/devnet-wallet.sh`. Record the
invoice ID, expected amount, finalized signature, Recebi terminal status,
Explorer URL, scheduler run, and payer balance delta in `docs/operator-diary.md`.
