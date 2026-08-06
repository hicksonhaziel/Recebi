# Showcase Guide

The ZeroClaw Solana bounty submission is a post in `#solana-bounty`, not a repository document. This guide keeps the live video and write-up accurate, reproducible, and within three minutes.

> Repository docs support the submission; they do not replace a running use case. Record fresh behavior and label devnet, self-operated payments, and any unavailable dependency honestly.

## Submission position

**Recebi is a self-hosted, non-custodial receivables operator for Brazilian freelancers and small studios.** It creates reference-bound USDC requests in Telegram, independently verifies the exact finalized Solana payment, rejects convincing mismatches, and closes a local evidence ledger with strict same-day BCB PTAX provenance.

The core claim is not “the agent can create a payment link.” The core claim is:

> Recebi proves which receivable settled, records it exactly once, and preserves an auditable Brazil-ready evidence trail without giving the model a wallet key.

## What to declare

### User and job

- **User:** a Brazilian freelancer, studio, or small merchant accepting USDC.
- **Recurring job:** issue receivables, detect payment, handle mismatches, and prepare month-end evidence.
- **Channel:** private Telegram conversation with a running ZeroClaw agent.
- **Chain role:** Solana Pay request construction plus finalized SPL Token verification.
- **Brazil-first role:** official BCB PTAX provenance for nominal BRL bookkeeping reference.

Do not describe Recebi as a PIX integration; PIX is not implemented.

### ZeroClaw features used

- private Telegram channel;
- per-agent local stdio MCP bundle;
- private Recebi skill;
- persistent manual SOP with out-of-band approval;
- cron scheduling;
- narrow tool allowlist;
- memory-disabled background reconciliation;
- attachment markers for QR and CSV delivery; and
- direct deterministic Telegram notifications from the hot worker.

### What was built

- native Rust settlement and PTAX core;
- bounded Solana and BCB adapters;
- transactional SQLite ledger and export revisions;
- local MCP server with nine bounded discoverable tools;
- ZeroClaw skill and SOP assets; and
- scheduler, review, validation, and devnet helper scripts.

This is not a standalone plugin submission. It is a complete use case around a running stock ZeroClaw deployment.

### Custody and trust

Declare the custody tier exactly:

- **T1 Build:** unsigned Solana Pay URL and QR;
- **T0 Read:** finalized Solana and BCB queries;
- **not T2:** no key, signer, transaction submission, transfer, refund, or swap.

Declare third-party dependencies: configured Solana RPC, official BCB endpoint, Telegram transport, and ZeroClaw runtime. The canonical ledger stays local.

## Three-minute video

Target: **2:35–2:55**. Use a phone screen and terminal. No slides.

### Before recording

- Use one fresh receivable and one preserved wrong-amount record.
- Confirm ZeroClaw, Telegram, MCP health, and both scheduler jobs.
- Send `/new` so the conversation has current tool discovery.
- Keep the isolated devnet payer ready without exposing its key file.
- Pre-run `./scripts/check.sh`; keep only the final summary visible.
- Enlarge text and disable unrelated notifications.
- Put `DEVNET — SELF-OPERATED TEST PAYMENT` visibly on screen when applicable.

### 0:00–0:15 — Problem

Show Telegram and say:

> A wallet notification does not tell a freelancer which invoice was paid or what evidence belongs in the monthly books. Recebi verifies the exact receivable and closes the evidence ledger.

### 0:15–0:40 — Create

Send:

```text
Create invoice DEMO-001 for 0.10 USDC with public label Demo invoice
```

Show the ID, amount, reference, Solana Pay URL, and QR. State that the payer signs independently and Recebi has no wallet key.

### 0:40–1:05 — Reject a convincing mismatch

Send:

```text
Check PHASE6-OPERATOR-001
```

Show `Unpaid`, `wrong_amount`, expected versus received, and the Explorer link. Explain that the transaction is finalized and uses the correct reference, but still cannot become exact payment.

### 1:05–1:35 — Verify the real payment

Pay `DEMO-001` from the isolated devnet wallet without showing key material. Let hot reconciliation post the result. If network timing exceeds the recording window, send one explicit check rather than hiding the delay.

Show:

- `payment_verified`;
- exact amount;
- Explorer link;
- one notification; and
- no signing action in Recebi.

### 1:35–2:00 — Brazil-ready evidence

Request an active-month snapshot or show a completed-month revision. Explain:

- payment truth and valuation are independent;
- only same-day official BCB PTAX is accepted;
- an outage becomes `valuation_pending` rather than an invented rate;
- `1 USDC = 1 USD` is an explicit nominal assumption; and
- the accountant CSV is delivered as a document.

### 2:00–2:30 — Safety and architecture

Show the terminal tool list or [Architecture](ARCHITECTURE.md). State:

> ZeroClaw orchestrates bounded tools. Rust verifies the complete finalized transfer, SQLite records it once, and no refund or signing capability exists.

Briefly show that review mutation is absent from `tools/list` and that the supported local helper requires a durable approval receipt. State that direct stdio access is privileged and must be restricted; non-discovery alone is not authorization.

### 2:30–2:50 — Reproduction

Show:

```bash
./scripts/check.sh
sha256sum target/release/recebi-mcp
```

End with:

> Recebi does not take custody. It proves what settled and closes the evidence ledger.

## Prompt-injection segment

The observed four-part Telegram transcript is recorded in [Threat Model](THREAT_MODEL.md#observed-prompt-injection-transcript) and [Evidence](EVIDENCE.md). It covers payout redirect, false exact-paid state, trusted RPC/merchant/mint override, and instructions carried in a real finalized transaction memo.

In the write-up, include the transcript and its deterministic proof:

- exact inbound messages and agent responses;
- runtime traces showing zero tool calls for the false-paid, override, and memo prompts;
- identical checkpoint sequence, ledger root, and checkpoint hash before and after;
- `INJECT-SAFE-001` still open with zero settlements and reviews; and
- the malicious raw memo absent from MCP output and ZeroClaw runtime traces.

Do not present model refusal as the control. The control is capability absence, closed schemas, exclusion of memo bytes from model context, and an unchanged material ledger.

## Showcase post outline

Use this structure in the Discord post.

### 1. One sentence

> Recebi is a self-hosted ZeroClaw receivables operator that creates reference-bound USDC requests in Telegram, verifies exact finalized Solana settlement in Rust, and produces local month-end evidence with strict same-day BCB PTAX provenance.

### 2. Who it is for

Brazilian freelancers, studios, and small merchants who accept USDC but need reliable invoice matching and reproducible records without giving an AI agent custody.

### 3. What happens

1. Operator creates an invoice in Telegram.
2. Recebi persists a random single-use reference and returns a Solana Pay QR.
3. Payer signs in their own wallet.
4. Bounded polling finds finalized reference candidates.
5. Rust verifies the exact transfer and records it once.
6. Wrong or ambiguous payments remain unpaid for operator review.
7. Month close attaches strict BCB evidence and emits JSON/CSV/manifest revisions.

### 4. ZeroClaw composition

List the channel, MCP bundle, skill, cron jobs, SOP approval, and attachment delivery. Link the exact repository assets under [`../zeroclaw`](../zeroclaw).

### 5. Custody and threat model

State T1 Build + T0 Read, no T2 capability. Link [Threat Model](THREAT_MODEL.md) and include the prompt-injection transcript directly in the post.

### 6. What is demonstrated

Link [Evidence](EVIDENCE.md) and distinguish:

- automated tests;
- self-operated finalized devnet transfers;
- real Telegram/ZeroClaw operation; and
- anything newly demonstrated in the video.

Do not claim mainnet, independent customers, accountant approval, or production restore testing unless those have actually happened and are recorded.

### 7. Reproduce it

Link [Installation](INSTALLATION.md), [Operations](OPERATIONS.md), example configuration, skill, SOP, and source. Explicitly say secrets and live identifiers are redacted.

### 8. Limitations

Include single merchant/mint, classic SPL Token only, polling, no PIX, no Token-2022/splits/overpayments, nominal PTAX conversion, and current evidence environment.

## Required links

- repository root;
- exact commit used in the video;
- video under three minutes;
- [Installation](INSTALLATION.md);
- [Architecture](ARCHITECTURE.md);
- [Threat Model](THREAT_MODEL.md);
- [Evidence](EVIDENCE.md);
- [`zeroclaw/config.example.toml`](../zeroclaw/config.example.toml);
- [`zeroclaw/skills/recebi/SKILL.md`](../zeroclaw/skills/recebi/SKILL.md); and
- [`zeroclaw/sops`](../zeroclaw/sops).

## Redaction checklist

Redact or avoid showing:

- Telegram bot token and private chat IDs;
- RPC credentials and private URL query parameters;
- private configuration and absolute home paths;
- SQLite contents containing client identifiers;
- payer key files or seed material;
- ZeroClaw gateway/device tokens;
- unrelated messages and desktop notifications; and
- environment variables.

Public devnet addresses, references, signatures, and Explorer links may be shown only when intentionally selected for the demonstration.

## Failure rule

If Solana RPC, Telegram, or BCB is unavailable during recording, do not fake success or splice a planned result into a live claim. Keep the failure visible and explain the fail-closed state:

- unavailable chain evidence remains `incomplete` or pending;
- an unavailable PTAX quote becomes `valuation_pending`; and
- no state is inferred from chat.

## Final submission gate

Before posting:

- [ ] The video is three minutes or less and shows a real channel.
- [ ] The exact repository commit is public and buildable.
- [ ] The custody tier and third-party trust are explicit.
- [ ] A prompt-injection transcript is included.
- [ ] Devnet/mainnet and self-operated/independent evidence are labeled.
- [ ] No secret or private path is visible.
- [ ] Installation commands match the repository.
- [ ] The use case—not an isolated component—is the center of the post.
- [ ] No registry PR is presented as the bounty submission.
- [ ] Every claim can be traced to the video, tests, or [Evidence](EVIDENCE.md).
