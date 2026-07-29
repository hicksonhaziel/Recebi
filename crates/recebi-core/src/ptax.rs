use serde::{Deserialize, Serialize};

use crate::{AtomicAmount, CoreError};

pub const PTAX_DECIMAL_SCALE: u64 = 100_000;
pub const PTAX_SOURCE_ID: &str = "bcb_ptax_v1_cotacao_dolar_dia";
pub const PTAX_POLICY_VERSION: &str = "strict_same_day_closing_v1";
pub const NOMINAL_USDC_USD_METHOD: &str = "nominal_usdc_equals_usd";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PtaxDate(String);

impl PtaxDate {
    /// Parses a strict proleptic-Gregorian `YYYY-MM-DD` date.
    ///
    /// # Errors
    ///
    /// Returns a typed error for malformed or impossible dates.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, CoreError> {
        let value = value.as_ref();
        let bytes = value.as_bytes();
        if bytes.len() != 10
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || !bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        {
            return Err(CoreError::InvalidPtaxDate);
        }
        let year = parse_u32(&value[0..4])?;
        let month = parse_u32(&value[5..7])?;
        let day = parse_u32(&value[8..10])?;
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(year) => 29,
            2 => 28,
            _ => return Err(CoreError::InvalidPtaxDate),
        };
        if year == 0 || day == 0 || day > days {
            return Err(CoreError::InvalidPtaxDate);
        }
        Ok(Self(value.to_owned()))
    }

    /// Converts a non-negative Unix timestamp to its UTC calendar date.
    ///
    /// # Errors
    ///
    /// Negative timestamps and years outside the four-digit evidence format
    /// are rejected.
    pub fn from_unix_seconds(timestamp: i64) -> Result<Self, CoreError> {
        if timestamp < 0 {
            return Err(CoreError::InvalidPtaxDate);
        }
        let days = timestamp / 86_400;
        let (year, month, day) = civil_from_days(days);
        if !(1..=9_999).contains(&year) {
            return Err(CoreError::InvalidPtaxDate);
        }
        Self::parse(format!("{year:04}-{month:02}-{day:02}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn as_bcb_parameter(&self) -> String {
        format!("{}-{}-{}", &self.0[5..7], &self.0[8..10], &self.0[0..4])
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PtaxDecimal(u64);

impl PtaxDecimal {
    /// Parses a positive decimal into exactly five fixed decimal places.
    ///
    /// # Errors
    ///
    /// Signs, exponent notation, zero, excess precision, and overflow fail.
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        if value.is_empty() || value.starts_with(['+', '-']) || value.contains(['e', 'E']) {
            return Err(CoreError::InvalidPtaxDecimal);
        }
        let mut parts = value.split('.');
        let whole = parts.next().ok_or(CoreError::InvalidPtaxDecimal)?;
        let fraction = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() > 5
        {
            return Err(CoreError::InvalidPtaxDecimal);
        }
        let whole = whole
            .parse::<u64>()
            .map_err(|_| CoreError::InvalidPtaxDecimal)?;
        let fraction_value = if fraction.is_empty() {
            0
        } else {
            fraction
                .parse::<u64>()
                .map_err(|_| CoreError::InvalidPtaxDecimal)?
        };
        let padding =
            u32::try_from(5_usize - fraction.len()).map_err(|_| CoreError::InvalidPtaxDecimal)?;
        let scaled = whole
            .checked_mul(PTAX_DECIMAL_SCALE)
            .and_then(|value| value.checked_add(fraction_value * 10_u64.pow(padding)))
            .ok_or(CoreError::InvalidPtaxDecimal)?;
        if scaled == 0 {
            return Err(CoreError::InvalidPtaxDecimal);
        }
        Ok(Self(scaled))
    }

    #[must_use]
    pub const fn scaled(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn canonical(self) -> String {
        format!(
            "{}.{:05}",
            self.0 / PTAX_DECIMAL_SCALE,
            self.0 % PTAX_DECIMAL_SCALE
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PtaxQuoteCandidate {
    pub purchase: PtaxDecimal,
    pub sale: PtaxDecimal,
    pub quote_date: PtaxDate,
    pub bulletin_type: Option<String>,
    pub bulletin_timestamp: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PtaxEvidence {
    pub operation_date: PtaxDate,
    pub quote_date: PtaxDate,
    pub purchase: String,
    pub sale: String,
    pub bulletin_type: Option<String>,
    pub bulletin_timestamp: String,
    pub retrieved_at_unix_ms: i64,
    pub response_sha256: String,
    pub source_id: String,
    pub policy_version: String,
}

/// Selects the one same-day closing quote returned by the pinned daily API.
///
/// # Errors
///
/// Empty rows mean unavailable (`Ok(None)`). Duplicate, mismatched, or
/// non-closing rows fail closed.
pub fn select_strict_same_day_quote(
    operation_date: &PtaxDate,
    rows: Vec<PtaxQuoteCandidate>,
    retrieved_at_unix_ms: i64,
    response_sha256: String,
) -> Result<Option<PtaxEvidence>, CoreError> {
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(CoreError::InvalidPtaxQuote);
    }
    let row = rows.into_iter().next().ok_or(CoreError::InvalidPtaxQuote)?;
    if &row.quote_date != operation_date
        || row
            .bulletin_type
            .as_deref()
            .is_some_and(|kind| kind != "Fechamento PTAX")
        || response_sha256.len() != 64
        || !response_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(CoreError::InvalidPtaxQuote);
    }
    Ok(Some(PtaxEvidence {
        operation_date: operation_date.clone(),
        quote_date: row.quote_date,
        purchase: row.purchase.canonical(),
        sale: row.sale.canonical(),
        bulletin_type: row.bulletin_type,
        bulletin_timestamp: row.bulletin_timestamp,
        retrieved_at_unix_ms,
        response_sha256,
        source_id: PTAX_SOURCE_ID.to_owned(),
        policy_version: PTAX_POLICY_VERSION.to_owned(),
    }))
}

/// Calculates a BRL-cent reference with integer half-up rounding.
///
/// This is a nominal record-keeping reference, not verified USDC fair value.
///
/// # Errors
///
/// Missing operator value, unsupported token precision, and overflow fail.
pub fn nominal_brl_reference_cents(
    token_amount: AtomicAmount,
    token_decimals: u8,
    sale: PtaxDecimal,
) -> Result<u64, CoreError> {
    let token_scale = 10_u128
        .checked_pow(u32::from(token_decimals))
        .ok_or(CoreError::ValuationOverflow)?;
    let usd_atomic = u128::from(token_amount.get());
    let numerator = usd_atomic
        .checked_mul(u128::from(sale.scaled()))
        .and_then(|value| value.checked_mul(100))
        .ok_or(CoreError::ValuationOverflow)?;
    let denominator = token_scale
        .checked_mul(u128::from(PTAX_DECIMAL_SCALE))
        .ok_or(CoreError::ValuationOverflow)?;
    let rounded = numerator
        .checked_add(denominator / 2)
        .ok_or(CoreError::ValuationOverflow)?
        / denominator;
    u64::try_from(rounded).map_err(|_| CoreError::ValuationOverflow)
}

fn parse_u32(value: &str) -> Result<u32, CoreError> {
    value.parse().map_err(|_| CoreError::InvalidPtaxDate)
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

// Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(date: &str, sale: &str) -> PtaxQuoteCandidate {
        PtaxQuoteCandidate {
            purchase: PtaxDecimal::parse("5.11710").expect("purchase"),
            sale: PtaxDecimal::parse(sale).expect("sale"),
            quote_date: PtaxDate::parse(date).expect("date"),
            bulletin_type: None,
            bulletin_timestamp: format!("{date} 13:25:31.150278"),
        }
    }

    #[test]
    fn validates_dates_and_converts_settlement_time_in_utc() {
        assert_eq!(
            PtaxDate::from_unix_seconds(1_753_717_531)
                .expect("date")
                .as_str(),
            "2025-07-28"
        );
        assert!(PtaxDate::parse("2024-02-29").is_ok());
        assert_eq!(
            PtaxDate::parse("2026-02-29"),
            Err(CoreError::InvalidPtaxDate)
        );
        assert_eq!(
            PtaxDate::parse("07-28-2026"),
            Err(CoreError::InvalidPtaxDate)
        );
    }

    #[test]
    fn parses_ptax_decimal_without_binary_float() {
        assert_eq!(
            PtaxDecimal::parse("5.1177").expect("decimal").canonical(),
            "5.11770"
        );
        for invalid in ["", "0", "-5.1", "5.123456", "5e0", "x"] {
            assert_eq!(
                PtaxDecimal::parse(invalid),
                Err(CoreError::InvalidPtaxDecimal)
            );
        }
    }

    #[test]
    fn enforces_same_day_single_quote_and_unavailable_dates() {
        let date = PtaxDate::parse("2026-07-28").expect("date");
        let hash = "ab".repeat(32);
        let evidence =
            select_strict_same_day_quote(&date, vec![row(date.as_str(), "5.11770")], 42, hash)
                .expect("selection")
                .expect("available");
        assert_eq!(evidence.sale, "5.11770");
        assert_eq!(evidence.source_id, PTAX_SOURCE_ID);
        assert_eq!(
            select_strict_same_day_quote(&date, Vec::new(), 42, "ab".repeat(32)).expect("weekend"),
            None
        );
        assert_eq!(
            select_strict_same_day_quote(
                &date,
                vec![row(date.as_str(), "5.1"), row(date.as_str(), "5.1")],
                42,
                "ab".repeat(32)
            ),
            Err(CoreError::InvalidPtaxQuote)
        );
        assert_eq!(
            select_strict_same_day_quote(
                &date,
                vec![row("2026-07-27", "5.1")],
                42,
                "ab".repeat(32)
            ),
            Err(CoreError::InvalidPtaxQuote)
        );
    }

    #[test]
    fn calculates_known_value_and_half_up_rounding_with_assumption_disclosed() {
        let sale = PtaxDecimal::parse("5.11770").expect("sale");
        assert_eq!(
            nominal_brl_reference_cents(AtomicAmount::new(100_000), 6, sale).expect("BRL"),
            51
        );
        assert_eq!(
            nominal_brl_reference_cents(
                AtomicAmount::new(1),
                2,
                PtaxDecimal::parse("0.50000").expect("sale")
            )
            .expect("rounding"),
            1
        );
        assert_eq!(NOMINAL_USDC_USD_METHOD, "nominal_usdc_equals_usd");
    }
}
