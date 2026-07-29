use std::{
    collections::HashSet,
    fmt::Write,
    time::{SystemTime, UNIX_EPOCH},
};

use getrandom::fill;
use recebi_core::{
    ReceivableId, ReceivableState, ReviewResolutionAction, SettlementExpectation,
    SettlementVerdict, decode_transaction,
    limits::{MAX_ANOMALY_SAMPLES, MAX_RECONCILE_RECEIVABLES},
    verify_settlement_once,
};
use recebi_store::{ReceivableStore, StoreError, StoredReceivable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    config::AppConfig,
    rpc::{HttpSolanaRpc, RpcError, SolanaRpc},
};

const LEASE_DURATION_MS: i64 = 60_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckInput {
    pub receivable_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileOpenInput {
    pub max_count: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveReviewInput {
    pub receivable_id: String,
    pub candidate_fingerprint: String,
    pub action: ReviewResolutionAction,
    pub approval_run_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pending,
    PaymentVerified,
    NeedsReview,
    CancelledUnpaid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckResult {
    pub receivable_id: String,
    pub status: CheckStatus,
    pub signature: Option<String>,
    pub reason: Option<String>,
    pub candidate_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolveReviewResult {
    pub receivable_id: String,
    pub candidate_fingerprint: String,
    pub action: ReviewResolutionAction,
    pub state: ReceivableState,
    pub approval_run_id: String,
}

#[derive(Debug, Serialize)]
pub struct ReconcileOpenResult {
    pub checked: usize,
    pub payment_verified: usize,
    pub pending: usize,
    pub needs_review: usize,
    pub anomaly_samples: Vec<String>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ReconcileError {
    #[error("request input is invalid")]
    InvalidInput,
    #[error("receivable was not found")]
    NotFound,
    #[error("configured RPC cluster identity does not match")]
    WrongCluster,
    #[error("bounded RPC reconciliation is incomplete")]
    RpcIncomplete,
    #[error("transaction evidence is malformed")]
    MalformedEvidence,
    #[error("local receivable storage is unavailable")]
    StorageUnavailable,
    #[error("another reconciliation is already running")]
    Busy,
    #[error("review resolution is stale or conflicts with current state")]
    ReviewConflict,
}

#[derive(Clone)]
pub struct ReconciliationService<R: SolanaRpc> {
    config: AppConfig,
    store: ReceivableStore,
    rpc: R,
}

impl ReconciliationService<HttpSolanaRpc> {
    pub fn live(config: AppConfig) -> Result<Self, ReconcileError> {
        config
            .ensure_data_directory()
            .map_err(|_| ReconcileError::StorageUnavailable)?;
        let store = ReceivableStore::open(config.database_path())
            .map_err(|_| ReconcileError::StorageUnavailable)?;
        let rpc = HttpSolanaRpc::new(config.recebi.rpc_url.clone());
        Ok(Self { config, store, rpc })
    }
}

impl<R: SolanaRpc> ReconciliationService<R> {
    #[cfg(test)]
    fn with_rpc(config: AppConfig, rpc: R) -> Result<Self, ReconcileError> {
        config
            .ensure_data_directory()
            .map_err(|_| ReconcileError::StorageUnavailable)?;
        let store = ReceivableStore::open(config.database_path())
            .map_err(|_| ReconcileError::StorageUnavailable)?;
        Ok(Self { config, store, rpc })
    }

    pub fn check(&self, input: CheckInput) -> Result<CheckResult, ReconcileError> {
        let id =
            ReceivableId::new(input.receivable_id).map_err(|_| ReconcileError::InvalidInput)?;
        self.check_id(&id)
    }

    fn check_id(&self, id: &ReceivableId) -> Result<CheckResult, ReconcileError> {
        let stored = self
            .store
            .get(id)
            .map_err(|error| map_store(&error))?
            .ok_or(ReconcileError::NotFound)?;
        match stored.state {
            ReceivableState::PaymentVerified => {
                let signature = self
                    .store
                    .settlement_signature(id)
                    .map_err(|error| map_store(&error))?
                    .ok_or(ReconcileError::StorageUnavailable)?;
                return Ok(CheckResult {
                    receivable_id: id.as_str().to_owned(),
                    status: CheckStatus::PaymentVerified,
                    signature: Some(signature),
                    reason: None,
                    candidate_fingerprint: None,
                });
            }
            ReceivableState::NeedsReview => {
                let candidate = self
                    .store
                    .review_candidate(id)
                    .map_err(|error| map_store(&error))?
                    .ok_or(ReconcileError::StorageUnavailable)?;
                return Ok(CheckResult {
                    receivable_id: id.as_str().to_owned(),
                    status: CheckStatus::NeedsReview,
                    signature: Some(candidate.signature),
                    reason: Some(candidate.verdict),
                    candidate_fingerprint: Some(candidate.candidate_fingerprint),
                });
            }
            ReceivableState::Cancelled => {
                return Ok(CheckResult {
                    receivable_id: id.as_str().to_owned(),
                    status: CheckStatus::CancelledUnpaid,
                    signature: None,
                    reason: Some("cancelled_unpaid".to_owned()),
                    candidate_fingerprint: None,
                });
            }
            ReceivableState::Open => {}
            _ => return Err(ReconcileError::StorageUnavailable),
        }
        self.check_open(&stored)
    }

    #[allow(clippy::too_many_lines)] // Keep the ordered fail-closed verification path auditable.
    fn check_open(&self, stored: &StoredReceivable) -> Result<CheckResult, ReconcileError> {
        let expected_genesis = self.config.recebi.genesis_hash.clone();
        let observed_genesis = self.rpc.genesis_hash().map_err(map_rpc)?;
        if observed_genesis != expected_genesis {
            return Err(ReconcileError::WrongCluster);
        }
        let candidates = self
            .rpc
            .signatures_for_reference(&stored.request.reference)
            .map_err(map_rpc)?;
        if candidates.is_empty() {
            return Ok(pending_result(&stored.request.receivable_id));
        }
        let expected = SettlementExpectation {
            receivable_id: stored.request.receivable_id.clone(),
            cluster_genesis_hash: expected_genesis.clone(),
            merchant_wallet: stored.request.recipient.clone(),
            mint: stored.request.mint.clone(),
            amount: stored.request.amount,
            token_decimals: stored.request.decimals,
            reference: stored.request.reference.clone(),
        };
        let resolved_fingerprints = self
            .store
            .resolved_review_fingerprints(&stored.request.receivable_id)
            .map_err(|error| map_store(&error))?
            .into_iter()
            .collect::<HashSet<_>>();
        let mut first_anomaly = None;
        for candidate in candidates {
            let raw = self
                .rpc
                .transaction(&candidate.signature, &expected_genesis)
                .map_err(map_rpc)?;
            let snapshot =
                decode_transaction(&raw).map_err(|_| ReconcileError::MalformedEvidence)?;
            if snapshot.signature != candidate.signature
                || snapshot.slot != candidate.slot
                || snapshot.succeeded != candidate.succeeded
                || snapshot.block_time_unix != candidate.block_time_unix
            {
                return Err(ReconcileError::MalformedEvidence);
            }
            let (signature_used, reference_used) = self
                .store
                .replay_state(&snapshot.signature, &expected.reference)
                .map_err(|error| map_store(&error))?;
            let signatures = if signature_used {
                HashSet::from([snapshot.signature.clone()])
            } else {
                HashSet::new()
            };
            let references = if reference_used {
                HashSet::from([expected.reference.clone()])
            } else {
                HashSet::new()
            };
            match verify_settlement_once(&snapshot, &expected, &signatures, &references) {
                Ok(evidence) => {
                    self.store
                        .mark_payment_verified(
                            &expected.receivable_id,
                            &expected.reference,
                            &evidence,
                            now_unix_ms()?,
                        )
                        .map_err(|error| map_store(&error))?;
                    return Ok(CheckResult {
                        receivable_id: expected.receivable_id.as_str().to_owned(),
                        status: CheckStatus::PaymentVerified,
                        signature: Some(evidence.signature),
                        reason: None,
                        candidate_fingerprint: None,
                    });
                }
                Err(verdict) => {
                    let verdict = verdict_code(verdict);
                    let fingerprint = anomaly_fingerprint(
                        &stored.request.receivable_id,
                        &stored.request.reference,
                        &candidate.signature,
                        candidate.slot,
                        verdict,
                    );
                    if !resolved_fingerprints.contains(&fingerprint) {
                        first_anomaly.get_or_insert((
                            candidate.signature,
                            candidate.slot,
                            verdict,
                            fingerprint,
                        ));
                    }
                }
            }
        }
        let Some((signature, slot, verdict, candidate_fingerprint)) = first_anomaly else {
            return Ok(pending_result(&stored.request.receivable_id));
        };
        self.record_anomaly(stored, &signature, slot, verdict, &candidate_fingerprint)?;
        Ok(CheckResult {
            receivable_id: stored.request.receivable_id.as_str().to_owned(),
            status: CheckStatus::NeedsReview,
            signature: Some(signature),
            reason: Some(verdict.to_owned()),
            candidate_fingerprint: Some(candidate_fingerprint),
        })
    }

    pub fn resolve_review(
        &self,
        input: ResolveReviewInput,
    ) -> Result<ResolveReviewResult, ReconcileError> {
        let receivable_id =
            ReceivableId::new(input.receivable_id).map_err(|_| ReconcileError::InvalidInput)?;
        if input.candidate_fingerprint.len() != 64
            || !input
                .candidate_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ReconcileError::InvalidInput);
        }
        if input.approval_run_id.len() > 128
            || !input.approval_run_id.starts_with("run-")
            || !input
                .approval_run_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(ReconcileError::InvalidInput);
        }
        let state = self
            .store
            .resolve_review(
                &receivable_id,
                &input.candidate_fingerprint,
                input.action,
                &input.approval_run_id,
                now_unix_ms()?,
            )
            .map_err(|error| match error {
                StoreError::InvalidTransition => ReconcileError::ReviewConflict,
                _ => map_store(&error),
            })?;
        Ok(ResolveReviewResult {
            receivable_id: receivable_id.as_str().to_owned(),
            candidate_fingerprint: input.candidate_fingerprint,
            action: input.action,
            state,
            approval_run_id: input.approval_run_id,
        })
    }

    fn record_anomaly(
        &self,
        stored: &StoredReceivable,
        signature: &str,
        slot: u64,
        verdict: &'static str,
        fingerprint: &str,
    ) -> Result<(), ReconcileError> {
        self.store
            .mark_needs_review(
                &stored.request.receivable_id,
                signature,
                slot,
                verdict,
                fingerprint,
                now_unix_ms()?,
            )
            .map_err(|error| map_store(&error))
    }

    pub fn reconcile_open(
        &self,
        input: ReconcileOpenInput,
    ) -> Result<ReconcileOpenResult, ReconcileError> {
        let configured = usize::from(self.config.recebi.max_open_reconcile);
        let requested = input.max_count.map_or(configured, usize::from);
        let limit = requested.min(configured).min(MAX_RECONCILE_RECEIVABLES);
        if limit == 0 {
            return Err(ReconcileError::InvalidInput);
        }
        let mut owner_bytes = [0_u8; 16];
        fill(&mut owner_bytes).map_err(|_| ReconcileError::StorageUnavailable)?;
        let owner = bs58::encode(owner_bytes).into_string();
        let now = now_unix_ms()?;
        self.store
            .acquire_reconciliation_lease(&owner, now, now + LEASE_DURATION_MS)
            .map_err(|error| map_store(&error))?;
        let result = self.reconcile_acquired(limit);
        let released = self
            .store
            .release_reconciliation_lease(&owner)
            .map_err(|error| map_store(&error));
        match (result, released) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    fn reconcile_acquired(&self, limit: usize) -> Result<ReconcileOpenResult, ReconcileError> {
        let open = self
            .store
            .list_open(limit)
            .map_err(|error| map_store(&error))?;
        let mut result = ReconcileOpenResult {
            checked: 0,
            payment_verified: 0,
            pending: 0,
            needs_review: 0,
            anomaly_samples: vec![],
        };
        for receivable in open {
            let checked = self.check_id(&receivable.request.receivable_id)?;
            result.checked += 1;
            match checked.status {
                CheckStatus::PaymentVerified => result.payment_verified += 1,
                CheckStatus::Pending => result.pending += 1,
                CheckStatus::NeedsReview => {
                    result.needs_review += 1;
                    if result.anomaly_samples.len() < MAX_ANOMALY_SAMPLES {
                        result.anomaly_samples.push(checked.receivable_id.clone());
                    }
                }
                CheckStatus::CancelledUnpaid => return Err(ReconcileError::StorageUnavailable),
            }
        }
        Ok(result)
    }
}

fn now_unix_ms() -> Result<i64, ReconcileError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ReconcileError::StorageUnavailable)?
            .as_millis(),
    )
    .map_err(|_| ReconcileError::StorageUnavailable)
}

fn pending_result(receivable_id: &ReceivableId) -> CheckResult {
    CheckResult {
        receivable_id: receivable_id.as_str().to_owned(),
        status: CheckStatus::Pending,
        signature: None,
        reason: None,
        candidate_fingerprint: None,
    }
}

fn map_rpc(_error: RpcError) -> ReconcileError {
    ReconcileError::RpcIncomplete
}

fn map_store(error: &StoreError) -> ReconcileError {
    match error {
        StoreError::ReconciliationBusy => ReconcileError::Busy,
        StoreError::Unavailable
        | StoreError::Integrity
        | StoreError::IdempotencyConflict
        | StoreError::ReferenceReuse
        | StoreError::InvalidTransition
        | StoreError::Replay
        | StoreError::MonthlyExportBusy
        | StoreError::ConcurrentMutation => ReconcileError::StorageUnavailable,
    }
}

fn verdict_code(verdict: SettlementVerdict) -> &'static str {
    match verdict {
        SettlementVerdict::NotFinalized => "not_finalized",
        SettlementVerdict::TransactionFailed => "transaction_failed",
        SettlementVerdict::BoundsExceeded => "bounds_exceeded",
        SettlementVerdict::InvalidSignature => "invalid_signature",
        SettlementVerdict::UnsupportedToken2022 => "unsupported_token_2022",
        SettlementVerdict::UnsupportedProgram => "unsupported_program",
        SettlementVerdict::MalformedInstruction => "malformed_instruction",
        SettlementVerdict::MissingTokenAccount => "missing_token_account",
        SettlementVerdict::WrongMint => "wrong_mint",
        SettlementVerdict::WrongCluster => "wrong_cluster",
        SettlementVerdict::WrongDecimals => "wrong_decimals",
        SettlementVerdict::WrongRecipient => "wrong_recipient",
        SettlementVerdict::SelfTransfer => "self_transfer",
        SettlementVerdict::WrongAmount => "wrong_amount",
        SettlementVerdict::MissingReference => "missing_reference",
        SettlementVerdict::UnsafeReference => "unsafe_reference",
        SettlementVerdict::MultipleCandidateTransfers => "multiple_transfers",
        SettlementVerdict::NoExactTransfer => "no_exact_transfer",
        SettlementVerdict::UnresolvedAddressTable => "unresolved_address_table",
        SettlementVerdict::DuplicateSignature => "duplicate_signature",
        SettlementVerdict::ReferenceReused => "reference_reused",
    }
}

fn anomaly_fingerprint(
    receivable_id: &ReceivableId,
    reference: &recebi_core::Reference,
    signature: &str,
    slot: u64,
    verdict: &str,
) -> String {
    let canonical = format!(
        "v=1|id={}|reference={}|signature={signature}|slot={slot}|verdict={verdict}",
        receivable_id.as_str(),
        reference.as_base58()
    );
    let digest = Sha256::digest(canonical.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use recebi_core::{
        AtomicAmount, BoundedText, GenesisHash, PaymentRequest, PublicKey, RawTokenAccount,
        RawTransaction, Reference, limits::MAX_PUBLIC_LABEL_BYTES, solana::derive_classic_ata,
    };
    use recebi_store::ReceivableStore;
    use solana_message::{
        Address, Hash, Message, MessageHeader, VersionedMessage,
        compiled_instruction::CompiledInstruction,
    };
    use solana_signature::Signature;
    use solana_transaction::versioned::VersionedTransaction;
    use spl_token_interface::instruction::TokenInstruction;

    use super::*;
    use crate::rpc::CandidateSignature;

    #[derive(Clone)]
    struct MockRpc {
        genesis: GenesisHash,
        candidates: Vec<CandidateSignature>,
        transactions: Arc<HashMap<String, RawTransaction>>,
        genesis_error: Option<RpcError>,
        candidates_error: Option<RpcError>,
        transaction_error: Option<RpcError>,
    }

    impl SolanaRpc for MockRpc {
        fn genesis_hash(&self) -> Result<GenesisHash, RpcError> {
            self.genesis_error
                .map_or_else(|| Ok(self.genesis.clone()), Err)
        }

        fn signatures_for_reference(
            &self,
            _reference: &Reference,
        ) -> Result<Vec<CandidateSignature>, RpcError> {
            self.candidates_error
                .map_or_else(|| Ok(self.candidates.clone()), Err)
        }

        fn transaction(
            &self,
            signature: &str,
            _genesis_hash: &GenesisHash,
        ) -> Result<RawTransaction, RpcError> {
            self.transaction_error.map_or_else(
                || {
                    self.transactions
                        .get(signature)
                        .cloned()
                        .ok_or(RpcError::TransactionUnavailable)
                },
                Err,
            )
        }
    }

    fn key(value: &str) -> PublicKey {
        PublicKey::parse(value).expect("key")
    }

    fn wire_key(value: &PublicKey) -> Address {
        value.as_str().parse().expect("wire key")
    }

    fn setup(
        amount_in_transaction: u64,
        include_second_correct: bool,
    ) -> (
        tempfile::TempDir,
        ReconciliationService<MockRpc>,
        ReceivableId,
    ) {
        let directory = tempfile::tempdir().expect("directory");
        let config_path = directory.path().join("recebi.toml");
        let merchant = key("CmQXip6WcPrzbx1waawoPMerj5A1jvtqZjHBxv6C4uit");
        let mint = key("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
        let config_text = format!(
            r#"
[recebi]
cluster = "devnet"
genesis_hash = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG"
merchant_wallet = "{}"
accepted_mint = "{}"
token_decimals = 6
rpc_url = "https://api.devnet.solana.com"
data_dir = "{}"
ptax_policy = "strict_same_day"
max_open_reconcile = 10
"#,
            merchant.as_str(),
            mint.as_str(),
            directory.path().join("data").display()
        );
        std::fs::write(&config_path, config_text).expect("config");
        let config = AppConfig::load(&config_path).expect("load config");
        config.ensure_data_directory().expect("data");
        let store = ReceivableStore::open(config.database_path()).expect("store");
        let id = ReceivableId::new("LIVE-001").expect("id");
        let reference = Reference::from_bytes([7; 32]);
        store
            .create_or_get(
                PaymentRequest {
                    receivable_id: id.clone(),
                    recipient: merchant.clone(),
                    mint: mint.clone(),
                    amount: AtomicAmount::new(100_000),
                    decimals: 6,
                    reference: reference.clone(),
                    public_label: BoundedText::<MAX_PUBLIC_LABEL_BYTES>::new("Live test")
                        .expect("label"),
                },
                1,
            )
            .expect("create");
        let first = raw_transaction(&merchant, &mint, &reference, amount_in_transaction, 7);
        let first_signature = signature(&first);
        let mut candidates = vec![CandidateSignature {
            signature: first_signature.clone(),
            slot: first.slot,
            succeeded: true,
            block_time_unix: first.block_time_unix,
        }];
        let mut transactions = HashMap::from([(first_signature, first)]);
        if include_second_correct {
            let second = raw_transaction(&merchant, &mint, &reference, 100_000, 8);
            let second_signature = signature(&second);
            candidates.push(CandidateSignature {
                signature: second_signature.clone(),
                slot: second.slot,
                succeeded: true,
                block_time_unix: second.block_time_unix,
            });
            transactions.insert(second_signature, second);
        }
        let rpc = MockRpc {
            genesis: config.recebi.genesis_hash.clone(),
            candidates,
            transactions: Arc::new(transactions),
            genesis_error: None,
            candidates_error: None,
            transaction_error: None,
        };
        let service = ReconciliationService::with_rpc(config, rpc).expect("service");
        (directory, service, id)
    }

    fn signature(raw: &RawTransaction) -> String {
        let transaction: VersionedTransaction =
            wincode::deserialize(&raw.serialized_transaction).expect("wire transaction");
        transaction.signatures[0].to_string()
    }

    fn raw_transaction(
        merchant: &PublicKey,
        mint: &PublicKey,
        reference: &Reference,
        amount: u64,
        signature_byte: u8,
    ) -> RawTransaction {
        let destination = derive_classic_ata(merchant, mint).expect("ATA");
        let source = key("11111111111111111111111111111111");
        let authority = key("SysvarC1ock11111111111111111111111111111111");
        let token = key("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let reference_key = key(&reference.as_base58());
        let transaction = VersionedTransaction {
            signatures: vec![Signature::from([signature_byte; 64])],
            message: VersionedMessage::Legacy(Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 3,
                },
                account_keys: vec![
                    wire_key(&authority),
                    wire_key(&source),
                    wire_key(&destination),
                    wire_key(mint),
                    wire_key(&token),
                    wire_key(&reference_key),
                ],
                recent_blockhash: Hash::default(),
                instructions: vec![CompiledInstruction {
                    program_id_index: 4,
                    accounts: vec![1, 3, 2, 0, 5],
                    data: TokenInstruction::TransferChecked {
                        amount,
                        decimals: 6,
                    }
                    .pack(),
                }],
            }),
        };
        RawTransaction {
            serialized_transaction: wincode::serialize(&transaction).expect("serialize"),
            slot: u64::from(signature_byte),
            block_time_unix: Some(1_700_000_000),
            finalized: true,
            succeeded: true,
            cluster_genesis_hash: GenesisHash::parse(
                "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG",
            )
            .expect("genesis"),
            loaded_writable_addresses: vec![],
            loaded_readonly_addresses: vec![],
            token_accounts: vec![
                RawTokenAccount {
                    account_index: 1,
                    mint: mint.clone(),
                },
                RawTokenAccount {
                    account_index: 2,
                    mint: mint.clone(),
                },
            ],
        }
    }

    #[test]
    fn exact_payment_is_committed_once() {
        let (_directory, service, id) = setup(100_000, false);
        let first = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("check");
        assert_eq!(first.status, CheckStatus::PaymentVerified);
        let repeated = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("repeat");
        assert_eq!(repeated.status, CheckStatus::PaymentVerified);
        service.store.verify_event_chain().expect("event chain");
    }

    #[test]
    fn wrong_candidate_then_correct_candidate_settles() {
        let (_directory, service, id) = setup(10_000, true);
        let result = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("check");
        assert_eq!(result.status, CheckStatus::PaymentVerified);
    }

    #[test]
    fn wrong_amount_remains_unpaid_in_needs_review() {
        let (_directory, service, id) = setup(10_000, false);
        let result = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("check");
        assert_eq!(result.status, CheckStatus::NeedsReview);
        assert_eq!(result.reason, Some("wrong_amount".to_owned()));
        assert_eq!(
            result.candidate_fingerprint.as_deref().map(str::len),
            Some(64)
        );
        assert_eq!(
            service
                .store
                .get(&id)
                .expect("store")
                .expect("record")
                .state,
            ReceivableState::NeedsReview
        );
        let repeated = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("repeat");
        assert_eq!(repeated.signature, result.signature);
        assert_eq!(repeated.reason, result.reason);
        assert_eq!(repeated.candidate_fingerprint, result.candidate_fingerprint);
    }

    #[test]
    fn review_resolution_reopens_or_cancels_but_cannot_accept_as_paid() {
        let (_directory, service, id) = setup(10_000, false);
        let candidate = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("candidate");
        let fingerprint = candidate.candidate_fingerprint.expect("fingerprint");
        let reopened = service
            .resolve_review(ResolveReviewInput {
                receivable_id: id.as_str().to_owned(),
                candidate_fingerprint: fingerprint.clone(),
                action: ReviewResolutionAction::IgnoreCandidateAndReopen,
                approval_run_id: "run-test-reopen".to_owned(),
            })
            .expect("reopen");
        assert_eq!(reopened.state, ReceivableState::Open);
        assert_eq!(
            service.resolve_review(ResolveReviewInput {
                receivable_id: id.as_str().to_owned(),
                candidate_fingerprint: "cd".repeat(32),
                action: ReviewResolutionAction::CancelUnpaid,
                approval_run_id: "run-test-stale".to_owned(),
            }),
            Err(ReconcileError::ReviewConflict)
        );
        let reopened_check = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("resolved candidate is skipped");
        assert_eq!(reopened_check.status, CheckStatus::Pending);

        let (_directory, service, id) = setup(10_000, false);
        let candidate = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("candidate to cancel");
        let cancelled = service
            .resolve_review(ResolveReviewInput {
                receivable_id: id.as_str().to_owned(),
                candidate_fingerprint: candidate.candidate_fingerprint.expect("fingerprint"),
                action: ReviewResolutionAction::CancelUnpaid,
                approval_run_id: "run-test-cancel".to_owned(),
            })
            .expect("cancel");
        assert_eq!(cancelled.state, ReceivableState::Cancelled);
        let checked = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("cancelled check");
        assert_eq!(checked.status, CheckStatus::CancelledUnpaid);
    }

    #[test]
    fn wrong_cluster_fails_without_changing_state() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.genesis =
            GenesisHash::parse("11111111111111111111111111111111").expect("genesis");
        assert!(matches!(
            service.check(CheckInput {
                receivable_id: id.as_str().to_owned()
            }),
            Err(ReconcileError::WrongCluster)
        ));
        assert_eq!(
            service
                .store
                .get(&id)
                .expect("store")
                .expect("record")
                .state,
            ReceivableState::Open
        );
    }

    fn assert_open(service: &ReconciliationService<MockRpc>, id: &ReceivableId) {
        assert_eq!(
            service.store.get(id).expect("store").expect("record").state,
            ReceivableState::Open
        );
    }

    #[test]
    fn no_candidates_stays_pending_and_open() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.candidates.clear();
        let result = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("check");
        assert_eq!(result.status, CheckStatus::Pending);
        assert_open(&service, &id);
    }

    #[test]
    fn rpc_and_pruned_transaction_errors_leave_state_open() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.candidates_error = Some(RpcError::Transport);
        assert!(matches!(
            service.check(CheckInput {
                receivable_id: id.as_str().to_owned()
            }),
            Err(ReconcileError::RpcIncomplete)
        ));
        assert_open(&service, &id);

        service.rpc.candidates_error = None;
        service.rpc.transaction_error = Some(RpcError::TransactionUnavailable);
        assert!(matches!(
            service.check(CheckInput {
                receivable_id: id.as_str().to_owned()
            }),
            Err(ReconcileError::RpcIncomplete)
        ));
        assert_open(&service, &id);
    }

    #[test]
    fn inconsistent_candidate_metadata_fails_closed() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.candidates[0].block_time_unix = None;
        assert!(matches!(
            service.check(CheckInput {
                receivable_id: id.as_str().to_owned()
            }),
            Err(ReconcileError::MalformedEvidence)
        ));
        assert_open(&service, &id);
    }

    #[test]
    fn overlapping_batch_reconciliation_is_rejected() {
        let (_directory, service, id) = setup(100_000, false);
        service
            .store
            .acquire_reconciliation_lease("other-run", 0, i64::MAX)
            .expect("lease");
        assert!(matches!(
            service.reconcile_open(ReconcileOpenInput { max_count: Some(1) }),
            Err(ReconcileError::Busy)
        ));
        assert_open(&service, &id);
    }
}
