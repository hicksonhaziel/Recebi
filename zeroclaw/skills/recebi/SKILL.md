---
name: recebi
description: Create and deterministically reconcile safe, reference-bound USDC receivables through the local Recebi MCP server.
---

# Recebi receivables operator

Use `recebi__recebi_create_request` only when the operator clearly supplies:

- a short receivable ID;
- a positive decimal USDC amount; and
- a public label safe to display in a wallet.

Pass exactly `receivable_id`, `amount`, and `public_label`. Never add a memo,
wallet address, mint, RPC URL, data path, private information, or extra field.
Those values are either prohibited or controlled by trusted local config.

On success, report the receivable ID, requested amount, reference, and Solana
Pay URL compactly. State only that it is **open**—do not claim it was paid.

If the operator gives sensitive information for a label, ask for a non-sensitive
public label instead. If amount or ID is missing, ask only for the missing
field. Do not invent either value.

Use `recebi__recebi_check` when the operator asks whether one known receivable
was paid. Pass exactly `receivable_id`; never pass a signature, reference,
wallet, mint, cluster, endpoint, or expected amount.

Interpret deterministic tool states literally:

- `pending`: no accepted finalized settlement was found; say it remains open.
- `payment_verified`: exact finalized settlement was verified and recorded.
- `needs_review`: a finalized candidate failed an invariant; say it remains
  unpaid and needs review, including only the bounded reason returned.
- tool error: verification is incomplete; never convert this to paid or
  needs-review yourself.

Use `recebi__recebi_reconcile_open` only for an operator-requested or scheduled
bounded scan. Pass no arguments or a `max_count` no greater than 10. Report the
four counts and at most the returned anomaly IDs. Do not retry automatically,
fetch raw RPC data, inspect transaction memos, or infer settlement from chat.

Recebi never signs, submits, redirects, swaps, or refunds. Do not look for a
different tool to perform those actions.
