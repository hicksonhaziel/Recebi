use std::{collections::BTreeMap, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use recebi_core::{
    GenesisHash, PublicKey, RawTokenAccount, RawTransaction, Reference,
    limits::{
        MAX_CANDIDATE_SIGNATURES, MAX_RPC_REQUEST_BYTES, MAX_RPC_RESPONSE_BYTES, RPC_TIMEOUT_SECS,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateSignature {
    pub signature: String,
    pub slot: u64,
    pub succeeded: bool,
    pub block_time_unix: Option<i64>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RpcError {
    #[error("RPC transport failed")]
    Transport,
    #[error("RPC response exceeded the byte limit")]
    ResponseTooLarge,
    #[error("RPC response was malformed")]
    MalformedResponse,
    #[error("RPC returned an error")]
    RemoteError,
    #[error("RPC candidate limit was exceeded")]
    CandidateOverflow,
    #[error("RPC transaction was unavailable")]
    TransactionUnavailable,
}

pub trait SolanaRpc {
    fn genesis_hash(&self) -> Result<GenesisHash, RpcError>;
    fn signatures_for_reference(
        &self,
        reference: &Reference,
    ) -> Result<Vec<CandidateSignature>, RpcError>;
    fn transaction(
        &self,
        signature: &str,
        genesis_hash: &GenesisHash,
    ) -> Result<RawTransaction, RpcError>;
}

#[derive(Clone)]
pub struct HttpSolanaRpc {
    endpoint: Url,
    agent: ureq::Agent,
}

impl HttpSolanaRpc {
    #[must_use]
    pub fn new(endpoint: Url) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(RPC_TIMEOUT_SECS)))
            .build();
        Self {
            endpoint,
            agent: config.into(),
        }
    }

    fn call(&self, request: &Value) -> Result<Vec<u8>, RpcError> {
        let encoded = serde_json::to_vec(request).map_err(|_| RpcError::MalformedResponse)?;
        if encoded.len() > MAX_RPC_REQUEST_BYTES {
            return Err(RpcError::MalformedResponse);
        }
        let mut response = self
            .agent
            .post(self.endpoint.as_str())
            .header("Content-Type", "application/json")
            .send(&encoded)
            .map_err(|_| RpcError::Transport)?;
        if !response.status().is_success() {
            return Err(RpcError::Transport);
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_RPC_RESPONSE_BYTES as u64)
            .read_to_vec()
            .map_err(|_| RpcError::ResponseTooLarge)
    }
}

impl SolanaRpc for HttpSolanaRpc {
    fn genesis_hash(&self) -> Result<GenesisHash, RpcError> {
        parse_genesis_hash(&self.call(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "getGenesisHash"
        }))?)
    }

    fn signatures_for_reference(
        &self,
        reference: &Reference,
    ) -> Result<Vec<CandidateSignature>, RpcError> {
        parse_signatures(&self.call(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "getSignaturesForAddress",
            "params": [
                reference.as_base58(),
                {
                    "commitment": "finalized",
                    "limit": MAX_CANDIDATE_SIGNATURES + 1
                }
            ]
        }))?)
    }

    fn transaction(
        &self,
        signature: &str,
        genesis_hash: &GenesisHash,
    ) -> Result<RawTransaction, RpcError> {
        validate_signature(signature)?;
        parse_transaction(
            &self.call(&json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "getTransaction",
                "params": [
                    signature,
                    {
                        "commitment": "finalized",
                        "encoding": "base64",
                        "maxSupportedTransactionVersion": 0
                    }
                ]
            }))?,
            genesis_hash,
        )
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    result: Option<T>,
    error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignatureEntry {
    signature: String,
    slot: u64,
    err: Option<Value>,
    block_time: Option<i64>,
    confirmation_status: Option<String>,
}

fn parse_genesis_hash(bytes: &[u8]) -> Result<GenesisHash, RpcError> {
    let envelope: Envelope<String> =
        serde_json::from_slice(bytes).map_err(|_| RpcError::MalformedResponse)?;
    if envelope.error.is_some() {
        return Err(RpcError::RemoteError);
    }
    GenesisHash::parse(envelope.result.ok_or(RpcError::MalformedResponse)?)
        .map_err(|_| RpcError::MalformedResponse)
}

fn parse_signatures(bytes: &[u8]) -> Result<Vec<CandidateSignature>, RpcError> {
    let envelope: Envelope<Vec<SignatureEntry>> =
        serde_json::from_slice(bytes).map_err(|_| RpcError::MalformedResponse)?;
    if envelope.error.is_some() {
        return Err(RpcError::RemoteError);
    }
    let entries = envelope.result.ok_or(RpcError::MalformedResponse)?;
    if entries.len() > MAX_CANDIDATE_SIGNATURES {
        return Err(RpcError::CandidateOverflow);
    }
    entries
        .into_iter()
        .map(|entry| {
            validate_signature(&entry.signature)?;
            if entry.confirmation_status.as_deref() != Some("finalized") {
                return Err(RpcError::MalformedResponse);
            }
            Ok(CandidateSignature {
                signature: entry.signature,
                slot: entry.slot,
                succeeded: entry.err.is_none(),
                block_time_unix: entry.block_time,
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionResult {
    slot: u64,
    block_time: Option<i64>,
    transaction: (String, String),
    meta: Option<TransactionMeta>,
    version: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionMeta {
    err: Option<Value>,
    #[serde(default)]
    loaded_addresses: LoadedAddresses,
    #[serde(default)]
    pre_token_balances: Vec<TokenBalance>,
    #[serde(default)]
    post_token_balances: Vec<TokenBalance>,
}

#[derive(Default, Deserialize)]
struct LoadedAddresses {
    #[serde(default)]
    writable: Vec<PublicKey>,
    #[serde(default)]
    readonly: Vec<PublicKey>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenBalance {
    account_index: u8,
    mint: PublicKey,
}

fn parse_transaction(bytes: &[u8], genesis_hash: &GenesisHash) -> Result<RawTransaction, RpcError> {
    let envelope: Envelope<TransactionResult> =
        serde_json::from_slice(bytes).map_err(|_| RpcError::MalformedResponse)?;
    if envelope.error.is_some() {
        return Err(RpcError::RemoteError);
    }
    let result = envelope.result.ok_or(RpcError::TransactionUnavailable)?;
    if result.transaction.1 != "base64"
        || !(result.version == json!("legacy") || result.version == json!(0))
    {
        return Err(RpcError::MalformedResponse);
    }
    let serialized_transaction = STANDARD
        .decode(result.transaction.0)
        .map_err(|_| RpcError::MalformedResponse)?;
    let meta = result.meta.ok_or(RpcError::MalformedResponse)?;
    let mut token_mints = BTreeMap::new();
    for balance in meta
        .pre_token_balances
        .into_iter()
        .chain(meta.post_token_balances)
    {
        if let Some(existing) = token_mints.insert(balance.account_index, balance.mint.clone())
            && existing != balance.mint
        {
            return Err(RpcError::MalformedResponse);
        }
    }
    Ok(RawTransaction {
        serialized_transaction,
        slot: result.slot,
        block_time_unix: result.block_time,
        finalized: true,
        succeeded: meta.err.is_none(),
        cluster_genesis_hash: genesis_hash.clone(),
        loaded_writable_addresses: meta.loaded_addresses.writable,
        loaded_readonly_addresses: meta.loaded_addresses.readonly,
        token_accounts: token_mints
            .into_iter()
            .map(|(account_index, mint)| RawTokenAccount {
                account_index,
                mint,
            })
            .collect(),
    })
}

fn validate_signature(signature: &str) -> Result<(), RpcError> {
    let decoded = bs58::decode(signature)
        .into_vec()
        .map_err(|_| RpcError::MalformedResponse)?;
    if decoded.len() == 64 && bs58::encode(decoded).into_string() == signature {
        Ok(())
    } else {
        Err(RpcError::MalformedResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature(byte: u8) -> String {
        bs58::encode([byte; 64]).into_string()
    }

    #[test]
    fn parses_finalized_candidates_and_rejects_overflow() {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": [{
                "signature": signature(7),
                "slot": 42,
                "err": null,
                "memo": null,
                "blockTime": 1_700_000_000,
                "confirmationStatus": "finalized"
            }]
        });
        let parsed = parse_signatures(&serde_json::to_vec(&body).expect("JSON")).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].succeeded);

        let overflow = json!({
            "result": (0..=MAX_CANDIDATE_SIGNATURES)
                .map(|index| json!({
                    "signature": signature(u8::try_from(index + 1).expect("small")),
                    "slot": index,
                    "err": null,
                    "blockTime": null,
                    "confirmationStatus": "finalized"
                }))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            parse_signatures(&serde_json::to_vec(&overflow).expect("JSON")),
            Err(RpcError::CandidateOverflow)
        );
    }

    #[test]
    fn rejects_non_finalized_and_malformed_candidate_responses() {
        let body = json!({"result": [{
            "signature": signature(7),
            "slot": 42,
            "err": null,
            "blockTime": null,
            "confirmationStatus": "confirmed"
        }]});
        assert_eq!(
            parse_signatures(&serde_json::to_vec(&body).expect("JSON")),
            Err(RpcError::MalformedResponse)
        );
        assert_eq!(
            parse_signatures(br#"{"result":"not-an-array"}"#),
            Err(RpcError::MalformedResponse)
        );
    }

    #[test]
    fn parses_genesis_and_rejects_rpc_errors() {
        let genesis = "11111111111111111111111111111111";
        assert_eq!(
            parse_genesis_hash(&serde_json::to_vec(&json!({"result": genesis})).expect("JSON"))
                .expect("genesis")
                .as_str(),
            genesis
        );
        assert_eq!(
            parse_genesis_hash(br#"{"error":{"code":-32000}}"#),
            Err(RpcError::RemoteError)
        );
    }

    #[test]
    fn parses_bounded_base64_transaction_metadata_and_null_block_time() {
        let mint = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
        let body = json!({
            "result": {
                "slot": 42,
                "blockTime": null,
                "transaction": [STANDARD.encode([1_u8, 2, 3]), "base64"],
                "meta": {
                    "err": null,
                    "loadedAddresses": {"writable": [], "readonly": []},
                    "preTokenBalances": [{"accountIndex": 2, "mint": mint}],
                    "postTokenBalances": [{"accountIndex": 2, "mint": mint}]
                },
                "version": "legacy"
            }
        });
        let genesis =
            GenesisHash::parse("EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG").expect("genesis");
        let parsed =
            parse_transaction(&serde_json::to_vec(&body).expect("JSON"), &genesis).expect("parse");
        assert_eq!(parsed.serialized_transaction, [1, 2, 3]);
        assert_eq!(parsed.block_time_unix, None);
        assert_eq!(parsed.token_accounts.len(), 1);
        assert!(parsed.finalized);
        assert!(parsed.succeeded);
    }

    #[test]
    fn rejects_null_pruned_malformed_and_inconsistent_transactions() {
        let genesis =
            GenesisHash::parse("EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG").expect("genesis");
        assert_eq!(
            parse_transaction(br#"{"result":null}"#, &genesis),
            Err(RpcError::TransactionUnavailable)
        );
        assert_eq!(
            parse_transaction(br#"{"result":"malformed"}"#, &genesis),
            Err(RpcError::MalformedResponse)
        );

        let inconsistent = json!({
            "result": {
                "slot": 42,
                "blockTime": 1,
                "transaction": [STANDARD.encode([1_u8]), "base64"],
                "meta": {
                    "err": null,
                    "preTokenBalances": [{
                        "accountIndex": 2,
                        "mint": "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU"
                    }],
                    "postTokenBalances": [{
                        "accountIndex": 2,
                        "mint": "11111111111111111111111111111111"
                    }]
                },
                "version": 0
            }
        });
        assert_eq!(
            parse_transaction(&serde_json::to_vec(&inconsistent).expect("JSON"), &genesis),
            Err(RpcError::MalformedResponse)
        );
    }

    #[test]
    fn http_adapter_freezes_https_redirect_and_whole_call_deadline_policy() {
        let endpoint = Url::parse("https://api.devnet.solana.com").expect("URL");
        let rpc = HttpSolanaRpc::new(endpoint);
        let config = rpc.agent.config();
        assert!(config.https_only());
        assert_eq!(config.max_redirects(), 0);
        assert_eq!(
            config.timeouts().global,
            Some(Duration::from_secs(RPC_TIMEOUT_SECS))
        );
    }
}
