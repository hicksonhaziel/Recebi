# Recebi

Recebi is a self-hosted operational assistant for Solana USDC receivables in
Brazil. The agent may explain and report a deterministic system's result; it
must not decide what was paid, set a financial value, sign, submit, or refund a
transaction.

## Phase 2 status

This repository now creates durable, reference-bound USDC payment requests. It
contains:

- `recebi-core`: pure offline Rust types for public keys, atomic amounts,
  bounded text, provenance, and receivable states.
- `recebi-store`: local SQLite persistence with migrations, append-only events,
  and a SHA-256 hash chain.
- `recebi-mcp`: a stdio MCP server with `recebi_health` and
  `recebi_create_request`.

`recebi_create_request` accepts only a receivable ID, positive decimal amount,
and public wallet-display label. It derives recipient, mint, decimals, and
storage location from trusted local configuration; creates a CSPRNG 32-byte
reference; returns a Solana Pay URL; and saves an `open` receivable atomically.
Retries with the same ID and terms return the original durable record.

It still intentionally contains no chain polling, settlement verification,
PTAX valuation, transaction construction, signing, broadcasting, refund, or
wallet-key handling.

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

`recebi_health` returns configuration and local-directory availability. The
creation tool generates a request only; it deliberately does not contact an
RPC endpoint, determine whether payment occurred, or perform a financial
action.

The repository includes an installable operator prompt at
[zeroclaw/skills/recebi/SKILL.md](zeroclaw/skills/recebi/SKILL.md). It is
deliberately limited to the creation flow in this phase.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
