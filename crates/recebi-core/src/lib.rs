//! Pure, offline domain types for Recebi.
//!
//! This crate intentionally has no network, filesystem, database, LLM, MCP,
//! wallet, signer, or transaction-submission dependency.

pub mod error;
pub mod limits;
pub mod model;
pub mod ptax;
pub mod settlement;
pub mod solana;
pub mod transaction_decode;

pub use error::CoreError;
pub use model::{
    AtomicAmount, BoundedText, GenesisHash, PaymentRequest, Provenance, PublicKey, ReceivableId,
    ReceivableState, Reference, ReviewResolutionAction,
};
pub use ptax::{
    NOMINAL_USDC_USD_METHOD, PtaxDate, PtaxDecimal, PtaxEvidence, PtaxQuoteCandidate,
    nominal_brl_reference_cents, select_strict_same_day_quote,
};
pub use settlement::{
    AccountMeta, CompiledInstruction, SettlementEvidence, SettlementExpectation, SettlementVerdict,
    TokenAccountSnapshot, TransactionSnapshot, verify_settlement, verify_settlement_once,
};
pub use transaction_decode::{
    RawTokenAccount, RawTransaction, TransactionDecodeVerdict, decode_transaction,
};
