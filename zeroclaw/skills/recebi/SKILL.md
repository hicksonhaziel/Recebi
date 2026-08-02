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

On success, the result includes a private Telegram-compatible PNG rendered
from the persisted canonical Solana Pay URL. This is a mandatory outbound
transport protocol: when `attachment_marker` is not null, the final reply MUST
end with that exact marker as its own final line. Copy it verbatim, without
backticks, a code block, explanation, alteration, or omission. It is not a
path to discuss with the operator. ZeroClaw removes the marker and uploads its
PNG as a Telegram photo. A response that says a QR was rendered but does not
end in the marker is incorrect. Report the receivable ID, requested amount,
reference, and Solana Pay URL compactly before that final line. State only that
it is **open**—do not claim it was paid.

If the create result has `qr_error` or a null `attachment_marker`, call
`recebi__recebi_render_qr` once with exactly the same `receivable_id`. If that
retry also fails, report the URL and bounded rendering error; do not claim that
a QR was sent.

If the operator later asks to resend or display a QR for a known receivable,
call `recebi__recebi_render_qr` with exactly `receivable_id`, then end the
final reply with its exact `attachment_marker` line. Never pass a URL, path,
wallet, mint, or image format.

If the operator gives sensitive information for a label, ask for a non-sensitive
public label instead. If amount or ID is missing, ask only for the missing
field. Do not invent either value.

Use `recebi__recebi_check` when the operator asks whether one known receivable
was paid. Pass exactly `receivable_id`; never pass a signature, reference,
wallet, mint, cluster, endpoint, or expected amount.

Use `recebi__recebi_watch_payment` only when the operator explicitly says they
are expecting payment now or asks to watch one known receivable. Pass exactly
`receivable_id` and `window`. Start with `window: 1`. Each stock-host-safe
window checks immediately and once more after ten seconds. When and only when
the outcome is `continue`, call the same tool again with the same receivable ID
and the next window number. Never exceed window 4. Do not narrate intermediate
windows. Stop immediately on any outcome other than `continue` and return one
final answer to the operator.

Do not start a watch before returning a newly created payment URL: first return
the URL, then let the operator say `Watch <receivable_id>`.

The watch outcome is also literal:

- `terminal`: interpret only the nested `last_observation` using the states
  below;
- `continue`: silently invoke the next numbered window, unless window 4 was
  already used—in that impossible case, fail closed and stop;
- `pending_timeout`: the watch window ended after a complete final check and
  no accepted settlement was found; say it remains open and may be watched
  again when payment is expected;
- `incomplete_timeout`: RPC or transaction evidence was incomplete on the
  final check; say the current chain status is unknown and never call it paid.

Each tool window's ten-second loop is deterministic. The model may bridge at
most four bounded windows only while this payment is expected. Never repeat a
window number, invent window 5, change the interval, or start an unbounded
retry.

Interpret deterministic tool states literally:

- `pending`: no accepted finalized settlement was found; say it remains open.
- `payment_verified`: exact finalized settlement was verified and recorded.
- `needs_review`: a finalized candidate failed an invariant; say it remains
  unpaid and needs review. Include the bounded reason and candidate fingerprint.
  When the tool explicitly marks it variance-eligible, also report the expected,
  received, and shortfall amounts. Never infer eligibility yourself.
- `settled_with_variance`: a finalized canonical underpayment was explicitly
  accepted by the operator. Report the expected, received, and shortfall amounts
  plus the recorded `variance_reason` field. Do not use the generic `reason`
  field as the business reason. Never describe this as an exact payment.
- `cancelled_unpaid`: the operator cancelled the unpaid receivable; never
  describe it as paid, refunded, or settled.
- tool error: verification is incomplete; never convert this to paid or
  needs-review yourself.

No review mutation operation is available from chat, and the agent must never
start this procedure with `sop_execute`. Review disposition is an operator-only
action through the `recebi-resolve-review` SOP. Tell the local operator to run:

`scripts/review.sh <receivable_id>`

The guided command independently checks and displays the current evidence,
offers only eligible actions, creates the approval request, and applies only
the resulting durable receipt. The local-only mutation is deliberately absent
from the model's MCP tool list and instructions. Recebi atomically rechecks the
live candidate state and full fingerprint before changing anything.

The SOP must pause for an out-of-band approval. The agent cannot approve its
own run. The normal actions are `ignore_candidate_and_reopen` and
`cancel_unpaid`. Only when Recebi explicitly proves a single canonical
finalized underpayment may the guided command also offer
`accept_underpayment_with_variance`, with exactly one reason:
`rounding_adjustment`, `commercial_discount`, or `merchant_write_off`.
Never suggest an arbitrary tolerance, signature override, refund, or any other
action. A stale fingerprint, ineligible transaction, denied/timed-out run, or
tool error changes nothing.

For Portuguese operator messages, reply in concise Brazilian Portuguese:

- `pending`: `Ainda em aberto; nenhum pagamento finalizado válido foi encontrado.`
- `payment_verified`: `Pagamento exato verificado e registrado.`
- `needs_review`: `Continua não pago e requer revisão: <reason>.`
- `settled_with_variance`: `Pagamento menor aceito pelo operador com diferença registrada: esperado <expected>, recebido <received>, diferença <shortfall>, motivo <variance_reason>.`
- `cancelled_unpaid`: `Recebível não pago cancelado pelo operador.`

Use concise English equivalents when the operator writes in English. Never
translate identifiers, signatures, hashes, reasons, or action names.

Use `recebi__recebi_reconcile_open` only for an operator-requested bounded scan
or an explicitly enabled low-frequency fallback schedule. It is not the
default live-payment path; use the on-demand watch while payment is expected.
Pass no arguments or a `max_count` no greater than 10. Report the
checked, payment_verified, pending, needs_review, and incomplete counts plus at
most the returned anomaly and incomplete IDs. An incomplete record means its
status is unknown and must not be called paid. For a scheduled run, report a
compact alert when payment_verified, needs_review, or incomplete is non-zero;
when all three are zero, return exactly `NO_REPLY[INFO]: no new Recebi
activity`. Do not retry automatically, fetch raw RPC data, inspect transaction
memos, or infer settlement from chat.

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
