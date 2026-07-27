use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("text is empty")]
    EmptyText,
    #[error("text exceeds configured limit")]
    TextTooLong,
}
