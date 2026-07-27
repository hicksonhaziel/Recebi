//! Pure, offline domain types for Recebi.
//!
//! This crate intentionally has no network, filesystem, database, LLM, MCP,
//! wallet, signer, or transaction-submission dependency.

pub mod error;
pub mod limits;
pub mod model;

pub use error::CoreError;
pub use model::{AtomicAmount, BoundedText, Provenance, PublicKey, ReceivableState};
