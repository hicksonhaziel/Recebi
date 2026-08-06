# Operations

This runbook covers normal Recebi operation after [installation](INSTALLATION.md). Commands assume the repository is available locally and ZeroClaw already has a working private Telegram channel.

## Operating principles

1. Treat chain evidence—not chat—as payment truth.
2. Treat `incomplete` as unknown, never as paid or unpaid.
3. Treat `needs_review` as unpaid until the local approval flow completes.
4. Keep configuration, databases, QR files, exports, scheduler backups, and SOP state private.
5. Never paste private RPC URLs, tokens, local paths, or payer key material into Telegram.

## Daily Telegram flow

### Create a receivable

```text
Create invoice INV-001 for 0.10 USDC with public label Acme invoice
```

All three values are required:

- a stable receivable ID;
- a positive decimal USDC amount; and
- a non-sensitive public wallet-display label.

Expected response fields are the invoice, amount, `Awaiting payment`, unique reference, Solana Pay URL, and QR attachment. PTAX is not attached at creation.

Do not include customer personal data in the public label; it may appear in a wallet interface.

### Check a receivable

```text
Check INV-001
```

Interpret terminal states literally:

| Status | Operator meaning |
|---|---|
| `open` / `pending` | No accepted finalized settlement yet |
| `payment_verified` | Exact supported transfer recorded |
| `needs_review` | Candidate mismatch or ambiguity; still unpaid |
| `incomplete` | Chain evidence unavailable or bounded processing stopped |
| `settled_with_variance` | Approved canonical underpayment, not exact payment |
| `cancelled_unpaid` | Operator cancelled it without recording settlement |

Use the trusted Explorer URL returned by Recebi. Telegram output should not display raw transaction signatures.

### Snapshot or close a month

```text
Snapshot 2026-08
Close 2026-07
```

A snapshot is provisional and may cover the active UTC month. A close is an immutable revision for a completed UTC month; active and future month closes are rejected.

Telegram receives bounded counts and, when available, an accountant CSV attachment. The MCP result includes private attachment paths required by ZeroClaw; the skill suppresses ordinary path fields and stock ZeroClaw is expected to strip the attachment marker before chat delivery. Verify this behavior in the deployed version because the model can see the MCP result.

## Scheduled reconciliation

The intended deployment has two jobs:

| Job | Cadence | Runtime | Purpose |
|---|---|---|---|
| Hot watchdog | Every second | Shell, no LLM | Start a bounded five-second loop for invoices younger than three minutes |
| Background pass | Every five minutes | Isolated agent, memory off | Reconcile older open invoices and recover missed work |

### Create and harden the hot watchdog

> **Single-flight requirement:** `run-reconcile-job.sh` has no process-lifetime lock. Its SQLite lease covers each reconciliation pass, not the sleeping worker between passes. Before using a one-second schedule, verify that your ZeroClaw version does not overlap executions of the same shell job. If that cannot be demonstrated, leave the hot job disabled and use the five-minute background pass until an external process lock is implemented.

Replace the repository path, agent alias, and Telegram peer ID:

```bash
zeroclaw cron add-every 1000 \
  '/absolute/path/to/Recebi/scripts/run-reconcile-job.sh hot TELEGRAM_CHAT_ID' \
  --agent hickson

scripts/install-hot-reconcile-cron.sh JOB_ID TELEGRAM_CHAT_ID
```

The helper verifies the exact shell command, accepted schedule shape, agent alias, and local dependencies. It creates a mode-`0600` scheduler database backup before enabling the job. It does not prove scheduler-level single-flight behavior.

The one-second schedule is a lightweight pickup watchdog, not a perpetual network poll. A single worker exits after one no-op pass or performs at most 36 passes at five-second deadlines.

### Create and harden the background job

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

The helper enforces:

- the five-minute schedule;
- the intended agent;
- only `recebi__recebi_reconcile_open`;
- memory disabled;
- bounded prompt and Telegram delivery;
- fail-closed delivery behavior; and
- an enabled job after a mode-`0600` backup.

These helpers directly update the ZeroClaw 0.8.3 cron SQLite database after validation because that CLI version does not expose every required delivery field. Review the scripts before use and retain their printed backup paths.

### Verify scheduler state

```bash
zeroclaw cron list
sqlite3 "$HOME/.zeroclaw/data/cron/jobs.db" \
  'SELECT id,name,enabled,schedule,allowed_tools,uses_memory,last_status FROM cron_jobs;'
```

Expected properties:

- one enabled shell job pinned to `run-reconcile-job.sh hot TELEGRAM_CHAT_ID`;
- one enabled agent job restricted to `recebi__recebi_reconcile_open`;
- memory disabled on the background job; and
- recent successful runs or an explicit visible failure.

## Review an anomaly

Never resolve a mismatch from chat. Run the guided helper locally:

```bash
scripts/review.sh RECEIVABLE_ID
```

The helper:

1. reads the deterministic current candidate;
2. displays the signature, reason, amounts, and candidate fingerprint;
3. offers only supported dispositions;
4. requires the full receivable ID as confirmation;
5. starts the private ZeroClaw SOP through the loopback gateway;
6. requires explicit `APPROVE`, or fails closed on denial/timeout;
7. verifies the durable completed approval receipt; and
8. atomically rechecks the same candidate before mutation.

Supported outcomes are:

| Action | Result |
|---|---|
| Ignore candidate and keep waiting | Returns to `open`; candidate remains recorded |
| Cancel unpaid | Becomes `cancelled_unpaid`; no settlement is created |
| Accept eligible underpayment | Becomes `settled_with_variance` with expected, received, and shortfall amounts |

No review path can convert an inexact transaction to `payment_verified`.

## Month-end procedure

Before closing `YYYY-MM`:

1. Confirm the month is complete in UTC.
2. Check scheduler health and unresolved `needs_review` records.
3. Run a provisional snapshot if review is still in progress.
4. Close the completed month from the private operator channel.
5. Save the CSV with the canonical JSON and manifest from the private export directory.
6. Verify artifact hashes before copying them elsewhere.
7. Record any `valuation_pending` rows and retry only when BCB evidence is expected to be available.

Recebi accepts only official same-day PTAX evidence. It does not invent a weekend, holiday, nearest-date, or manual rate. A later successful retry produces a new append-only close revision.

## Health checks

### ZeroClaw

```bash
zeroclaw doctor
zeroclaw cron list
```

### MCP health

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recebi_health","arguments":{}}}' \
  | /absolute/path/to/recebi-mcp --config "$HOME/.zeroclaw/recebi.toml"
```

### Local permissions

```bash
stat -c '%a %n' \
  "$HOME/.zeroclaw/recebi.toml" \
  "$HOME/.zeroclaw/recebi-data" \
  "$HOME/.zeroclaw/recebi-data/recebi.sqlite3"
```

Expected: configuration and database `0600`, data directory `0700`.

### Code gate

```bash
./scripts/check.sh
```

Run after source changes, dependency updates, or before recording a showcase.

## Failure handling

| Symptom | Safe response |
|---|---|
| `incomplete` | Leave state unchanged; inspect RPC/network health; retry later |
| `needs_review` | Keep invoice unpaid; run local review if a business disposition is needed |
| Hot job missing | Background job remains authoritative; repair the watchdog |
| Background job missing | Manual checks still work; restore the bounded cron job |
| Telegram send fails | Do not acknowledge delivery; durable outbox permits retry |
| PTAX unavailable | Preserve payment; leave valuation pending |
| Ledger integrity error | Stop mutations, preserve files, investigate before restart |
| ZeroClaw session lacks tools | Send `/new` after confirming MCP configuration and restart |
| Wrong merchant/mint/cluster configured | Stop service; correct trusted config; do not pay old URLs |

On ZeroClaw 0.8.3, the deterministic sender resolves `[channels.telegram.default]`. The live Telegram bot alias must therefore be named `default` for the hot runner’s direct delivery path.

## Backup

Recebi uses SQLite WAL mode and private files under the configured `data_dir`. Take a consistent SQLite backup with the SQLite backup API:

```bash
data_dir="$HOME/.zeroclaw/recebi-data"
backup_dir="$HOME/.zeroclaw/backups/recebi"
install -d -m 700 "$backup_dir"
backup="$backup_dir/recebi-$(date -u +%Y%m%dT%H%M%SZ).sqlite3"
sqlite3 "$data_dir/recebi.sqlite3" ".backup '$backup'"
chmod 600 "$backup"
sha256sum "$backup" > "$backup.sha256"
chmod 600 "$backup.sha256"
```

Back up these items together:

- `recebi.toml`, with RPC credentials protected;
- the SQLite backup;
- canonical export revisions and manifests;
- ZeroClaw SOP run state;
- ZeroClaw cron database backups; and
- the exact Recebi binary checksum and source revision.

Store backups outside the live data directory with equivalent or stronger access controls.

## Recovery boundary

Recebi has atomic writes, WAL, leases, integrity checks, and scheduler backups, but this repository does **not** yet ship an automated disaster-restore command or a validated clean-room recovery test.

Before mainnet use:

1. define your backup retention and encryption policy;
2. restore a copy into an isolated machine or user account;
3. point a reviewed config at the restored private directory;
4. verify MCP health and historical checks without running schedulers;
5. compare close manifests and known receivable states; and
6. document the tested procedure for the operator environment.

Do not discover the restore procedure during an incident.

## Devnet evidence runs

Use only the isolated helper under [`../scripts/devnet-wallet.sh`](../scripts/devnet-wallet.sh). Never expose its key file in terminal recordings or chat.

For each run, record:

- environment and date;
- invoice ID and expected amount;
- whether the payer is builder-operated or independent;
- finalized transaction and Explorer URL;
- Recebi’s terminal status;
- scheduler path used;
- idempotent recheck result; and
- any PTAX or approval behavior.

Append verified observations to [Evidence](EVIDENCE.md). Do not turn planned behavior into evidence.
