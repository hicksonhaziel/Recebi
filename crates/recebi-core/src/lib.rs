//! Pure, offline domain types for Recebi.
//!
//! This crate intentionally has no network, filesystem, database, LLM, MCP,
//! wallet, signer, or transaction-submission dependency.

pub mod error;
pub mod limits;
pub mod model;
pub mod settlement;
pub mod solana;

pub use error::CoreError;
pub use model::{
    AtomicAmount, BoundedText, PaymentRequest, Provenance, PublicKey, ReceivableId,
    ReceivableState, Reference,
};
pub use settlement::{
    AccountMeta, CompiledInstruction, SettlementEvidence, SettlementExpectation, SettlementVerdict,
    TokenAccountSnapshot, TransactionSnapshot, verify_settlement, verify_settlement_once,
};
