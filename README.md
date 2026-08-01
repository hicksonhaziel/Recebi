# Recebi

Recebi is a self-hosted operational assistant for Solana USDC receivables in
Brazil. The agent may explain and report a deterministic system's result; it
must not decide what was paid, set a financial value, sign, submit, or refund a
transaction.

## Phase 5 status

Recebi creates durable reference-bound USDC requests and independently
reconciles them against finalized Solana transactions. It contains:

- `recebi-core`: pure offline Rust types for public keys, atomic amounts,
  transaction decoding, exact classic-SPL settlement verification, evidence
  fingerprints, provenance, and receivable states.
- `recebi-store`: local SQLite persistence with migrations, append-only events,
  SHA-256 hash chaining, atomic settlement/review transitions, replay
  protection, immutable PTAX evidence/month closes, and a singleton
  reconciliation lease.
- `recebi-mcp`: a stdio MCP server with `recebi_health`,
  `recebi_create_request`, `recebi_check`, `recebi_watch_payment`,
  `recebi_reconcile_open`, and `recebi_close_month`.

`recebi_create_request` accepts only a receivable ID, positive decimal amount,
and public wallet-display label. It derives recipient, mint, decimals, and
storage location from trusted local configuration; creates a CSPRNG 32-byte
reference; returns a Solana Pay URL; and saves an `open` receivable atomically.
Retries with the same ID and terms return the original durable record.
Reconciliation uses bounded HTTPS reads, checks the configured genesis hash,
retrieves at most eight finalized candidates, decodes complete legacy or v0
wire transactions, and accepts only one exact classic-SPL transfer to the
merchant's derived token account. A mismatch remains unpaid in
`needs_review`. Exact settlement consumes its signature and reference and is
recorded once.

For verified payments, monthly close requests the pinned official BCB
`CotacaoDolarDia` endpoint over bounded HTTPS. The strict policy accepts one
same-day closing quote, hashes the exact response bytes, and never substitutes
a weekend, holiday, future, or nearest-day value. Its BRL figure is explicitly
a nominal reference using `1 USDC = 1 USD`, PTAX sale, integer arithmetic, and
half-up cent rounding—not a claim of USDC fair value. A source failure leaves
the payment verified and the valuation pending.

Monthly close writes canonical JSON evidence, an accountant-oriented CSV, and
a hash manifest under the trusted data directory. CSV is presentation only,
not canonical state. These artifacts are accountant-ready evidence that may
assist record keeping; they are not tax or legal advice.

Recebi intentionally contains no transaction construction, signing,
broadcasting, refund, or wallet-key handling.

## Local build

Requires Rust 1.91 or later.

```bash
./scripts/check.sh
./scripts/install.sh
```

The release binary is written to `target/release/recebi-mcp`. The installer
does not copy a binary into a system directory or modify ZeroClaw.

## Trusted configuration

Copy [zeroclaw/recebi.example.toml](zeroclaw/recebi.example.toml) outside this
repository, set the trusted values, then run:

```bash
target/release/recebi-mcp --config /absolute/path/to/recebi.toml
```

The config is an operator-controlled boundary. MCP calls cannot override its
RPC endpoint, wallet, mint, data directory, cluster, or reconciliation limit.
Only HTTPS RPC URLs without embedded credentials are accepted. Do not commit a
real configuration file, API key, seed phrase, or wallet private key.

## ZeroClaw connection

Build the binary, then merge the server and bundle snippet in
[zeroclaw/config.example.toml](zeroclaw/config.example.toml) into an existing
ZeroClaw configuration. Attach the `recebi` bundle only to the intended agent.

`recebi_health` returns configuration and local-directory availability.
Creation does not contact an RPC endpoint. `recebi_check` reconciles one ID;
`recebi_watch_payment` performs stock-host-safe two-poll windows while that
payment is expected. The focused skill starts at window one, advances only on
`continue`, and stops after at most window four or immediately on a terminal
result.
`recebi_reconcile_open` checks a configured bounded batch and is available for
manual or explicitly enabled low-frequency recovery scans.
`recebi_close_month` accepts only `YYYY-MM`, performs bounded official PTAX
reads for verified records, and writes deterministic local evidence artifacts.
None of these tools has a financial write capability.

The repository includes an installable operator prompt at
[zeroclaw/skills/recebi/SKILL.md](zeroclaw/skills/recebi/SKILL.md). It is
limited to safe creation and deterministic reconciliation. The minimum
scheduled workflow is documented in
[zeroclaw/sops/reconcile-open-receivables.md](zeroclaw/sops/reconcile-open-receivables.md).

## Isolated devnet payer

For reproducible testing without a phone, install the separate Node devtool:

```bash
npm ci --prefix devtools
scripts/devnet-wallet.sh create
scripts/devnet-wallet.sh balance
```

Its keypair is stored with mode `0600` under
`~/.local/share/recebi-devnet-payer`, outside the repository. It is never read
by Recebi or ZeroClaw. The tool supports `create`, `reset`, `address`,
`balance`, `airdrop`, and a customizable `pay` command. Run
`scripts/devnet-wallet.sh help` for exact arguments.

This payer is devnet-only test infrastructure with signing capability. Never
fund it with mainnet assets, never configure it as the merchant, and never
represent its self-operated payments as independent customer usage.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
