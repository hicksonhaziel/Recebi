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
    SettledWithVariance,
    ValuationPending,
    Reconciled,
    Closed,
}

/// The bounded operator-approved dispositions for a review candidate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewResolutionAction {
    IgnoreCandidateAndReopen,
    CancelUnpaid,
    AcceptUnderpaymentWithVariance,
}

impl ReviewResolutionAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IgnoreCandidateAndReopen => "ignore_candidate_and_reopen",
            Self::CancelUnpaid => "cancel_unpaid",
            Self::AcceptUnderpaymentWithVariance => "accept_underpayment_with_variance",
        }
    }
}

/// A bounded commercial reason for accepting a deterministically verified
/// underpayment. Free-form model or transaction text is never accepted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VarianceReason {
    RoundingAdjustment,
    CommercialDiscount,
    MerchantWriteOff,
}

impl VarianceReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RoundingAdjustment => "rounding_adjustment",
            Self::CommercialDiscount => "commercial_discount",
            Self::MerchantWriteOff => "merchant_write_off",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReceivableState, ReviewResolutionAction, VarianceReason};

    #[test]
    fn state_serializes_to_a_bounded_snake_case_vocabulary() {
        let state =
            serde_json::to_string(&ReceivableState::PaymentVerified).expect("state serializes");
        assert_eq!(state, "\"payment_verified\"");
        assert_eq!(
            serde_json::to_string(&ReviewResolutionAction::CancelUnpaid)
                .expect("action serializes"),
            "\"cancel_unpaid\""
        );
        assert_eq!(
            serde_json::to_string(&VarianceReason::MerchantWriteOff).expect("reason serializes"),
            "\"merchant_write_off\""
        );
    }
}
