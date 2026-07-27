use serde::{Deserialize, Serialize};

/// The bounded lifecycle vocabulary for a future receivable record.
///
/// Phase 1 defines the vocabulary only. It does not create, mutate, or
/// persist receivables.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceivableState {
    Open,
    PaymentVerified,
    NeedsReview,
    Cancelled,
    ValuationPending,
    Reconciled,
    Closed,
}

#[cfg(test)]
mod tests {
    use super::ReceivableState;

    #[test]
    fn state_serializes_to_a_bounded_snake_case_vocabulary() {
        let state =
            serde_json::to_string(&ReceivableState::PaymentVerified).expect("state serializes");
        assert_eq!(state, "\"payment_verified\"");
    }
}
