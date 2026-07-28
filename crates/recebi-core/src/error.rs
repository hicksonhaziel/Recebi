use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("text is empty")]
    EmptyText,
    #[error("text exceeds configured limit")]
    TextTooLong,
    #[error("invalid payment reference")]
    InvalidReference,
    #[error("amount must be greater than zero")]
    ZeroAmount,
    #[error("amount has unsupported decimal precision")]
    ExcessivePrecision,
    #[error("amount is invalid")]
    InvalidAmount,
    #[error("amount exceeds supported range")]
    AmountOverflow,
    #[error("associated token account derivation failed")]
    AtaDerivation,
}
