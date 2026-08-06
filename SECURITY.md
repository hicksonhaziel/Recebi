# Security policy

Recebi is designed so payment truth is deterministic and outside the LLM. Please report suspected vulnerabilities privately to the maintainer before opening a public issue. Never include private keys, seed phrases, RPC credentials, Telegram tokens, personal data, production configuration, or raw private logs in a report.

## Custody classification

Recebi is **T1 Build + T0 Read** under the ZeroClaw Solana bounty custody ladder: it creates unsigned Solana Pay requests and reads finalized chain/BCB evidence. It is never T2. No private key, seed phrase, signer, session wallet, transaction submission, refund, swap, or transfer capability exists.

See the complete [threat model](docs/THREAT_MODEL.md) for trust boundaries, the observed prompt-injection transcript and unchanged-ledger proof, residual risks, and demonstrated safety behavior.

## Security invariants

- Recebi has no private key, seed phrase, signer, transaction submission, refund, swap, or custody capability.
- A receivable becomes `payment_verified` only after deterministic Rust verifies a finalized successful transaction, exact transfer-bound reference, configured recipient, configured classic SPL mint, exact atomic amount, cluster identity, and replay uniqueness.
- Chat text, transaction memos, public labels, and model output are never settlement or valuation truth sources.
- Recipient, mint, cluster, RPC endpoint, data directory, and PTAX policy are trusted local configuration and are not accepted as tool input.
- A mismatch remains unpaid. Operator review cannot rewrite chain evidence.
- Payment status and PTAX valuation status are independent.
- Material state is transactional, append-only where applicable, hash-checkpointed, and verified before reads and mutations.
- Network calls and MCP input/output are byte-, count-, and time-bounded and fail closed.

## Tool and local-process boundary

The model-discoverable MCP surface is non-custodial and bounded:

- health;
- create request and render its persisted URL as a QR;
- check or manually watch one receivable;
- hot and batch reconciliation;
- active-month snapshot;
- completed-month close.

Review mutation and delivery acknowledgement are absent from MCP discovery. The supported review path is the local operator helper, which verifies a durable out-of-band ZeroClaw approval receipt before invoking the internal operation. `recebi-mcp` itself rechecks candidate state, fingerprint, action, and amounts, but does not validate the ZeroClaw SOP database. Any process with direct stdio access is therefore privileged and must be restricted to the trusted operator.

## Trust boundaries

Trusted within the stated local threat model:

- reviewed Recebi binary and local operating system;
- mode-`0600` local configuration;
- authenticated local operator; and
- local processes permitted to invoke `recebi-mcp` directly.

Conditionally trusted and independently checked:

- configured Solana RPC as a bounded view of finalized chain state;
- official BCB HTTPS endpoint;
- ZeroClaw runtime and Telegram transport.

Untrusted:

- all chat content;
- payer claims and public labels;
- transaction memos and unrelated instructions;
- candidate transactions and RPC response shape/size;
- copied exports and prompts derived from external text.

## Expected fail-closed behavior

| Condition | Behavior |
|---|---|
| “I paid” in chat with no valid transaction | remains open |
| right reference, wrong amount/mint/recipient | unpaid `needs_review` |
| failed or non-finalized transaction | not accepted |
| reference in an unrelated instruction | not accepted |
| duplicate signature/reference | counted at most once |
| malformed/oversized RPC response or timeout | `incomplete`; state unchanged |
| PTAX unavailable/date mismatch | payment preserved; valuation pending |
| ledger modification | integrity verification fails |
| overlapping reconciliation pass | SQLite lease rejects simultaneous state work |
| overlapping hot-worker processes | not prevented; deployment must provide scheduler single-flight |
| request supplies wallet, mint, endpoint, path, or memo | arguments rejected |
| prompt asks for refund, signing, or transfer | capability does not exist |

## Data and logging

Private data directories are expected to be mode `0700`; configuration, databases, backups, exports, and generated QR files are mode `0600`. Errors and final chat output must not include credentials, complete private RPC URLs, raw transactions, arbitrary memo text, environment variables, client PII, or unnecessary absolute paths.

QR and CSV MCP results include absolute local attachment paths required by ZeroClaw. The model can see those values. The skill instructs it not to repeat plain path fields, and stock ZeroClaw is expected to strip attachment markers before Telegram delivery. Operators must test that behavior in their exact version and treat ZeroClaw as part of the confidentiality boundary.

Monthly evidence contains operator-supplied invoice identifiers and chain/BCB provenance. Treat it as private bookkeeping data even though transaction signatures are public.

## Scope limitations

Recebi does not prove payer legal identity, contractual performance, tax obligations, USDC fair value, or acceptance by an accountant. Its BRL result is a nominal reference based on an explicit `1 USDC = 1 USD` assumption and official same-day PTAX evidence where available.

PTAX artifacts preserve parsed quote fields and a contemporaneous response SHA-256, not the raw bounded BCB response. They support deterministic calculation checks but are not a self-contained archive of the source payload.

The tested MVP supports one configured merchant wallet, one classic SPL USDC mint, legacy and bounded resolved-v0 Solana transactions, and Linux with stock ZeroClaw 0.8.3. Token-2022, split payments, arbitrary tolerance, and overpayment acceptance fail closed.
