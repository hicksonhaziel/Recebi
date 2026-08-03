use std::{
    collections::HashSet,
    fmt::Write,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use getrandom::fill;
use recebi_core::{
    GenesisHash, ReceivableId, ReceivableState, ReviewResolutionAction, SettlementAssessment,
    SettlementExpectation, SettlementVerdict, UnderpaymentEvidence, VarianceReason,
    assess_settlement_once, decode_transaction,
    limits::{MAX_ANOMALY_SAMPLES, MAX_RECONCILE_RECEIVABLES},
};
use recebi_store::{ReceivableStore, StoreError, StoredReceivable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    config::{AppConfig, Cluster},
    rpc::{HttpSolanaRpc, RpcError, SolanaRpc},
};

const LEASE_DURATION_MS: i64 = 60_000;
const WATCH_INTERVAL: Duration = Duration::from_secs(5);
const WATCH_POLLS_PER_WINDOW: u8 = 2;
const WATCH_MAX_WINDOWS: u8 = 4;
const HOT_WINDOW: Duration = Duration::from_secs(180);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckInput {
    pub receivable_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchPaymentInput {
    pub receivable_id: String,
    pub window: u8,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileOpenInput {
    pub max_count: Option<u16>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotReconcileInput {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeNotificationInput {
    pub notification_id: u64,
    pub delivery_receipt: String,
}

#[derive(Debug, Serialize)]
pub struct AcknowledgeNotificationResult {
    pub notification_id: u64,
    pub status: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveReviewInput {
    pub receivable_id: String,
    pub candidate_fingerprint: String,
    pub action: ReviewResolutionAction,
    pub variance_reason: Option<VarianceReason>,
    pub approval_run_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pending,
    PaymentVerified,
    NeedsReview,
    CancelledUnpaid,
    SettledWithVariance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckResult {
    pub receivable_id: String,
    pub status: CheckStatus,
    pub signature: Option<String>,
    pub explorer_url: Option<String>,
    pub reason: Option<String>,
    pub candidate_fingerprint: Option<String>,
    pub expected_amount: Option<String>,
    pub received_amount: Option<String>,
    pub shortfall_amount: Option<String>,
    pub variance_eligible: bool,
    pub variance_reason: Option<VarianceReason>,
    pub official_ptax: OfficialPtaxStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OfficialPtaxStatus {
    pub status: &'static str,
    pub operation_date: Option<String>,
    pub quote_date: Option<String>,
    pub purchase: Option<String>,
    pub sale: Option<String>,
    pub response_sha256: Option<String>,
    pub brl_reference: Option<String>,
    pub source_id: Option<String>,
    pub policy_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TerminalNotification {
    pub notification_id: u64,
    #[serde(flatten)]
    pub result: CheckResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchOutcome {
    Terminal,
    Continue,
    PendingTimeout,
    IncompleteTimeout,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WatchPaymentResult {
    pub receivable_id: String,
    pub window: u8,
    pub outcome: WatchOutcome,
    pub polls: u8,
    pub incomplete_polls: u8,
    pub poll_interval_seconds: u64,
    pub last_observation: Option<CheckResult>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolveReviewResult {
    pub receivable_id: String,
    pub candidate_fingerprint: String,
    pub action: ReviewResolutionAction,
    pub state: ReceivableState,
    pub variance_reason: Option<VarianceReason>,
    pub expected_amount: Option<String>,
    pub received_amount: Option<String>,
    pub shortfall_amount: Option<String>,
    pub approval_run_id: String,
}

#[derive(Debug, Serialize)]
pub struct ReconcileOpenResult {
    pub checked: usize,
    pub payment_verified: usize,
    pub pending: usize,
    pub needs_review: usize,
    pub incomplete: usize,
    pub anomaly_samples: Vec<String>,
    pub incomplete_samples: Vec<String>,
    pub terminal: Vec<TerminalNotification>,
}

#[derive(Debug, Serialize)]
pub struct HotReconcileResult {
    pub window_seconds: u64,
    pub checked: usize,
    pub payment_verified: usize,
    pub pending: usize,
    pub needs_review: usize,
    pub incomplete: usize,
    pub incomplete_samples: Vec<String>,
    pub terminal: Vec<TerminalNotification>,
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

    /// Watch one expected receivable for a short, bounded settlement window.
    ///
    /// The model may bridge bounded numbered windows. Polling within each
    /// window, cluster validation, state transitions, and stopping conditions
    /// stay inside deterministic Rust.
    pub fn watch_payment(
        &self,
        input: WatchPaymentInput,
    ) -> Result<WatchPaymentResult, ReconcileError> {
        self.watch_payment_with(input, thread::sleep)
    }

    fn watch_payment_with<F>(
        &self,
        input: WatchPaymentInput,
        sleep_between: F,
    ) -> Result<WatchPaymentResult, ReconcileError>
    where
        F: FnMut(Duration),
    {
        if !(1..=WATCH_MAX_WINDOWS).contains(&input.window) {
            return Err(ReconcileError::InvalidInput);
        }
        let id =
            ReceivableId::new(input.receivable_id).map_err(|_| ReconcileError::InvalidInput)?;
        let mut result =
            self.watch_id_with(&id, WATCH_POLLS_PER_WINDOW, WATCH_INTERVAL, sleep_between)?;
        result.window = input.window;
        if input.window < WATCH_MAX_WINDOWS
            && matches!(
                result.outcome,
                WatchOutcome::PendingTimeout | WatchOutcome::IncompleteTimeout
            )
        {
            result.outcome = WatchOutcome::Continue;
        }
        Ok(result)
    }

    fn watch_id_with<F>(
        &self,
        id: &ReceivableId,
        max_polls: u8,
        interval: Duration,
        sleep_between: F,
    ) -> Result<WatchPaymentResult, ReconcileError>
    where
        F: FnMut(Duration),
    {
        if max_polls == 0 {
            return Err(ReconcileError::InvalidInput);
        }
        let mut owner_bytes = [0_u8; 16];
        fill(&mut owner_bytes).map_err(|_| ReconcileError::StorageUnavailable)?;
        let owner = bs58::encode(owner_bytes).into_string();
        let now = now_unix_ms()?;
        self.store
            .acquire_reconciliation_lease(&owner, now, now + LEASE_DURATION_MS)
            .map_err(|error| map_store(&error))?;
        let result = self.watch_id_acquired(id, max_polls, interval, sleep_between);
        let released = self
            .store
            .release_reconciliation_lease(&owner)
            .map_err(|error| map_store(&error));
        match (result, released) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    fn watch_id_acquired<F>(
        &self,
        id: &ReceivableId,
        max_polls: u8,
        interval: Duration,
        mut sleep_between: F,
    ) -> Result<WatchPaymentResult, ReconcileError>
    where
        F: FnMut(Duration),
    {
        let stored = self
            .store
            .get(id)
            .map_err(|error| map_store(&error))?
            .ok_or(ReconcileError::NotFound)?;
        if stored.state != ReceivableState::Open {
            return Ok(watch_terminal(
                self.check_id_with_genesis(id, None)?,
                1,
                0,
                interval,
            ));
        }

        let expected_genesis = self.config.recebi.genesis_hash.clone();
        let mut observed_genesis = None;
        let mut last_observation = None;
        let mut incomplete_polls = 0_u8;
        let mut final_poll_complete = false;

        for poll in 1..=max_polls {
            if observed_genesis.is_none() {
                match self.rpc.genesis_hash() {
                    Ok(genesis) if genesis == expected_genesis => observed_genesis = Some(genesis),
                    Ok(_) => return Err(ReconcileError::WrongCluster),
                    Err(_) => {
                        incomplete_polls = incomplete_polls.saturating_add(1);
                        final_poll_complete = false;
                        if poll < max_polls {
                            sleep_between(interval);
                        }
                        continue;
                    }
                }
            }

            match self.check_id_with_genesis(id, observed_genesis.as_ref()) {
                Ok(checked) if checked.status != CheckStatus::Pending => {
                    return Ok(watch_terminal(checked, poll, incomplete_polls, interval));
                }
                Ok(checked) => {
                    last_observation = Some(checked);
                    final_poll_complete = true;
                }
                Err(ReconcileError::RpcIncomplete) => {
                    incomplete_polls = incomplete_polls.saturating_add(1);
                    final_poll_complete = false;
                }
                Err(ReconcileError::MalformedEvidence) => {
                    return Err(ReconcileError::MalformedEvidence);
                }
                Err(error) => return Err(error),
            }
            if poll < max_polls {
                sleep_between(interval);
            }
        }

        Ok(WatchPaymentResult {
            receivable_id: id.as_str().to_owned(),
            window: 0,
            outcome: if final_poll_complete {
                WatchOutcome::PendingTimeout
            } else {
                WatchOutcome::IncompleteTimeout
            },
            polls: max_polls,
            incomplete_polls,
            poll_interval_seconds: interval.as_secs(),
            last_observation,
        })
    }

    #[allow(clippy::too_many_lines)]
    // Keep every persisted state mapped explicitly to its externally visible
    // result so a new state cannot silently inherit another state's meaning.
    fn check_id(&self, id: &ReceivableId) -> Result<CheckResult, ReconcileError> {
        self.check_id_with_genesis(id, None)
    }

    #[allow(clippy::too_many_lines)]
    fn check_id_with_genesis(
        &self,
        id: &ReceivableId,
        observed_genesis: Option<&GenesisHash>,
    ) -> Result<CheckResult, ReconcileError> {
        let stored = self
            .store
            .get(id)
            .map_err(|error| map_store(&error))?
            .ok_or(ReconcileError::NotFound)?;
        match stored.state {
            ReceivableState::PaymentVerified => {
                let settlement = self
                    .store
                    .settlement_summary(id)
                    .map_err(|error| map_store(&error))?
                    .ok_or(ReconcileError::StorageUnavailable)?;
                let signature = settlement.signature;
                let official_ptax = self.official_ptax_status(id, true)?;
                return Ok(CheckResult {
                    receivable_id: id.as_str().to_owned(),
                    status: CheckStatus::PaymentVerified,
                    explorer_url: Some(explorer_url(self.config.recebi.cluster, &signature)),
                    signature: Some(signature),
                    reason: None,
                    candidate_fingerprint: None,
                    expected_amount: Some(
                        settlement.expected_amount.format(stored.request.decimals),
                    ),
                    received_amount: Some(
                        settlement.received_amount.format(stored.request.decimals),
                    ),
                    shortfall_amount: None,
                    variance_eligible: false,
                    variance_reason: None,
                    official_ptax,
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
                    explorer_url: Some(explorer_url(
                        self.config.recebi.cluster,
                        &candidate.signature,
                    )),
                    signature: Some(candidate.signature),
                    reason: Some(candidate.verdict),
                    candidate_fingerprint: Some(candidate.candidate_fingerprint),
                    expected_amount: candidate
                        .underpayment
                        .as_ref()
                        .map(|evidence| evidence.expected_amount.format(stored.request.decimals)),
                    received_amount: candidate
                        .underpayment
                        .as_ref()
                        .map(|evidence| evidence.received_amount.format(stored.request.decimals)),
                    shortfall_amount: candidate
                        .underpayment
                        .as_ref()
                        .map(|evidence| evidence.shortfall_amount.format(stored.request.decimals)),
                    variance_eligible: candidate.underpayment.is_some(),
                    variance_reason: None,
                    official_ptax: official_ptax_unavailable(),
                });
            }
            ReceivableState::Cancelled => {
                return Ok(CheckResult {
                    receivable_id: id.as_str().to_owned(),
                    status: CheckStatus::CancelledUnpaid,
                    signature: None,
                    explorer_url: None,
                    reason: Some("cancelled_unpaid".to_owned()),
                    candidate_fingerprint: None,
                    expected_amount: None,
                    received_amount: None,
                    shortfall_amount: None,
                    variance_eligible: false,
                    variance_reason: None,
                    official_ptax: official_ptax_unavailable(),
                });
            }
            ReceivableState::SettledWithVariance => {
                let settlement = self
                    .store
                    .settlement_summary(id)
                    .map_err(|error| map_store(&error))?
                    .ok_or(ReconcileError::StorageUnavailable)?;
                let shortfall = settlement
                    .expected_amount
                    .get()
                    .checked_sub(settlement.received_amount.get())
                    .ok_or(ReconcileError::StorageUnavailable)?;
                return Ok(CheckResult {
                    receivable_id: id.as_str().to_owned(),
                    status: CheckStatus::SettledWithVariance,
                    explorer_url: Some(explorer_url(
                        self.config.recebi.cluster,
                        &settlement.signature,
                    )),
                    signature: Some(settlement.signature),
                    reason: Some("operator_accepted_underpayment".to_owned()),
                    candidate_fingerprint: None,
                    expected_amount: Some(
                        settlement.expected_amount.format(stored.request.decimals),
                    ),
                    received_amount: Some(
                        settlement.received_amount.format(stored.request.decimals),
                    ),
                    shortfall_amount: Some(
                        recebi_core::AtomicAmount::new(shortfall).format(stored.request.decimals),
                    ),
                    variance_eligible: false,
                    variance_reason: settlement.variance_reason,
                    official_ptax: self.official_ptax_status(id, true)?,
                });
            }
            ReceivableState::Open => {}
            _ => return Err(ReconcileError::StorageUnavailable),
        }
        self.check_open(&stored, observed_genesis)
    }

    #[allow(clippy::too_many_lines)] // Keep the ordered fail-closed verification path auditable.
    fn check_open(
        &self,
        stored: &StoredReceivable,
        observed_genesis: Option<&GenesisHash>,
    ) -> Result<CheckResult, ReconcileError> {
        let expected_genesis = self.config.recebi.genesis_hash.clone();
        let observed_genesis = observed_genesis
            .cloned()
            .map_or_else(|| self.rpc.genesis_hash().map_err(map_rpc), Ok)?;
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
            match assess_settlement_once(&snapshot, &expected, &signatures, &references) {
                Ok(SettlementAssessment::Exact(evidence)) => {
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
                        explorer_url: Some(explorer_url(
                            self.config.recebi.cluster,
                            &evidence.signature,
                        )),
                        signature: Some(evidence.signature),
                        reason: None,
                        candidate_fingerprint: None,
                        expected_amount: Some(expected.amount.format(expected.token_decimals)),
                        received_amount: Some(expected.amount.format(expected.token_decimals)),
                        shortfall_amount: None,
                        variance_eligible: false,
                        variance_reason: None,
                        official_ptax: official_ptax_pending(),
                    });
                }
                Ok(SettlementAssessment::Underpayment(evidence)) => {
                    let fingerprint = evidence.fingerprint.clone();
                    if !resolved_fingerprints.contains(&fingerprint) {
                        first_anomaly.get_or_insert((
                            evidence.signature.clone(),
                            evidence.slot,
                            "wrong_amount",
                            fingerprint,
                            Some(evidence),
                        ));
                    }
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
                            None,
                        ));
                    }
                }
            }
        }
        let Some((signature, slot, verdict, candidate_fingerprint, underpayment)) = first_anomaly
        else {
            return Ok(pending_result(&stored.request.receivable_id));
        };
        self.record_anomaly(
            stored,
            &signature,
            slot,
            verdict,
            &candidate_fingerprint,
            underpayment.as_ref(),
        )?;
        Ok(CheckResult {
            receivable_id: stored.request.receivable_id.as_str().to_owned(),
            status: CheckStatus::NeedsReview,
            explorer_url: Some(explorer_url(self.config.recebi.cluster, &signature)),
            signature: Some(signature),
            reason: Some(verdict.to_owned()),
            candidate_fingerprint: Some(candidate_fingerprint),
            expected_amount: underpayment
                .as_ref()
                .map(|evidence| evidence.expected_amount.format(stored.request.decimals)),
            received_amount: underpayment
                .as_ref()
                .map(|evidence| evidence.received_amount.format(stored.request.decimals)),
            shortfall_amount: underpayment
                .as_ref()
                .map(|evidence| evidence.shortfall_amount.format(stored.request.decimals)),
            variance_eligible: underpayment.is_some(),
            variance_reason: None,
            official_ptax: official_ptax_unavailable(),
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
                input.variance_reason,
                &input.approval_run_id,
                now_unix_ms()?,
            )
            .map_err(|error| match error {
                StoreError::InvalidTransition => ReconcileError::ReviewConflict,
                _ => map_store(&error),
            })?;
        let variance = if state == ReceivableState::SettledWithVariance {
            self.store
                .settlement_summary(&receivable_id)
                .map_err(|error| map_store(&error))?
        } else {
            None
        };
        let shortfall = variance
            .as_ref()
            .map(|summary| {
                summary
                    .expected_amount
                    .get()
                    .checked_sub(summary.received_amount.get())
                    .ok_or(ReconcileError::StorageUnavailable)
            })
            .transpose()?;
        Ok(ResolveReviewResult {
            receivable_id: receivable_id.as_str().to_owned(),
            candidate_fingerprint: input.candidate_fingerprint,
            action: input.action,
            state,
            variance_reason: input.variance_reason,
            expected_amount: variance.as_ref().map(|summary| {
                summary
                    .expected_amount
                    .format(self.config.recebi.token_decimals)
            }),
            received_amount: variance.as_ref().map(|summary| {
                summary
                    .received_amount
                    .format(self.config.recebi.token_decimals)
            }),
            shortfall_amount: shortfall.map(|amount| {
                recebi_core::AtomicAmount::new(amount).format(self.config.recebi.token_decimals)
            }),
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
        underpayment: Option<&UnderpaymentEvidence>,
    ) -> Result<(), ReconcileError> {
        let observed_at = now_unix_ms()?;
        match underpayment {
            None => self.store.mark_needs_review(
                &stored.request.receivable_id,
                signature,
                slot,
                verdict,
                fingerprint,
                observed_at,
            ),
            Some(evidence) => self.store.mark_underpayment_review(
                &stored.request.receivable_id,
                evidence,
                observed_at,
            ),
        }
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
            incomplete: 0,
            anomaly_samples: vec![],
            incomplete_samples: vec![],
            terminal: vec![],
        };
        if open.is_empty() {
            result.terminal = self.pending_terminal_notifications()?;
            return Ok(result);
        }
        let expected_genesis = self.config.recebi.genesis_hash.clone();
        let observed_genesis = match self.rpc.genesis_hash() {
            Ok(genesis) if genesis == expected_genesis => Some(genesis),
            Ok(_) => return Err(ReconcileError::WrongCluster),
            Err(_) => None,
        };
        for receivable in open {
            result.checked += 1;
            let checked = match observed_genesis.as_ref() {
                Some(genesis) => {
                    self.check_id_with_genesis(&receivable.request.receivable_id, Some(genesis))
                }
                None => Err(ReconcileError::RpcIncomplete),
            };
            let checked = match checked {
                Ok(checked) => checked,
                Err(ReconcileError::RpcIncomplete | ReconcileError::MalformedEvidence) => {
                    result.incomplete += 1;
                    if result.incomplete_samples.len() < MAX_ANOMALY_SAMPLES {
                        result
                            .incomplete_samples
                            .push(receivable.request.receivable_id.as_str().to_owned());
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            match checked.status {
                CheckStatus::PaymentVerified => {
                    result.payment_verified += 1;
                }
                CheckStatus::Pending => result.pending += 1,
                CheckStatus::NeedsReview => {
                    result.needs_review += 1;
                    if result.anomaly_samples.len() < MAX_ANOMALY_SAMPLES {
                        result.anomaly_samples.push(checked.receivable_id.clone());
                    }
                }
                CheckStatus::CancelledUnpaid => return Err(ReconcileError::StorageUnavailable),
                CheckStatus::SettledWithVariance => {
                    return Err(ReconcileError::StorageUnavailable);
                }
            }
        }
        result.terminal = self.pending_terminal_notifications()?;
        Ok(result)
    }

    /// Reconcile only invoices created during the short hot-monitoring window.
    /// The permanent five-minute pass remains responsible for older invoices.
    pub fn hot_reconcile(
        &self,
        _input: HotReconcileInput,
    ) -> Result<HotReconcileResult, ReconcileError> {
        let mut owner_bytes = [0_u8; 16];
        fill(&mut owner_bytes).map_err(|_| ReconcileError::StorageUnavailable)?;
        let owner = bs58::encode(owner_bytes).into_string();
        let now = now_unix_ms()?;
        self.store
            .acquire_reconciliation_lease(&owner, now, now + LEASE_DURATION_MS)
            .map_err(|error| map_store(&error))?;
        let result = self.hot_reconcile_acquired(now);
        let released = self
            .store
            .release_reconciliation_lease(&owner)
            .map_err(|error| map_store(&error));
        match (result, released) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    pub fn acknowledge_notification(
        &self,
        input: &AcknowledgeNotificationInput,
    ) -> Result<AcknowledgeNotificationResult, ReconcileError> {
        if input.delivery_receipt.is_empty()
            || input.delivery_receipt.len() > 128
            || !input
                .delivery_receipt
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
        {
            return Err(ReconcileError::InvalidInput);
        }
        self.store
            .mark_terminal_notification_delivered(
                input.notification_id,
                now_unix_ms()?,
                &input.delivery_receipt,
            )
            .map_err(|error| map_store(&error))?;
        Ok(AcknowledgeNotificationResult {
            notification_id: input.notification_id,
            status: "delivered",
        })
    }

    fn official_ptax_status(
        &self,
        id: &ReceivableId,
        settled: bool,
    ) -> Result<OfficialPtaxStatus, ReconcileError> {
        let valuation = self
            .store
            .valuation(id)
            .map_err(|error| map_store(&error))?;
        Ok(valuation.map_or_else(
            || {
                if settled {
                    official_ptax_pending()
                } else {
                    official_ptax_unavailable()
                }
            },
            |valuation| OfficialPtaxStatus {
                status: "bcb_verified",
                operation_date: Some(valuation.evidence.operation_date.as_str().to_owned()),
                quote_date: Some(valuation.evidence.quote_date.as_str().to_owned()),
                purchase: Some(valuation.evidence.purchase),
                sale: Some(valuation.evidence.sale),
                response_sha256: Some(valuation.evidence.response_sha256),
                brl_reference: Some(format_brl_cents(valuation.brl_reference_cents)),
                source_id: Some(valuation.evidence.source_id),
                policy_version: Some(valuation.evidence.policy_version),
            },
        ))
    }

    fn hot_reconcile_acquired(&self, now: i64) -> Result<HotReconcileResult, ReconcileError> {
        let cutoff = now.saturating_sub(
            i64::try_from(HOT_WINDOW.as_millis())
                .map_err(|_| ReconcileError::StorageUnavailable)?,
        );
        let open = self
            .store
            .list_open_since(cutoff, MAX_RECONCILE_RECEIVABLES)
            .map_err(|error| map_store(&error))?;
        let mut result = HotReconcileResult {
            window_seconds: HOT_WINDOW.as_secs(),
            checked: 0,
            payment_verified: 0,
            pending: 0,
            needs_review: 0,
            incomplete: 0,
            incomplete_samples: Vec::new(),
            terminal: Vec::new(),
        };
        for receivable in open {
            result.checked += 1;
            match self.check_id(&receivable.request.receivable_id) {
                Ok(checked) => match checked.status {
                    CheckStatus::Pending => result.pending += 1,
                    CheckStatus::PaymentVerified => result.payment_verified += 1,
                    CheckStatus::NeedsReview => {
                        result.needs_review += 1;
                    }
                    CheckStatus::CancelledUnpaid | CheckStatus::SettledWithVariance => {
                        return Err(ReconcileError::StorageUnavailable);
                    }
                },
                Err(ReconcileError::RpcIncomplete | ReconcileError::MalformedEvidence) => {
                    result.incomplete += 1;
                    if result.incomplete_samples.len() < MAX_ANOMALY_SAMPLES {
                        result
                            .incomplete_samples
                            .push(receivable.request.receivable_id.as_str().to_owned());
                    }
                }
                Err(error) => return Err(error),
            }
        }
        result.terminal = self.pending_terminal_notifications()?;
        Ok(result)
    }

    fn pending_terminal_notifications(&self) -> Result<Vec<TerminalNotification>, ReconcileError> {
        self.store
            .pending_terminal_notifications(MAX_ANOMALY_SAMPLES)
            .map_err(|error| map_store(&error))?
            .into_iter()
            .map(|notification| {
                let result = self.check_id(&notification.receivable_id)?;
                let expected_status = match result.status {
                    CheckStatus::PaymentVerified => "payment_verified",
                    CheckStatus::NeedsReview => "needs_review",
                    _ => return Err(ReconcileError::StorageUnavailable),
                };
                if expected_status != notification.status {
                    return Err(ReconcileError::StorageUnavailable);
                }
                Ok(TerminalNotification {
                    notification_id: notification.id,
                    result,
                })
            })
            .collect()
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
        explorer_url: None,
        reason: None,
        candidate_fingerprint: None,
        expected_amount: None,
        received_amount: None,
        shortfall_amount: None,
        variance_eligible: false,
        variance_reason: None,
        official_ptax: official_ptax_unavailable(),
    }
}

fn official_ptax_pending() -> OfficialPtaxStatus {
    OfficialPtaxStatus {
        status: "pending_monthly_close",
        operation_date: None,
        quote_date: None,
        purchase: None,
        sale: None,
        response_sha256: None,
        brl_reference: None,
        source_id: None,
        policy_version: None,
    }
}

fn official_ptax_unavailable() -> OfficialPtaxStatus {
    OfficialPtaxStatus {
        status: "not_available_unpaid",
        operation_date: None,
        quote_date: None,
        purchase: None,
        sale: None,
        response_sha256: None,
        brl_reference: None,
        source_id: None,
        policy_version: None,
    }
}

fn format_brl_cents(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn explorer_url(cluster: Cluster, signature: &str) -> String {
    match cluster {
        Cluster::MainnetBeta => format!("https://explorer.solana.com/tx/{signature}"),
        Cluster::Devnet => {
            format!("https://explorer.solana.com/tx/{signature}?cluster=devnet")
        }
    }
}

fn watch_terminal(
    checked: CheckResult,
    polls: u8,
    incomplete_polls: u8,
    interval: Duration,
) -> WatchPaymentResult {
    WatchPaymentResult {
        receivable_id: checked.receivable_id.clone(),
        window: 0,
        outcome: WatchOutcome::Terminal,
        polls,
        incomplete_polls,
        poll_interval_seconds: interval.as_secs(),
        last_observation: Some(checked),
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
    use std::{cell::Cell, collections::HashMap, sync::Arc};

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
        setup_at(
            amount_in_transaction,
            include_second_correct,
            now_unix_ms().expect("test timestamp"),
        )
    }

    fn setup_at(
        amount_in_transaction: u64,
        include_second_correct: bool,
        created_at_unix_ms: i64,
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
                created_at_unix_ms,
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
        assert_eq!(first.expected_amount.as_deref(), Some("0.1"));
        assert_eq!(first.received_amount.as_deref(), Some("0.1"));
        assert!(
            first
                .explorer_url
                .as_deref()
                .is_some_and(|url| url.starts_with("https://explorer.solana.com/tx/")
                    && url.ends_with("?cluster=devnet"))
        );
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
                variance_reason: None,
                approval_run_id: "run-test-reopen".to_owned(),
            })
            .expect("reopen");
        assert_eq!(reopened.state, ReceivableState::Open);
        assert_eq!(
            service.resolve_review(ResolveReviewInput {
                receivable_id: id.as_str().to_owned(),
                candidate_fingerprint: "cd".repeat(32),
                action: ReviewResolutionAction::CancelUnpaid,
                variance_reason: None,
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
                variance_reason: None,
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
    fn operator_can_accept_only_a_canonical_underpayment_as_variance() {
        let (_directory, service, id) = setup(99_000, false);
        let candidate = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("candidate");
        assert_eq!(candidate.status, CheckStatus::NeedsReview);
        assert!(candidate.variance_eligible);
        assert_eq!(candidate.expected_amount.as_deref(), Some("0.1"));
        assert_eq!(candidate.received_amount.as_deref(), Some("0.099"));
        assert_eq!(candidate.shortfall_amount.as_deref(), Some("0.001"));
        let fingerprint = candidate.candidate_fingerprint.expect("fingerprint");

        assert_eq!(
            service.resolve_review(ResolveReviewInput {
                receivable_id: id.as_str().to_owned(),
                candidate_fingerprint: fingerprint.clone(),
                action: ReviewResolutionAction::AcceptUnderpaymentWithVariance,
                variance_reason: None,
                approval_run_id: "run-missing-reason".to_owned(),
            }),
            Err(ReconcileError::ReviewConflict)
        );
        let accepted = service
            .resolve_review(ResolveReviewInput {
                receivable_id: id.as_str().to_owned(),
                candidate_fingerprint: fingerprint,
                action: ReviewResolutionAction::AcceptUnderpaymentWithVariance,
                variance_reason: Some(VarianceReason::MerchantWriteOff),
                approval_run_id: "run-variance".to_owned(),
            })
            .expect("accept variance");
        assert_eq!(accepted.state, ReceivableState::SettledWithVariance);
        let checked = service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("settled check");
        assert_eq!(checked.status, CheckStatus::SettledWithVariance);
        assert_eq!(
            checked.reason.as_deref(),
            Some("operator_accepted_underpayment")
        );
        assert_eq!(checked.expected_amount.as_deref(), Some("0.1"));
        assert_eq!(checked.received_amount.as_deref(), Some("0.099"));
        assert_eq!(checked.shortfall_amount.as_deref(), Some("0.001"));
        assert_eq!(
            checked.variance_reason,
            Some(VarianceReason::MerchantWriteOff)
        );
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
    fn expected_payment_watch_stops_immediately_on_terminal_state() {
        let (_directory, service, id) = setup(100_000, false);
        let sleeps = Cell::new(0_u8);
        let result = service
            .watch_id_with(&id, 3, Duration::ZERO, |_| sleeps.set(sleeps.get() + 1))
            .expect("watch");
        assert_eq!(result.outcome, WatchOutcome::Terminal);
        assert_eq!(result.polls, 1);
        assert_eq!(result.incomplete_polls, 0);
        assert_eq!(sleeps.get(), 0);
        assert_eq!(
            result.last_observation.expect("observation").status,
            CheckStatus::PaymentVerified
        );
    }

    #[test]
    fn expected_payment_watch_is_bounded_when_payment_is_pending() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.candidates.clear();
        let sleeps = Cell::new(0_u8);
        let result = service
            .watch_id_with(&id, 3, Duration::ZERO, |_| sleeps.set(sleeps.get() + 1))
            .expect("watch");
        assert_eq!(result.outcome, WatchOutcome::PendingTimeout);
        assert_eq!(result.polls, 3);
        assert_eq!(result.incomplete_polls, 0);
        assert_eq!(sleeps.get(), 2);
        assert_eq!(
            result.last_observation.expect("observation").status,
            CheckStatus::Pending
        );
        assert_open(&service, &id);
    }

    #[test]
    fn stock_host_watch_windows_continue_then_stop_at_the_rust_cap() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.candidates.clear();
        let first = service
            .watch_payment_with(
                WatchPaymentInput {
                    receivable_id: id.as_str().to_owned(),
                    window: 1,
                },
                |_| {},
            )
            .expect("first window");
        assert_eq!(first.window, 1);
        assert_eq!(first.polls, WATCH_POLLS_PER_WINDOW);
        assert_eq!(first.outcome, WatchOutcome::Continue);

        let final_window = service
            .watch_payment_with(
                WatchPaymentInput {
                    receivable_id: id.as_str().to_owned(),
                    window: WATCH_MAX_WINDOWS,
                },
                |_| {},
            )
            .expect("final window");
        assert_eq!(final_window.window, WATCH_MAX_WINDOWS);
        assert_eq!(final_window.outcome, WatchOutcome::PendingTimeout);
        assert_open(&service, &id);
    }

    #[test]
    fn stock_host_watch_rejects_a_window_outside_the_rust_cap() {
        let (_directory, service, id) = setup(100_000, false);
        assert_eq!(
            service.watch_payment_with(
                WatchPaymentInput {
                    receivable_id: id.as_str().to_owned(),
                    window: WATCH_MAX_WINDOWS + 1,
                },
                |_| {},
            ),
            Err(ReconcileError::InvalidInput)
        );
    }

    #[test]
    fn expected_payment_watch_never_turns_rpc_failure_into_pending_or_paid() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.genesis_error = Some(RpcError::Transport);
        let result = service
            .watch_id_with(&id, 3, Duration::ZERO, |_| {})
            .expect("bounded incomplete watch");
        assert_eq!(result.outcome, WatchOutcome::IncompleteTimeout);
        assert_eq!(result.polls, 3);
        assert_eq!(result.incomplete_polls, 3);
        assert!(result.last_observation.is_none());
        assert_open(&service, &id);
    }

    #[test]
    fn expected_payment_watch_does_not_retry_malformed_evidence() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.candidates[0].block_time_unix = None;
        let sleeps = Cell::new(0_u8);
        assert_eq!(
            service.watch_id_with(&id, 3, Duration::ZERO, |_| {
                sleeps.set(sleeps.get() + 1);
            }),
            Err(ReconcileError::MalformedEvidence)
        );
        assert_eq!(sleeps.get(), 0);
        assert_open(&service, &id);
    }

    #[test]
    fn expected_payment_watch_reads_an_already_terminal_state_without_rpc() {
        let (_directory, mut service, id) = setup(100_000, false);
        service
            .check(CheckInput {
                receivable_id: id.as_str().to_owned(),
            })
            .expect("settle");
        service.rpc.genesis_error = Some(RpcError::Transport);
        let result = service
            .watch_id_with(&id, 3, Duration::ZERO, |_| {})
            .expect("terminal state");
        assert_eq!(result.outcome, WatchOutcome::Terminal);
        assert_eq!(
            result.last_observation.expect("observation").status,
            CheckStatus::PaymentVerified
        );
    }

    #[test]
    fn scheduled_reconcile_reports_bounded_counts_and_keeps_transient_rpc_unknown() {
        let (_directory, mut service, id) = setup(100_000, false);
        service.rpc.candidates.clear();
        let result = service
            .reconcile_open(ReconcileOpenInput { max_count: Some(1) })
            .expect("scheduled pass");
        assert_eq!(result.checked, 1);
        assert_eq!(result.pending, 1);
        assert_eq!(result.payment_verified, 0);
        assert_eq!(result.needs_review, 0);
        assert_eq!(result.incomplete, 0);

        service.rpc.genesis_error = Some(RpcError::Transport);
        let incomplete = service
            .reconcile_open(ReconcileOpenInput { max_count: Some(1) })
            .expect("incomplete pass is reported, not inferred");
        assert_eq!(incomplete.checked, 1);
        assert_eq!(incomplete.incomplete, 1);
        assert_eq!(incomplete.incomplete_samples, vec![id.as_str().to_owned()]);
        assert_eq!(incomplete.payment_verified, 0);
        assert_open(&service, &id);
    }

    #[test]
    fn hot_reconcile_checks_only_the_recent_window_and_returns_terminal_evidence() {
        let (_directory, service, id) = setup(100_000, false);
        let result = service
            .hot_reconcile(HotReconcileInput {})
            .expect("hot reconciliation");
        assert_eq!(result.window_seconds, 180);
        assert_eq!(result.checked, 1);
        assert_eq!(result.payment_verified, 1);
        assert_eq!(result.pending, 0);
        assert_eq!(result.needs_review, 0);
        assert_eq!(result.incomplete, 0);
        assert_eq!(result.terminal.len(), 1);
        assert_eq!(result.terminal[0].result.receivable_id, id.as_str());
        assert_eq!(
            result.terminal[0].result.status,
            CheckStatus::PaymentVerified
        );
    }

    #[test]
    fn hot_reconcile_leaves_expired_invoices_for_the_background_pass() {
        let old = now_unix_ms()
            .expect("test timestamp")
            .saturating_sub(181_000);
        let (_directory, service, id) = setup_at(100_000, false, old);
        let result = service
            .hot_reconcile(HotReconcileInput {})
            .expect("hot reconciliation");
        assert_eq!(result.checked, 0);
        assert!(result.terminal.is_empty());
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

    #[test]
    fn overlapping_expected_payment_watch_is_rejected() {
        let (_directory, service, id) = setup(100_000, false);
        service
            .store
            .acquire_reconciliation_lease("other-run", 0, i64::MAX)
            .expect("lease");
        assert!(matches!(
            service.watch_id_with(&id, 1, Duration::ZERO, |_| {}),
            Err(ReconcileError::Busy)
        ));
        assert_open(&service, &id);
    }
}
