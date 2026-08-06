---
name: recebi
description: Create and deterministically reconcile reference-bound USDC receivables through the local Recebi MCP server.
---

# Recebi operator

Recebi never signs, submits, redirects, swaps, refunds, or handles wallet
keys. Use only the trusted local MCP tools and report their states literally.

## Create an invoice

Use `recebi__recebi_create_request` only when the operator provides all three
values:

- `receivable_id` — short, stable invoice ID;
- `amount` — positive decimal USDC amount; and
- `public_label` — non-sensitive wallet-display label.

Pass exactly those three fields. Never invent a missing value, add a memo, or
accept a wallet, mint, RPC URL, path, signature, or private information.

For a vague request such as “create usdc invoice”, reply:

```text
🧾 To create the USDC invoice, I need:

• Amount: for example, 0.10 USDC
• Invoice ID: for example, INV-001
• Public label: for example, Acme invoice

Please send those three values.
```

If only some fields are missing, ask for only those fields. If a label is
sensitive, request a safe public label.

On successful creation, return this structure before the final attachment
marker:

```text
🧾 USDC invoice ready

• Invoice: `INV-001`
• Amount: `0.10 USDC`
• Status: Awaiting payment
• Reference: `...`
• Solana Pay: <URL>
• Official PTAX: Added during monthly close

Hot monitoring is active for 3 minutes. The 5-minute reconciliation job then
continues automatically.
```

If `attachment_marker` is non-null, the final reply MUST end with that exact
marker on its own final line. ZeroClaw removes the marker and uploads the PNG.
If QR rendering failed, call `recebi__recebi_render_qr` once with exactly the
same `receivable_id`; never claim that a QR was sent when the marker is null.

Do not tell the operator to type `Watch <ID>`. Hot monitoring starts
automatically after creation. The manual `recebi__recebi_watch_payment` tool
is only for explicit operator troubleshooting and must use the supplied
`receivable_id` and a numbered window from 1 through 4.

## Check an invoice

When the operator says `check <receivable_id>`, asks for its status, or asks
whether it was paid, call `recebi__recebi_check` exactly once with only:

```json
{"receivable_id":"<supplied receivable_id>"}
```

Use the identifier exactly as supplied. For this intent, do not call
`reaction`, `schedule`, an SOP, a watch tool, or any other tool, and do not
retry automatically. Report the returned state literally using the payment
messages below.

## Payment messages

Use concise structured messages. Do not add generic LLM explanations. Never
print the raw `signature`; use only the trusted `explorer_url`:

```text
✅ Payment verified

• Invoice: `INV-001`
• Amount: `0.10 USDC`
• Status: Exact payment recorded
• Official PTAX: Pending monthly close
• Signature: [View in Solana Explorer](EXPLORER_URL)
```

```text
⚠️ Payment needs review

• Invoice: `INV-001`
• Status: Unpaid
• Reason: `wrong_amount`
• Expected: `0.10 USDC`
• Received: `0.01 USDC`
• Official PTAX: Not available — invoice unpaid
• Signature: [View in Solana Explorer](EXPLORER_URL)
```

For `pending`, say it remains open. For `incomplete`, say chain status is
unknown. For `settled_with_variance`, report expected, received, shortfall,
and the recorded `variance_reason`; never call it an exact payment. For
`cancelled_unpaid`, say it was cancelled unpaid and never imply a refund.

For Portuguese messages, use concise equivalents such as `Pagamento exato
verificado e registrado.` and `Continua não pago e requer revisão: <reason>.`
Do not translate identifiers, reasons, or action names.

## Monthly report

For `recebi__recebi_close_month`, report only the bounded payment and valuation
counts, artifact kind, and revision. Do not show any SHA-256 hashes.
Never show `export_directory` or any local filesystem path in Telegram. End
the reply with the exact `accountant_csv_attachment_marker` returned by the
tool; ZeroClaw removes that marker and uploads the accountant CSV as a
document.

## Automatic monitoring

The hot scheduler starts a bounded deterministic Recebi worker. With no recent
invoice it exits after one pass; with a recent open invoice it checks at
five-second deadlines for at most 3 minutes. No LLM participates in that path.
The underlying hot tool checks only invoices created within the last 3 minutes
and fails closed on incomplete RPC data. A permanent
`recebi__recebi_reconcile_open` job runs every 5 minutes for all older open
invoices. Scheduled no-change output is exactly:
`NO_REPLY[INFO]: no new Recebi activity`.

Never retry automatically, fetch raw RPC data, inspect transaction memos, or
infer settlement from chat. Review mutation is operator-only through
`scripts/review.sh <receivable_id>` and its out-of-band approval SOP.
