use serde::{Deserialize, Serialize};

use crate::CoreError;

/// Authoritative token quantity. Decimal rendering is presentation only.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AtomicAmount(u64);

impl AtomicAmount {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Parses a non-zero, non-scientific decimal into exact atomic units.
    ///
    /// # Errors
    ///
    /// Rejects negative values, unsupported precision, and overflow.
    pub fn from_decimal(value: &str, decimals: u8) -> Result<Self, CoreError> {
        if value.is_empty() || value.starts_with(['-', '+']) || value.contains(['e', 'E']) {
            return Err(CoreError::InvalidAmount);
        }
        let mut parts = value.split('.');
        let whole = parts.next().ok_or(CoreError::InvalidAmount)?;
        let fraction = parts.next();
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|part| {
                part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(CoreError::InvalidAmount);
        }
        let fraction = fraction.unwrap_or_default();
        if fraction.len() > usize::from(decimals) {
            return Err(CoreError::ExcessivePrecision);
        }
        let scale = 10_u64
            .checked_pow(u32::from(decimals))
            .ok_or(CoreError::AmountOverflow)?;
        let whole = whole
            .parse::<u64>()
            .map_err(|_| CoreError::AmountOverflow)?;
        let fractional_value = if fraction.is_empty() {
            0
        } else {
            fraction
                .parse::<u64>()
                .map_err(|_| CoreError::AmountOverflow)?
        };
        let padding = usize::from(decimals) - fraction.len();
        let fractional_units = fractional_value
            .checked_mul(
                10_u64
                    .checked_pow(u32::try_from(padding).map_err(|_| CoreError::AmountOverflow)?)
                    .ok_or(CoreError::AmountOverflow)?,
            )
            .ok_or(CoreError::AmountOverflow)?;
        let atomic = whole
            .checked_mul(scale)
            .and_then(|scaled| scaled.checked_add(fractional_units))
            .ok_or(CoreError::AmountOverflow)?;
        if atomic == 0 {
            Err(CoreError::ZeroAmount)
        } else {
            Ok(Self(atomic))
        }
    }

    #[must_use]
    pub fn format(self, decimals: u8) -> String {
        if decimals == 0 {
            return self.0.to_string();
        }
        let scale = 10_u64.pow(u32::from(decimals));
        let whole = self.0 / scale;
        let fraction = self.0 % scale;
        let mut rendered = format!("{whole}.{fraction:0width$}", width = usize::from(decimals));
        while rendered.ends_with('0') {
            rendered.pop();
        }
        if rendered.ends_with('.') {
            rendered.pop();
        }
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::AtomicAmount;
    use crate::CoreError;

    #[test]
    fn parses_and_formats_exact_amounts() {
        let amount = AtomicAmount::from_decimal("12.3400", 6).expect("valid amount");
        assert_eq!(amount.get(), 12_340_000);
        assert_eq!(amount.format(6), "12.34");
    }

    #[test]
    fn rejects_zero_precision_loss_and_overflow() {
        assert_eq!(
            AtomicAmount::from_decimal("0", 6),
            Err(CoreError::ZeroAmount)
        );
        assert_eq!(
            AtomicAmount::from_decimal("0.0000001", 6),
            Err(CoreError::ExcessivePrecision)
        );
        assert_eq!(
            AtomicAmount::from_decimal("1e2", 6),
            Err(CoreError::InvalidAmount)
        );
        assert_eq!(
            AtomicAmount::from_decimal("18446744073709551616", 0),
            Err(CoreError::AmountOverflow)
        );
    }
}
