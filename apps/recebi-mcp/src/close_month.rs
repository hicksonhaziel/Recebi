use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use recebi_core::{PtaxDate, PtaxDecimal, UsdValuationMethod, brl_reference_cents};
use recebi_store::{
    ReceivableStore, StoreError, StoredMonthClose, StoredSettledReceivable, StoredValuation,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{config::AppConfig, ptax::PtaxClient};

const EXPORT_SCHEMA: &str = "recebi.monthly_evidence.v1";
const MANIFEST_SCHEMA: &str = "recebi.export_manifest.v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseMonthInput {
    pub month: String,
}

#[derive(Debug, Serialize)]
pub struct CloseMonthOutput {
    pub status: &'static str,
    pub month: String,
    pub payment_verified: usize,
    pub valued: usize,
    pub valuation_pending: usize,
    pub evidence_json_sha256: String,
    pub accountant_csv_sha256: String,
    pub manifest_sha256: String,
    pub export_directory: String,
    pub note: &'static str,
}

#[derive(Debug, Error)]
pub enum CloseMonthError {
    #[error("month must use YYYY-MM")]
    InvalidMonth,
    #[error("monthly ledger integrity check failed")]
    Integrity,
    #[error("monthly export is unavailable")]
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct CloseMonthService<P> {
    store: ReceivableStore,
    ptax: P,
    export_root: PathBuf,
    token_decimals: u8,
}

impl<P: PtaxClient> CloseMonthService<P> {
    /// Creates a monthly close service from trusted local configuration.
    ///
    /// # Errors
    ///
    /// Storage initialization errors are redacted.
    pub fn new(config: &AppConfig, ptax: P) -> Result<Self, CloseMonthError> {
        config
            .ensure_data_directory()
            .map_err(|_| CloseMonthError::Unavailable)?;
        Ok(Self {
            store: ReceivableStore::open(config.database_path())
                .map_err(|_| CloseMonthError::Unavailable)?,
            ptax,
            export_root: config.recebi.data_dir.join("exports"),
            token_decimals: config.recebi.token_decimals,
        })
    }

    /// Attaches available strict same-day PTAX evidence and closes one month.
    ///
    /// # Errors
    ///
    /// Invalid month, ledger integrity, or local export failures are returned
    /// without weakening a previously verified payment.
    pub fn close(&self, input: CloseMonthInput) -> Result<CloseMonthOutput, CloseMonthError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CloseMonthError::Unavailable)?;
        let now_ms = i64::try_from(now.as_millis()).map_err(|_| CloseMonthError::Unavailable)?;
        self.close_at(input, now_ms)
    }

    fn close_at(
        &self,
        input: CloseMonthInput,
        retrieved_at_unix_ms: i64,
    ) -> Result<CloseMonthOutput, CloseMonthError> {
        let (start, end) = month_bounds(&input.month)?;
        let initial = self
            .store
            .list_settled_between(start, end)
            .map_err(|error| map_store(&error))?;
        let mut quotes = BTreeMap::new();
        for row in initial.iter().filter(|row| row.valuation.is_none()) {
            let operation_date = PtaxDate::from_unix_seconds(row.block_time_unix)
                .map_err(|_| CloseMonthError::Integrity)?;
            let evidence = if let Some(cached) = quotes.get(&operation_date).cloned() {
                cached
            } else {
                let fetched = self
                    .ptax
                    .quote(&operation_date, retrieved_at_unix_ms)
                    .unwrap_or_default();
                quotes.insert(operation_date.clone(), fetched.clone());
                fetched
            };
            if let Some(evidence) = evidence {
                let sale =
                    PtaxDecimal::parse(&evidence.sale).map_err(|_| CloseMonthError::Integrity)?;
                let method = UsdValuationMethod::NominalUsdcEqualsUsd;
                let brl_reference_cents = brl_reference_cents(
                    row.receivable.request.amount,
                    self.token_decimals,
                    sale,
                    &method,
                )
                .map_err(|_| CloseMonthError::Integrity)?;
                self.store
                    .attach_valuation(
                        &row.receivable.request.receivable_id,
                        &StoredValuation {
                            evidence,
                            valuation_method: method,
                            brl_reference_cents,
                        },
                    )
                    .map_err(|error| map_store(&error))?;
            }
        }
        let rows = self
            .store
            .list_settled_between(start, end)
            .map_err(|error| map_store(&error))?;
        let canonical_json =
            canonical_evidence(&input.month, &rows).map_err(|_| CloseMonthError::Integrity)?;
        let accountant_csv = accountant_csv(&rows);
        let evidence_hash = sha256_hex(&canonical_json);
        let csv_hash = sha256_hex(&accountant_csv);
        let manifest_json = manifest(&input.month, &evidence_hash, &csv_hash)
            .map_err(|_| CloseMonthError::Integrity)?;
        let manifest_hash = sha256_hex(&manifest_json);
        let close = self
            .store
            .record_month_close(StoredMonthClose {
                month: input.month.clone(),
                canonical_json,
                accountant_csv,
                manifest_json,
            })
            .map_err(|error| map_store(&error))?;
        let directory = self.export_root.join(&input.month);
        write_exports(&directory, &input.month, &close)?;
        let valued = rows.iter().filter(|row| row.valuation.is_some()).count();
        Ok(CloseMonthOutput {
            status: "closed",
            month: input.month,
            payment_verified: rows.len(),
            valued,
            valuation_pending: rows.len() - valued,
            evidence_json_sha256: evidence_hash,
            accountant_csv_sha256: csv_hash,
            manifest_sha256: manifest_hash,
            export_directory: directory.display().to_string(),
            note: "Accountant-ready evidence that may assist record keeping; not tax or legal advice.",
        })
    }
}

#[derive(Serialize)]
struct MonthlyEvidence<'a> {
    schema: &'static str,
    month: &'a str,
    canonical_state: &'static str,
    valuation_policy: &'static str,
    rounding_policy: &'static str,
    records: Vec<EvidenceRecord<'a>>,
}

#[derive(Serialize)]
struct EvidenceRecord<'a> {
    receivable_id: &'a str,
    payment_status: &'static str,
    valuation_status: &'static str,
    amount_atomic: u64,
    token_decimals: u8,
    recipient: &'a str,
    mint: &'a str,
    reference: String,
    signature: &'a str,
    slot: u64,
    block_time_unix: i64,
    settlement_fingerprint: &'a str,
    valuation: Option<&'a StoredValuation>,
}

fn canonical_evidence(
    month: &str,
    rows: &[StoredSettledReceivable],
) -> Result<Vec<u8>, serde_json::Error> {
    let records = rows
        .iter()
        .map(|row| EvidenceRecord {
            receivable_id: row.receivable.request.receivable_id.as_str(),
            payment_status: "verified",
            valuation_status: if row.valuation.is_some() {
                "bcb_verified"
            } else {
                "pending"
            },
            amount_atomic: row.receivable.request.amount.get(),
            token_decimals: row.receivable.request.decimals,
            recipient: row.receivable.request.recipient.as_str(),
            mint: row.receivable.request.mint.as_str(),
            reference: row.receivable.request.reference.as_base58(),
            signature: &row.signature,
            slot: row.slot,
            block_time_unix: row.block_time_unix,
            settlement_fingerprint: &row.settlement_fingerprint,
            valuation: row.valuation.as_ref(),
        })
        .collect();
    let mut bytes = serde_json::to_vec(&MonthlyEvidence {
        schema: EXPORT_SCHEMA,
        month,
        canonical_state: "JSON evidence; CSV is presentation only",
        valuation_policy: "strict same-day BCB PTAX sale; nominal 1 USDC = 1 USD assumption",
        rounding_policy: "BRL cents, integer half-up",
        records,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn accountant_csv(rows: &[StoredSettledReceivable]) -> Vec<u8> {
    let mut csv = String::from(
        "receivable_id,payment_status,valuation_status,token_amount,operation_date,ptax_purchase,ptax_sale,brl_reference,signature\n",
    );
    for row in rows {
        let operation_date = PtaxDate::from_unix_seconds(row.block_time_unix)
            .map_or_else(|_| "invalid".to_owned(), |date| date.as_str().to_owned());
        let (status, purchase, sale, brl) = row.valuation.as_ref().map_or_else(
            || ("pending", "", "", ""),
            |valuation| {
                (
                    "bcb_verified",
                    valuation.evidence.purchase.as_str(),
                    valuation.evidence.sale.as_str(),
                    "",
                )
            },
        );
        let brl_owned = row
            .valuation
            .as_ref()
            .map(|valuation| format_cents(valuation.brl_reference_cents));
        let brl = brl_owned.as_deref().unwrap_or(brl);
        let fields = [
            row.receivable.request.receivable_id.as_str().to_owned(),
            "verified".to_owned(),
            status.to_owned(),
            row.receivable
                .request
                .amount
                .format(row.receivable.request.decimals),
            operation_date,
            purchase.to_owned(),
            sale.to_owned(),
            brl.to_owned(),
            row.signature.clone(),
        ];
        csv.push_str(
            &fields
                .iter()
                .map(|field| csv_field(field))
                .collect::<Vec<_>>()
                .join(","),
        );
        csv.push('\n');
    }
    csv.into_bytes()
}

fn manifest(
    month: &str,
    evidence_hash: &str,
    csv_hash: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "schema": MANIFEST_SCHEMA,
        "month": month,
        "files": [
            {"name": format!("recebi-{month}.evidence.json"), "sha256": evidence_hash},
            {"name": format!("recebi-{month}.accountant.csv"), "sha256": csv_hash}
        ]
    }))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_exports(
    directory: &Path,
    month: &str,
    close: &StoredMonthClose,
) -> Result<(), CloseMonthError> {
    fs::create_dir_all(directory).map_err(|_| CloseMonthError::Unavailable)?;
    let files = [
        (
            directory.join(format!("recebi-{month}.evidence.json")),
            &close.canonical_json,
        ),
        (
            directory.join(format!("recebi-{month}.accountant.csv")),
            &close.accountant_csv,
        ),
        (
            directory.join(format!("recebi-{month}.manifest.json")),
            &close.manifest_json,
        ),
    ];
    for (path, bytes) in files {
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, bytes).map_err(|_| CloseMonthError::Unavailable)?;
        fs::rename(&temporary, &path).map_err(|_| CloseMonthError::Unavailable)?;
    }
    Ok(())
}

fn month_bounds(month: &str) -> Result<(i64, i64), CloseMonthError> {
    if month.len() != 7 || month.as_bytes()[4] != b'-' {
        return Err(CloseMonthError::InvalidMonth);
    }
    let year = month[0..4]
        .parse::<i64>()
        .map_err(|_| CloseMonthError::InvalidMonth)?;
    let month_number = month[5..7]
        .parse::<i64>()
        .map_err(|_| CloseMonthError::InvalidMonth)?;
    if year == 0 || !(1..=12).contains(&month_number) {
        return Err(CloseMonthError::InvalidMonth);
    }
    let next_year = if month_number == 12 { year + 1 } else { year };
    let next_month = if month_number == 12 {
        1
    } else {
        month_number + 1
    };
    Ok((
        days_from_civil(year, month_number, 1) * 86_400,
        days_from_civil(next_year, next_month, 1) * 86_400,
    ))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn format_cents(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn map_store(error: &StoreError) -> CloseMonthError {
    match error {
        StoreError::Integrity | StoreError::InvalidTransition => CloseMonthError::Integrity,
        _ => CloseMonthError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use recebi_core::{
        AtomicAmount, BoundedText, PaymentRequest, PublicKey, ReceivableId, Reference,
        SettlementEvidence, limits::MAX_PUBLIC_LABEL_BYTES,
    };
    use recebi_store::ReceivableStore;

    use super::*;
    use crate::{config::AppConfig, ptax::PtaxError};

    #[derive(Clone)]
    struct FakePtax {
        result: Arc<Mutex<Result<Option<recebi_core::PtaxEvidence>, PtaxError>>>,
    }

    impl PtaxClient for FakePtax {
        fn quote(
            &self,
            _date: &PtaxDate,
            _retrieved_at_unix_ms: i64,
        ) -> Result<Option<recebi_core::PtaxEvidence>, PtaxError> {
            self.result.lock().expect("lock").clone()
        }
    }

    fn config(directory: &tempfile::TempDir) -> AppConfig {
        let path = directory.path().join("recebi.toml");
        std::fs::write(
            &path,
            format!(
                r#"[recebi]
cluster = "devnet"
genesis_hash = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG"
merchant_wallet = "11111111111111111111111111111111"
accepted_mint = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU"
token_decimals = 6
rpc_url = "https://api.devnet.solana.com"
data_dir = "{}"
ptax_policy = "strict_same_day"
max_open_reconcile = 10
"#,
                directory.path().join("data").display()
            ),
        )
        .expect("config");
        AppConfig::load(&path).expect("load")
    }

    fn settled_store(config: &AppConfig) -> ReceivableStore {
        config.ensure_data_directory().expect("data directory");
        let store = ReceivableStore::open(config.database_path()).expect("store");
        let request = PaymentRequest {
            receivable_id: ReceivableId::new("JULY-001").expect("id"),
            recipient: PublicKey::parse("11111111111111111111111111111111").expect("recipient"),
            mint: PublicKey::parse("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU").expect("mint"),
            amount: AtomicAmount::new(100_000),
            decimals: 6,
            reference: Reference::from_bytes([7; 32]),
            public_label: BoundedText::<MAX_PUBLIC_LABEL_BYTES>::new("July").expect("label"),
        };
        store.create_or_get(request.clone(), 1).expect("create");
        store
            .mark_payment_verified(
                &request.receivable_id,
                &request.reference,
                &SettlementEvidence {
                    signature: bs58::encode([8; 64]).into_string(),
                    slot: 99,
                    block_time_unix: Some(1_753_747_200), // 2025-07-28 UTC
                    cluster_genesis_hash: recebi_core::GenesisHash::parse(
                        "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG",
                    )
                    .expect("genesis"),
                    recipient: request.recipient,
                    mint: request.mint,
                    amount: request.amount,
                    transfer_instruction_position: 1,
                    fingerprint: "ab".repeat(32),
                },
                2,
            )
            .expect("settle");
        store
    }

    fn evidence() -> recebi_core::PtaxEvidence {
        recebi_core::PtaxEvidence {
            operation_date: PtaxDate::parse("2025-07-28").expect("date"),
            quote_date: PtaxDate::parse("2025-07-28").expect("date"),
            purchase: "5.11710".to_owned(),
            sale: "5.11770".to_owned(),
            bulletin_type: None,
            bulletin_timestamp: "2025-07-28 13:25:31.150278".to_owned(),
            retrieved_at_unix_ms: 123,
            response_sha256: "ab".repeat(32),
            source_id: "bcb_ptax_v1_cotacao_dolar_dia".to_owned(),
            policy_version: "strict_same_day_closing_v1".to_owned(),
        }
    }

    #[test]
    fn closes_deterministically_with_bcb_evidence_and_manifest_hashes() {
        let directory = tempfile::tempdir().expect("dir");
        let config = config(&directory);
        let store = settled_store(&config);
        let service = CloseMonthService::new(
            &config,
            FakePtax {
                result: Arc::new(Mutex::new(Ok(Some(evidence())))),
            },
        )
        .expect("service");
        let first = service
            .close_at(
                CloseMonthInput {
                    month: "2025-07".to_owned(),
                },
                123,
            )
            .expect("close");
        let second = service
            .close_at(
                CloseMonthInput {
                    month: "2025-07".to_owned(),
                },
                999,
            )
            .expect("idempotent close");
        assert_eq!(first.evidence_json_sha256, second.evidence_json_sha256);
        assert_eq!(first.valued, 1);
        let rows = store
            .list_settled_between(1_751_328_000, 1_754_006_400)
            .expect("rows");
        assert_eq!(
            rows[0]
                .valuation
                .as_ref()
                .expect("valuation")
                .brl_reference_cents,
            51
        );
        let manifest = std::fs::read(
            directory
                .path()
                .join("data/exports/2025-07/recebi-2025-07.manifest.json"),
        )
        .expect("manifest");
        assert_eq!(sha256_hex(&manifest), first.manifest_sha256);
    }

    #[test]
    fn closes_with_paid_but_pending_valuation_during_source_failure() {
        let directory = tempfile::tempdir().expect("dir");
        let config = config(&directory);
        settled_store(&config);
        let result = Arc::new(Mutex::new(Err(PtaxError::Unavailable)));
        let service = CloseMonthService::new(
            &config,
            FakePtax {
                result: Arc::clone(&result),
            },
        )
        .expect("service");
        let output = service
            .close_at(
                CloseMonthInput {
                    month: "2025-07".to_owned(),
                },
                123,
            )
            .expect("honest pending close");
        assert_eq!(output.payment_verified, 1);
        assert_eq!(output.valuation_pending, 1);
        assert_eq!(output.valued, 0);

        *result.lock().expect("lock") = Ok(Some(evidence()));
        let revised = service
            .close_at(
                CloseMonthInput {
                    month: "2025-07".to_owned(),
                },
                456,
            )
            .expect("append-only revised close");
        assert_eq!(revised.valued, 1);
        assert_ne!(output.evidence_json_sha256, revised.evidence_json_sha256);
        let connection = rusqlite::Connection::open(config.database_path()).expect("db");
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM month_close_revisions WHERE month='2025-07'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("revision count"),
            2
        );
    }

    #[test]
    fn rejects_bad_month_and_detects_modified_event_ledger() {
        let directory = tempfile::tempdir().expect("dir");
        let config = config(&directory);
        settled_store(&config);
        let service = CloseMonthService::new(
            &config,
            FakePtax {
                result: Arc::new(Mutex::new(Ok(None))),
            },
        )
        .expect("service");
        assert!(matches!(
            service.close_at(
                CloseMonthInput {
                    month: "2025-13".to_owned()
                },
                1
            ),
            Err(CloseMonthError::InvalidMonth)
        ));
        let connection = rusqlite::Connection::open(config.database_path()).expect("db");
        connection
            .execute_batch(
                "DROP TRIGGER receivable_events_no_update;
                 UPDATE receivable_events SET canonical_event_bytes = x'00' WHERE sequence = 1;",
            )
            .expect("tamper");
        assert!(matches!(
            service.close_at(
                CloseMonthInput {
                    month: "2025-07".to_owned()
                },
                1
            ),
            Err(CloseMonthError::Integrity)
        ));
    }
}
