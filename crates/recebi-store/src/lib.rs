use std::{
    fmt::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use recebi_core::{
    AtomicAmount, BoundedText, NOMINAL_USDC_USD_METHOD, PaymentRequest, PtaxDate, PtaxEvidence,
    PublicKey, ReceivableId, ReceivableState, Reference, ReviewResolutionAction,
    SettlementEvidence, UnderpaymentEvidence, VarianceReason,
    limits::{MAX_MONTH_EXPORT_ROWS, MAX_PUBLIC_LABEL_BYTES},
    solana::derive_classic_ata,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use thiserror::Error;

const EVENT_DOMAIN: &str = "recebi.receivable_event.v1";
const EVENT_SCHEMA_VERSION: i64 = 1;
/// A deployment can open several stores at once: the `ZeroClaw` session server,
/// a scheduled reconcile job, and an operator command. Opening runs schema
/// creation and integrity verification inside a write transaction, so a short
/// busy timeout made concurrent startup fail as `Unavailable`. This bound waits
/// instead of failing, while still guaranteeing forward progress.
const BUSY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("storage is unavailable")]
    Unavailable,
    #[error("receivable ID is already bound to different terms")]
    IdempotencyConflict,
    #[error("payment reference is already in use")]
    ReferenceReuse,
    #[error("ledger integrity check failed")]
    Integrity,
    #[error("receivable state does not allow this transition")]
    InvalidTransition,
    #[error("settlement signature or reference was already consumed")]
    Replay,
    #[error("another reconciliation is already running")]
    ReconciliationBusy,
    #[error("another monthly export is already running")]
    MonthlyExportBusy,
    #[error("the material ledger changed during export")]
    ConcurrentMutation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredReceivable {
    pub request: PaymentRequest,
    pub state: ReceivableState,
    pub created_at_unix_ms: i64,
    pub solana_pay_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredReviewCandidate {
    pub signature: String,
    pub slot: u64,
    pub verdict: String,
    pub candidate_fingerprint: String,
    pub underpayment: Option<StoredUnderpaymentEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredUnderpaymentEvidence {
    pub block_time_unix: i64,
    pub recipient: PublicKey,
    pub mint: PublicKey,
    pub expected_amount: AtomicAmount,
    pub received_amount: AtomicAmount,
    pub shortfall_amount: AtomicAmount,
    pub instruction_position: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredValuation {
    pub evidence: PtaxEvidence,
    pub brl_reference_cents: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSettledReceivable {
    pub receivable: StoredReceivable,
    pub signature: String,
    pub slot: u64,
    pub block_time_unix: i64,
    pub settlement_fingerprint: String,
    pub settlement_kind: String,
    pub expected_amount: AtomicAmount,
    pub received_amount: AtomicAmount,
    pub variance_reason: Option<VarianceReason>,
    pub approval_run_id: Option<String>,
    pub valuation: Option<StoredValuation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSettlementSummary {
    pub signature: String,
    pub settlement_kind: String,
    pub expected_amount: AtomicAmount,
    pub received_amount: AtomicAmount,
    pub variance_reason: Option<VarianceReason>,
    pub approval_run_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredTerminalNotification {
    pub id: u64,
    pub receivable_id: ReceivableId,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredMonthClose {
    pub month: String,
    pub revision: u32,
    pub artifact_kind: String,
    pub canonical_json: Vec<u8>,
    pub accountant_csv: Vec<u8>,
    pub manifest_json: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ReceivableStore {
    path: PathBuf,
}

impl ReceivableStore {
    /// # Errors
    ///
    /// Initializes the local schema or returns a redacted storage error.
    #[allow(clippy::too_many_lines)] // The explicit SQLite schema is kept together for auditability.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let store = Self {
            path: path.as_ref().to_path_buf(),
        };
        let connection = store.connection()?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS receivables (
                 receivable_id TEXT PRIMARY KEY NOT NULL,
                 recipient TEXT NOT NULL,
                 mint TEXT NOT NULL,
                 atomic_amount INTEGER NOT NULL,
                 decimals INTEGER NOT NULL,
                 reference TEXT NOT NULL UNIQUE,
                 public_label TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN ('open','payment_verified','needs_review','cancelled','settled_with_variance')),
                 created_at_unix_ms INTEGER NOT NULL,
                 solana_pay_url TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS receivable_events (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 receivable_id TEXT NOT NULL,
                 event_schema_version INTEGER NOT NULL,
                 event_domain TEXT NOT NULL,
                 previous_event_hash BLOB,
                 canonical_event_bytes BLOB NOT NULL,
                 event_hash BLOB NOT NULL UNIQUE
             );
             CREATE TRIGGER IF NOT EXISTS receivable_events_no_update BEFORE UPDATE ON receivable_events BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS receivable_events_no_delete BEFORE DELETE ON receivable_events BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TABLE IF NOT EXISTS settlements (
                 receivable_id TEXT PRIMARY KEY NOT NULL,
                 signature TEXT NOT NULL UNIQUE,
                 reference TEXT NOT NULL UNIQUE,
                 slot INTEGER NOT NULL,
                 block_time_unix INTEGER,
                 recipient TEXT NOT NULL,
                 mint TEXT NOT NULL,
                 atomic_amount INTEGER NOT NULL,
                 instruction_position INTEGER NOT NULL,
                 fingerprint TEXT NOT NULL UNIQUE,
                 observed_at_unix_ms INTEGER NOT NULL,
                 settlement_kind TEXT NOT NULL CHECK(settlement_kind IN ('exact','accepted_underpayment')) DEFAULT 'exact',
                 expected_atomic_amount INTEGER NOT NULL,
                 variance_reason TEXT CHECK(variance_reason IN ('rounding_adjustment','commercial_discount','merchant_write_off')),
                 approval_run_id TEXT
             );
             CREATE TABLE IF NOT EXISTS terminal_notifications (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 receivable_id TEXT NOT NULL,
                 status TEXT NOT NULL CHECK(status IN ('payment_verified','needs_review')),
                 evidence_fingerprint TEXT NOT NULL UNIQUE,
                 created_at_unix_ms INTEGER NOT NULL
             );
             CREATE TRIGGER IF NOT EXISTS terminal_notifications_no_update BEFORE UPDATE ON terminal_notifications BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS terminal_notifications_no_delete BEFORE DELETE ON terminal_notifications BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TABLE IF NOT EXISTS terminal_notification_deliveries (
                 notification_id INTEGER PRIMARY KEY NOT NULL,
                 delivered_at_unix_ms INTEGER NOT NULL,
                 delivery_receipt TEXT NOT NULL,
                 FOREIGN KEY(notification_id) REFERENCES terminal_notifications(id)
             );
             CREATE TRIGGER IF NOT EXISTS terminal_notification_deliveries_no_update BEFORE UPDATE ON terminal_notification_deliveries BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS terminal_notification_deliveries_no_delete BEFORE DELETE ON terminal_notification_deliveries BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TABLE IF NOT EXISTS review_candidates (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 receivable_id TEXT NOT NULL,
                 signature TEXT NOT NULL,
                 slot INTEGER NOT NULL,
                 verdict TEXT NOT NULL,
                 candidate_fingerprint TEXT NOT NULL UNIQUE,
                 observed_at_unix_ms INTEGER NOT NULL,
                 block_time_unix INTEGER,
                 recipient TEXT,
                 mint TEXT,
                 expected_atomic_amount INTEGER,
                 received_atomic_amount INTEGER,
                 shortfall_atomic_amount INTEGER,
                 instruction_position INTEGER
             );
             CREATE TABLE IF NOT EXISTS review_resolutions (
                 candidate_fingerprint TEXT PRIMARY KEY NOT NULL,
                 receivable_id TEXT NOT NULL,
                 action TEXT NOT NULL CHECK(action IN ('ignore_candidate_and_reopen','cancel_unpaid','accept_underpayment_with_variance')),
                 variance_reason TEXT CHECK(variance_reason IN ('rounding_adjustment','commercial_discount','merchant_write_off')),
                 approval_run_id TEXT,
                 resolved_at_unix_ms INTEGER NOT NULL
             );
             CREATE TRIGGER IF NOT EXISTS review_candidates_no_update BEFORE UPDATE ON review_candidates BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS review_candidates_no_delete BEFORE DELETE ON review_candidates BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS review_resolutions_no_update BEFORE UPDATE ON review_resolutions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS review_resolutions_no_delete BEFORE DELETE ON review_resolutions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TABLE IF NOT EXISTS reconciliation_lease (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 owner TEXT NOT NULL,
                 expires_at_unix_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS valuations (
                 receivable_id TEXT PRIMARY KEY NOT NULL,
                 operation_date TEXT NOT NULL,
                 quote_date TEXT NOT NULL,
                 purchase TEXT NOT NULL,
                 sale TEXT NOT NULL,
                 bulletin_type TEXT,
                 bulletin_timestamp TEXT NOT NULL,
                 retrieved_at_unix_ms INTEGER NOT NULL,
                 response_sha256 TEXT NOT NULL,
                 source_id TEXT NOT NULL,
                 policy_version TEXT NOT NULL,
                 valuation_method TEXT NOT NULL,
                 brl_reference_cents INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS month_close_revisions (
                 month TEXT NOT NULL,
                 revision INTEGER NOT NULL,
                 artifact_kind TEXT NOT NULL DEFAULT 'final_close',
                 canonical_json BLOB NOT NULL,
                 accountant_csv BLOB NOT NULL,
                 manifest_json BLOB NOT NULL,
                 close_hash BLOB NOT NULL UNIQUE,
                 PRIMARY KEY(month,revision)
             );
             CREATE TRIGGER IF NOT EXISTS valuations_no_update BEFORE UPDATE ON valuations BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS valuations_no_delete BEFORE DELETE ON valuations BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_close_revisions_no_update BEFORE UPDATE ON month_close_revisions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_close_revisions_no_delete BEFORE DELETE ON month_close_revisions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TABLE IF NOT EXISTS monthly_export_leases (
                 month TEXT PRIMARY KEY NOT NULL,
                 owner TEXT NOT NULL,
                 expires_at_unix_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ledger_checkpoints (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 previous_checkpoint_hash BLOB,
                 ledger_root BLOB NOT NULL,
                 checkpoint_hash BLOB NOT NULL UNIQUE
             );
             CREATE TRIGGER IF NOT EXISTS ledger_checkpoints_no_update BEFORE UPDATE ON ledger_checkpoints BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS ledger_checkpoints_no_delete BEFORE DELETE ON ledger_checkpoints BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (1);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (2);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (3);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (4);
             COMMIT;",
        ).map_err(|_| StoreError::Unavailable)?;
        migrate_state_constraint(&connection)?;
        migrate_phase_five_schema(&connection)?;
        migrate_phase_six_schema(&connection)?;
        migrate_variance_schema(&connection)?;
        initialize_ledger_checkpoints(&connection)?;
        secure_file(&store.path)?;
        Ok(store)
    }

    fn connection(&self) -> Result<Connection, StoreError> {
        let connection = Connection::open(&self.path).map_err(|_| StoreError::Unavailable)?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|_| StoreError::Unavailable)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .map_err(|_| StoreError::Unavailable)?;
        Ok(connection)
    }

    /// Creates the record and its creation event atomically, or returns the
    /// existing same-term record for an idempotent retry.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage, reference-reuse, or idempotency error.
    pub fn create_or_get(
        &self,
        request: PaymentRequest,
        created_at_unix_ms: i64,
    ) -> Result<StoredReceivable, StoreError> {
        self.verify_ledger_integrity()?;
        let url = request
            .solana_pay_url()
            .map_err(|_| StoreError::Unavailable)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StoreError::Unavailable)?;
        if let Some(existing) = find_in(&transaction, request.receivable_id.as_str())? {
            if same_terms(&existing, &request) {
                transaction.commit().map_err(|_| StoreError::Unavailable)?;
                return Ok(existing);
            }
            return Err(StoreError::IdempotencyConflict);
        }
        let previous: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT event_hash FROM receivable_events ORDER BY sequence DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        let canonical = canonical_creation_event(&request, created_at_unix_ms, previous.as_deref());
        let event_hash = Sha256::digest(&canonical).to_vec();
        let inserted = transaction.execute(
            "INSERT INTO receivables (receivable_id,recipient,mint,atomic_amount,decimals,reference,public_label,state,created_at_unix_ms,solana_pay_url) VALUES (?1,?2,?3,?4,?5,?6,?7,'open',?8,?9)",
            params![request.receivable_id.as_str(), request.recipient.as_str(), request.mint.as_str(), i64::try_from(request.amount.get()).map_err(|_| StoreError::Unavailable)?, i64::from(request.decimals), request.reference.as_base58(), request.public_label.as_str(), created_at_unix_ms, url],
        );
        match inserted {
            Ok(_) => {}
            Err(error) if error.to_string().contains("receivables.reference") => {
                return Err(StoreError::ReferenceReuse);
            }
            Err(_) => return Err(StoreError::Unavailable),
        }
        transaction.execute(
            "INSERT INTO receivable_events (receivable_id,event_schema_version,event_domain,previous_event_hash,canonical_event_bytes,event_hash) VALUES (?1,?2,?3,?4,?5,?6)",
            params![request.receivable_id.as_str(), EVENT_SCHEMA_VERSION, EVENT_DOMAIN, previous, canonical, event_hash],
        ).map_err(|_| StoreError::Unavailable)?;
        append_ledger_checkpoint(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)?;
        Ok(StoredReceivable {
            request,
            state: ReceivableState::Open,
            created_at_unix_ms,
            solana_pay_url: url,
        })
    }

    /// # Errors
    ///
    /// Returns a redacted error if no durable record exists or storage fails.
    pub fn get(
        &self,
        receivable_id: &ReceivableId,
    ) -> Result<Option<StoredReceivable>, StoreError> {
        self.verify_ledger_integrity()?;
        find_in(&self.connection()?, receivable_id.as_str())
    }

    /// Returns open receivables in deterministic creation order.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage or integrity error.
    pub fn list_open(&self, limit: usize) -> Result<Vec<StoredReceivable>, StoreError> {
        self.verify_ledger_integrity()?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT receivable_id FROM receivables WHERE state = 'open'
                 ORDER BY created_at_unix_ms, receivable_id LIMIT ?1",
            )
            .map_err(|_| StoreError::Unavailable)?;
        let ids = statement
            .query_map(
                [i64::try_from(limit).map_err(|_| StoreError::Unavailable)?],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| StoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Unavailable)?;
        ids.into_iter()
            .map(|id| find_in(&connection, &id)?.ok_or(StoreError::Integrity))
            .collect()
    }

    /// Returns open receivables created at or after the supplied timestamp.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage or integrity error.
    pub fn list_open_since(
        &self,
        created_at_or_after_unix_ms: i64,
        limit: usize,
    ) -> Result<Vec<StoredReceivable>, StoreError> {
        self.verify_ledger_integrity()?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT receivable_id FROM receivables
                 WHERE state = 'open' AND created_at_unix_ms >= ?1
                 ORDER BY created_at_unix_ms, receivable_id LIMIT ?2",
            )
            .map_err(|_| StoreError::Unavailable)?;
        let ids = statement
            .query_map(
                params![
                    created_at_or_after_unix_ms,
                    i64::try_from(limit).map_err(|_| StoreError::Unavailable)?
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| StoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Unavailable)?;
        ids.into_iter()
            .map(|id| find_in(&connection, &id)?.ok_or(StoreError::Integrity))
            .collect()
    }

    /// Atomically records exact settlement evidence and consumes both the
    /// signature and reference.
    ///
    /// # Errors
    ///
    /// Fails closed on replay, stale state, or storage failure.
    pub fn mark_payment_verified(
        &self,
        receivable_id: &ReceivableId,
        reference: &Reference,
        evidence: &SettlementEvidence,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        self.verify_ledger_integrity()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StoreError::Unavailable)?;
        let state: Option<String> = transaction
            .query_row(
                "SELECT state FROM receivables WHERE receivable_id = ?1",
                [receivable_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        match state.as_deref() {
            Some("payment_verified") => {
                let existing: Option<String> = transaction
                    .query_row(
                        "SELECT fingerprint FROM settlements WHERE receivable_id = ?1",
                        [receivable_id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|_| StoreError::Unavailable)?;
                return if existing.as_deref() == Some(&evidence.fingerprint) {
                    transaction.commit().map_err(|_| StoreError::Unavailable)
                } else {
                    Err(StoreError::InvalidTransition)
                };
            }
            Some("open") => {}
            Some(_) => return Err(StoreError::InvalidTransition),
            None => return Err(StoreError::Integrity),
        }
        let inserted = transaction.execute(
            "INSERT INTO settlements (
                receivable_id,signature,reference,slot,block_time_unix,recipient,mint,
                atomic_amount,instruction_position,fingerprint,observed_at_unix_ms,
                settlement_kind,expected_atomic_amount,variance_reason,approval_run_id
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'exact',?12,NULL,NULL)",
            params![
                receivable_id.as_str(),
                evidence.signature,
                reference.as_base58(),
                i64::try_from(evidence.slot).map_err(|_| StoreError::Unavailable)?,
                evidence.block_time_unix,
                evidence.recipient.as_str(),
                evidence.mint.as_str(),
                i64::try_from(evidence.amount.get()).map_err(|_| StoreError::Unavailable)?,
                i64::try_from(evidence.transfer_instruction_position)
                    .map_err(|_| StoreError::Unavailable)?,
                evidence.fingerprint,
                observed_at_unix_ms,
                i64::try_from(evidence.amount.get()).map_err(|_| StoreError::Unavailable)?
            ],
        );
        if inserted.is_err() {
            return Err(StoreError::Replay);
        }
        transaction
            .execute(
                "UPDATE receivables SET state = 'payment_verified' WHERE receivable_id = ?1 AND state = 'open'",
                [receivable_id.as_str()],
            )
            .map_err(|_| StoreError::Unavailable)?;
        append_event(
            &transaction,
            receivable_id,
            format!(
                "event=payment_verified\nsignature={}\nslot={}\nfingerprint={}\nobserved_at_unix_ms={observed_at_unix_ms}\n",
                evidence.signature, evidence.slot, evidence.fingerprint
            )
            .as_bytes(),
        )?;
        transaction
            .execute(
                "INSERT INTO terminal_notifications (
                    receivable_id,status,evidence_fingerprint,created_at_unix_ms
                 ) VALUES (?1,'payment_verified',?2,?3)",
                params![
                    receivable_id.as_str(),
                    evidence.fingerprint,
                    observed_at_unix_ms
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        append_ledger_checkpoint(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)
    }

    /// Atomically records a bounded mismatch without treating it as payment.
    ///
    /// # Errors
    ///
    /// Fails closed on stale state or storage failure.
    pub fn mark_needs_review(
        &self,
        receivable_id: &ReceivableId,
        signature: &str,
        slot: u64,
        verdict: &str,
        candidate_fingerprint: &str,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        self.mark_review_candidate(
            receivable_id,
            signature,
            slot,
            verdict,
            candidate_fingerprint,
            None,
            observed_at_unix_ms,
        )
    }

    /// Atomically records a canonical underpayment with transaction-derived
    /// expected, received, and shortfall amounts. It remains `needs_review`
    /// until a separate operator approval is durably applied.
    ///
    /// # Errors
    ///
    /// Fails closed on inconsistent evidence, stale state, or storage failure.
    pub fn mark_underpayment_review(
        &self,
        receivable_id: &ReceivableId,
        evidence: &UnderpaymentEvidence,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        if evidence.expected_amount <= evidence.received_amount
            || evidence.received_amount.get() == 0
            || evidence.shortfall_amount.get()
                != evidence.expected_amount.get() - evidence.received_amount.get()
        {
            return Err(StoreError::Integrity);
        }
        self.mark_review_candidate(
            receivable_id,
            &evidence.signature,
            evidence.slot,
            "wrong_amount",
            &evidence.fingerprint,
            Some(evidence),
            observed_at_unix_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn mark_review_candidate(
        &self,
        receivable_id: &ReceivableId,
        signature: &str,
        slot: u64,
        verdict: &str,
        candidate_fingerprint: &str,
        underpayment: Option<&UnderpaymentEvidence>,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        self.verify_ledger_integrity()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StoreError::Unavailable)?;
        let changed = transaction
            .execute(
                "UPDATE receivables SET state = 'needs_review' WHERE receivable_id = ?1 AND state = 'open'",
                [receivable_id.as_str()],
            )
            .map_err(|_| StoreError::Unavailable)?;
        if changed != 1 {
            return Err(StoreError::InvalidTransition);
        }
        transaction
            .execute(
                "INSERT INTO review_candidates (
                    receivable_id,signature,slot,verdict,candidate_fingerprint,
                    observed_at_unix_ms,block_time_unix,recipient,mint,
                    expected_atomic_amount,received_atomic_amount,
                    shortfall_atomic_amount,instruction_position
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    receivable_id.as_str(),
                    signature,
                    i64::try_from(slot).map_err(|_| StoreError::Unavailable)?,
                    verdict,
                    candidate_fingerprint,
                    observed_at_unix_ms,
                    underpayment.and_then(|evidence| evidence.block_time_unix),
                    underpayment.map(|evidence| evidence.recipient.as_str()),
                    underpayment.map(|evidence| evidence.mint.as_str()),
                    underpayment
                        .map(|evidence| i64::try_from(evidence.expected_amount.get()))
                        .transpose()
                        .map_err(|_| StoreError::Unavailable)?,
                    underpayment
                        .map(|evidence| i64::try_from(evidence.received_amount.get()))
                        .transpose()
                        .map_err(|_| StoreError::Unavailable)?,
                    underpayment
                        .map(|evidence| i64::try_from(evidence.shortfall_amount.get()))
                        .transpose()
                        .map_err(|_| StoreError::Unavailable)?,
                    underpayment
                        .map(|evidence| i64::try_from(evidence.transfer_instruction_position))
                        .transpose()
                        .map_err(|_| StoreError::Unavailable)?
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        append_event(
            &transaction,
            receivable_id,
            format!(
                "event=needs_review\nsignature={signature}\nslot={slot}\nverdict={verdict}\ncandidate_fingerprint={candidate_fingerprint}\nexpected_atomic_amount={}\nreceived_atomic_amount={}\nshortfall_atomic_amount={}\nobserved_at_unix_ms={observed_at_unix_ms}\n",
                underpayment.map(|evidence| evidence.expected_amount.get()).map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
                underpayment.map(|evidence| evidence.received_amount.get()).map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
                underpayment.map(|evidence| evidence.shortfall_amount.get()).map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
            )
            .as_bytes(),
        )?;
        transaction
            .execute(
                "INSERT INTO terminal_notifications (
                    receivable_id,status,evidence_fingerprint,created_at_unix_ms
                 ) VALUES (?1,'needs_review',?2,?3)",
                params![
                    receivable_id.as_str(),
                    candidate_fingerprint,
                    observed_at_unix_ms
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        append_ledger_checkpoint(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)
    }

    /// Acquires the singleton reconciliation lease, replacing only an expired
    /// lease.
    ///
    /// # Errors
    ///
    /// Returns `ReconciliationBusy` when another live run owns the lease.
    pub fn acquire_reconciliation_lease(
        &self,
        owner: &str,
        now_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        let changed = connection
            .execute(
                "INSERT INTO reconciliation_lease(singleton,owner,expires_at_unix_ms)
                 VALUES (1,?1,?2)
                 ON CONFLICT(singleton) DO UPDATE SET owner=excluded.owner,
                 expires_at_unix_ms=excluded.expires_at_unix_ms
                 WHERE reconciliation_lease.expires_at_unix_ms <= ?3",
                params![owner, expires_at_unix_ms, now_unix_ms],
            )
            .map_err(|_| StoreError::Unavailable)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::ReconciliationBusy)
        }
    }

    /// Releases only the caller's reconciliation lease.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error.
    pub fn release_reconciliation_lease(&self, owner: &str) -> Result<(), StoreError> {
        self.connection()?
            .execute(
                "DELETE FROM reconciliation_lease WHERE singleton = 1 AND owner = ?1",
                [owner],
            )
            .map_err(|_| StoreError::Unavailable)?;
        Ok(())
    }

    /// Acquires a per-month export lease, replacing only an expired lease.
    ///
    /// # Errors
    ///
    /// Returns `MonthlyExportBusy` while another process owns the month.
    pub fn acquire_monthly_export_lease(
        &self,
        month: &str,
        owner: &str,
        now_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let changed = self
            .connection()?
            .execute(
                "INSERT INTO monthly_export_leases(month,owner,expires_at_unix_ms)
                 VALUES (?1,?2,?3)
                 ON CONFLICT(month) DO UPDATE SET owner=excluded.owner,
                 expires_at_unix_ms=excluded.expires_at_unix_ms
                 WHERE monthly_export_leases.expires_at_unix_ms <= ?4",
                params![month, owner, expires_at_unix_ms, now_unix_ms],
            )
            .map_err(|_| StoreError::Unavailable)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::MonthlyExportBusy)
        }
    }

    /// Releases only the caller's per-month export lease.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error.
    pub fn release_monthly_export_lease(&self, month: &str, owner: &str) -> Result<(), StoreError> {
        self.connection()?
            .execute(
                "DELETE FROM monthly_export_leases WHERE month=?1 AND owner=?2",
                params![month, owner],
            )
            .map_err(|_| StoreError::Unavailable)?;
        Ok(())
    }

    /// Returns whether a signature or reference already has durable settlement
    /// evidence.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error.
    pub fn replay_state(
        &self,
        signature: &str,
        reference: &Reference,
    ) -> Result<(bool, bool), StoreError> {
        self.verify_ledger_integrity()?;
        let connection = self.connection()?;
        let signature_used = connection
            .query_row(
                "SELECT 1 FROM settlements WHERE signature = ?1",
                [signature],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?
            .is_some();
        let reference_used = connection
            .query_row(
                "SELECT 1 FROM settlements WHERE reference = ?1",
                [reference.as_base58()],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?
            .is_some();
        Ok((signature_used, reference_used))
    }

    /// Returns the durable signature for a verified receivable.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error.
    pub fn settlement_signature(
        &self,
        receivable_id: &ReceivableId,
    ) -> Result<Option<String>, StoreError> {
        self.verify_ledger_integrity()?;
        self.connection()?
            .query_row(
                "SELECT signature FROM settlements WHERE receivable_id = ?1",
                [receivable_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)
    }

    /// Returns the compact immutable settlement summary for operator status.
    ///
    /// # Errors
    ///
    /// Malformed evidence, integrity failure, or storage failure fails closed.
    pub fn settlement_summary(
        &self,
        receivable_id: &ReceivableId,
    ) -> Result<Option<StoredSettlementSummary>, StoreError> {
        self.verify_ledger_integrity()?;
        let raw = self
            .connection()?
            .query_row(
                "SELECT signature,settlement_kind,expected_atomic_amount,
                        atomic_amount,variance_reason,approval_run_id
                 FROM settlements WHERE receivable_id=?1",
                [receivable_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        raw.map(
            |(signature, kind, expected, received, reason, approval_run_id)| {
                let variance_reason = reason
                    .map(|value| parse_variance_reason(&value))
                    .transpose()?;
                if (kind == "accepted_underpayment") != variance_reason.is_some()
                    || (kind != "exact" && kind != "accepted_underpayment")
                {
                    return Err(StoreError::Integrity);
                }
                Ok(StoredSettlementSummary {
                    signature,
                    settlement_kind: kind,
                    expected_amount: AtomicAmount::new(
                        u64::try_from(expected).map_err(|_| StoreError::Integrity)?,
                    ),
                    received_amount: AtomicAmount::new(
                        u64::try_from(received).map_err(|_| StoreError::Integrity)?,
                    ),
                    variance_reason,
                    approval_run_id,
                })
            },
        )
        .transpose()
    }

    /// Returns terminal settlement/review notifications that have not yet
    /// received a durable delivery receipt.
    ///
    /// # Errors
    ///
    /// Malformed identifiers or storage failures fail closed.
    pub fn pending_terminal_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<StoredTerminalNotification>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.verify_ledger_integrity()?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT n.id,n.receivable_id,n.status
                 FROM terminal_notifications n
                 LEFT JOIN terminal_notification_deliveries d
                   ON d.notification_id=n.id
                 WHERE d.notification_id IS NULL
                 ORDER BY n.id LIMIT ?1",
            )
            .map_err(|_| StoreError::Unavailable)?;
        let rows = statement
            .query_map(
                [i64::try_from(limit).map_err(|_| StoreError::Unavailable)?],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(|_| StoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Unavailable)?;
        rows.into_iter()
            .map(|(id, receivable_id, status)| {
                if status != "payment_verified" && status != "needs_review" {
                    return Err(StoreError::Integrity);
                }
                Ok(StoredTerminalNotification {
                    id: u64::try_from(id).map_err(|_| StoreError::Integrity)?,
                    receivable_id: ReceivableId::new(receivable_id)
                        .map_err(|_| StoreError::Integrity)?,
                    status,
                })
            })
            .collect()
    }

    /// Appends a durable receipt after the external notification transport
    /// confirms delivery. Exact receipt retries are idempotent.
    ///
    /// # Errors
    ///
    /// Unknown notifications, conflicting receipts, or storage failures fail
    /// closed.
    pub fn mark_terminal_notification_delivered(
        &self,
        notification_id: u64,
        delivered_at_unix_ms: i64,
        delivery_receipt: &str,
    ) -> Result<(), StoreError> {
        if delivery_receipt.is_empty() || delivery_receipt.len() > 128 {
            return Err(StoreError::Integrity);
        }
        let connection = self.connection()?;
        let id = i64::try_from(notification_id).map_err(|_| StoreError::Integrity)?;
        let exists = connection
            .query_row(
                "SELECT 1 FROM terminal_notifications WHERE id=?1",
                [id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?
            .is_some();
        if !exists {
            return Err(StoreError::Integrity);
        }
        let existing = connection
            .query_row(
                "SELECT delivery_receipt FROM terminal_notification_deliveries
                 WHERE notification_id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        if let Some(existing) = existing {
            return if existing == delivery_receipt {
                Ok(())
            } else {
                Err(StoreError::Integrity)
            };
        }
        connection
            .execute(
                "INSERT INTO terminal_notification_deliveries (
                    notification_id,delivered_at_unix_ms,delivery_receipt
                 ) VALUES (?1,?2,?3)",
                params![id, delivered_at_unix_ms, delivery_receipt],
            )
            .map_err(|_| StoreError::Unavailable)?;
        Ok(())
    }

    /// Returns immutable official PTAX evidence when it has been attached by
    /// a monthly snapshot or final close.
    ///
    /// # Errors
    ///
    /// Malformed evidence or storage failures fail closed.
    pub fn valuation(
        &self,
        receivable_id: &ReceivableId,
    ) -> Result<Option<StoredValuation>, StoreError> {
        self.verify_ledger_integrity()?;
        find_valuation_in(&self.connection()?, receivable_id.as_str())
    }

    /// Returns the bounded candidate summary for a receivable in review.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error.
    pub fn review_candidate(
        &self,
        receivable_id: &ReceivableId,
    ) -> Result<Option<StoredReviewCandidate>, StoreError> {
        self.verify_ledger_integrity()?;
        self.connection()?
            .query_row(
                "SELECT c.signature,c.slot,c.verdict,c.candidate_fingerprint,
                        c.block_time_unix,c.recipient,c.mint,
                        c.expected_atomic_amount,c.received_atomic_amount,
                        c.shortfall_atomic_amount,c.instruction_position
                 FROM review_candidates c
                 LEFT JOIN review_resolutions r USING(candidate_fingerprint)
                 WHERE c.receivable_id = ?1 AND r.candidate_fingerprint IS NULL
                 ORDER BY c.sequence DESC LIMIT 1",
                [receivable_id.as_str()],
                |row| {
                    Ok(StoredReviewCandidate {
                        signature: row.get(0)?,
                        slot: u64::try_from(row.get::<_, i64>(1)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        verdict: row.get(2)?,
                        candidate_fingerprint: row.get(3)?,
                        underpayment: parse_stored_underpayment(row)?,
                    })
                },
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)
    }

    /// Returns the immutable candidate fingerprints already dispositioned for
    /// one receivable.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage or integrity error.
    pub fn resolved_review_fingerprints(
        &self,
        receivable_id: &ReceivableId,
    ) -> Result<Vec<String>, StoreError> {
        self.verify_ledger_integrity()?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT candidate_fingerprint FROM review_resolutions
                 WHERE receivable_id=?1 ORDER BY candidate_fingerprint",
            )
            .map_err(|_| StoreError::Unavailable)?;
        statement
            .query_map([receivable_id.as_str()], |row| row.get(0))
            .map_err(|_| StoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Unavailable)
    }

    /// Records one approved unpaid-candidate disposition atomically.
    ///
    /// # Errors
    ///
    /// A stale fingerprint, unsupported state, conflicting retry, integrity
    /// failure, or storage failure fails closed.
    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    // Keeping the reads, invariant checks, writes, event, and checkpoint in one
    // immediate transaction makes this security boundary auditable as a unit.
    pub fn resolve_review(
        &self,
        receivable_id: &ReceivableId,
        candidate_fingerprint: &str,
        action: ReviewResolutionAction,
        variance_reason: Option<VarianceReason>,
        approval_run_id: &str,
        resolved_at_unix_ms: i64,
    ) -> Result<ReceivableState, StoreError> {
        if (action == ReviewResolutionAction::AcceptUnderpaymentWithVariance)
            != variance_reason.is_some()
        {
            return Err(StoreError::InvalidTransition);
        }
        self.verify_ledger_integrity()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StoreError::Unavailable)?;
        let existing: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT receivable_id,action,variance_reason FROM review_resolutions
                 WHERE candidate_fingerprint=?1",
                [candidate_fingerprint],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        let target_state = match action {
            ReviewResolutionAction::IgnoreCandidateAndReopen => ReceivableState::Open,
            ReviewResolutionAction::CancelUnpaid => ReceivableState::Cancelled,
            ReviewResolutionAction::AcceptUnderpaymentWithVariance => {
                ReceivableState::SettledWithVariance
            }
        };
        if let Some((existing_id, existing_action, existing_reason)) = existing {
            return if existing_id == receivable_id.as_str()
                && existing_action == action.as_str()
                && existing_reason.as_deref() == variance_reason.map(VarianceReason::as_str)
            {
                transaction.commit().map_err(|_| StoreError::Unavailable)?;
                Ok(target_state)
            } else {
                Err(StoreError::InvalidTransition)
            };
        }
        let current: Option<(
            String,
            String,
            String,
            i64,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            String,
            String,
            String,
            String,
            i64,
        )> = transaction
            .query_row(
                "SELECT r.state,c.candidate_fingerprint,c.signature,c.slot,
                        c.block_time_unix,c.recipient,c.mint,
                        c.expected_atomic_amount,c.received_atomic_amount,
                        c.shortfall_atomic_amount,c.instruction_position,
                        c.verdict,r.reference,r.recipient,r.mint,r.atomic_amount
                 FROM receivables r JOIN review_candidates c USING(receivable_id)
                 LEFT JOIN review_resolutions d USING(candidate_fingerprint)
                 WHERE r.receivable_id=?1 AND d.candidate_fingerprint IS NULL
                 ORDER BY c.sequence DESC LIMIT 1",
                [receivable_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        let Some((
            state,
            fingerprint,
            signature,
            slot,
            block_time,
            candidate_recipient,
            candidate_mint,
            expected,
            received,
            shortfall,
            instruction_position,
            candidate_verdict,
            reference,
            configured_recipient,
            configured_mint,
            configured_amount,
        )) = current
        else {
            return Err(StoreError::InvalidTransition);
        };
        if state != "needs_review" || fingerprint != candidate_fingerprint {
            return Err(StoreError::InvalidTransition);
        }
        transaction
            .execute(
                "INSERT INTO review_resolutions (
                    candidate_fingerprint,receivable_id,action,variance_reason,
                    approval_run_id,resolved_at_unix_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    candidate_fingerprint,
                    receivable_id.as_str(),
                    action.as_str(),
                    variance_reason.map(VarianceReason::as_str),
                    approval_run_id,
                    resolved_at_unix_ms
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        if action == ReviewResolutionAction::AcceptUnderpaymentWithVariance {
            let (
                Some(block_time),
                Some(candidate_recipient),
                Some(candidate_mint),
                Some(expected),
                Some(received),
                Some(shortfall),
                Some(instruction_position),
            ) = (
                block_time,
                candidate_recipient,
                candidate_mint,
                expected,
                received,
                shortfall,
                instruction_position,
            )
            else {
                return Err(StoreError::InvalidTransition);
            };
            let merchant_wallet =
                PublicKey::parse(configured_recipient).map_err(|_| StoreError::Integrity)?;
            let configured_mint_key =
                PublicKey::parse(configured_mint.clone()).map_err(|_| StoreError::Integrity)?;
            let expected_recipient = derive_classic_ata(&merchant_wallet, &configured_mint_key)
                .map_err(|_| StoreError::Integrity)?;
            if expected <= 0
                || received <= 0
                || received >= expected
                || shortfall != expected - received
                || candidate_verdict != "wrong_amount"
                || expected != configured_amount
                || candidate_recipient != expected_recipient.as_str()
                || candidate_mint != configured_mint
            {
                return Err(StoreError::InvalidTransition);
            }
            transaction
                .execute(
                    "INSERT INTO settlements (
                        receivable_id,signature,reference,slot,block_time_unix,
                        recipient,mint,atomic_amount,instruction_position,
                        fingerprint,observed_at_unix_ms,settlement_kind,
                        expected_atomic_amount,variance_reason,approval_run_id
                     ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,
                               'accepted_underpayment',?12,?13,?14)",
                    params![
                        receivable_id.as_str(),
                        signature,
                        reference,
                        slot,
                        block_time,
                        candidate_recipient,
                        candidate_mint,
                        received,
                        instruction_position,
                        candidate_fingerprint,
                        resolved_at_unix_ms,
                        expected,
                        variance_reason.map(VarianceReason::as_str),
                        approval_run_id
                    ],
                )
                .map_err(|_| StoreError::Replay)?;
        }
        let state = match target_state {
            ReceivableState::Open => "open",
            ReceivableState::Cancelled => "cancelled",
            ReceivableState::SettledWithVariance => "settled_with_variance",
            _ => return Err(StoreError::Integrity),
        };
        let changed = transaction
            .execute(
                "UPDATE receivables SET state=?1
                 WHERE receivable_id=?2 AND state='needs_review'",
                params![state, receivable_id.as_str()],
            )
            .map_err(|_| StoreError::Unavailable)?;
        if changed != 1 {
            return Err(StoreError::InvalidTransition);
        }
        append_event(
            &transaction,
            receivable_id,
            if action == ReviewResolutionAction::AcceptUnderpaymentWithVariance {
                format!(
                    "event=review_resolved\ncandidate_fingerprint={candidate_fingerprint}\naction={}\nexpected_atomic_amount={}\nreceived_atomic_amount={}\nshortfall_atomic_amount={}\nvariance_reason={}\napproval_run_id={approval_run_id}\nresolved_at_unix_ms={resolved_at_unix_ms}\n",
                    action.as_str(),
                    expected.ok_or(StoreError::Integrity)?,
                    received.ok_or(StoreError::Integrity)?,
                    shortfall.ok_or(StoreError::Integrity)?,
                    variance_reason.ok_or(StoreError::Integrity)?.as_str(),
                )
            } else {
                format!(
                    "event=review_resolved\ncandidate_fingerprint={candidate_fingerprint}\naction={}\napproval_run_id={approval_run_id}\nresolved_at_unix_ms={resolved_at_unix_ms}\n",
                    action.as_str(),
                )
            }
            .as_bytes(),
        )?;
        append_ledger_checkpoint(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)?;
        Ok(target_state)
    }

    /// Attaches immutable valuation evidence to an already verified payment.
    ///
    /// # Errors
    ///
    /// The exact same retry is idempotent. Conflicts, unverified payments, and
    /// storage failures fail closed.
    pub fn attach_valuation(
        &self,
        receivable_id: &ReceivableId,
        valuation: &StoredValuation,
    ) -> Result<(), StoreError> {
        self.verify_ledger_integrity()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StoreError::Unavailable)?;
        let state: Option<String> = transaction
            .query_row(
                "SELECT state FROM receivables WHERE receivable_id = ?1",
                [receivable_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        if !matches!(
            state.as_deref(),
            Some("payment_verified" | "settled_with_variance")
        ) {
            return Err(StoreError::InvalidTransition);
        }
        let existing: Option<(String, String, i64)> = transaction
            .query_row(
                "SELECT response_sha256,valuation_method,brl_reference_cents
                 FROM valuations WHERE receivable_id = ?1",
                [receivable_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        if let Some((hash, method, cents)) = existing {
            return if hash == valuation.evidence.response_sha256
                && method == NOMINAL_USDC_USD_METHOD
                && u64::try_from(cents).ok() == Some(valuation.brl_reference_cents)
            {
                transaction.commit().map_err(|_| StoreError::Unavailable)
            } else {
                Err(StoreError::InvalidTransition)
            };
        }
        transaction
            .execute(
                "INSERT INTO valuations (
                    receivable_id,operation_date,quote_date,purchase,sale,bulletin_type,
                    bulletin_timestamp,retrieved_at_unix_ms,response_sha256,source_id,
                    policy_version,valuation_method,brl_reference_cents
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    receivable_id.as_str(),
                    valuation.evidence.operation_date.as_str(),
                    valuation.evidence.quote_date.as_str(),
                    valuation.evidence.purchase,
                    valuation.evidence.sale,
                    valuation.evidence.bulletin_type,
                    valuation.evidence.bulletin_timestamp,
                    valuation.evidence.retrieved_at_unix_ms,
                    valuation.evidence.response_sha256,
                    valuation.evidence.source_id,
                    valuation.evidence.policy_version,
                    NOMINAL_USDC_USD_METHOD,
                    i64::try_from(valuation.brl_reference_cents)
                        .map_err(|_| StoreError::Unavailable)?
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        append_event(
            &transaction,
            receivable_id,
            format!(
                "event=valuation_attached\noperation_date={}\nquote_date={}\nresponse_sha256={}\nbrl_reference_cents={}\n",
                valuation.evidence.operation_date.as_str(),
                valuation.evidence.quote_date.as_str(),
                valuation.evidence.response_sha256,
                valuation.brl_reference_cents
            )
            .as_bytes(),
        )?;
        append_ledger_checkpoint(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)
    }

    /// Returns verified settlements in a UTC half-open interval.
    ///
    /// # Errors
    ///
    /// Missing block time, malformed durable evidence, row overflow, or
    /// storage failure fails closed.
    pub fn list_settled_between(
        &self,
        start_unix: i64,
        end_unix: i64,
    ) -> Result<Vec<StoredSettledReceivable>, StoreError> {
        self.verify_ledger_integrity()?;
        let connection = self.connection()?;
        let invalid_verified_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*)
                 FROM receivables r LEFT JOIN settlements s USING(receivable_id)
                 WHERE r.state IN ('payment_verified','settled_with_variance')
                   AND (s.receivable_id IS NULL OR s.block_time_unix IS NULL)",
                [],
                |row| row.get(0),
            )
            .map_err(|_| StoreError::Unavailable)?;
        if invalid_verified_rows != 0 {
            return Err(StoreError::Integrity);
        }
        let mut statement = connection
            .prepare(
                "SELECT r.receivable_id,s.signature,s.slot,s.block_time_unix,
                        s.fingerprint,s.settlement_kind,s.expected_atomic_amount,
                        s.atomic_amount,s.variance_reason,s.approval_run_id
                 FROM receivables r JOIN settlements s USING(receivable_id)
                 WHERE s.block_time_unix >= ?1 AND s.block_time_unix < ?2
                 ORDER BY s.block_time_unix,r.receivable_id LIMIT ?3",
            )
            .map_err(|_| StoreError::Unavailable)?;
        let rows = statement
            .query_map(
                params![
                    start_unix,
                    end_unix,
                    i64::try_from(MAX_MONTH_EXPORT_ROWS + 1)
                        .map_err(|_| StoreError::Unavailable)?
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .map_err(|_| StoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Unavailable)?;
        if rows.len() > MAX_MONTH_EXPORT_ROWS {
            return Err(StoreError::Integrity);
        }
        rows.into_iter()
            .map(
                |(
                    id,
                    signature,
                    slot,
                    block_time,
                    fingerprint,
                    settlement_kind,
                    expected_amount,
                    received_amount,
                    variance_reason,
                    approval_run_id,
                )| {
                    let receivable = find_in(&connection, &id)?.ok_or(StoreError::Integrity)?;
                    let valuation = find_valuation_in(&connection, &id)?;
                    let variance_reason = variance_reason
                        .map(|reason| parse_variance_reason(&reason))
                        .transpose()?;
                    if (settlement_kind == "accepted_underpayment") != variance_reason.is_some()
                        || (settlement_kind != "exact"
                            && settlement_kind != "accepted_underpayment")
                    {
                        return Err(StoreError::Integrity);
                    }
                    Ok(StoredSettledReceivable {
                        receivable,
                        signature,
                        slot: u64::try_from(slot).map_err(|_| StoreError::Integrity)?,
                        block_time_unix: block_time.ok_or(StoreError::Integrity)?,
                        settlement_fingerprint: fingerprint,
                        settlement_kind,
                        expected_amount: AtomicAmount::new(
                            u64::try_from(expected_amount).map_err(|_| StoreError::Integrity)?,
                        ),
                        received_amount: AtomicAmount::new(
                            u64::try_from(received_amount).map_err(|_| StoreError::Integrity)?,
                        ),
                        variance_reason,
                        approval_run_id,
                        valuation,
                    })
                },
            )
            .collect()
    }

    /// Returns one settled receivable with its settlement and any valuation.
    ///
    /// # Errors
    ///
    /// Integrity or storage failures fail closed. An unsettled or unknown
    /// receivable returns `Ok(None)`.
    pub fn settled_receivable(
        &self,
        receivable_id: &ReceivableId,
    ) -> Result<Option<StoredSettledReceivable>, StoreError> {
        self.verify_ledger_integrity()?;
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT s.signature,s.slot,s.block_time_unix,s.fingerprint,
                        s.settlement_kind,s.expected_atomic_amount,s.atomic_amount,
                        s.variance_reason,s.approval_run_id
                 FROM receivables r JOIN settlements s USING(receivable_id)
                 WHERE r.receivable_id = ?1",
                params![receivable_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        let Some((
            signature,
            slot,
            block_time,
            fingerprint,
            settlement_kind,
            expected_amount,
            received_amount,
            variance_reason,
            approval_run_id,
        )) = row
        else {
            return Ok(None);
        };
        let receivable =
            find_in(&connection, receivable_id.as_str())?.ok_or(StoreError::Integrity)?;
        let valuation = find_valuation_in(&connection, receivable_id.as_str())?;
        let variance_reason = variance_reason
            .map(|reason| parse_variance_reason(&reason))
            .transpose()?;
        if (settlement_kind == "accepted_underpayment") != variance_reason.is_some()
            || (settlement_kind != "exact" && settlement_kind != "accepted_underpayment")
        {
            return Err(StoreError::Integrity);
        }
        Ok(Some(StoredSettledReceivable {
            receivable,
            signature,
            slot: u64::try_from(slot).map_err(|_| StoreError::Integrity)?,
            block_time_unix: block_time.ok_or(StoreError::Integrity)?,
            settlement_fingerprint: fingerprint,
            settlement_kind,
            expected_amount: AtomicAmount::new(
                u64::try_from(expected_amount).map_err(|_| StoreError::Integrity)?,
            ),
            received_amount: AtomicAmount::new(
                u64::try_from(received_amount).map_err(|_| StoreError::Integrity)?,
            ),
            variance_reason,
            approval_run_id,
            valuation,
        }))
    }

    /// Persists immutable deterministic close revisions for a month.
    ///
    /// # Errors
    ///
    /// Same-byte retries are idempotent. A changed snapshot appends a revision,
    /// preserving all earlier close bytes.
    pub fn record_month_close(
        &self,
        close: StoredMonthClose,
        expected_ledger_root: &[u8],
    ) -> Result<StoredMonthClose, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StoreError::Unavailable)?;
        verify_ledger_integrity_in(&transaction)?;
        if ledger_root(&transaction)? != expected_ledger_root {
            return Err(StoreError::ConcurrentMutation);
        }
        if let Some(existing) = find_month_close_in(&transaction, &close.month)?
            && same_close_artifacts(&existing, &close)
        {
            transaction.commit().map_err(|_| StoreError::Unavailable)?;
            return Ok(existing);
        }
        let hash = month_close_hash(&close);
        let next_revision: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(revision),0) + 1 FROM month_close_revisions
                 WHERE month = ?1",
                [&close.month],
                |row| row.get(0),
            )
            .map_err(|_| StoreError::Unavailable)?;
        transaction
            .execute(
                "INSERT INTO month_close_revisions (
                    month,revision,artifact_kind,canonical_json,accountant_csv,manifest_json,close_hash
                ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    &close.month,
                    next_revision,
                    &close.artifact_kind,
                    &close.canonical_json,
                    &close.accountant_csv,
                    &close.manifest_json,
                    hash
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        append_ledger_checkpoint(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)?;
        Ok(StoredMonthClose {
            revision: u32::try_from(next_revision).map_err(|_| StoreError::Integrity)?,
            ..close
        })
    }

    /// # Errors
    ///
    /// Returns a redacted error when the event hash chain is invalid or cannot
    /// be read from local storage.
    pub fn verify_event_chain(&self) -> Result<(), StoreError> {
        let connection = self.connection()?;
        verify_event_chain_in(&connection)
    }

    /// Returns the verified material-ledger root from one `SQLite` read snapshot.
    ///
    /// # Errors
    ///
    /// Integrity or storage failures fail closed.
    pub fn ledger_fingerprint(&self) -> Result<Vec<u8>, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|_| StoreError::Unavailable)?;
        verify_ledger_integrity_in(&transaction)?;
        let root = ledger_root(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)?;
        Ok(root)
    }

    /// Verifies the event chain, material-table root, and checkpoint chain.
    ///
    /// # Errors
    ///
    /// Any missing, malformed, or mismatched checkpoint fails closed.
    pub fn verify_ledger_integrity(&self) -> Result<(), StoreError> {
        // The event chain, material tables, and checkpoint chain must be read
        // from one snapshot. Separate statements on a bare connection can
        // straddle another writer's commit and report a torn view as an
        // integrity failure.
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|_| StoreError::Unavailable)?;
        verify_ledger_integrity_in(&transaction)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)
    }
}

fn verify_event_chain_in(connection: &Connection) -> Result<(), StoreError> {
    let mut statement = connection.prepare("SELECT previous_event_hash, canonical_event_bytes, event_hash FROM receivable_events ORDER BY sequence").map_err(|_| StoreError::Unavailable)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|_| StoreError::Unavailable)?;
    let mut expected_previous: Option<Vec<u8>> = None;
    for row in rows {
        let (previous, canonical, event_hash) = row.map_err(|_| StoreError::Unavailable)?;
        if previous != expected_previous
            || Sha256::digest(&canonical).as_slice() != event_hash.as_slice()
        {
            return Err(StoreError::Integrity);
        }
        expected_previous = Some(event_hash);
    }
    Ok(())
}

fn verify_ledger_integrity_in(connection: &Connection) -> Result<(), StoreError> {
    verify_event_chain_in(connection)?;
    verify_ledger_checkpoints(connection)
}

fn find_valuation_in(
    connection: &Connection,
    id: &str,
) -> Result<Option<StoredValuation>, StoreError> {
    let raw = connection
        .query_row(
            "SELECT operation_date,quote_date,purchase,sale,bulletin_type,
                    bulletin_timestamp,retrieved_at_unix_ms,response_sha256,source_id,
                    policy_version,valuation_method,brl_reference_cents
             FROM valuations WHERE receivable_id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            },
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?;
    raw.map(
        |(
            operation_date,
            quote_date,
            purchase,
            sale,
            bulletin_type,
            bulletin_timestamp,
            retrieved_at,
            response_sha256,
            source_id,
            policy_version,
            method,
            cents,
        )| {
            if method != NOMINAL_USDC_USD_METHOD {
                return Err(StoreError::Integrity);
            }
            Ok(StoredValuation {
                evidence: PtaxEvidence {
                    operation_date: PtaxDate::parse(operation_date)
                        .map_err(|_| StoreError::Integrity)?,
                    quote_date: PtaxDate::parse(quote_date).map_err(|_| StoreError::Integrity)?,
                    purchase,
                    sale,
                    bulletin_type,
                    bulletin_timestamp,
                    retrieved_at_unix_ms: retrieved_at,
                    response_sha256,
                    source_id,
                    policy_version,
                },
                brl_reference_cents: u64::try_from(cents).map_err(|_| StoreError::Integrity)?,
            })
        },
    )
    .transpose()
}

fn parse_stored_underpayment(
    row: &rusqlite::Row<'_>,
) -> Result<Option<StoredUnderpaymentEvidence>, rusqlite::Error> {
    let values = (
        row.get::<_, Option<i64>>(4)?,
        row.get::<_, Option<String>>(5)?,
        row.get::<_, Option<String>>(6)?,
        row.get::<_, Option<i64>>(7)?,
        row.get::<_, Option<i64>>(8)?,
        row.get::<_, Option<i64>>(9)?,
        row.get::<_, Option<i64>>(10)?,
    );
    match values {
        (
            Some(block_time),
            Some(recipient),
            Some(mint),
            Some(expected),
            Some(received),
            Some(shortfall),
            Some(position),
        ) => Ok(Some(StoredUnderpaymentEvidence {
            block_time_unix: block_time,
            recipient: PublicKey::parse(recipient).map_err(|_| rusqlite::Error::InvalidQuery)?,
            mint: PublicKey::parse(mint).map_err(|_| rusqlite::Error::InvalidQuery)?,
            expected_amount: AtomicAmount::new(
                u64::try_from(expected).map_err(|_| rusqlite::Error::InvalidQuery)?,
            ),
            received_amount: AtomicAmount::new(
                u64::try_from(received).map_err(|_| rusqlite::Error::InvalidQuery)?,
            ),
            shortfall_amount: AtomicAmount::new(
                u64::try_from(shortfall).map_err(|_| rusqlite::Error::InvalidQuery)?,
            ),
            instruction_position: usize::try_from(position)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
        })),
        (None, None, None, None, None, None, None) => Ok(None),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn parse_variance_reason(value: &str) -> Result<VarianceReason, StoreError> {
    match value {
        "rounding_adjustment" => Ok(VarianceReason::RoundingAdjustment),
        "commercial_discount" => Ok(VarianceReason::CommercialDiscount),
        "merchant_write_off" => Ok(VarianceReason::MerchantWriteOff),
        _ => Err(StoreError::Integrity),
    }
}

fn find_month_close_in(
    connection: &Connection,
    month: &str,
) -> Result<Option<StoredMonthClose>, StoreError> {
    let raw = connection
        .query_row(
            "SELECT revision,artifact_kind,canonical_json,accountant_csv,manifest_json,close_hash
             FROM month_close_revisions WHERE month = ?1
             ORDER BY revision DESC LIMIT 1",
            [month],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            },
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?;
    raw.map(
        |(revision, artifact_kind, canonical_json, accountant_csv, manifest_json, hash)| {
            let close = StoredMonthClose {
                month: month.to_owned(),
                revision: u32::try_from(revision).map_err(|_| StoreError::Integrity)?,
                artifact_kind,
                canonical_json,
                accountant_csv,
                manifest_json,
            };
            if hash == month_close_hash(&close) {
                Ok(close)
            } else {
                Err(StoreError::Integrity)
            }
        },
    )
    .transpose()
}

fn same_close_artifacts(left: &StoredMonthClose, right: &StoredMonthClose) -> bool {
    left.month == right.month
        && left.artifact_kind == right.artifact_kind
        && left.canonical_json == right.canonical_json
        && left.accountant_csv == right.accountant_csv
        && left.manifest_json == right.manifest_json
}

fn month_close_hash(close: &StoredMonthClose) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"recebi.month_close.v2");
    hash_field(&mut hasher, close.month.as_bytes());
    hash_field(&mut hasher, close.artifact_kind.as_bytes());
    hash_field(&mut hasher, &close.canonical_json);
    hash_field(&mut hasher, &close.accountant_csv);
    hash_field(&mut hasher, &close.manifest_json);
    hasher.finalize().to_vec()
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(
        u64::try_from(value.len())
            .expect("slice length always fits in u64 on supported targets")
            .to_be_bytes(),
    );
    hasher.update(value);
}

fn migrate_phase_five_schema(connection: &Connection) -> Result<(), StoreError> {
    let installed = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version=6",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?
        .is_some();
    if installed {
        return Ok(());
    }
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             DROP TRIGGER IF EXISTS valuations_no_update;
             DROP TRIGGER IF EXISTS valuations_no_delete;
             DROP TRIGGER IF EXISTS month_close_revisions_no_update;
             DROP TRIGGER IF EXISTS month_close_revisions_no_delete;",
        )
        .map_err(|_| StoreError::Unavailable)?;
    if column_exists(connection, "valuations", "valuation_method_json")? {
        connection
            .execute_batch(
                "ALTER TABLE valuations RENAME COLUMN valuation_method_json TO valuation_method;",
            )
            .map_err(|_| StoreError::Unavailable)?;
    }
    connection
        .execute(
            "UPDATE valuations SET valuation_method=?1
             WHERE valuation_method=?2 OR valuation_method=?3",
            params![
                NOMINAL_USDC_USD_METHOD,
                r#"{"kind":"nominal_usdc_equals_usd"}"#,
                r#""nominal_usdc_equals_usd""#
            ],
        )
        .map_err(|_| StoreError::Unavailable)?;
    if !column_exists(connection, "month_close_revisions", "artifact_kind")? {
        connection
            .execute_batch(
                "ALTER TABLE month_close_revisions
                 ADD COLUMN artifact_kind TEXT NOT NULL DEFAULT 'legacy_final_close';",
            )
            .map_err(|_| StoreError::Unavailable)?;
    }
    if table_exists(connection, "month_closes")? {
        connection
            .execute_batch(
                "INSERT OR IGNORE INTO month_close_revisions (
                    month,revision,artifact_kind,canonical_json,accountant_csv,
                    manifest_json,close_hash
                 )
                 SELECT month,1,'legacy_final_close',canonical_json,accountant_csv,
                        manifest_json,close_hash
                 FROM month_closes;
                 DROP TRIGGER IF EXISTS month_closes_no_update;
                 DROP TRIGGER IF EXISTS month_closes_no_delete;
                 DROP TABLE month_closes;",
            )
            .map_err(|_| StoreError::Unavailable)?;
    }
    refresh_month_close_hashes(connection)?;
    connection
        .execute_batch(
            "CREATE TRIGGER IF NOT EXISTS valuations_no_update BEFORE UPDATE ON valuations BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS valuations_no_delete BEFORE DELETE ON valuations BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_close_revisions_no_update BEFORE UPDATE ON month_close_revisions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_close_revisions_no_delete BEFORE DELETE ON month_close_revisions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             INSERT INTO schema_migrations(version) VALUES (6);
             COMMIT;",
        )
        .map_err(|_| StoreError::Unavailable)
}

fn refresh_month_close_hashes(connection: &Connection) -> Result<(), StoreError> {
    let rows = {
        let mut statement = connection
            .prepare(
                "SELECT month,revision,artifact_kind,canonical_json,accountant_csv,manifest_json
                 FROM month_close_revisions ORDER BY month,revision",
            )
            .map_err(|_| StoreError::Unavailable)?;
        statement
            .query_map([], |row| {
                Ok(StoredMonthClose {
                    month: row.get(0)?,
                    revision: u32::try_from(row.get::<_, i64>(1)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    artifact_kind: row.get(2)?,
                    canonical_json: row.get(3)?,
                    accountant_csv: row.get(4)?,
                    manifest_json: row.get(5)?,
                })
            })
            .map_err(|_| StoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Unavailable)?
    };
    for close in rows {
        connection
            .execute(
                "UPDATE month_close_revisions SET close_hash=?1
                 WHERE month=?2 AND revision=?3",
                params![
                    month_close_hash(&close),
                    close.month,
                    i64::from(close.revision)
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(|_| StoreError::Unavailable)
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| StoreError::Unavailable)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| StoreError::Unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Unavailable)?;
    Ok(names.iter().any(|name| name == column))
}

fn migrate_phase_six_schema(connection: &Connection) -> Result<(), StoreError> {
    let installed = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version=7",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?
        .is_some();
    if installed {
        return Ok(());
    }
    connection
        .execute_batch("BEGIN IMMEDIATE;")
        .map_err(|_| StoreError::Unavailable)?;
    if !column_exists(connection, "review_candidates", "sequence")? {
        connection
            .execute_batch(
                "ALTER TABLE review_candidates RENAME TO review_candidates_phase5;
                 CREATE TABLE review_candidates (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    receivable_id TEXT NOT NULL,
                    signature TEXT NOT NULL,
                    slot INTEGER NOT NULL,
                    verdict TEXT NOT NULL,
                    candidate_fingerprint TEXT NOT NULL UNIQUE,
                    observed_at_unix_ms INTEGER NOT NULL
                 );
                 INSERT INTO review_candidates (
                    receivable_id,signature,slot,verdict,candidate_fingerprint,
                    observed_at_unix_ms
                 )
                 SELECT receivable_id,signature,slot,verdict,candidate_fingerprint,
                        observed_at_unix_ms
                 FROM review_candidates_phase5;
                 DROP TABLE review_candidates_phase5;",
            )
            .map_err(|_| StoreError::Unavailable)?;
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS review_resolutions (
                candidate_fingerprint TEXT PRIMARY KEY NOT NULL,
                receivable_id TEXT NOT NULL,
                action TEXT NOT NULL CHECK(action IN ('ignore_candidate_and_reopen','cancel_unpaid')),
                resolved_at_unix_ms INTEGER NOT NULL
             );
             CREATE TRIGGER IF NOT EXISTS review_candidates_no_update BEFORE UPDATE ON review_candidates BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS review_candidates_no_delete BEFORE DELETE ON review_candidates BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS review_resolutions_no_update BEFORE UPDATE ON review_resolutions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS review_resolutions_no_delete BEFORE DELETE ON review_resolutions BEGIN SELECT RAISE(ABORT, 'append_only'); END;",
        )
        .map_err(|_| StoreError::Unavailable)?;
    let receivables_schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='receivables'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| StoreError::Unavailable)?;
    if !receivables_schema.contains("'cancelled'") {
        connection
            .execute_batch(
                "ALTER TABLE receivables RENAME TO receivables_phase5;
                 CREATE TABLE receivables (
                    receivable_id TEXT PRIMARY KEY NOT NULL,
                    recipient TEXT NOT NULL,
                    mint TEXT NOT NULL,
                    atomic_amount INTEGER NOT NULL,
                    decimals INTEGER NOT NULL,
                    reference TEXT NOT NULL UNIQUE,
                    public_label TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('open','payment_verified','needs_review','cancelled')),
                    created_at_unix_ms INTEGER NOT NULL,
                    solana_pay_url TEXT NOT NULL
                 );
                 INSERT INTO receivables SELECT * FROM receivables_phase5;
                 DROP TABLE receivables_phase5;",
            )
            .map_err(|_| StoreError::Unavailable)?;
    }
    connection
        .execute_batch(
            "INSERT INTO schema_migrations(version) VALUES (7);
             COMMIT;",
        )
        .map_err(|_| StoreError::Unavailable)
}

#[allow(clippy::too_many_lines)]
// One transactional SQL batch keeps the table rebuild all-or-nothing.
fn migrate_variance_schema(connection: &Connection) -> Result<(), StoreError> {
    let installed = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version=8",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?
        .is_some();
    let needs_rebuild = !column_exists(connection, "settlements", "settlement_kind")?
        || !column_exists(connection, "review_candidates", "received_atomic_amount")?
        || !column_exists(connection, "review_resolutions", "approval_run_id")?;
    if installed && !needs_rebuild {
        return Ok(());
    }
    connection
        .execute_batch("BEGIN IMMEDIATE;")
        .map_err(|_| StoreError::Unavailable)?;
    if needs_rebuild {
        connection
            .execute_batch(
                "DROP TRIGGER IF EXISTS review_candidates_no_update;
                 DROP TRIGGER IF EXISTS review_candidates_no_delete;
                 DROP TRIGGER IF EXISTS review_resolutions_no_update;
                 DROP TRIGGER IF EXISTS review_resolutions_no_delete;

                 ALTER TABLE receivables RENAME TO receivables_phase6;
                 CREATE TABLE receivables (
                    receivable_id TEXT PRIMARY KEY NOT NULL,
                    recipient TEXT NOT NULL,
                    mint TEXT NOT NULL,
                    atomic_amount INTEGER NOT NULL,
                    decimals INTEGER NOT NULL,
                    reference TEXT NOT NULL UNIQUE,
                    public_label TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('open','payment_verified','needs_review','cancelled','settled_with_variance')),
                    created_at_unix_ms INTEGER NOT NULL,
                    solana_pay_url TEXT NOT NULL
                 );
                 INSERT INTO receivables SELECT * FROM receivables_phase6;
                 DROP TABLE receivables_phase6;

                 ALTER TABLE settlements RENAME TO settlements_phase6;
                 CREATE TABLE settlements (
                    receivable_id TEXT PRIMARY KEY NOT NULL,
                    signature TEXT NOT NULL UNIQUE,
                    reference TEXT NOT NULL UNIQUE,
                    slot INTEGER NOT NULL,
                    block_time_unix INTEGER,
                    recipient TEXT NOT NULL,
                    mint TEXT NOT NULL,
                    atomic_amount INTEGER NOT NULL,
                    instruction_position INTEGER NOT NULL,
                    fingerprint TEXT NOT NULL UNIQUE,
                    observed_at_unix_ms INTEGER NOT NULL,
                    settlement_kind TEXT NOT NULL CHECK(settlement_kind IN ('exact','accepted_underpayment')),
                    expected_atomic_amount INTEGER NOT NULL,
                    variance_reason TEXT CHECK(variance_reason IN ('rounding_adjustment','commercial_discount','merchant_write_off')),
                    approval_run_id TEXT
                 );
                 INSERT INTO settlements (
                    receivable_id,signature,reference,slot,block_time_unix,
                    recipient,mint,atomic_amount,instruction_position,fingerprint,
                    observed_at_unix_ms,settlement_kind,expected_atomic_amount,
                    variance_reason,approval_run_id
                 )
                 SELECT receivable_id,signature,reference,slot,block_time_unix,
                        recipient,mint,atomic_amount,instruction_position,fingerprint,
                        observed_at_unix_ms,'exact',atomic_amount,NULL,NULL
                 FROM settlements_phase6;
                 DROP TABLE settlements_phase6;

                 ALTER TABLE review_candidates RENAME TO review_candidates_phase6_variance;
                 CREATE TABLE review_candidates (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    receivable_id TEXT NOT NULL,
                    signature TEXT NOT NULL,
                    slot INTEGER NOT NULL,
                    verdict TEXT NOT NULL,
                    candidate_fingerprint TEXT NOT NULL UNIQUE,
                    observed_at_unix_ms INTEGER NOT NULL,
                    block_time_unix INTEGER,
                    recipient TEXT,
                    mint TEXT,
                    expected_atomic_amount INTEGER,
                    received_atomic_amount INTEGER,
                    shortfall_atomic_amount INTEGER,
                    instruction_position INTEGER
                 );
                 INSERT INTO review_candidates (
                    sequence,receivable_id,signature,slot,verdict,
                    candidate_fingerprint,observed_at_unix_ms
                 )
                 SELECT sequence,receivable_id,signature,slot,verdict,
                        candidate_fingerprint,observed_at_unix_ms
                 FROM review_candidates_phase6_variance;
                 DROP TABLE review_candidates_phase6_variance;

                 ALTER TABLE review_resolutions RENAME TO review_resolutions_phase6;
                 CREATE TABLE review_resolutions (
                    candidate_fingerprint TEXT PRIMARY KEY NOT NULL,
                    receivable_id TEXT NOT NULL,
                    action TEXT NOT NULL CHECK(action IN ('ignore_candidate_and_reopen','cancel_unpaid','accept_underpayment_with_variance')),
                    variance_reason TEXT CHECK(variance_reason IN ('rounding_adjustment','commercial_discount','merchant_write_off')),
                    approval_run_id TEXT,
                    resolved_at_unix_ms INTEGER NOT NULL
                 );
                 INSERT INTO review_resolutions (
                    candidate_fingerprint,receivable_id,action,resolved_at_unix_ms
                 )
                 SELECT candidate_fingerprint,receivable_id,action,resolved_at_unix_ms
                 FROM review_resolutions_phase6;
                 DROP TABLE review_resolutions_phase6;

                 CREATE TRIGGER review_candidates_no_update BEFORE UPDATE ON review_candidates BEGIN SELECT RAISE(ABORT, 'append_only'); END;
                 CREATE TRIGGER review_candidates_no_delete BEFORE DELETE ON review_candidates BEGIN SELECT RAISE(ABORT, 'append_only'); END;
                 CREATE TRIGGER review_resolutions_no_update BEFORE UPDATE ON review_resolutions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
                 CREATE TRIGGER review_resolutions_no_delete BEFORE DELETE ON review_resolutions BEGIN SELECT RAISE(ABORT, 'append_only'); END;",
            )
            .map_err(|_| StoreError::Unavailable)?;
        if table_exists(connection, "ledger_checkpoints")? {
            append_ledger_checkpoint(connection)?;
        }
    }
    connection
        .execute_batch(
            "INSERT OR REPLACE INTO schema_migrations(version) VALUES (8);
             COMMIT;",
        )
        .map_err(|_| StoreError::Unavailable)
}

fn initialize_ledger_checkpoints(connection: &Connection) -> Result<(), StoreError> {
    let installed = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version=5",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?
        .is_some();
    if installed {
        return verify_ledger_checkpoints(connection);
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| StoreError::Unavailable)?;
    append_ledger_checkpoint(&transaction)?;
    transaction
        .execute("INSERT INTO schema_migrations(version) VALUES (5)", [])
        .map_err(|_| StoreError::Unavailable)?;
    transaction.commit().map_err(|_| StoreError::Unavailable)
}

fn append_ledger_checkpoint(connection: &Connection) -> Result<(), StoreError> {
    let root = ledger_root(connection)?;
    let previous: Option<Vec<u8>> = connection
        .query_row(
            "SELECT checkpoint_hash FROM ledger_checkpoints ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?;
    let checkpoint_hash = checkpoint_hash(previous.as_deref(), &root);
    connection
        .execute(
            "INSERT INTO ledger_checkpoints (
                previous_checkpoint_hash,ledger_root,checkpoint_hash
             ) VALUES (?1,?2,?3)",
            params![previous, root, checkpoint_hash],
        )
        .map_err(|_| StoreError::Unavailable)?;
    Ok(())
}

fn verify_ledger_checkpoints(connection: &Connection) -> Result<(), StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT previous_checkpoint_hash,ledger_root,checkpoint_hash
             FROM ledger_checkpoints ORDER BY sequence",
        )
        .map_err(|_| StoreError::Unavailable)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|_| StoreError::Unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StoreError::Unavailable)?;
    if rows.is_empty() {
        return Err(StoreError::Integrity);
    }
    let mut previous = None;
    for (stored_previous, root, hash) in &rows {
        if stored_previous.as_ref() != previous.as_ref()
            || *hash != checkpoint_hash(stored_previous.as_deref(), root)
        {
            return Err(StoreError::Integrity);
        }
        previous = Some(hash.clone());
    }
    if rows.last().map(|row| &row.1) != Some(&ledger_root(connection)?) {
        return Err(StoreError::Integrity);
    }
    Ok(())
}

fn checkpoint_hash(previous: Option<&[u8]>, root: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"recebi.ledger_checkpoint.v1");
    hash_field(&mut hasher, previous.unwrap_or(b"GENESIS"));
    hash_field(&mut hasher, root);
    hasher.finalize().to_vec()
}

fn ledger_root(connection: &Connection) -> Result<Vec<u8>, StoreError> {
    const QUERIES: [&str; 7] = [
        "SELECT 'r|'||quote(receivable_id)||'|'||quote(recipient)||'|'||quote(mint)||'|'||quote(atomic_amount)||'|'||quote(decimals)||'|'||quote(reference)||'|'||quote(public_label)||'|'||quote(state)||'|'||quote(created_at_unix_ms)||'|'||quote(solana_pay_url) FROM receivables ORDER BY receivable_id",
        "SELECT 'e|'||quote(sequence)||'|'||quote(receivable_id)||'|'||quote(event_schema_version)||'|'||quote(event_domain)||'|'||quote(hex(previous_event_hash))||'|'||quote(hex(canonical_event_bytes))||'|'||quote(hex(event_hash)) FROM receivable_events ORDER BY sequence",
        "SELECT 's|'||quote(receivable_id)||'|'||quote(signature)||'|'||quote(reference)||'|'||quote(slot)||'|'||quote(block_time_unix)||'|'||quote(recipient)||'|'||quote(mint)||'|'||quote(atomic_amount)||'|'||quote(instruction_position)||'|'||quote(fingerprint)||'|'||quote(observed_at_unix_ms)||'|'||quote(settlement_kind)||'|'||quote(expected_atomic_amount)||'|'||quote(variance_reason)||'|'||quote(approval_run_id) FROM settlements ORDER BY receivable_id",
        "SELECT 'a|'||quote(sequence)||'|'||quote(receivable_id)||'|'||quote(signature)||'|'||quote(slot)||'|'||quote(verdict)||'|'||quote(candidate_fingerprint)||'|'||quote(observed_at_unix_ms)||'|'||quote(block_time_unix)||'|'||quote(recipient)||'|'||quote(mint)||'|'||quote(expected_atomic_amount)||'|'||quote(received_atomic_amount)||'|'||quote(shortfall_atomic_amount)||'|'||quote(instruction_position) FROM review_candidates ORDER BY sequence",
        "SELECT 'd|'||quote(candidate_fingerprint)||'|'||quote(receivable_id)||'|'||quote(action)||'|'||quote(variance_reason)||'|'||quote(approval_run_id)||'|'||quote(resolved_at_unix_ms) FROM review_resolutions ORDER BY candidate_fingerprint",
        "SELECT 'v|'||quote(receivable_id)||'|'||quote(operation_date)||'|'||quote(quote_date)||'|'||quote(purchase)||'|'||quote(sale)||'|'||quote(bulletin_type)||'|'||quote(bulletin_timestamp)||'|'||quote(retrieved_at_unix_ms)||'|'||quote(response_sha256)||'|'||quote(source_id)||'|'||quote(policy_version)||'|'||quote(valuation_method)||'|'||quote(brl_reference_cents) FROM valuations ORDER BY receivable_id",
        "SELECT 'c|'||quote(month)||'|'||quote(revision)||'|'||quote(artifact_kind)||'|'||quote(hex(canonical_json))||'|'||quote(hex(accountant_csv))||'|'||quote(hex(manifest_json))||'|'||quote(hex(close_hash)) FROM month_close_revisions ORDER BY month,revision",
    ];
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"recebi.material_ledger.v1");
    for query in QUERIES {
        let mut statement = connection
            .prepare(query)
            .map_err(|_| StoreError::Unavailable)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| StoreError::Unavailable)?;
        for row in rows {
            hash_field(
                &mut hasher,
                row.map_err(|_| StoreError::Unavailable)?.as_bytes(),
            );
        }
    }
    Ok(hasher.finalize().to_vec())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| StoreError::Unavailable)
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn migrate_state_constraint(connection: &Connection) -> Result<(), StoreError> {
    let schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'receivables'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| StoreError::Unavailable)?;
    if !schema.contains("CHECK(state = 'open')") {
        return Ok(());
    }
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE receivables RENAME TO receivables_v1;
             CREATE TABLE receivables (
                 receivable_id TEXT PRIMARY KEY NOT NULL,
                 recipient TEXT NOT NULL,
                 mint TEXT NOT NULL,
                 atomic_amount INTEGER NOT NULL,
                 decimals INTEGER NOT NULL,
                 reference TEXT NOT NULL UNIQUE,
                 public_label TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN ('open','payment_verified','needs_review','cancelled')),
                 created_at_unix_ms INTEGER NOT NULL,
                 solana_pay_url TEXT NOT NULL
             );
             INSERT INTO receivables SELECT * FROM receivables_v1;
             DROP TABLE receivables_v1;
             COMMIT;",
        )
        .map_err(|_| StoreError::Unavailable)
}

fn find_in(connection: &Connection, id: &str) -> Result<Option<StoredReceivable>, StoreError> {
    let raw = connection.query_row(
        "SELECT receivable_id,recipient,mint,atomic_amount,decimals,reference,public_label,state,created_at_unix_ms,solana_pay_url FROM receivables WHERE receivable_id = ?1", [id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, String>(7)?, row.get::<_, i64>(8)?, row.get::<_, String>(9)?)),
    ).optional().map_err(|_| StoreError::Unavailable)?;
    raw.map(
        |(id, recipient, mint, amount, decimals, reference, label, state, created, url)| {
            let request = PaymentRequest {
                receivable_id: ReceivableId::new(id).map_err(|_| StoreError::Integrity)?,
                recipient: PublicKey::parse(recipient).map_err(|_| StoreError::Integrity)?,
                mint: PublicKey::parse(mint).map_err(|_| StoreError::Integrity)?,
                amount: AtomicAmount::new(
                    u64::try_from(amount).map_err(|_| StoreError::Integrity)?,
                ),
                decimals: u8::try_from(decimals).map_err(|_| StoreError::Integrity)?,
                reference: Reference::parse(reference).map_err(|_| StoreError::Integrity)?,
                public_label: BoundedText::<MAX_PUBLIC_LABEL_BYTES>::new(label)
                    .map_err(|_| StoreError::Integrity)?,
            };
            let state = match state.as_str() {
                "open" => ReceivableState::Open,
                "payment_verified" => ReceivableState::PaymentVerified,
                "needs_review" => ReceivableState::NeedsReview,
                "cancelled" => ReceivableState::Cancelled,
                "settled_with_variance" => ReceivableState::SettledWithVariance,
                _ => return Err(StoreError::Integrity),
            };
            let generated_url = request
                .solana_pay_url()
                .map_err(|_| StoreError::Integrity)?;
            if !urls_equivalent(&generated_url, &url) {
                return Err(StoreError::Integrity);
            }
            Ok(StoredReceivable {
                request,
                state,
                created_at_unix_ms: created,
                solana_pay_url: url,
            })
        },
    )
    .transpose()
}

fn urls_equivalent(left: &str, right: &str) -> bool {
    percent_encoding::percent_decode_str(left)
        .decode_utf8()
        .ok()
        .zip(
            percent_encoding::percent_decode_str(right)
                .decode_utf8()
                .ok(),
        )
        .is_some_and(|(left, right)| left == right)
}

fn same_terms(existing: &StoredReceivable, request: &PaymentRequest) -> bool {
    existing.request.recipient == request.recipient
        && existing.request.mint == request.mint
        && existing.request.amount == request.amount
        && existing.request.decimals == request.decimals
        && existing.request.public_label == request.public_label
}

fn canonical_creation_event(
    request: &PaymentRequest,
    created_at_unix_ms: i64,
    previous: Option<&[u8]>,
) -> Vec<u8> {
    let previous = previous.map_or_else(|| "GENESIS".to_owned(), hex);
    format!("schema_version={EVENT_SCHEMA_VERSION}\ndomain={EVENT_DOMAIN}\nevent=receivable_created\nprevious_event_hash={previous}\nreceivable_id={}\nstate=open\nrecipient={}\nmint={}\natomic_amount={}\ndecimals={}\nreference={}\npublic_label={}\ncreated_at_unix_ms={created_at_unix_ms}\n", request.receivable_id.as_str(), request.recipient.as_str(), request.mint.as_str(), request.amount.get(), request.decimals, request.reference.as_base58(), request.public_label.as_str()).into_bytes()
}

fn append_event(
    transaction: &rusqlite::Transaction<'_>,
    receivable_id: &ReceivableId,
    event_fields: &[u8],
) -> Result<(), StoreError> {
    let previous: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT event_hash FROM receivable_events ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)?;
    let previous_text = previous
        .as_deref()
        .map_or_else(|| "GENESIS".to_owned(), hex);
    let mut canonical = format!(
        "schema_version={EVENT_SCHEMA_VERSION}\ndomain={EVENT_DOMAIN}\nprevious_event_hash={previous_text}\nreceivable_id={}\n",
        receivable_id.as_str()
    )
    .into_bytes();
    canonical.extend_from_slice(event_fields);
    let event_hash = Sha256::digest(&canonical).to_vec();
    transaction
        .execute(
            "INSERT INTO receivable_events (
                receivable_id,event_schema_version,event_domain,previous_event_hash,
                canonical_event_bytes,event_hash
             ) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                receivable_id.as_str(),
                EVENT_SCHEMA_VERSION,
                EVENT_DOMAIN,
                previous,
                canonical,
                event_hash
            ],
        )
        .map_err(|_| StoreError::Unavailable)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use recebi_core::GenesisHash;
    use std::sync::Arc;
    use std::thread;

    fn request(id: &str, reference: u8) -> PaymentRequest {
        PaymentRequest {
            receivable_id: ReceivableId::new(id).expect("id"),
            recipient: PublicKey::parse("11111111111111111111111111111111").expect("recipient"),
            mint: PublicKey::parse("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").expect("mint"),
            amount: AtomicAmount::from_decimal("0.1", 6).expect("amount"),
            decimals: 6,
            reference: Reference::from_bytes([reference; 32]),
            public_label: BoundedText::new("ACME-412").expect("label"),
        }
    }

    #[test]
    fn persists_across_restart_and_is_idempotent() {
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        let store = ReceivableStore::open(&path).expect("store");
        let first = store
            .create_or_get(request("ACME-412", 7), 100)
            .expect("create");
        let reopened = ReceivableStore::open(&path).expect("reopen");
        let retry = reopened
            .create_or_get(request("ACME-412", 8), 101)
            .expect("retry");
        assert_eq!(first, retry);
        assert_eq!(
            reopened.get(&first.request.receivable_id).expect("get"),
            Some(first)
        );
        reopened.verify_event_chain().expect("chain");
    }

    #[test]
    fn rejects_reference_reuse_and_id_conflicts() {
        let directory = tempfile::tempdir().expect("dir");
        let store = ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store");
        store.create_or_get(request("A", 7), 100).expect("first");
        assert_eq!(
            store.create_or_get(request("B", 7), 101),
            Err(StoreError::ReferenceReuse)
        );
        let mut changed = request("A", 8);
        changed.public_label = BoundedText::new("different").expect("label");
        assert_eq!(
            store.create_or_get(changed, 101),
            Err(StoreError::IdempotencyConflict)
        );
    }

    #[test]
    fn concurrent_same_id_creates_one_durable_record() {
        let directory = tempfile::tempdir().expect("dir");
        let store = Arc::new(
            ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store"),
        );
        let handles: Vec<_> = (1..=4)
            .map(|reference| {
                let store = Arc::clone(&store);
                thread::spawn(move || store.create_or_get(request("CONCURRENT", reference), 100))
            })
            .collect();
        for handle in handles {
            assert!(handle.join().expect("thread").is_ok());
        }
        store.verify_event_chain().expect("chain");
    }

    #[test]
    fn detects_event_hash_mutation() {
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        let store = ReceivableStore::open(&path).expect("store");
        store
            .create_or_get(request("TAMPER", 7), 100)
            .expect("create");
        let connection = Connection::open(&path).expect("connection");
        connection.execute_batch("DROP TRIGGER receivable_events_no_update; UPDATE receivable_events SET canonical_event_bytes = x'00';").expect("tamper");
        assert_eq!(store.verify_event_chain(), Err(StoreError::Integrity));
    }

    fn evidence(request: &PaymentRequest, signature_byte: u8) -> SettlementEvidence {
        SettlementEvidence {
            signature: bs58::encode([signature_byte; 64]).into_string(),
            slot: 42,
            block_time_unix: Some(1_700_000_000),
            cluster_genesis_hash: GenesisHash::parse(
                "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG",
            )
            .expect("genesis"),
            recipient: request.recipient.clone(),
            mint: request.mint.clone(),
            amount: request.amount,
            transfer_instruction_position: 0,
            fingerprint: format!("fingerprint-{signature_byte}"),
        }
    }

    #[test]
    fn settlement_is_atomic_idempotent_and_replay_protected_after_restart() {
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        let store = ReceivableStore::open(&path).expect("store");
        let first = store
            .create_or_get(request("SETTLED", 7), 100)
            .expect("create");
        let settled = evidence(&first.request, 9);
        store
            .mark_payment_verified(
                &first.request.receivable_id,
                &first.request.reference,
                &settled,
                200,
            )
            .expect("settle");

        let reopened = ReceivableStore::open(&path).expect("restart");
        reopened
            .mark_payment_verified(
                &first.request.receivable_id,
                &first.request.reference,
                &settled,
                201,
            )
            .expect("idempotent");
        assert_eq!(
            reopened
                .get(&first.request.receivable_id)
                .expect("get")
                .expect("record")
                .state,
            ReceivableState::PaymentVerified
        );
        assert_eq!(
            reopened
                .replay_state(&settled.signature, &first.request.reference)
                .expect("replay state"),
            (true, true)
        );
        assert_eq!(
            reopened
                .settlement_signature(&first.request.receivable_id)
                .expect("signature"),
            Some(settled.signature.clone())
        );

        let second = reopened
            .create_or_get(request("REPLAY", 8), 300)
            .expect("second");
        assert_eq!(
            reopened.mark_payment_verified(
                &second.request.receivable_id,
                &second.request.reference,
                &settled,
                301,
            ),
            Err(StoreError::Replay)
        );
        assert_eq!(
            reopened
                .get(&second.request.receivable_id)
                .expect("get")
                .expect("record")
                .state,
            ReceivableState::Open
        );
        reopened.verify_event_chain().expect("chain");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One lifecycle test documents both permitted outcomes.
    fn review_resolution_is_fingerprint_bound_idempotent_and_never_marks_paid() {
        let directory = tempfile::tempdir().expect("dir");
        let store = ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store");
        let stored = store
            .create_or_get(request("REVIEW", 7), 100)
            .expect("create");
        let first_fingerprint = "ab".repeat(32);
        store
            .mark_needs_review(
                &stored.request.receivable_id,
                &bs58::encode([8_u8; 64]).into_string(),
                101,
                "wrong_amount",
                &first_fingerprint,
                200,
            )
            .expect("first candidate");
        assert_eq!(
            store.resolve_review(
                &stored.request.receivable_id,
                &first_fingerprint,
                ReviewResolutionAction::AcceptUnderpaymentWithVariance,
                Some(VarianceReason::MerchantWriteOff),
                "run-not-eligible",
                299,
            ),
            Err(StoreError::InvalidTransition)
        );
        assert_eq!(
            store.resolve_review(
                &stored.request.receivable_id,
                &"cd".repeat(32),
                ReviewResolutionAction::IgnoreCandidateAndReopen,
                None,
                "run-stale",
                300,
            ),
            Err(StoreError::InvalidTransition)
        );
        assert_eq!(
            store
                .resolve_review(
                    &stored.request.receivable_id,
                    &first_fingerprint,
                    ReviewResolutionAction::IgnoreCandidateAndReopen,
                    None,
                    "run-reopen",
                    301,
                )
                .expect("reopen"),
            ReceivableState::Open
        );
        assert_eq!(
            store
                .resolve_review(
                    &stored.request.receivable_id,
                    &first_fingerprint,
                    ReviewResolutionAction::IgnoreCandidateAndReopen,
                    None,
                    "run-reopen",
                    302,
                )
                .expect("idempotent retry"),
            ReceivableState::Open
        );
        assert_eq!(
            store.resolve_review(
                &stored.request.receivable_id,
                &first_fingerprint,
                ReviewResolutionAction::CancelUnpaid,
                None,
                "run-conflict",
                303,
            ),
            Err(StoreError::InvalidTransition)
        );
        assert_eq!(
            store
                .review_candidate(&stored.request.receivable_id)
                .expect("resolved candidate"),
            None
        );

        let second_fingerprint = "ef".repeat(32);
        store
            .mark_needs_review(
                &stored.request.receivable_id,
                &bs58::encode([9_u8; 64]).into_string(),
                102,
                "wrong_recipient",
                &second_fingerprint,
                400,
            )
            .expect("second candidate");
        assert_eq!(
            store
                .resolve_review(
                    &stored.request.receivable_id,
                    &second_fingerprint,
                    ReviewResolutionAction::CancelUnpaid,
                    None,
                    "run-cancel",
                    500,
                )
                .expect("cancel"),
            ReceivableState::Cancelled
        );
        assert_eq!(
            store
                .get(&stored.request.receivable_id)
                .expect("get")
                .expect("record")
                .state,
            ReceivableState::Cancelled
        );
        assert_eq!(
            store.mark_payment_verified(
                &stored.request.receivable_id,
                &stored.request.reference,
                &evidence(&stored.request, 10),
                600,
            ),
            Err(StoreError::InvalidTransition)
        );
        store.verify_ledger_integrity().expect("ledger");
    }

    #[test]
    fn review_resolution_material_tampering_is_detected() {
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        let store = ReceivableStore::open(&path).expect("store");
        let stored = store
            .create_or_get(request("REVIEW-TAMPER", 7), 100)
            .expect("create");
        let fingerprint = "ab".repeat(32);
        store
            .mark_needs_review(
                &stored.request.receivable_id,
                &bs58::encode([8_u8; 64]).into_string(),
                101,
                "wrong_amount",
                &fingerprint,
                200,
            )
            .expect("candidate");
        store
            .resolve_review(
                &stored.request.receivable_id,
                &fingerprint,
                ReviewResolutionAction::CancelUnpaid,
                None,
                "run-tamper",
                300,
            )
            .expect("resolution");
        Connection::open(&path)
            .expect("connection")
            .execute_batch(
                "DROP TRIGGER review_resolutions_no_update;
                 UPDATE review_resolutions SET action='ignore_candidate_and_reopen';",
            )
            .expect("tamper");
        assert_eq!(store.verify_ledger_integrity(), Err(StoreError::Integrity));
    }

    #[test]
    fn concurrent_opens_all_succeed() {
        // A deployment opens the store from the session server, a scheduled
        // job, and operator commands at the same time. Schema creation runs in
        // an immediate transaction so contention waits instead of failing.
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        ReceivableStore::open(&path).expect("initial store");
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles = (0..8)
            .map(|index| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let store = ReceivableStore::open(&path).expect("concurrent open");
                    store
                        .create_or_get(request(&format!("CONCURRENT-OPEN-{index}"), index + 1), 100)
                        .expect("create");
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().expect("thread");
        }
        let store = ReceivableStore::open(&path).expect("store");
        store.verify_ledger_integrity().expect("integrity");
        for index in 0..8 {
            assert!(
                store
                    .get(&ReceivableId::new(format!("CONCURRENT-OPEN-{index}")).expect("id"))
                    .expect("get")
                    .is_some()
            );
        }
    }

    #[test]
    fn concurrent_identical_review_resolution_commits_once_and_is_idempotent() {
        let directory = tempfile::tempdir().expect("dir");
        let store = Arc::new(
            ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store"),
        );
        let stored = store
            .create_or_get(request("REVIEW-RACE", 7), 100)
            .expect("create");
        let fingerprint = "ab".repeat(32);
        store
            .mark_needs_review(
                &stored.request.receivable_id,
                &bs58::encode([8_u8; 64]).into_string(),
                101,
                "wrong_amount",
                &fingerprint,
                200,
            )
            .expect("candidate");
        let handles = (0..4)
            .map(|offset| {
                let store = Arc::clone(&store);
                let id = stored.request.receivable_id.clone();
                let fingerprint = fingerprint.clone();
                thread::spawn(move || {
                    store.resolve_review(
                        &id,
                        &fingerprint,
                        ReviewResolutionAction::IgnoreCandidateAndReopen,
                        None,
                        "run-race",
                        300 + offset,
                    )
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(
                handle.join().expect("thread").expect("idempotent"),
                ReceivableState::Open
            );
        }
        let connection = store.connection().expect("connection");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM review_resolutions", [], |row| row
                    .get::<_, i64>(0))
                .expect("resolution count"),
            1
        );
        store.verify_ledger_integrity().expect("ledger");
    }

    #[test]
    fn phase_six_migration_preserves_existing_review_candidate_once() {
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        let store = ReceivableStore::open(&path).expect("store");
        let stored = store
            .create_or_get(request("REVIEW-MIGRATE", 7), 100)
            .expect("create");
        let fingerprint = "ab".repeat(32);
        store
            .mark_needs_review(
                &stored.request.receivable_id,
                &bs58::encode([8_u8; 64]).into_string(),
                101,
                "wrong_amount",
                &fingerprint,
                200,
            )
            .expect("candidate");
        drop(store);
        let connection = Connection::open(&path).expect("connection");
        connection
            .execute_batch(
                "DELETE FROM schema_migrations WHERE version=7;
                 DROP TRIGGER review_candidates_no_update;
                 DROP TRIGGER review_candidates_no_delete;
                 DROP TRIGGER review_resolutions_no_update;
                 DROP TRIGGER review_resolutions_no_delete;
                 DROP TABLE review_resolutions;
                 ALTER TABLE review_candidates RENAME TO review_candidates_phase6;
                 CREATE TABLE review_candidates (
                    receivable_id TEXT PRIMARY KEY NOT NULL,
                    signature TEXT NOT NULL,
                    slot INTEGER NOT NULL,
                    verdict TEXT NOT NULL,
                    candidate_fingerprint TEXT NOT NULL UNIQUE,
                    observed_at_unix_ms INTEGER NOT NULL
                 );
                 INSERT INTO review_candidates (
                    receivable_id,signature,slot,verdict,candidate_fingerprint,
                    observed_at_unix_ms
                 )
                 SELECT receivable_id,signature,slot,verdict,candidate_fingerprint,
                        observed_at_unix_ms
                 FROM review_candidates_phase6;
                 DROP TABLE review_candidates_phase6;",
            )
            .expect("simulate phase-five schema");
        drop(connection);

        let migrated = ReceivableStore::open(&path).expect("migrate");
        ReceivableStore::open(&path).expect("idempotent reopen");
        let candidate = migrated
            .review_candidate(&stored.request.receivable_id)
            .expect("candidate")
            .expect("preserved");
        assert_eq!(candidate.candidate_fingerprint, fingerprint);
        let connection = Connection::open(&path).expect("connection");
        assert!(column_exists(&connection, "review_candidates", "sequence").expect("sequence"));
        assert!(table_exists(&connection, "review_resolutions").expect("resolutions"));
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE version=7",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("version"),
            1
        );
        migrated.verify_ledger_integrity().expect("ledger");
    }

    #[test]
    fn monthly_snapshot_fails_closed_when_verified_settlement_has_no_operation_date() {
        let directory = tempfile::tempdir().expect("dir");
        let store = ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store");
        let stored = store
            .create_or_get(request("NO-DATE", 7), 100)
            .expect("create");
        let mut settlement = evidence(&stored.request, 9);
        settlement.block_time_unix = None;
        store
            .mark_payment_verified(
                &stored.request.receivable_id,
                &stored.request.reference,
                &settlement,
                200,
            )
            .expect("settle");
        assert_eq!(
            store.list_settled_between(0, i64::MAX),
            Err(StoreError::Integrity)
        );
    }

    #[test]
    fn reconciliation_lease_excludes_overlap_and_allows_expiry() {
        let directory = tempfile::tempdir().expect("dir");
        let store = ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store");
        store
            .acquire_reconciliation_lease("owner-a", 100, 200)
            .expect("first lease");
        assert_eq!(
            store.acquire_reconciliation_lease("owner-b", 150, 250),
            Err(StoreError::ReconciliationBusy)
        );
        store
            .acquire_reconciliation_lease("owner-b", 200, 300)
            .expect("expired replacement");
        store
            .release_reconciliation_lease("owner-a")
            .expect("non-owner release is harmless");
        assert_eq!(
            store.acquire_reconciliation_lease("owner-c", 250, 350),
            Err(StoreError::ReconciliationBusy)
        );
        store
            .release_reconciliation_lease("owner-b")
            .expect("release");
        store
            .acquire_reconciliation_lease("owner-c", 250, 350)
            .expect("lease after release");
    }

    #[test]
    fn monthly_export_lease_excludes_overlap_by_month_and_allows_expiry() {
        let directory = tempfile::tempdir().expect("dir");
        let store = ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store");
        store
            .acquire_monthly_export_lease("2025-07", "owner-a", 100, 200)
            .expect("first lease");
        assert_eq!(
            store.acquire_monthly_export_lease("2025-07", "owner-b", 150, 250),
            Err(StoreError::MonthlyExportBusy)
        );
        store
            .acquire_monthly_export_lease("2025-08", "owner-b", 150, 250)
            .expect("different month");
        store
            .acquire_monthly_export_lease("2025-07", "owner-b", 200, 300)
            .expect("expired replacement");
        store
            .release_monthly_export_lease("2025-07", "owner-a")
            .expect("non-owner release");
        assert_eq!(
            store.acquire_monthly_export_lease("2025-07", "owner-c", 250, 350),
            Err(StoreError::MonthlyExportBusy)
        );
        store
            .release_monthly_export_lease("2025-07", "owner-b")
            .expect("release");
    }

    #[test]
    fn month_close_rejects_a_stale_material_ledger_snapshot() {
        let directory = tempfile::tempdir().expect("dir");
        let store = ReceivableStore::open(directory.path().join("recebi.sqlite3")).expect("store");
        store
            .create_or_get(request("BEFORE-SNAPSHOT", 7), 100)
            .expect("first row");
        let stale_root = store.ledger_fingerprint().expect("snapshot root");
        store
            .create_or_get(request("AFTER-SNAPSHOT", 8), 101)
            .expect("concurrent row");
        assert_eq!(
            store.record_month_close(
                StoredMonthClose {
                    month: "2025-07".to_owned(),
                    revision: 0,
                    artifact_kind: "final_close".to_owned(),
                    canonical_json: b"{}\n".to_vec(),
                    accountant_csv: b"a,b\n".to_vec(),
                    manifest_json: b"{}\n".to_vec(),
                },
                &stale_root,
            ),
            Err(StoreError::ConcurrentMutation)
        );
    }

    #[test]
    fn material_row_tampering_is_detected_for_settlement_valuation_and_close() {
        for target in ["settlement", "valuation", "close"] {
            let directory = tempfile::tempdir().expect("dir");
            let path = directory.path().join("recebi.sqlite3");
            let store = ReceivableStore::open(&path).expect("store");
            let stored = store
                .create_or_get(request("MATERIAL", 7), 100)
                .expect("create");
            store
                .mark_payment_verified(
                    &stored.request.receivable_id,
                    &stored.request.reference,
                    &evidence(&stored.request, 9),
                    200,
                )
                .expect("settle");
            store
                .attach_valuation(
                    &stored.request.receivable_id,
                    &StoredValuation {
                        evidence: PtaxEvidence {
                            operation_date: PtaxDate::parse("2023-11-14").expect("date"),
                            quote_date: PtaxDate::parse("2023-11-14").expect("date"),
                            purchase: "4.85000".to_owned(),
                            sale: "4.85100".to_owned(),
                            bulletin_type: None,
                            bulletin_timestamp: "2023-11-14 13:00:00".to_owned(),
                            retrieved_at_unix_ms: 300,
                            response_sha256: "ab".repeat(32),
                            source_id: "bcb".to_owned(),
                            policy_version: "strict_same_day_closing_v1".to_owned(),
                        },
                        brl_reference_cents: 49,
                    },
                )
                .expect("valuation");
            let root = store.ledger_fingerprint().expect("ledger root");
            store
                .record_month_close(
                    StoredMonthClose {
                        month: "2023-11".to_owned(),
                        revision: 0,
                        artifact_kind: "final_close".to_owned(),
                        canonical_json: b"{}\n".to_vec(),
                        accountant_csv: b"a,b\n".to_vec(),
                        manifest_json: b"{}\n".to_vec(),
                    },
                    &root,
                )
                .expect("close");
            let connection = Connection::open(&path).expect("connection");
            match target {
                "settlement" => {
                    connection
                        .execute("UPDATE settlements SET atomic_amount=atomic_amount+1", [])
                        .expect("tamper settlement");
                }
                "valuation" => connection
                    .execute_batch(
                        "DROP TRIGGER valuations_no_update;
                         UPDATE valuations SET sale='9.99999';",
                    )
                    .expect("tamper valuation"),
                "close" => connection
                    .execute_batch(
                        "DROP TRIGGER month_close_revisions_no_update;
                         UPDATE month_close_revisions SET canonical_json=x'00';",
                    )
                    .expect("tamper close"),
                _ => unreachable!(),
            }
            assert_eq!(
                store.verify_ledger_integrity(),
                Err(StoreError::Integrity),
                "{target} tamper must fail closed"
            );
            assert_eq!(
                store.get(&stored.request.receivable_id),
                Err(StoreError::Integrity),
                "{target} tamper must also fail a normal agent read"
            );
        }
    }

    #[test]
    fn phase_five_schema_migrates_once_and_removes_legacy_table() {
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        let connection = Connection::open(&path).expect("connection");
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                 INSERT INTO schema_migrations(version) VALUES (1),(2),(3),(4);
                 CREATE TABLE valuations (
                    receivable_id TEXT PRIMARY KEY NOT NULL,
                    operation_date TEXT NOT NULL, quote_date TEXT NOT NULL,
                    purchase TEXT NOT NULL, sale TEXT NOT NULL,
                    bulletin_type TEXT, bulletin_timestamp TEXT NOT NULL,
                    retrieved_at_unix_ms INTEGER NOT NULL,
                    response_sha256 TEXT NOT NULL, source_id TEXT NOT NULL,
                    policy_version TEXT NOT NULL,
                    valuation_method_json TEXT NOT NULL,
                    brl_reference_cents INTEGER NOT NULL
                 );
                 CREATE TABLE month_closes (
                    month TEXT PRIMARY KEY NOT NULL, canonical_json BLOB NOT NULL,
                    accountant_csv BLOB NOT NULL, manifest_json BLOB NOT NULL,
                    close_hash BLOB NOT NULL UNIQUE
                 );
                 CREATE TABLE month_close_revisions (
                    month TEXT NOT NULL, revision INTEGER NOT NULL,
                    canonical_json BLOB NOT NULL, accountant_csv BLOB NOT NULL,
                    manifest_json BLOB NOT NULL, close_hash BLOB NOT NULL UNIQUE,
                    PRIMARY KEY(month,revision)
                 );
                 INSERT INTO month_closes VALUES ('2025-07',x'7b7d',x'612c62',x'7b7d',x'00');",
            )
            .expect("legacy schema");
        drop(connection);

        ReceivableStore::open(&path).expect("migrate");
        ReceivableStore::open(&path).expect("reopen without rerunning migration");
        let connection = Connection::open(&path).expect("connection");
        assert!(!table_exists(&connection, "month_closes").expect("table check"));
        assert!(column_exists(&connection, "valuations", "valuation_method").expect("column"));
        assert!(
            column_exists(&connection, "month_close_revisions", "artifact_kind").expect("column")
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT artifact_kind FROM month_close_revisions WHERE month='2025-07'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .expect("legacy close"),
            "legacy_final_close"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE version IN (5,6)",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("migration versions"),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        ReceivableStore::open(&path).expect("store");
        assert_eq!(
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn migrates_phase_two_rows_and_accepts_equivalent_encoded_urls() {
        let directory = tempfile::tempdir().expect("dir");
        let path = directory.path().join("recebi.sqlite3");
        let original = request("MIGRATE", 7);
        let generated = original.solana_pay_url().expect("url");
        let legacy = percent_encoding::percent_decode_str(&generated)
            .decode_utf8()
            .expect("utf8")
            .into_owned();
        let connection = Connection::open(&path).expect("connection");
        connection
            .execute_batch(
                "CREATE TABLE receivables (
                    receivable_id TEXT PRIMARY KEY NOT NULL,
                    recipient TEXT NOT NULL,
                    mint TEXT NOT NULL,
                    atomic_amount INTEGER NOT NULL,
                    decimals INTEGER NOT NULL,
                    reference TEXT NOT NULL UNIQUE,
                    public_label TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state = 'open'),
                    created_at_unix_ms INTEGER NOT NULL,
                    solana_pay_url TEXT NOT NULL
                );",
            )
            .expect("old schema");
        connection
            .execute(
                "INSERT INTO receivables VALUES (?1,?2,?3,?4,?5,?6,?7,'open',?8,?9)",
                params![
                    original.receivable_id.as_str(),
                    original.recipient.as_str(),
                    original.mint.as_str(),
                    i64::try_from(original.amount.get()).expect("amount"),
                    i64::from(original.decimals),
                    original.reference.as_base58(),
                    original.public_label.as_str(),
                    100_i64,
                    legacy
                ],
            )
            .expect("old row");
        drop(connection);

        let migrated = ReceivableStore::open(&path).expect("migrate");
        let stored = migrated
            .get(&original.receivable_id)
            .expect("get")
            .expect("row");
        assert_eq!(stored.request, original);
        assert!(urls_equivalent(&stored.solana_pay_url, &generated));
    }
}
