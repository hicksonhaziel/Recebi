---
name: recebi
description: Create, deterministically reconcile, and close safe reference-bound USDC receivables through the local Recebi MCP server.
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
  unpaid and needs review, including only the bounded reason and candidate
  fingerprint returned.
- `cancelled_unpaid`: the operator cancelled the unpaid receivable; never
  describe it as paid, refunded, or settled.
- tool error: verification is incomplete; never convert this to paid or
  needs-review yourself.

Never call `recebi__recebi_resolve_review` or `sop_execute` directly from chat.
Review disposition is an operator-only action through the
`recebi-resolve-review` SOP. In ZeroClaw v0.8.3, tell the operator to start the
SOP from the authenticated local dashboard/API, then approve it out of band
with `zeroclaw sop approve <run_id>`. The dashboard/API and approval CLI share
one SOP engine; chat-local `sop_execute` does not reliably expose its live gate
to the CLI until restart.

The operator may start the SOP only after supplying:

- the receivable ID;
- the exact candidate fingerprint returned by `recebi__recebi_check`; and
- exactly `ignore_candidate_and_reopen` or `cancel_unpaid`.

The SOP must pause for an out-of-band approval. The agent cannot approve its
own run. Never suggest, invent, or emulate `accept_as_paid`, tolerance,
signature override, refund, or any other action. Both supported actions keep
the candidate unpaid. A stale fingerprint, a denied/timed-out run, or a tool
error changes nothing.

For Portuguese operator messages, reply in concise Brazilian Portuguese:

- `pending`: `Ainda em aberto; nenhum pagamento finalizado válido foi encontrado.`
- `payment_verified`: `Pagamento exato verificado e registrado.`
- `needs_review`: `Continua não pago e requer revisão: <reason>.`
- `cancelled_unpaid`: `Recebível não pago cancelado pelo operador.`

Use concise English equivalents when the operator writes in English. Never
translate identifiers, signatures, hashes, reasons, or action names.

Use `recebi__recebi_reconcile_open` only for an operator-requested or scheduled
bounded scan. Pass no arguments or a `max_count` no greater than 10. Report the
four counts and at most the returned anomaly IDs. Do not retry automatically,
fetch raw RPC data, inspect transaction memos, or infer settlement from chat.

Use `recebi__recebi_snapshot_month` when the operator asks for a report,
preview, or export of the active UTC month. Pass exactly `month` in `YYYY-MM`
form. Describe it as a provisional snapshot, never as a final close.

Use `recebi__recebi_close_month` only when the operator explicitly asks to
finally close a completed UTC settlement month. Never use it for the active or
a future month. Pass exactly `month` in `YYYY-MM` form. Report the artifact
kind, revision, verified, valued, and valuation-pending counts plus the three
hashes and export directory returned by the tool.

A successful snapshot or close does not mean every valuation exists. Say
`valuation_pending` literally when the count is non-zero. Describe the files
only as “accountant-ready evidence” that “may assist record keeping.” Never
claim that PTAX proves USDC fair value, tax treatment, DeCripto compliance, or
legal acceptance. The nominal valuation explicitly assumes 1 USDC = 1 USD and
uses the official same-day PTAX sale quote; it is not a market-price proof.

Recebi never signs, submits, redirects, swaps, or refunds. Do not look for a
different tool to perform those actions.
