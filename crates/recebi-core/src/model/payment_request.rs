use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::{
    AtomicAmount, BoundedText, CoreError, PublicKey, Reference,
    limits::{MAX_PUBLIC_LABEL_BYTES, MAX_RECEIVABLE_ID_BYTES, MAX_SOLANA_PAY_URL_BYTES},
};

pub type ReceivableId = BoundedText<MAX_RECEIVABLE_ID_BYTES>;

/// Deterministic, non-interactive Solana Pay transfer request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentRequest {
    pub receivable_id: ReceivableId,
    pub recipient: PublicKey,
    pub mint: PublicKey,
    pub amount: AtomicAmount,
    pub decimals: u8,
    pub reference: Reference,
    pub public_label: BoundedText<MAX_PUBLIC_LABEL_BYTES>,
}

impl PaymentRequest {
    /// # Errors
    ///
    /// Returns a typed error if the canonical URL exceeds its bounded size.
    pub fn solana_pay_url(&self) -> Result<String, CoreError> {
        let label = utf8_percent_encode(self.public_label.as_str(), NON_ALPHANUMERIC).to_string();
        let url = format!(
            "solana:{}?amount={}&spl-token={}&reference={}&label={label}",
            self.recipient.as_str(),
            self.amount.format(self.decimals),
            self.mint.as_str(),
            self.reference.as_base58(),
        );
        if url.len() > MAX_SOLANA_PAY_URL_BYTES {
            Err(CoreError::TextTooLong)
        } else {
            Ok(url)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PaymentRequest, ReceivableId};
    use crate::{AtomicAmount, BoundedText, PublicKey, Reference, limits::MAX_PUBLIC_LABEL_BYTES};

    #[test]
    fn produces_a_canonical_solana_pay_transfer_request_without_memo() {
        let request = PaymentRequest {
            receivable_id: ReceivableId::new("ACME-412").expect("id"),
            recipient: PublicKey::parse("11111111111111111111111111111111").expect("recipient"),
            mint: PublicKey::parse("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").expect("mint"),
            amount: AtomicAmount::from_decimal("0.01", 6).expect("amount"),
            decimals: 6,
            reference: Reference::from_bytes([7; 32]),
            public_label: BoundedText::<MAX_PUBLIC_LABEL_BYTES>::new("ACME 412").expect("label"),
        };
        let url = request.solana_pay_url().expect("url");
        assert_eq!(
            url,
            "solana:11111111111111111111111111111111?amount=0.01&spl-token=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&reference=US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx&label=ACME%20412"
        );
        assert!(!url.contains("memo="));
    }
}
