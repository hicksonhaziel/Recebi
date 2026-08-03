# Telegram UX

Recebi messages are compact, structured, and evidence-linked.

## Missing invoice fields

```text
🧾 To create the USDC invoice, I need:

• Amount: for example, 0.10 USDC
• Invoice ID: for example, INV-001
• Public label: for example, Acme invoice

Please send those three values.
```

Never invent missing values.

## Invoice created

```text
🧾 USDC invoice ready

• Invoice: `INV-001`
• Amount: `0.10 USDC`
• Status: Awaiting payment
• Reference: `...`
• Solana Pay: <URL>

Hot monitoring is active for 3 minutes. The 5-minute reconciliation job then
continues automatically.
```

The QR attachment marker remains the final line of the model reply.

## Exact payment

```text
✅ Payment verified

• Invoice: `INV-001`
• Amount: `0.10 USDC`
• Status: Exact payment recorded
• Signature: [View in Solana Explorer](EXPLORER_URL)
```

## Review required

```text
⚠️ Payment needs review

• Invoice: `INV-001`
• Status: Unpaid
• Reason: `wrong_amount`
• Expected: `0.10 USDC`
• Received: `0.01 USDC`
• Signature: [View in Solana Explorer](EXPLORER_URL)
```

Raw transaction signatures and generic LLM commentary are never shown.
