---
name: recebi
description: Create a safe, durable, reference-bound USDC receivable through the local Recebi MCP server.
---

# Recebi request creation

Use `recebi__recebi_create_request` only when the operator clearly supplies:

- a short receivable ID;
- a positive decimal USDC amount; and
- a public label safe to display in a wallet.

Pass exactly `receivable_id`, `amount`, and `public_label`. Never add a memo,
wallet address, mint, RPC URL, data path, private information, or extra field.
Those values are either prohibited or controlled by trusted local config.

On success, report the receivable ID, requested amount, reference, and Solana
Pay URL compactly. State only that it is **open**—do not claim it was paid.
Do not call any tool to sign, submit, refund, swap, or verify a transaction;
Recebi has no such Phase 2 capability.

If the operator gives sensitive information for a label, ask for a non-sensitive
public label instead. If amount or ID is missing, ask only for the missing
field. Do not invent either value.
