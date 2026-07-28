use std::time::{SystemTime, UNIX_EPOCH};

use getrandom::fill;
use recebi_core::{
    AtomicAmount, BoundedText, PaymentRequest, ReceivableId, Reference,
    limits::MAX_PUBLIC_LABEL_BYTES,
};
use recebi_store::{ReceivableStore, StoreError, StoredReceivable};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::AppConfig;

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
    custody: &'static str,
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
        Ok(result_from(stored))
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
        | StoreError::ReconciliationBusy => CreateRequestError::StorageUnavailable,
    }
}

fn result_from(stored: StoredReceivable) -> CreateRequestResult {
    CreateRequestResult {
        receivable_id: stored.request.receivable_id.as_str().to_owned(),
        state: "open",
        amount: stored.request.amount.format(stored.request.decimals),
        reference: stored.request.reference.as_base58(),
        solana_pay_url: stored.solana_pay_url,
        custody: "none",
    }
}
