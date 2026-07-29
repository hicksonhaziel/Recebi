# Recebi operator diary

This is a dated record of observed operation. Devnet and self-operated tests
are labelled honestly; they are not represented as client or mainnet usage.

## 2026-07-27 — Telegram request creation

- Environment: Solana devnet; locally built ZeroClaw 0.8.3; Telegram.
- Operator sent a real inbound Telegram request for `TEST-TELEGRAM-004`.
- ZeroClaw invoked only `recebi__recebi_create_request`.
- Recebi returned an open 0.10 devnet-USDC request with an intact public
  reference and Solana Pay URL.
- No payment or settlement was claimed.

## 2026-07-28 — bounded scheduled reconciliation started

- Environment: Solana devnet; ZeroClaw model `gpt-5.6-luna`; local stdio MCP.
- The release binary reached the configured HTTPS RPC and returned
  `TEST-TELEGRAM-004` as pending.
- A post-restart one-shot ZeroClaw job exposed only
  `recebi__recebi_reconcile_open`, called it exactly once with
  `{"max_count":10}`, and completed successfully.
- Observed result: 5 checked, 0 payment verified, 5 pending, 0 needs review,
  no anomaly samples.
- A memory-disabled recurring job remains scheduled every 15 minutes in
  `Africa/Lagos`, restricted to the same single tool.
- Live Phase 4 test receivables `PHASE4-EXACT-001` and
  `PHASE4-WRONG-001` were created. At this entry, neither wallet action had
  occurred; the exact and deliberate wrong-amount proofs remain pending.

## 2026-07-28 — trusted merchant changed before payment

- Before either Phase 4 wallet action occurred, the operator replaced the
  trusted devnet merchant wallet.
- The original `PHASE4-EXACT-001` and `PHASE4-WRONG-001` links remain
  append-only historical test records targeting the former wallet and must not
  be paid.
- ZeroClaw was restarted after the mode-0600 trusted configuration changed.
  `recebi_health` passed without exposing the wallet, and the recurring
  reconciliation schedule remained installed.
- Replacement records `PHASE4-EXACT-002` and `PHASE4-WRONG-002` were created
  against the new trusted wallet. Both were independently checked through the
  live devnet RPC and returned `pending`.

## 2026-07-28 — exact and wrong-amount settlement proof

- Payer: builder-operated isolated devnet test wallet. This is self-operated
  devnet evidence, not an independent customer or mainnet payment.
- `PHASE4-EXACT-002` received exactly 0.10 devnet USDC with its reference.
  Finalized signature:
  `YRz3msWunCXiPp7kWM58nYe3R6q1FNnqNKziYCia9gSGxWJTBvvFdwh9Tzf9pmyQ7PgjYS52udpjaRZ1Kp66m9N`.
- Recebi independently returned `payment_verified`. A repeated check returned
  the same signature. Read-only SQLite inspection found exactly one settlement
  row and one `payment_verified` event.
- `PHASE4-WRONG-002` expected 0.10 but received 0.01 devnet USDC with the
  correct reference. Finalized signature:
  `4Ev46ognTRF9jhRGFwhjcbUw3y42gKbRmPAb69247DPyo8QHPdkDqb7VToVYcVHcvqLvTPAbEiLGS9KWdtDKhLbD`.
- Recebi independently returned `needs_review` with `wrong_amount`; it remains
  unpaid. A repeated check returned the same evidence. Read-only SQLite
  inspection found exactly one review row and one `needs_review` event.
- The payer balance moved from 20 to 19.89 devnet USDC, matching the two
  finalized transfers. It retained approximately 0.19998 devnet SOL.
- ZeroClaw's recurring memory-disabled 15-minute reconciliation job remains
  installed. Its latest recorded run is successful.
- Real Telegram terminal-state proof passed at 2026-07-28 16:25 WAT. The
  operator sent one check for each ID. Runtime traces show
  `recebi__recebi_check` called once per message with only the receivable ID,
  2–3 ms tool execution, the exact durable results above, and successful
  outbound Telegram delivery.
- The exact reply included `payment_verified` and the complete signature. The
  adversarial reply said the receivable remains unpaid, needs review, and gave
  only `wrong_amount`.

## 2026-07-28 — official PTAX evidence and July close

- The locally built release closed UTC settlement month `2026-07` against the
  existing verified devnet receipt. No signing or wallet key was involved.
- Initial bounded BCB lookups encountered transient DNS timeouts. Recebi kept
  `PHASE4-EXACT-002` payment-verified and produced an honest
  `valuation_pending` close; it did not substitute another date or rate.
- A later operator retry reached the pinned official BCB
  `CotacaoDolarDia` endpoint. The strict same-day policy accepted exactly one
  quote for operation date `2026-07-28`: purchase `5.11710`, sale `5.11770`,
  bulletin timestamp `2026-07-28 13:25:31.150278`, and exact response SHA-256
  `3ca1a5079993b3484c85e8010b573fc41444bae25b1880dabe43f002af095e6f`.
- The evidence explicitly records the nominal `1 USDC = 1 USD` assumption.
  Integer half-up rounding produced a BRL reference of `0.51` for `0.10` USDC.
  This is not represented as USDC fair-value proof, tax advice, or legal
  acceptance.
- SQLite preserves both close revisions append-only: revision 1 records the
  source-outage/pending result and revision 2 records the later BCB evidence.
  Repeating identical closes is byte-idempotent.
- The current canonical JSON, presentation CSV, and manifest hashes were
  independently recomputed with `sha256sum` and matched the tool:
  `49b4e1e322ec356c3c46fdc609112024057ffb044cf8cbb1524c9da0cb51f120`,
  `d8c966bcb04f6b05188cf3ee913029438e114f8c4d2271e220455706b5a418ae`,
  and
  `cef5607686de502a990d68fe8252acfd56702eba75d5f73608163b5dc897df32`.

## 2026-07-29 — approval-gated unpaid anomaly disposition

- Environment: self-operated Solana devnet; locally built Recebi; stock
  ZeroClaw 0.8.3; `gpt-5.6-luna`. This is builder-operated test evidence, not
  client or mainnet activity.
- Recebi added only two operator-approved review outcomes:
  `ignore_candidate_and_reopen` and `cancel_unpaid`. Neither outcome can mark a
  payment verified.
- The existing wrong-amount candidate on `PHASE4-WRONG-002` was inspected,
  fingerprint-bound, and reopened through a durable out-of-band approval SOP.
  Its old finalized 0.01 transaction stayed ignored. The isolated devnet payer
  then sent the exact requested 0.10 USDC using the original reference.
  Recebi verified only the later finalized signature:
  `3VbwpV5MgFR6bJM4oRrNibVe3tqqNydjcmd3A9FZzzjvC3jjfqqGWFon9XDuzRpSNzmNgBRPgFwztBMZGMroZkTT`.
- `PHASE6-CANCEL-001` and `PHASE6-API-001` each expected 0.10 USDC and received
  a deliberate finalized 0.01 mismatch. Both were subsequently dispositioned
  as `cancel_unpaid` and now return `cancelled_unpaid`; no settlement row was
  created.
- The production SOP path starts through ZeroClaw's authenticated local
  dashboard/API and pauses before the mutation step. Run
  `run-1785341983660726113-0001` appeared immediately in
  `zeroclaw sop pending`, resumed only after
  `zeroclaw sop approve`, and completed with `state: cancelled`.
- A stock ZeroClaw 0.8.3 issue was observed and preserved honestly:
  chat-local `sop_execute` runs use a different live engine from the gateway
  out-of-band approval surface until restart. Recebi therefore forbids that
  launch path and uses the shared authenticated dashboard/API engine.
- After repeated daemon restarts, Telegram, channels, gateway, scheduler, and
  daemon health were all `ok`. The existing 15-minute, memory-disabled,
  single-tool cron ran at `2026-07-29T16:15:27Z` and returned a bounded
  seven-record scan with no anomalies.
- A Portuguese live check returned
  `Recebível não pago cancelado pelo operador.` An English live check returned
  `Payment verified and recorded.`

## 2026-07-29 — approval boundary correction and fail-closed proof

- Adversarial testing found a stock ZeroClaw 0.8.3 boundary flaw in the initial
  two-step design: `sop_advance` returned the post-gate step instructions to the
  same model while the durable run still said `waiting_approval`. The engine
  later rejected or failed the run, but a model-visible mutation tool could
  already have produced a side effect. This was observed on self-operated
  devnet test records only; no signing, submission, refund, or fund movement was
  available.
- Recebi now removes review mutation from MCP discovery entirely. Telegram and
  the model see six tools, none capable of resolving a review. The SOP's first
  and only step is the out-of-band confirmation and calls no tools.
- The completed SOP is only an approval receipt. A separate local operator
  command reads the trusted durable run database, requires an exact terminal
  `completed` run and matching fingerprint/action receipt, derives the request
  itself, and invokes the non-discoverable local operation. Recebi atomically
  rechecks that the same candidate is still the unresolved `needs_review`
  candidate before changing state.
- `PHASE6-OPERATOR-001` expected 0.10 devnet USDC and received a deliberate
  finalized 0.01 mismatch. Signature:
  `4h1jBZxSL6EJ4kWJkV1Aw6w3bApN1CKjt36U9bJG6PzBAT7hbzopnoMPaQop633NrkhCo8FGSECThsJVr3SZH4wz`.
  Recebi recorded `wrong_amount` with fingerprint
  `e248a2c285c84a4ea32df41cd0197ea33d9f4aae9dd4c8c552eac2bb8c70f5e5`.
- Denied run `run-1785343900191740766-0001` became durably `cancelled`; the
  operator command refused it and the receivable remained `needs_review`.
  With a temporary five-second test timeout, run
  `run-1785343960723504603-0001` also became durably `cancelled` and was
  refused. Production timeout settings were restored to 300/30 seconds.
- Approved run `run-1785343991945313520-0001` became durably `completed`.
  Only then did the local operator command change the record to
  `cancelled_unpaid`. The hash-chained event includes that exact approval run
  ID. Replaying the same receipt was idempotent and added no event.
- A direct `accept_as_paid` injection still failed argument parsing. SQLite
  integrity returned `ok`; live config/database/SOP files remained mode 0600
  and private data directories mode 0700.
- Final correction gates passed: 83 tests, strict all-target/all-feature
  Clippy, RustSec, cargo-deny advisories/licenses/sources, cargo-machete,
  locked release build, SOP validation, diff check, and live health. Deployed
  release SHA-256:
  `9aba8bf10c7956044445ff9dbe04753ab1ee300122ace0edf61945e3e49f8fe9`.
