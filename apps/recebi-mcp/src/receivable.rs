use std::time::{SystemTime, UNIX_EPOCH};

use getrandom::fill;
use recebi_core::{
    AtomicAmount, BoundedText, PaymentRequest, ReceivableId, ReceivableState, Reference,
    limits::MAX_PUBLIC_LABEL_BYTES,
};
use recebi_store::{ReceivableStore, StoreError, StoredReceivable};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::AppConfig;
use crate::qr::{self, QrError};

#[derive(Clone)]
pub struct ReceivableService {
    config: AppConfig,
    store: ReceivableStore,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequestInput {
    pub receivable_id: String,
    pub amount: String,
    pub public_label: String,
}

#[derive(Debug, Serialize)]
pub struct CreateRequestResult {
    receivable_id: String,
    state: &'static str,
    amount: String,
    reference: String,
    solana_pay_url: String,
    qr_image_path: Option<String>,
    attachment_marker: Option<String>,
    qr_error: Option<&'static str>,
    custody: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderQrInput {
    pub receivable_id: String,
}

#[derive(Debug, Serialize)]
pub struct RenderQrResult {
    receivable_id: String,
    state: &'static str,
    qr_image_path: String,
    attachment_marker: String,
    png_sha256: String,
}

#[derive(Debug, Error)]
pub enum CreateRequestError {
    #[error("request input is invalid")]
    InvalidInput,
    #[error("secure random generation is unavailable")]
    RandomUnavailable,
    #[error("receivable ID is already bound to different terms")]
    IdempotencyConflict,
    #[error("payment reference collision; retry the request")]
    ReferenceCollision,
    #[error("local receivable storage is unavailable")]
    StorageUnavailable,
}

#[derive(Debug, Error)]
pub enum RenderQrError {
    #[error("request input is invalid")]
    InvalidInput,
    #[error("receivable was not found")]
    NotFound,
    #[error("local receivable storage is unavailable")]
    StorageUnavailable,
    #[error("QR rendering is unavailable")]
    RenderingUnavailable,
}

impl ReceivableService {
    /// # Errors
    ///
    /// Returns a redacted error if trusted storage cannot be initialized.
    pub fn new(config: AppConfig) -> Result<Self, CreateRequestError> {
        config
            .ensure_data_directory()
            .map_err(|_| CreateRequestError::StorageUnavailable)?;
        let store = ReceivableStore::open(config.database_path())
            .map_err(|_| CreateRequestError::StorageUnavailable)?;
        Ok(Self { config, store })
    }

    /// # Errors
    ///
    /// Validates all operator input, generates a CSPRNG reference, and writes
    /// the receivable plus its append-only creation event atomically.
    pub fn create(
        &self,
        input: CreateRequestInput,
    ) -> Result<CreateRequestResult, CreateRequestError> {
        let receivable_id =
            ReceivableId::new(input.receivable_id).map_err(|_| CreateRequestError::InvalidInput)?;
        let public_label = BoundedText::<MAX_PUBLIC_LABEL_BYTES>::new(input.public_label)
            .map_err(|_| CreateRequestError::InvalidInput)?;
        let amount = AtomicAmount::from_decimal(&input.amount, self.config.recebi.token_decimals)
            .map_err(|_| CreateRequestError::InvalidInput)?;
        let mut bytes = [0_u8; 32];
        fill(&mut bytes).map_err(|_| CreateRequestError::RandomUnavailable)?;
        let request = PaymentRequest {
            receivable_id,
            recipient: self.config.recebi.merchant_wallet.clone(),
            mint: self.config.recebi.accepted_mint.clone(),
            amount,
            decimals: self.config.recebi.token_decimals,
            reference: Reference::from_bytes(bytes),
            public_label,
        };
        let created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CreateRequestError::StorageUnavailable)?
            .as_millis();
        let created_at_unix_ms = i64::try_from(created_at_unix_ms)
            .map_err(|_| CreateRequestError::StorageUnavailable)?;
        let stored = self
            .store
            .create_or_get(request, created_at_unix_ms)
            .map_err(|error| map_store_error(&error))?;
        let qr = qr::render_to_file(
            &self.config.recebi.data_dir,
            stored.request.receivable_id.as_str(),
            &stored.solana_pay_url,
        );
        Ok(result_from(stored, qr))
    }

    /// Render the persisted Solana Pay URL into a private, Telegram-compatible
    /// PNG. Payment terms are read from storage and cannot be supplied by MCP.
    pub fn render_qr(&self, input: RenderQrInput) -> Result<RenderQrResult, RenderQrError> {
        let receivable_id =
            ReceivableId::new(input.receivable_id).map_err(|_| RenderQrError::InvalidInput)?;
        let stored = self
            .store
            .get(&receivable_id)
            .map_err(|_| RenderQrError::StorageUnavailable)?
            .ok_or(RenderQrError::NotFound)?;
        let artifact = qr::render_to_file(
            &self.config.recebi.data_dir,
            receivable_id.as_str(),
            &stored.solana_pay_url,
        )
        .map_err(|error| map_qr_error(&error))?;
        Ok(RenderQrResult {
            receivable_id: receivable_id.as_str().to_owned(),
            state: state_name(stored.state),
            qr_image_path: artifact.path.to_string_lossy().into_owned(),
            attachment_marker: artifact.attachment_marker,
            png_sha256: artifact.png_sha256,
        })
    }
}

fn map_store_error(error: &StoreError) -> CreateRequestError {
    match error {
        StoreError::IdempotencyConflict => CreateRequestError::IdempotencyConflict,
        StoreError::ReferenceReuse => CreateRequestError::ReferenceCollision,
        StoreError::Unavailable
        | StoreError::Integrity
        | StoreError::InvalidTransition
        | StoreError::Replay
        | StoreError::ReconciliationBusy
        | StoreError::MonthlyExportBusy
        | StoreError::ConcurrentMutation => CreateRequestError::StorageUnavailable,
    }
}

fn result_from(
    stored: StoredReceivable,
    qr: Result<qr::QrArtifact, QrError>,
) -> CreateRequestResult {
    let (qr_image_path, attachment_marker, qr_error) = match qr {
        Ok(artifact) => (
            Some(artifact.path.to_string_lossy().into_owned()),
            Some(artifact.attachment_marker),
            None,
        ),
        Err(error) => (None, None, Some(qr_error_name(&error))),
    };
    CreateRequestResult {
        receivable_id: stored.request.receivable_id.as_str().to_owned(),
        state: "open",
        amount: stored.request.amount.format(stored.request.decimals),
        reference: stored.request.reference.as_base58(),
        solana_pay_url: stored.solana_pay_url,
        qr_image_path,
        attachment_marker,
        qr_error,
        custody: "none",
    }
}

fn qr_error_name(error: &QrError) -> &'static str {
    match error {
        QrError::PayloadTooLarge | QrError::Encoding | QrError::ImageTooLarge => {
            "rendering_unavailable"
        }
        QrError::StorageUnavailable | QrError::RandomUnavailable => "storage_unavailable",
    }
}

fn map_qr_error(error: &QrError) -> RenderQrError {
    match error {
        QrError::PayloadTooLarge | QrError::Encoding | QrError::ImageTooLarge => {
            RenderQrError::RenderingUnavailable
        }
        QrError::StorageUnavailable | QrError::RandomUnavailable => {
            RenderQrError::StorageUnavailable
        }
    }
}

fn state_name(state: ReceivableState) -> &'static str {
    match state {
        ReceivableState::Open => "open",
        ReceivableState::PaymentVerified => "payment_verified",
        ReceivableState::NeedsReview => "needs_review",
        ReceivableState::Cancelled => "cancelled",
        ReceivableState::SettledWithVariance => "settled_with_variance",
        ReceivableState::ValuationPending => "valuation_pending",
        ReceivableState::Reconciled => "reconciled",
        ReceivableState::Closed => "closed",
    }
}
