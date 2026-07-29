use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("invalid cluster genesis hash")]
    InvalidGenesisHash,
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
    #[error("PTAX date is invalid")]
    InvalidPtaxDate,
    #[error("PTAX decimal is invalid")]
    InvalidPtaxDecimal,
    #[error("PTAX response does not satisfy the quote policy")]
    InvalidPtaxQuote,
    #[error("valuation amount exceeds supported range")]
    ValuationOverflow,
}
