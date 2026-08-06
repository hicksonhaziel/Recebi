# Installation

This guide builds Recebi from source, places trusted configuration outside the repository, connects the local MCP server to ZeroClaw, and verifies the boundary before any live demonstration.

> Recebi has been exercised on Linux with stock ZeroClaw 0.8.3 and Telegram. Use devnet first. Do not represent an untested mainnet deployment as production evidence.

## 1. Prerequisites

Required to build and run the core service:

- Linux with Bash;
- Rust 1.91 or newer and Cargo;
- stock ZeroClaw 0.8.3 with an existing private operator agent;
- an HTTPS Solana RPC endpoint;
- a merchant public key and classic SPL Token USDC mint;
- Telegram already working in ZeroClaw.

Operational scripts also use:

```text
curl  jq  sha256sum  sqlite3  stat
```

No wallet secret, seed phrase, or private key is required by Recebi.

## 2. Build and validate

```bash
git clone https://github.com/hicksonhaziel/Recebi.git
cd Recebi
./scripts/check.sh
./scripts/install.sh
```

`./scripts/check.sh` runs:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`./scripts/install.sh` performs a locked release build and leaves the executable at:

```text
target/release/recebi-mcp
```

Despite its name, the script does not copy the binary into `PATH`. Either reference the absolute build path from ZeroClaw or install it explicitly:

```bash
install -Dm755 target/release/recebi-mcp "$HOME/.local/bin/recebi-mcp"
sha256sum "$HOME/.local/bin/recebi-mcp"
```

Use the resulting absolute path in the MCP configuration.

## 3. Create private configuration and data paths

```bash
install -d -m 700 "$HOME/.zeroclaw"
install -d -m 700 "$HOME/.zeroclaw/recebi-data"
install -m 600 zeroclaw/recebi.example.toml "$HOME/.zeroclaw/recebi.toml"
```

Edit `~/.zeroclaw/recebi.toml` and replace every placeholder:

```toml
[recebi]
cluster = "devnet"
genesis_hash = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG"
merchant_wallet = "REPLACE_WITH_MERCHANT_PUBLIC_KEY"
accepted_mint = "REPLACE_WITH_CLASSIC_SPL_USDC_MINT"
token_decimals = 6
rpc_url = "https://REPLACE_WITH_YOUR_RPC"
data_dir = "/home/REPLACE_WITH_USER/.zeroclaw/recebi-data"
ptax_policy = "strict_same_day"
max_open_reconcile = 10
```

### Configuration ownership

| Field | Meaning | Security rule |
|---|---|---|
| `cluster` | Explorer/network identity | Must match the intended Solana environment |
| `genesis_hash` | Chain identity pin | Prevents silent cluster substitution |
| `merchant_wallet` | Recipient owner | Trusted local configuration only |
| `accepted_mint` | Accepted classic SPL mint | Trusted local configuration only |
| `token_decimals` | Atomic amount scale | Must match the configured mint |
| `rpc_url` | Solana HTTPS endpoint | Keep credentials out of source control |
| `data_dir` | SQLite, QR, and export root | Absolute private path, mode `0700` |
| `ptax_policy` | BCB quote selection | Currently `strict_same_day` |
| `max_open_reconcile` | Batch cap | Must remain bounded |

Tool calls cannot override these values. Keep the configuration mode `0600`:

```bash
chmod 600 "$HOME/.zeroclaw/recebi.toml"
stat -c '%a %n' "$HOME/.zeroclaw/recebi.toml" "$HOME/.zeroclaw/recebi-data"
```

Expected modes are `600` for the file and `700` for the directory.

## 4. Verify the MCP server directly

Run a local health call before involving ZeroClaw:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recebi_health","arguments":{}}}' \
  | "$HOME/.local/bin/recebi-mcp" --config "$HOME/.zeroclaw/recebi.toml"
```

If ZeroClaw will use `target/release/recebi-mcp`, substitute that absolute path. A healthy response must not expose wallet secrets, RPC credentials, or local filesystem details.

Inspect the discoverable tool boundary:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  | "$HOME/.local/bin/recebi-mcp" --config "$HOME/.zeroclaw/recebi.toml" \
  | jq '.result.tools[].name'
```

The list should contain nine bounded tools and must not contain signing, submission, refund, or review-mutation tools.

## 5. Connect Recebi to ZeroClaw

Merge the MCP server and bundle from [`../zeroclaw/config.example.toml`](../zeroclaw/config.example.toml) into the existing ZeroClaw configuration. Replace both absolute paths:

```toml
[[mcp.servers]]
name = "recebi"
transport = "stdio"
command = "/absolute/path/to/recebi-mcp"
args = ["--config", "/absolute/path/to/recebi.toml"]
tool_timeout_secs = 180

[mcp_bundles.recebi]
servers = ["recebi"]
```

Add `recebi` only to the intended operator agent’s `mcp_bundles` list. Do not grant it to public or unrelated agents.

Copy the skill into that agent’s private skill directory. The following uses a conventional private workspace path; use the path configured by your ZeroClaw installation if it differs:

```bash
install -d -m 700 "$HOME/.zeroclaw/workspace/skills/recebi"
install -m 600 zeroclaw/skills/recebi/SKILL.md \
  "$HOME/.zeroclaw/workspace/skills/recebi/SKILL.md"
```

The skill defines narrow intent mapping and response shaping. It does not establish payment truth.

Confirm exactly one `SKILL.md` exists under the bundle directory before restarting:

```bash
find "$HOME/.zeroclaw" -name SKILL.md -path '*recebi*'
```

A ZeroClaw bundle can resolve a nested `<bundle>/<skill>/SKILL.md`, so it is easy to end up with two copies at different depths. If a stale copy exists at a different depth from the one the runtime loads, the agent silently follows the old instructions: message templates, attachment markers, and valuation wording all revert with no error anywhere in the logs. Keep one file, or keep every copy identical.

After any skill change, restart ZeroClaw and send `/new`. Session context is captured when the session starts.

Optionally enable deterministic QR delivery so the operator receives the image even if the model omits the attachment marker:

```toml
[recebi.qr_delivery]
zeroclaw_bin = "/home/OPERATOR/.cargo/bin/zeroclaw"
channel_id = "telegram"
recipient = "TELEGRAM_CHAT_ID"
# delay_ms = 6000
```

Recebi then sends the QR itself shortly after the agent's reply and reports `qr_delivery`. Leave the block absent to keep the process boundary narrower; see [Threat model](THREAT_MODEL.md#trust-boundaries).

## 6. Install the approval SOP

Copy the SOP files into the private `sops_dir` configured in ZeroClaw:

```bash
install -d -m 700 "$HOME/.zeroclaw/workspace/sops/recebi-resolve-review"
install -m 600 zeroclaw/sops/recebi-resolve-review/SOP.md \
  "$HOME/.zeroclaw/workspace/sops/recebi-resolve-review/SOP.md"
install -m 600 zeroclaw/sops/recebi-resolve-review/SOP.toml \
  "$HOME/.zeroclaw/workspace/sops/recebi-resolve-review/SOP.toml"
```

Merge the `[sop]` settings from [`../zeroclaw/config.example.toml`](../zeroclaw/config.example.toml), adjusting the private paths. The important properties are persistent run state, out-of-band approval, cancellation on timeout, one concurrent run, untrusted-input blocking, and no procedural memory.

The SOP creates a durable approval receipt only. The supported operator path is [`review.sh`](../scripts/review.sh), which verifies that receipt and rechecks the exact live candidate before invoking the non-discoverable mutation.

Non-discovery is not an authorization boundary for arbitrary local processes: a caller with direct stdio access to `recebi-mcp` is privileged, and the service does not read the ZeroClaw SOP database itself. Restrict executable/config access to the trusted operator and do not expose the stdio process through a network bridge.

## 7. Restart and verify ZeroClaw

```bash
zeroclaw service restart
zeroclaw doctor
```

Then send `/new` in the private Telegram conversation. ZeroClaw constructs MCP connections when a session starts, so an old chat session may retain stale tool discovery.

Use a harmless request first:

```text
Check INSTALL-SMOKE-001
```

An unknown identifier should return a bounded not-found/error result. It must not trigger wallet, HTTP, scheduling, or signing behavior.

Next create a devnet request:

```text
Create invoice INSTALL-SMOKE-001 for 0.10 USDC with public label Install smoke test
```

Confirm that Telegram returns the same ID and amount, a unique reference, a Solana Pay URL, and a QR attachment. Do not pay it until the configured merchant, mint, cluster, and URL have been independently checked.

## 8. Enable reconciliation

Recebi can be checked manually without schedulers. For the intended daily flow, install both bounded jobs:

1. a deterministic hot worker for invoices created in the last three minutes;
2. a five-minute ZeroClaw fallback for older open invoices.

Follow [Operations: scheduled reconciliation](OPERATIONS.md#scheduled-reconciliation). The hardening helpers modify the ZeroClaw scheduler database only after creating mode-`0600` backups.

## Optional supply-chain and shell checks

These commands are not part of `./scripts/check.sh` and require their respective tools:

```bash
cargo audit --deny warnings
cargo deny check advisories licenses sources
cargo machete
bash -n scripts/*.sh
```

## Upgrade procedure

1. Back up the Recebi SQLite database and trusted configuration.
2. Review the source diff and `Cargo.lock` changes.
3. Run `./scripts/check.sh`.
4. Run `./scripts/install.sh`.
5. Verify the direct MCP health and tool list.
6. Restart ZeroClaw and start a new Telegram session with `/new`.
7. Check one known open and one known settled receivable.

Recebi currently has no packaged release installer or automated clean-room deployment script. It does ship an offline `--verify-ledger` mode and a non-destructive [`restore-drill.sh`](../scripts/restore-drill.sh); run both after configuration changes and before mainnet use. Reproducible source builds and a recovery procedure tested on your own separate hardware remain operator responsibilities.
