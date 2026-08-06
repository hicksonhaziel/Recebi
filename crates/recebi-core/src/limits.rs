//! Limits shared by all Recebi layers.

pub const MAX_RECEIVABLE_ID_BYTES: usize = 64;
pub const MAX_PUBLIC_LABEL_BYTES: usize = 120;
pub const MAX_SOLANA_PAY_URL_BYTES: usize = 2_048;
pub const MAX_TOOL_RESULT_BYTES: usize = 4_096;
pub const MAX_RPC_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_RPC_REQUEST_BYTES: usize = 4 * 1024;
pub const MAX_CANDIDATE_SIGNATURES: usize = 8;
pub const MAX_RPC_CALLS_PER_RECEIVABLE: usize = 10;
pub const MAX_RECONCILE_RECEIVABLES: usize = 10;
pub const MAX_ANOMALY_SAMPLES: usize = 3;
pub const RPC_TIMEOUT_SECS: u64 = 5;
pub const MAX_PTAX_RESPONSE_BYTES: usize = 64 * 1024;
/// The official BCB endpoint is materially slower than Solana RPC, especially
/// on a cold TLS connection, so it carries its own bound instead of reusing the
/// Solana timeout.
pub const PTAX_TIMEOUT_SECS: u64 = 20;
/// Total bounded attempts for one official quote. Only transport failures are
/// retried; a malformed or oversized response still fails closed immediately.
pub const PTAX_MAX_ATTEMPTS: u8 = 3;
pub const PTAX_RETRY_DELAY_MS: u64 = 400;
pub const MAX_MONTH_EXPORT_ROWS: usize = 500;
