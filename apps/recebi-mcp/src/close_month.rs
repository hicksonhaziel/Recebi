use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use recebi_core::{NOMINAL_USDC_USD_METHOD, PtaxDate, PtaxDecimal, nominal_brl_reference_cents};
use recebi_store::{
    ReceivableStore, StoreError, StoredMonthClose, StoredSettledReceivable, StoredValuation,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{config::AppConfig, ptax::PtaxClient};

const EXPORT_SCHEMA: &str = "recebi.monthly_evidence.v2";
const MANIFEST_SCHEMA: &str = "recebi.export_manifest.v1";
const EXPORT_LEASE_MS: i64 = 10 * 60 * 1_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseMonthInput {
    pub month: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMonthInput {
    pub month: String,
}

#[derive(Debug, Serialize)]
pub struct CloseMonthOutput {
    pub status: &'static str,
    pub artifact_kind: &'static str,
    pub revision: Option<u32>,
    pub month: String,
    pub payment_verified: usize,
    pub settled_with_variance: usize,
    pub valued: usize,
    pub valuation_pending: usize,
    pub evidence_json_sha256: String,
    pub accountant_csv_sha256: String,
    pub manifest_sha256: String,
    pub export_directory: String,
    pub note: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportMode {
    ProvisionalSnapshot,
    FinalClose,
}

impl ExportMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProvisionalSnapshot => "provisional_snapshot",
            Self::FinalClose => "final_close",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ValuationPendingReason {
    QuoteUnavailable,
    SourceUnavailable,
    ResponseTooLarge,
    ResponseInvalid,
}

#[derive(Debug, Error)]
pub enum CloseMonthError {
    #[error("month must use YYYY-MM")]
    InvalidMonth,
    #[error("final close requires a completed UTC month")]
    MonthNotEligible,
    #[error("future UTC month cannot be exported")]
    FutureMonth,
    #[error("another export for this month is already running")]
    Busy,
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
    pub fn close(&self, input: &CloseMonthInput) -> Result<CloseMonthOutput, CloseMonthError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CloseMonthError::Unavailable)?;
        let now_ms = i64::try_from(now.as_millis()).map_err(|_| CloseMonthError::Unavailable)?;
        self.export_at(&input.month, ExportMode::FinalClose, now_ms)
    }

    /// Creates a provisional snapshot for the current or a completed month.
    ///
    /// # Errors
    ///
    /// Future months, ledger integrity failures, or publication errors fail.
    pub fn snapshot(
        &self,
        input: &SnapshotMonthInput,
    ) -> Result<CloseMonthOutput, CloseMonthError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CloseMonthError::Unavailable)?;
        let now_ms = i64::try_from(now.as_millis()).map_err(|_| CloseMonthError::Unavailable)?;
        self.export_at(&input.month, ExportMode::ProvisionalSnapshot, now_ms)
    }

    fn export_at(
        &self,
        month: &str,
        mode: ExportMode,
        retrieved_at_unix_ms: i64,
    ) -> Result<CloseMonthOutput, CloseMonthError> {
        validate_eligibility(month, mode, retrieved_at_unix_ms)?;
        let owner = random_owner()?;
        self.store
            .acquire_monthly_export_lease(
                month,
                &owner,
                retrieved_at_unix_ms,
                retrieved_at_unix_ms
                    .checked_add(EXPORT_LEASE_MS)
                    .ok_or(CloseMonthError::Unavailable)?,
            )
            .map_err(|error| map_store(&error))?;
        let result = self.export_locked(month, mode, retrieved_at_unix_ms, &owner);
        let released = self
            .store
            .release_monthly_export_lease(month, &owner)
            .map_err(|error| map_store(&error));
        match (result, released) {
            (Ok(output), Ok(())) => Ok(output),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    fn export_locked(
        &self,
        month: &str,
        mode: ExportMode,
        retrieved_at_unix_ms: i64,
        owner: &str,
    ) -> Result<CloseMonthOutput, CloseMonthError> {
        let (start, end) = month_bounds(month)?;
        let initial = self
            .store
            .list_settled_between(start, end)
            .map_err(|error| map_store(&error))?;
        let pending_reasons = self.attach_available_valuations(&initial, retrieved_at_unix_ms)?;
        let root_before = self
            .store
            .ledger_fingerprint()
            .map_err(|error| map_store(&error))?;
        let rows = self
            .store
            .list_settled_between(start, end)
            .map_err(|error| map_store(&error))?;
        let root_after = self
            .store
            .ledger_fingerprint()
            .map_err(|error| map_store(&error))?;
        if root_before != root_after {
            return Err(CloseMonthError::Busy);
        }
        let canonical_json = canonical_evidence(month, mode, &rows, &pending_reasons)
            .map_err(|_| CloseMonthError::Integrity)?;
        let accountant_csv = accountant_csv(&rows);
        let evidence_hash = sha256_hex(&canonical_json);
        let csv_hash = sha256_hex(&accountant_csv);
        let manifest_json = manifest(month, mode, &evidence_hash, &csv_hash)
            .map_err(|_| CloseMonthError::Integrity)?;
        let manifest_hash = sha256_hex(&manifest_json);
        let close = StoredMonthClose {
            month: month.to_owned(),
            revision: 0,
            artifact_kind: mode.as_str().to_owned(),
            canonical_json,
            accountant_csv,
            manifest_json,
        };
        let (close, directory) = match mode {
            ExportMode::FinalClose => {
                let close = self
                    .store
                    .record_month_close(close, &root_after)
                    .map_err(|error| map_store(&error))?;
                let directory = self
                    .export_root
                    .join("closed")
                    .join(month)
                    .join(format!("revision-{}", close.revision));
                write_exports_atomic(&directory, month, &close, owner)?;
                (close, directory)
            }
            ExportMode::ProvisionalSnapshot => {
                let directory = self
                    .export_root
                    .join("snapshots")
                    .join(month)
                    .join(&evidence_hash[..16]);
                write_exports_atomic(&directory, month, &close, owner)?;
                (close, directory)
            }
        };
        let valued = rows.iter().filter(|row| row.valuation.is_some()).count();
        let payment_verified = rows
            .iter()
            .filter(|row| row.settlement_kind == "exact")
            .count();
        let settled_with_variance = rows.len() - payment_verified;
        Ok(CloseMonthOutput {
            status: if mode == ExportMode::FinalClose {
                "closed"
            } else {
                "snapshot_created"
            },
            artifact_kind: mode.as_str(),
            revision: (mode == ExportMode::FinalClose).then_some(close.revision),
            month: month.to_owned(),
            payment_verified,
            settled_with_variance,
            valued,
            valuation_pending: rows.len() - valued,
            evidence_json_sha256: evidence_hash,
            accountant_csv_sha256: csv_hash,
            manifest_sha256: manifest_hash,
            export_directory: directory.display().to_string(),
            note: "Accountant-ready evidence that may assist record keeping; not tax or legal advice.",
        })
    }

    fn attach_available_valuations(
        &self,
        rows: &[StoredSettledReceivable],
        retrieved_at_unix_ms: i64,
    ) -> Result<BTreeMap<String, ValuationPendingReason>, CloseMonthError> {
        let mut quotes = BTreeMap::new();
        let mut pending_reasons = BTreeMap::new();
        for row in rows.iter().filter(|row| row.valuation.is_none()) {
            let operation_date = PtaxDate::from_unix_seconds(row.block_time_unix)
                .map_err(|_| CloseMonthError::Integrity)?;
            let evidence = if let Some(cached) = quotes.get(&operation_date).cloned() {
                cached
            } else {
                let fetched = self.ptax.quote(&operation_date, retrieved_at_unix_ms);
                quotes.insert(operation_date.clone(), fetched.clone());
                fetched
            };
            match evidence {
                Ok(Some(evidence)) => {
                    let sale = PtaxDecimal::parse(&evidence.sale)
                        .map_err(|_| CloseMonthError::Integrity)?;
                    let brl_reference_cents =
                        nominal_brl_reference_cents(row.received_amount, self.token_decimals, sale)
                            .map_err(|_| CloseMonthError::Integrity)?;
                    self.store
                        .attach_valuation(
                            &row.receivable.request.receivable_id,
                            &StoredValuation {
                                evidence,
                                brl_reference_cents,
                            },
                        )
                        .map_err(|error| map_store(&error))?;
                }
                Ok(None) => {
                    pending_reasons.insert(
                        row.receivable.request.receivable_id.as_str().to_owned(),
                        ValuationPendingReason::QuoteUnavailable,
                    );
                }
                Err(error) => {
                    let reason = match error {
                        crate::ptax::PtaxError::Unavailable => {
                            ValuationPendingReason::SourceUnavailable
                        }
                        crate::ptax::PtaxError::ResponseTooLarge => {
                            ValuationPendingReason::ResponseTooLarge
                        }
                        crate::ptax::PtaxError::MalformedResponse => {
                            ValuationPendingReason::ResponseInvalid
                        }
                    };
                    pending_reasons.insert(
                        row.receivable.request.receivable_id.as_str().to_owned(),
                        reason,
                    );
                }
            }
        }
        Ok(pending_reasons)
    }
}

#[derive(Serialize)]
struct MonthlyEvidence<'a> {
    schema: &'static str,
    month: &'a str,
    artifact_kind: &'static str,
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
    expected_amount_atomic: u64,
    received_amount_atomic: u64,
    shortfall_amount_atomic: u64,
    token_decimals: u8,
    recipient: &'a str,
    mint: &'a str,
    reference: String,
    signature: &'a str,
    slot: u64,
    block_time_unix: i64,
    settlement_fingerprint: &'a str,
    variance_reason: Option<&'static str>,
    approval_run_id: Option<&'a str>,
    valuation_pending_reason: Option<ValuationPendingReason>,
    valuation: Option<ValuationEvidence<'a>>,
}

#[derive(Serialize)]
struct ValuationEvidence<'a> {
    evidence: &'a recebi_core::PtaxEvidence,
    valuation_method: &'static str,
    brl_reference_cents: u64,
}

fn canonical_evidence(
    month: &str,
    mode: ExportMode,
    rows: &[StoredSettledReceivable],
    pending_reasons: &BTreeMap<String, ValuationPendingReason>,
) -> Result<Vec<u8>, serde_json::Error> {
    let records = rows
        .iter()
        .map(|row| EvidenceRecord {
            receivable_id: row.receivable.request.receivable_id.as_str(),
            payment_status: if row.settlement_kind == "exact" {
                "exact_verified"
            } else {
                "operator_accepted_underpayment"
            },
            valuation_status: if row.valuation.is_some() {
                "bcb_verified"
            } else {
                "pending"
            },
            amount_atomic: row.received_amount.get(),
            expected_amount_atomic: row.expected_amount.get(),
            received_amount_atomic: row.received_amount.get(),
            shortfall_amount_atomic: row
                .expected_amount
                .get()
                .checked_sub(row.received_amount.get())
                .unwrap_or_default(),
            token_decimals: row.receivable.request.decimals,
            recipient: row.receivable.request.recipient.as_str(),
            mint: row.receivable.request.mint.as_str(),
            reference: row.receivable.request.reference.as_base58(),
            signature: &row.signature,
            slot: row.slot,
            block_time_unix: row.block_time_unix,
            settlement_fingerprint: &row.settlement_fingerprint,
            variance_reason: row.variance_reason.map(recebi_core::VarianceReason::as_str),
            approval_run_id: row.approval_run_id.as_deref(),
            valuation_pending_reason: row.valuation.as_ref().map_or_else(
                || {
                    pending_reasons
                        .get(row.receivable.request.receivable_id.as_str())
                        .copied()
                },
                |_| None,
            ),
            valuation: row.valuation.as_ref().map(|valuation| ValuationEvidence {
                evidence: &valuation.evidence,
                valuation_method: NOMINAL_USDC_USD_METHOD,
                brl_reference_cents: valuation.brl_reference_cents,
            }),
        })
        .collect();
    let mut bytes = serde_json::to_vec(&MonthlyEvidence {
        schema: EXPORT_SCHEMA,
        month,
        artifact_kind: mode.as_str(),
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
        "receivable_id,payment_status,valuation_status,expected_token_amount,received_token_amount,shortfall_token_amount,variance_reason,approval_run_id,operation_date,ptax_purchase,ptax_sale,brl_reference,signature\n",
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
            if row.settlement_kind == "exact" {
                "exact_verified".to_owned()
            } else {
                "operator_accepted_underpayment".to_owned()
            },
            status.to_owned(),
            row.expected_amount.format(row.receivable.request.decimals),
            row.received_amount.format(row.receivable.request.decimals),
            recebi_core::AtomicAmount::new(
                row.expected_amount
                    .get()
                    .saturating_sub(row.received_amount.get()),
            )
            .format(row.receivable.request.decimals),
            row.variance_reason
                .map_or_else(String::new, |reason| reason.as_str().to_owned()),
            row.approval_run_id.clone().unwrap_or_default(),
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
    mode: ExportMode,
    evidence_hash: &str,
    csv_hash: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "schema": MANIFEST_SCHEMA,
        "month": month,
        "artifact_kind": mode.as_str(),
        "files": [
            {"name": format!("recebi-{month}.evidence.json"), "sha256": evidence_hash},
            {"name": format!("recebi-{month}.accountant.csv"), "sha256": csv_hash}
        ]
    }))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_exports_atomic(
    directory: &Path,
    month: &str,
    close: &StoredMonthClose,
    owner: &str,
) -> Result<(), CloseMonthError> {
    if directory.is_dir() {
        return verify_published_files(directory, month, close);
    }
    let parent = directory.parent().ok_or(CloseMonthError::Unavailable)?;
    create_secure_directory(parent)?;
    let name = directory
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(CloseMonthError::Unavailable)?;
    let temporary = parent.join(format!(".{name}.tmp-{owner}"));
    if temporary.exists() {
        fs::remove_dir_all(&temporary).map_err(|_| CloseMonthError::Unavailable)?;
    }
    create_secure_directory(&temporary)?;
    let files = [
        (
            temporary.join(format!("recebi-{month}.evidence.json")),
            &close.canonical_json,
        ),
        (
            temporary.join(format!("recebi-{month}.accountant.csv")),
            &close.accountant_csv,
        ),
        (
            temporary.join(format!("recebi-{month}.manifest.json")),
            &close.manifest_json,
        ),
    ];
    for (path, bytes) in files {
        write_secure_file(&path, bytes)?;
    }
    sync_directory(&temporary)?;
    fs::rename(&temporary, directory).map_err(|_| CloseMonthError::Unavailable)?;
    sync_directory(parent)?;
    Ok(())
}

fn verify_published_files(
    directory: &Path,
    month: &str,
    close: &StoredMonthClose,
) -> Result<(), CloseMonthError> {
    let expected = [
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
    for (path, bytes) in expected {
        if fs::read(path).map_err(|_| CloseMonthError::Integrity)? != *bytes {
            return Err(CloseMonthError::Integrity);
        }
    }
    Ok(())
}

fn write_secure_file(path: &Path, bytes: &[u8]) -> Result<(), CloseMonthError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| CloseMonthError::Unavailable)?;
    file.write_all(bytes)
        .map_err(|_| CloseMonthError::Unavailable)?;
    file.sync_all().map_err(|_| CloseMonthError::Unavailable)
}

fn create_secure_directory(path: &Path) -> Result<(), CloseMonthError> {
    fs::create_dir_all(path).map_err(|_| CloseMonthError::Unavailable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| CloseMonthError::Unavailable)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), CloseMonthError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| CloseMonthError::Unavailable)
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

fn validate_eligibility(
    month: &str,
    mode: ExportMode,
    now_unix_ms: i64,
) -> Result<(), CloseMonthError> {
    month_bounds(month)?;
    let now_seconds = now_unix_ms
        .checked_div(1_000)
        .ok_or(CloseMonthError::Unavailable)?;
    let current =
        PtaxDate::from_unix_seconds(now_seconds).map_err(|_| CloseMonthError::Unavailable)?;
    let current_month = &current.as_str()[0..7];
    if month > current_month {
        return Err(CloseMonthError::FutureMonth);
    }
    if mode == ExportMode::FinalClose && month >= current_month {
        return Err(CloseMonthError::MonthNotEligible);
    }
    Ok(())
}

fn random_owner() -> Result<String, CloseMonthError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| CloseMonthError::Unavailable)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
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
        StoreError::MonthlyExportBusy | StoreError::ConcurrentMutation => CloseMonthError::Busy,
        _ => CloseMonthError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use recebi_core::{
        AtomicAmount, BoundedText, PaymentRequest, PublicKey, ReceivableId, Reference,
        ReviewResolutionAction, SettlementEvidence, UnderpaymentEvidence, VarianceReason,
        limits::MAX_PUBLIC_LABEL_BYTES, solana::derive_classic_ata,
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

    fn variance_store(config: &AppConfig) -> ReceivableStore {
        config.ensure_data_directory().expect("data directory");
        let store = ReceivableStore::open(config.database_path()).expect("store");
        let request = PaymentRequest {
            receivable_id: ReceivableId::new("JULY-VARIANCE").expect("id"),
            recipient: PublicKey::parse("11111111111111111111111111111111").expect("recipient"),
            mint: PublicKey::parse("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU").expect("mint"),
            amount: AtomicAmount::new(100_000),
            decimals: 6,
            reference: Reference::from_bytes([9; 32]),
            public_label: BoundedText::<MAX_PUBLIC_LABEL_BYTES>::new("July variance")
                .expect("label"),
        };
        store
            .create_or_get(request.clone(), 1)
            .expect("create variance");
        let recipient = derive_classic_ata(&request.recipient, &request.mint).expect("ATA");
        let fingerprint = "cd".repeat(32);
        store
            .mark_underpayment_review(
                &request.receivable_id,
                &UnderpaymentEvidence {
                    signature: bs58::encode([9; 64]).into_string(),
                    slot: 100,
                    block_time_unix: Some(1_753_747_200),
                    cluster_genesis_hash: recebi_core::GenesisHash::parse(
                        "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG",
                    )
                    .expect("genesis"),
                    recipient,
                    mint: request.mint,
                    expected_amount: request.amount,
                    received_amount: AtomicAmount::new(99_000),
                    shortfall_amount: AtomicAmount::new(1_000),
                    transfer_instruction_position: 1,
                    fingerprint: fingerprint.clone(),
                },
                2,
            )
            .expect("underpayment");
        store
            .resolve_review(
                &request.receivable_id,
                &fingerprint,
                ReviewResolutionAction::AcceptUnderpaymentWithVariance,
                Some(VarianceReason::RoundingAdjustment),
                "run-close-variance",
                3,
            )
            .expect("accept variance");
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
            .export_at("2025-07", ExportMode::FinalClose, 1_754_006_400_000)
            .expect("close");
        let second = service
            .export_at("2025-07", ExportMode::FinalClose, 1_754_006_401_000)
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
                .join("data/exports/closed/2025-07/revision-1/recebi-2025-07.manifest.json"),
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
            .export_at("2025-07", ExportMode::FinalClose, 1_754_006_400_000)
            .expect("honest pending close");
        assert_eq!(output.payment_verified, 1);
        assert_eq!(output.valuation_pending, 1);
        assert_eq!(output.valued, 0);
        let pending_evidence = std::fs::read_to_string(
            directory
                .path()
                .join("data/exports/closed/2025-07/revision-1/recebi-2025-07.evidence.json"),
        )
        .expect("pending evidence");
        assert!(pending_evidence.contains(r#""valuation_pending_reason":"source_unavailable""#));

        *result.lock().expect("lock") = Ok(Some(evidence()));
        let revised = service
            .export_at("2025-07", ExportMode::FinalClose, 1_754_006_401_000)
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
    fn monthly_evidence_keeps_expected_received_and_variance_distinct() {
        let directory = tempfile::tempdir().expect("dir");
        let config = config(&directory);
        variance_store(&config);
        let service = CloseMonthService::new(
            &config,
            FakePtax {
                result: Arc::new(Mutex::new(Ok(Some(evidence())))),
            },
        )
        .expect("service");
        let output = service
            .export_at("2025-07", ExportMode::FinalClose, 1_754_006_400_000)
            .expect("close");
        assert_eq!(output.payment_verified, 0);
        assert_eq!(output.settled_with_variance, 1);
        let evidence = std::fs::read_to_string(
            directory
                .path()
                .join("data/exports/closed/2025-07/revision-1/recebi-2025-07.evidence.json"),
        )
        .expect("evidence");
        assert!(evidence.contains(r#""payment_status":"operator_accepted_underpayment""#));
        assert!(evidence.contains(r#""expected_amount_atomic":100000"#));
        assert!(evidence.contains(r#""received_amount_atomic":99000"#));
        assert!(evidence.contains(r#""shortfall_amount_atomic":1000"#));
        assert!(evidence.contains(r#""variance_reason":"rounding_adjustment""#));
        assert!(evidence.contains(r#""approval_run_id":"run-close-variance""#));
    }

    #[test]
    fn active_month_requires_snapshot_and_future_month_is_rejected() {
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
        let july_2025 = 1_753_747_200_000;
        assert!(matches!(
            service.export_at("2025-07", ExportMode::FinalClose, july_2025),
            Err(CloseMonthError::MonthNotEligible)
        ));
        let snapshot = service
            .export_at("2025-07", ExportMode::ProvisionalSnapshot, july_2025)
            .expect("active snapshot");
        assert_eq!(snapshot.status, "snapshot_created");
        assert_eq!(snapshot.artifact_kind, "provisional_snapshot");
        assert_eq!(snapshot.revision, None);
        assert!(snapshot.export_directory.contains("/snapshots/2025-07/"));
        assert!(matches!(
            service.export_at("2025-08", ExportMode::ProvisionalSnapshot, july_2025),
            Err(CloseMonthError::FutureMonth)
        ));
    }

    #[test]
    fn existing_month_lease_blocks_close_until_released() {
        let directory = tempfile::tempdir().expect("dir");
        let config = config(&directory);
        let store = settled_store(&config);
        let service = CloseMonthService::new(
            &config,
            FakePtax {
                result: Arc::new(Mutex::new(Ok(None))),
            },
        )
        .expect("service");
        let close_time = 1_754_006_400_000;
        store
            .acquire_monthly_export_lease(
                "2025-07",
                "competing-owner",
                close_time,
                close_time + EXPORT_LEASE_MS,
            )
            .expect("lease");
        assert!(matches!(
            service.export_at("2025-07", ExportMode::FinalClose, close_time),
            Err(CloseMonthError::Busy)
        ));
        store
            .release_monthly_export_lease("2025-07", "competing-owner")
            .expect("release");
        service
            .export_at("2025-07", ExportMode::FinalClose, close_time)
            .expect("close after release");
    }

    #[test]
    fn atomic_publication_ignores_stale_temp_and_rejects_visible_mismatch() {
        let directory = tempfile::tempdir().expect("dir");
        let target = directory.path().join("revision-1");
        let stale = directory.path().join(".revision-1.tmp-owner");
        std::fs::create_dir(&stale).expect("stale temp");
        std::fs::write(stale.join("partial"), b"incomplete").expect("partial");
        let close = StoredMonthClose {
            month: "2025-07".to_owned(),
            revision: 1,
            artifact_kind: "final_close".to_owned(),
            canonical_json: b"{\"complete\":true}\n".to_vec(),
            accountant_csv: b"a,b\n".to_vec(),
            manifest_json: b"{\"manifest\":true}\n".to_vec(),
        };
        write_exports_atomic(&target, "2025-07", &close, "owner").expect("publish");
        assert!(!stale.exists());
        verify_published_files(&target, "2025-07", &close).expect("complete visible revision");
        std::fs::write(target.join("recebi-2025-07.evidence.json"), b"tampered").expect("tamper");
        assert!(matches!(
            write_exports_atomic(&target, "2025-07", &close, "other-owner"),
            Err(CloseMonthError::Integrity)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn data_export_directories_and_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

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
        let output = service
            .export_at("2025-07", ExportMode::FinalClose, 1_754_006_400_000)
            .expect("close");
        assert_eq!(
            std::fs::metadata(&config.recebi.data_dir)
                .expect("data metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&output.export_directory)
                .expect("export metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for entry in std::fs::read_dir(&output.export_directory).expect("export files") {
            assert_eq!(
                entry
                    .expect("entry")
                    .metadata()
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
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
            service.export_at("2025-13", ExportMode::FinalClose, 1_754_006_400_000),
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
            service.export_at("2025-07", ExportMode::FinalClose, 1_754_006_400_000),
            Err(CloseMonthError::Integrity)
        ));
    }
}
