# Recebi

Recebi is a self-hosted operational assistant for Solana USDC receivables in
Brazil. The agent may explain and report a deterministic system's result; it
must not decide what was paid, set a financial value, sign, submit, or refund a
transaction.

## Phase 1 status

This repository is at the health-only foundation phase. It contains:

- `recebi-core`: pure offline Rust types for public keys, atomic amounts,
  bounded text, provenance, and receivable states.
- `recebi-mcp`: a stdio MCP server with exactly one no-argument tool,
  `recebi_health`.
- trusted local configuration validation and local data-directory checks.

It intentionally contains no payment-request creation, chain polling, ledger,
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

The only exposed tool is `recebi_health`. It returns configuration and local
directory availability plus declared policy metadata. It deliberately does not
contact an RPC endpoint and cannot perform a financial action.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
