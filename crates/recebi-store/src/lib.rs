use std::{
    fmt::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use recebi_core::{
    AtomicAmount, BoundedText, PaymentRequest, PtaxDate, PtaxEvidence, PublicKey, ReceivableId,
    ReceivableState, Reference, SettlementEvidence, UsdValuationMethod,
    limits::{MAX_MONTH_EXPORT_ROWS, MAX_PUBLIC_LABEL_BYTES},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const EVENT_DOMAIN: &str = "recebi.receivable_event.v1";
const EVENT_SCHEMA_VERSION: i64 = 1;

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
    pub verdict: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoredValuation {
    pub evidence: PtaxEvidence,
    pub valuation_method: UsdValuationMethod,
    pub brl_reference_cents: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSettledReceivable {
    pub receivable: StoredReceivable,
    pub signature: String,
    pub slot: u64,
    pub block_time_unix: i64,
    pub settlement_fingerprint: String,
    pub valuation: Option<StoredValuation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredMonthClose {
    pub month: String,
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
            "BEGIN;
             CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS receivables (
                 receivable_id TEXT PRIMARY KEY NOT NULL,
                 recipient TEXT NOT NULL,
                 mint TEXT NOT NULL,
                 atomic_amount INTEGER NOT NULL,
                 decimals INTEGER NOT NULL,
                 reference TEXT NOT NULL UNIQUE,
                 public_label TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN ('open','payment_verified','needs_review')),
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
                 observed_at_unix_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS review_candidates (
                 receivable_id TEXT PRIMARY KEY NOT NULL,
                 signature TEXT NOT NULL,
                 slot INTEGER NOT NULL,
                 verdict TEXT NOT NULL,
                 candidate_fingerprint TEXT NOT NULL UNIQUE,
                 observed_at_unix_ms INTEGER NOT NULL
             );
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
                 valuation_method_json TEXT NOT NULL,
                 brl_reference_cents INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS month_closes (
                 month TEXT PRIMARY KEY NOT NULL,
                 canonical_json BLOB NOT NULL,
                 accountant_csv BLOB NOT NULL,
                 manifest_json BLOB NOT NULL,
                 close_hash BLOB NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS month_close_revisions (
                 month TEXT NOT NULL,
                 revision INTEGER NOT NULL,
                 canonical_json BLOB NOT NULL,
                 accountant_csv BLOB NOT NULL,
                 manifest_json BLOB NOT NULL,
                 close_hash BLOB NOT NULL UNIQUE,
                 PRIMARY KEY(month,revision)
             );
             CREATE TRIGGER IF NOT EXISTS valuations_no_update BEFORE UPDATE ON valuations BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS valuations_no_delete BEFORE DELETE ON valuations BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_closes_no_update BEFORE UPDATE ON month_closes BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_closes_no_delete BEFORE DELETE ON month_closes BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_close_revisions_no_update BEFORE UPDATE ON month_close_revisions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             CREATE TRIGGER IF NOT EXISTS month_close_revisions_no_delete BEFORE DELETE ON month_close_revisions BEGIN SELECT RAISE(ABORT, 'append_only'); END;
             INSERT OR IGNORE INTO month_close_revisions (
                 month,revision,canonical_json,accountant_csv,manifest_json,close_hash
             )
             SELECT month,1,canonical_json,accountant_csv,manifest_json,close_hash
             FROM month_closes;
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (1);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (2);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (3);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (4);
             COMMIT;",
        ).map_err(|_| StoreError::Unavailable)?;
        migrate_state_constraint(&connection)?;
        Ok(store)
    }

    fn connection(&self) -> Result<Connection, StoreError> {
        let connection = Connection::open(&self.path).map_err(|_| StoreError::Unavailable)?;
        connection
            .busy_timeout(Duration::from_secs(3))
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
        find_in(&self.connection()?, receivable_id.as_str())
    }

    /// Returns open receivables in deterministic creation order.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage or integrity error.
    pub fn list_open(&self, limit: usize) -> Result<Vec<StoredReceivable>, StoreError> {
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
                atomic_amount,instruction_position,fingerprint,observed_at_unix_ms
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
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
                observed_at_unix_ms
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
                    receivable_id,signature,slot,verdict,candidate_fingerprint,observed_at_unix_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    receivable_id.as_str(),
                    signature,
                    i64::try_from(slot).map_err(|_| StoreError::Unavailable)?,
                    verdict,
                    candidate_fingerprint,
                    observed_at_unix_ms
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        append_event(
            &transaction,
            receivable_id,
            format!(
                "event=needs_review\nsignature={signature}\nslot={slot}\nverdict={verdict}\ncandidate_fingerprint={candidate_fingerprint}\nobserved_at_unix_ms={observed_at_unix_ms}\n"
            )
            .as_bytes(),
        )?;
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
        self.connection()?
            .query_row(
                "SELECT signature FROM settlements WHERE receivable_id = ?1",
                [receivable_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)
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
        self.connection()?
            .query_row(
                "SELECT signature,verdict FROM review_candidates WHERE receivable_id = ?1",
                [receivable_id.as_str()],
                |row| {
                    Ok(StoredReviewCandidate {
                        signature: row.get(0)?,
                        verdict: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)
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
        if state.as_deref() != Some("payment_verified") {
            return Err(StoreError::InvalidTransition);
        }
        let encoded_method = serde_json::to_string(&valuation.valuation_method)
            .map_err(|_| StoreError::Unavailable)?;
        let existing: Option<(String, String, i64)> = transaction
            .query_row(
                "SELECT response_sha256,valuation_method_json,brl_reference_cents
                 FROM valuations WHERE receivable_id = ?1",
                [receivable_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
        if let Some((hash, method, cents)) = existing {
            return if hash == valuation.evidence.response_sha256
                && method == encoded_method
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
                    policy_version,valuation_method_json,brl_reference_cents
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
                    encoded_method,
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
        self.verify_event_chain()?;
        let connection = self.connection()?;
        let invalid_verified_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*)
                 FROM receivables r LEFT JOIN settlements s USING(receivable_id)
                 WHERE r.state = 'payment_verified'
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
                "SELECT r.receivable_id,s.signature,s.slot,s.block_time_unix,s.fingerprint
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
            .map(|(id, signature, slot, block_time, fingerprint)| {
                let receivable = find_in(&connection, &id)?.ok_or(StoreError::Integrity)?;
                let valuation = find_valuation_in(&connection, &id)?;
                Ok(StoredSettledReceivable {
                    receivable,
                    signature,
                    slot: u64::try_from(slot).map_err(|_| StoreError::Integrity)?,
                    block_time_unix: block_time.ok_or(StoreError::Integrity)?,
                    settlement_fingerprint: fingerprint,
                    valuation,
                })
            })
            .collect()
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
    ) -> Result<StoredMonthClose, StoreError> {
        self.verify_event_chain()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StoreError::Unavailable)?;
        if let Some(existing) = find_month_close_in(&transaction, &close.month)?
            && existing == close
        {
            transaction.commit().map_err(|_| StoreError::Unavailable)?;
            return Ok(existing);
        }
        let mut hash_input = close.month.as_bytes().to_vec();
        hash_input.extend_from_slice(&close.canonical_json);
        hash_input.extend_from_slice(&close.accountant_csv);
        hash_input.extend_from_slice(&close.manifest_json);
        let hash = Sha256::digest(&hash_input).to_vec();
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
                    month,revision,canonical_json,accountant_csv,manifest_json,close_hash
                 ) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    close.month,
                    next_revision,
                    close.canonical_json,
                    close.accountant_csv,
                    close.manifest_json,
                    hash
                ],
            )
            .map_err(|_| StoreError::Unavailable)?;
        transaction.commit().map_err(|_| StoreError::Unavailable)?;
        Ok(close)
    }

    /// # Errors
    ///
    /// Returns a redacted error when the event hash chain is invalid or cannot
    /// be read from local storage.
    pub fn verify_event_chain(&self) -> Result<(), StoreError> {
        let connection = self.connection()?;
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
}

fn find_valuation_in(
    connection: &Connection,
    id: &str,
) -> Result<Option<StoredValuation>, StoreError> {
    let raw = connection
        .query_row(
            "SELECT operation_date,quote_date,purchase,sale,bulletin_type,
                    bulletin_timestamp,retrieved_at_unix_ms,response_sha256,source_id,
                    policy_version,valuation_method_json,brl_reference_cents
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
                valuation_method: serde_json::from_str(&method)
                    .map_err(|_| StoreError::Integrity)?,
                brl_reference_cents: u64::try_from(cents).map_err(|_| StoreError::Integrity)?,
            })
        },
    )
    .transpose()
}

fn find_month_close_in(
    connection: &Connection,
    month: &str,
) -> Result<Option<StoredMonthClose>, StoreError> {
    connection
        .query_row(
            "SELECT canonical_json,accountant_csv,manifest_json
             FROM month_close_revisions WHERE month = ?1
             ORDER BY revision DESC LIMIT 1",
            [month],
            |row| {
                Ok(StoredMonthClose {
                    month: month.to_owned(),
                    canonical_json: row.get(0)?,
                    accountant_csv: row.get(1)?,
                    manifest_json: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|_| StoreError::Unavailable)
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
                 state TEXT NOT NULL CHECK(state IN ('open','payment_verified','needs_review')),
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
