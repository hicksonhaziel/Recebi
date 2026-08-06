# Close a Recebi month

Purpose: attach bounded official same-day BCB PTAX evidence to verified
payments and create an immutable final revision for a completed UTC month.

## Agent prompt

```text
Close completed Recebi month YYYY-MM. Call only recebi__recebi_close_month with
{"month":"YYYY-MM"} exactly once. Send a concise structured report with
payment_verified, valued, valuation_pending, artifact_kind, and revision. Do
not show any SHA-256 hashes, `export_directory`, or any local filesystem
path in chat. End the reply with the exact `accountant_csv_attachment_marker`
returned by the tool so Telegram uploads the accountant CSV as a document.
Do not call this tool for the active or a future UTC month; use
recebi__recebi_snapshot_month for an active-month provisional snapshot. A
verified payment remains verified when PTAX is unavailable. Describe the output
only as accountant-ready evidence that may assist record keeping; it is not tax
or legal advice and the nominal 1 USDC = 1 USD assumption is not fair-value
proof.
```

This is an operator-triggered close, not a public-channel action. The tool pins
the BCB endpoint family, accepts no endpoint or rate override, uses strict
same-day quote selection, and never substitutes a weekend or nearest-day rate.
