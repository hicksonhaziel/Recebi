use std::{
    fmt::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use recebi_core::{
    AtomicAmount, BoundedText, PaymentRequest, PublicKey, ReceivableId, ReceivableState, Reference,
    limits::MAX_PUBLIC_LABEL_BYTES,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredReceivable {
    pub request: PaymentRequest,
    pub state: ReceivableState,
    pub created_at_unix_ms: i64,
    pub solana_pay_url: String,
}

#[derive(Clone, Debug)]
pub struct ReceivableStore {
    path: PathBuf,
}

impl ReceivableStore {
    /// # Errors
    ///
    /// Initializes the local schema or returns a redacted storage error.
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
                 state TEXT NOT NULL CHECK(state = 'open'),
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
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (1);
             COMMIT;",
        ).map_err(|_| StoreError::Unavailable)?;
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
            if state != "open"
                || request
                    .solana_pay_url()
                    .map_err(|_| StoreError::Integrity)?
                    != url
            {
                return Err(StoreError::Integrity);
            }
            Ok(StoredReceivable {
                request,
                state: ReceivableState::Open,
                created_at_unix_ms: created,
                solana_pay_url: url,
            })
        },
    )
    .transpose()
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
}
