# Evidence

This is the dated evidence record for Recebi. It separates observed behavior from planned work and labels self-operated devnet activity explicitly.

> **Current evidence boundary:** real Telegram and ZeroClaw operation, finalized Solana devnet transfers, official BCB responses, scheduler runs, restart behavior, and local approval tests have been observed. Independent customer usage, mainnet operation, accountant acceptance, a public showcase video, and a clean-machine reproduction have not yet been established.

## Coverage summary

| Capability | Evidence | Scope |
|---|---|---|
| Telegram request creation and QR | Observed through running ZeroClaw | Self-operated devnet |
| Exact reference-bound payment | Finalized transaction recorded once | Builder-operated payer |
| Correct reference, wrong amount | Finalized transaction remained unpaid | Builder-operated adversarial case |
| Later exact payment after ignored mismatch | Finalized and independently reconciled | Builder-operated payer |
| Durable review approval | Denial, timeout, approval, and replay exercised | Local ZeroClaw SOP |
| PTAX outage | Payment preserved, valuation pending | Live BCB network failure |
| PTAX success | Same-day quote and response hash retained | Official BCB endpoint |
| Automatic hot reconciliation | Payment detected by bounded worker | Self-operated devnet |
| Restart/idempotency | Repeated checks and daemon restarts | Local deployment |
| Mainnet or third-party receivable | Not demonstrated | Must not be claimed |

Historical entries below preserve the terminology and tool counts that were current on their dates. The current discoverable MCP surface contains nine bounded tools; review mutation and notification acknowledgement remain non-discoverable.

## 2026-07-27 — Telegram request creation

- Environment: Solana devnet; locally built ZeroClaw 0.8.3; Telegram.
- Operator sent a real inbound Telegram request for `TEST-TELEGRAM-004`.
- ZeroClaw invoked only `recebi__recebi_create_request`.
- Recebi returned an open 0.10 devnet-USDC request with an intact public reference and Solana Pay URL.
- No payment or settlement was claimed.

## 2026-07-28 — Bounded scheduled reconciliation started

- Environment: Solana devnet; ZeroClaw model `gpt-5.6-luna`; local stdio MCP.
- The release binary reached the configured HTTPS RPC and returned `TEST-TELEGRAM-004` as pending.
- A post-restart one-shot ZeroClaw job exposed only `recebi__recebi_reconcile_open`, called it exactly once with `{"max_count":10}`, and completed successfully.
- Observed result: 5 checked, 0 payment verified, 5 pending, 0 needs review, no anomaly samples.
- A memory-disabled recurring job was scheduled for bounded reconciliation.
- Test receivables `PHASE4-EXACT-001` and `PHASE4-WRONG-001` were created but not paid at this point.

## 2026-07-28 — Trusted merchant changed before payment

- Before either test wallet action occurred, the operator replaced the trusted devnet merchant wallet.
- The original links remained append-only historical records targeting the former wallet and were marked not to be paid.
- ZeroClaw was restarted after the mode-`0600` configuration changed.
- `recebi_health` passed without exposing the wallet, and the recurring schedule remained installed.
- Replacement records `PHASE4-EXACT-002` and `PHASE4-WRONG-002` were created against the new trusted wallet and returned `pending` through live devnet RPC checks.

## 2026-07-28 — Exact and wrong-amount settlement proof

- Payer: builder-operated isolated devnet test wallet. This is not an independent customer or mainnet payment.
- `PHASE4-EXACT-002` received exactly 0.10 devnet USDC with its reference.
- Finalized exact signature: `YRz3msWunCXiPp7kWM58nYe3R6q1FNnqNKziYCia9gSGxWJTBvvFdwh9Tzf9pmyQ7PgjYS52udpjaRZ1Kp66m9N`.
- Recebi returned `payment_verified`. A repeated check returned the same signature. Read-only SQLite inspection found exactly one settlement row and one `payment_verified` event.
- `PHASE4-WRONG-002` expected 0.10 but received 0.01 devnet USDC with the correct reference.
- Finalized mismatch signature: `4Ev46ognTRF9jhRGFwhjcbUw3y42gKbRmPAb69247DPyo8QHPdkDqb7VToVYcVHcvqLvTPAbEiLGS9KWdtDKhLbD`.
- Recebi returned `needs_review` with `wrong_amount`; the invoice remained unpaid. A repeated check returned the same evidence. SQLite contained one review row and one `needs_review` event.
- The payer balance moved from 20 to 19.89 devnet USDC, matching both finalized transfers.
- Telegram terminal-state checks invoked `recebi__recebi_check` once per message with only the receivable ID and delivered the exact durable results.

## 2026-07-28 — Official PTAX evidence and July close

- The local release closed UTC month `2026-07` against the verified devnet receipt. No signing or wallet key was involved.
- Initial bounded BCB lookups encountered DNS timeouts. Recebi preserved `payment_verified` and produced an honest `valuation_pending` revision.
- A later retry reached the official BCB `CotacaoDolarDia` endpoint.
- Strict same-day policy accepted one quote for `2026-07-28`: purchase `5.11710`, sale `5.11770`, bulletin timestamp `2026-07-28 13:25:31.150278`, response SHA-256 `3ca1a5079993b3484c85e8010b573fc41444bae25b1880dabe43f002af095e6f`.
- Evidence records the nominal `1 USDC = 1 USD` assumption. Integer half-up rounding produced BRL `0.51` for `0.10` USDC.
- SQLite preserved two append-only close revisions: one pending and one with BCB evidence. Repeating identical closes was byte-idempotent.
- Canonical JSON, CSV, and manifest hashes were independently recomputed and matched the tool outputs.

## 2026-07-29 — Approval-gated unpaid anomaly disposition

- Environment: self-operated Solana devnet; local Recebi; stock ZeroClaw 0.8.3.
- The existing wrong-amount candidate on `PHASE4-WRONG-002` was inspected, fingerprint-bound, and reopened through durable out-of-band approval.
- The old finalized 0.01 transaction stayed ignored. The isolated payer then sent the exact requested 0.10 USDC using the original reference.
- Recebi verified only the later exact signature: `3VbwpV5MgFR6bJM4oRrNibVe3tqqNydjcmd3A9FZzzjvC3jjfqqGWFon9XDuzRpSNzmNgBRPgFwztBMZGMroZkTT`.
- `PHASE6-CANCEL-001` and `PHASE6-API-001` each received deliberate finalized 0.01 mismatches and were later changed to `cancelled_unpaid`; no settlement row was created.
- An out-of-band SOP run appeared in `zeroclaw sop pending`, resumed only after operator approval, and completed with durable state.
- A ZeroClaw 0.8.3 issue was found: chat-local SOP runs used a different live engine from the gateway approval surface until restart. Recebi forbids that launch path and uses the shared authenticated dashboard/API engine.
- After daemon restarts, Telegram, channels, gateway, scheduler, and daemon health returned `ok`.

## 2026-07-29 — Approval boundary correction and fail-closed proof

- Adversarial testing found that the initial two-step SOP design could return post-gate instructions to the model while the durable run still said `waiting_approval`.
- Although the engine later rejected or failed the run and no fund movement existed, model-visible mutation was removed from discovery entirely.
- The corrected SOP creates only an approval receipt. A separate local operator command requires a terminal `completed` run with matching fingerprint/action fields, derives the request locally, and invokes the non-discoverable operation.
- The store atomically rechecks that the same candidate remains unresolved before any state change.
- `PHASE6-OPERATOR-001` expected 0.10 devnet USDC and received a finalized 0.01 mismatch.
- Signature: `4h1jBZxSL6EJ4kWJkV1Aw6w3bApN1CKjt36U9bJG6PzBAT7hbzopnoMPaQop633NrkhCo8FGSECThsJVr3SZH4wz`.
- Candidate fingerprint: `e248a2c285c84a4ea32df41cd0197ea33d9f4aae9dd4c8c552eac2bb8c70f5e5`.
- A denied run was refused and left the receivable `needs_review`.
- A five-second test timeout also produced a cancelled run that was refused; production timeout settings were restored.
- An approved run became durably `completed`. Only then did the local operator command change the record to `cancelled_unpaid`.
- Replaying the same receipt was idempotent and added no event.
- A direct `accept_as_paid` injection failed argument parsing.
- SQLite integrity returned `ok`; live configuration, database, and SOP files remained mode `0600`, with private data directories mode `0700`.

## 2026-08-03 — Automatic hot reconciliation

- Environment: self-operated Solana devnet; locally built Recebi; ZeroClaw 0.8.3; Telegram.
- Invoice `PHASE7-HOT-001` expected `0.10` USDC with reference `8qN7goL5WgmLojaAhXmRGeg2cjHscnLy7Axdndh4VEfe`.
- An isolated builder-operated payer sent the exact finalized devnet payment.
- Explorer: <https://explorer.solana.com/tx/4NCcDNhFSHnBdfhY6ZKLhjcZ595KgbiDrKYp673B6keKNTEXsppxH5Q6qDL78y2D6sEm9tuoXcCc2LU9sVkdMwrR?cluster=devnet>.
- Recebi created the invoice at `2026-08-03 00:58:45 UTC`; the automatic hot worker recorded `payment_verified` at `2026-08-03 00:59:30 UTC`.
- SQLite contained one exact settlement and durable `payment_verified` state. Recebi used no signing key.
- With no recent invoice, the watchdog worker exits after one pass. With a recent invoice, it checks at five-second deadlines for at most three minutes.
- The permanent background job remains the fallback for older open invoices.

## Evidence still required before stronger claims

- a clean installation by another operator;
- a recorded public video under three minutes;
- an observed prompt-injection transcript through the demonstrated ZeroClaw channel, including before/after state;
- an exact public commit matching the video binary;
- an independent payer or genuine business receivable;
- mainnet operation, if mainnet is claimed;
- a tested backup/restore drill; and
- professional accounting/legal review, if any compliance claim is made.

Add new entries only after observing the behavior. Include environment, actor ownership, transaction finality, exact Recebi state, relevant scheduler/SOP path, and any failure. Never rewrite a planned test as completed evidence.
