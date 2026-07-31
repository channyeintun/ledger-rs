//! Monetary amounts and currencies.
//!
//! Two rules are enforced by construction here, because they are the two that
//! silently destroy value when left to convention:
//!
//! 1. An amount never travels without its currency. There is no bare `Decimal`
//!    in any signature that means "money"; cross-currency arithmetic is a
//!    compile-time-shaped error surfaced as [`MoneyError::CurrencyMismatch`]
//!    rather than a wrong number.
//! 2. An amount that the database would have to round is rejected at the edge.
//!    `NUMERIC(20, 8)` rounds silently on INSERT, so a 9-decimal input would be
//!    accepted, stored as something else, and never reconcile.
//!
//! Floating point appears nowhere in this crate. See `CLAUDE.md`.

use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Maximum number of fractional digits an amount may carry, matching the
/// `NUMERIC(20, 8)` columns in the schema.
pub const MAX_SCALE: u32 = 8;

/// Number of integer digits available, matching `NUMERIC(20, 8)`.
pub const MAX_INTEGER_DIGITS: u32 = 12;

/// Exclusive upper bound on `|amount|`, i.e. `10^MAX_INTEGER_DIGITS`.
fn max_magnitude() -> Decimal {
    Decimal::from(1_000_000_000_000_i64)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error(
        "amount has {scale} decimal places but at most {MAX_SCALE} are representable; \
         storing it would silently round"
    )]
    TooManyDecimalPlaces { scale: u32 },

    #[error("amount magnitude exceeds the {MAX_INTEGER_DIGITS} integer digits available")]
    MagnitudeTooLarge,

    #[error("cannot combine {lhs} and {rhs}: amounts in different currencies are not comparable")]
    CurrencyMismatch { lhs: Currency, rhs: Currency },

    #[error("'{0}' is not a recognised ISO 4217 currency code")]
    UnknownCurrency(String),
}

/// An ISO 4217 currency code, validated against a controlled set.
///
/// Validating only the *shape* (`^[A-Z]{3}$`) would let a typo through as a
/// brand-new currency, silently partitioning the ledger into an account nobody
/// can transact with.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, sqlx::Type,
)]
#[serde(try_from = "String", into = "String")]
#[sqlx(transparent)]
pub struct Currency(String);

impl Currency {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Currency {
    type Error = MoneyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if ISO_4217.binary_search(&value.as_str()).is_ok() {
            Ok(Currency(value))
        } else {
            Err(MoneyError::UnknownCurrency(value))
        }
    }
}

impl std::str::FromStr for Currency {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Currency::try_from(s.to_owned())
    }
}

impl From<Currency> for String {
    fn from(c: Currency) -> String {
        c.0
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An amount of money in a specific currency.
///
/// Serialized with the amount as a JSON **string**. A bare JSON number would be
/// parsed as an IEEE-754 double by most clients, and `0.1 + 0.2` is where money
/// bugs come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    #[serde(with = "rust_decimal::serde::str")]
    amount: Decimal,
    currency: Currency,
}

impl Money {
    /// Builds an amount, rejecting anything the ledger could not store exactly.
    pub fn new(amount: Decimal, currency: Currency) -> Result<Self, MoneyError> {
        let scale = amount.scale();
        if scale > MAX_SCALE {
            // `normalize` first: 1.500000000 is representable, it just has
            // trailing zeros. Only genuinely finer precision is an error.
            let normalized = amount.normalize();
            if normalized.scale() > MAX_SCALE {
                return Err(MoneyError::TooManyDecimalPlaces {
                    scale: normalized.scale(),
                });
            }
            return Self::new(normalized, currency);
        }

        if amount.abs() >= max_magnitude() {
            return Err(MoneyError::MagnitudeTooLarge);
        }

        Ok(Money { amount, currency })
    }

    /// Zero in the given currency.
    pub fn zero(currency: Currency) -> Self {
        Money {
            amount: Decimal::ZERO,
            currency,
        }
    }

    pub fn amount(&self) -> Decimal {
        self.amount
    }

    pub fn currency(&self) -> &Currency {
        &self.currency
    }

    pub fn is_positive(&self) -> bool {
        self.amount > Decimal::ZERO
    }

    pub fn is_negative(&self) -> bool {
        self.amount < Decimal::ZERO
    }

    fn require_same_currency(&self, other: &Money) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch {
                lhs: self.currency.clone(),
                rhs: other.currency.clone(),
            })
        }
    }

    pub fn checked_add(&self, other: &Money) -> Result<Money, MoneyError> {
        self.require_same_currency(other)?;
        Money::new(self.amount + other.amount, self.currency.clone())
    }

    pub fn checked_sub(&self, other: &Money) -> Result<Money, MoneyError> {
        self.require_same_currency(other)?;
        Money::new(self.amount - other.amount, self.currency.clone())
    }

    /// Ordering within a single currency. Returns an error rather than a
    /// misleading `false` when the currencies differ.
    pub fn checked_lt(&self, other: &Money) -> Result<bool, MoneyError> {
        self.require_same_currency(other)?;
        Ok(self.amount < other.amount)
    }

    /// Canonical text form, used for request hashing so that `10` and `10.00`
    /// are recognised as the same payload on an idempotent replay.
    pub fn canonical(&self) -> String {
        format!("{} {}", self.amount.normalize(), self.currency)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.amount, self.currency)
    }
}

/// Active ISO 4217 alphabetic codes, sorted for binary search.
/// Includes the X-series clearing/precious-metal codes, which real ledgers use.
const ISO_4217: &[&str] = &[
    "AED", "AFN", "ALL", "AMD", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT",
    "BGN", "BHD", "BIF", "BMD", "BND", "BOB", "BOV", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD",
    "CAD", "CDF", "CHE", "CHF", "CHW", "CLF", "CLP", "CNY", "COP", "COU", "CRC", "CUP", "CVE",
    "CZK", "DJF", "DKK", "DOP", "DZD", "EGP", "ERN", "ETB", "EUR", "FJD", "FKP", "GBP", "GEL",
    "GHS", "GIP", "GMD", "GNF", "GTQ", "GYD", "HKD", "HNL", "HTG", "HUF", "IDR", "ILS", "INR",
    "IQD", "IRR", "ISK", "JMD", "JOD", "JPY", "KES", "KGS", "KHR", "KMF", "KPW", "KRW", "KWD",
    "KYD", "KZT", "LAK", "LBP", "LKR", "LRD", "LSL", "LYD", "MAD", "MDL", "MGA", "MKD", "MMK",
    "MNT", "MOP", "MRU", "MUR", "MVR", "MWK", "MXN", "MXV", "MYR", "MZN", "NAD", "NGN", "NIO",
    "NOK", "NPR", "NZD", "OMR", "PAB", "PEN", "PGK", "PHP", "PKR", "PLN", "PYG", "QAR", "RON",
    "RSD", "RUB", "RWF", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP", "SLE", "SOS", "SRD",
    "SSP", "STN", "SVC", "SYP", "SZL", "THB", "TJS", "TMT", "TND", "TOP", "TRY", "TTD", "TWD",
    "TZS", "UAH", "UGX", "USD", "USN", "UYI", "UYU", "UYW", "UZS", "VED", "VES", "VND", "VUV",
    "WST", "XAF", "XAG", "XAU", "XBA", "XBB", "XBC", "XBD", "XCD", "XCG", "XDR", "XOF", "XPD",
    "XPF", "XPT", "XSU", "XTS", "XUA", "XXX", "YER", "ZAR", "ZMW", "ZWG",
];

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromStr;

    fn usd() -> Currency {
        Currency::try_from("USD".to_string()).unwrap()
    }

    fn eur() -> Currency {
        Currency::try_from("EUR".to_string()).unwrap()
    }

    #[test]
    fn iso_4217_table_is_sorted_for_binary_search() {
        let mut sorted = ISO_4217.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, ISO_4217, "ISO_4217 must stay sorted");
        sorted.dedup();
        assert_eq!(sorted.len(), ISO_4217.len(), "ISO_4217 must not repeat");
    }

    #[test]
    fn unknown_currency_is_rejected() {
        assert!(Currency::try_from("XYZ".to_string()).is_err());
        assert!(Currency::try_from("usd".to_string()).is_err());
        assert!(Currency::try_from("US".to_string()).is_err());
        assert!(Currency::try_from("USD".to_string()).is_ok());
    }

    #[test]
    fn amount_finer_than_storage_precision_is_rejected() {
        let too_fine = Decimal::from_str("0.000000001").unwrap();
        assert!(matches!(
            Money::new(too_fine, usd()),
            Err(MoneyError::TooManyDecimalPlaces { .. })
        ));
    }

    #[test]
    fn trailing_zeros_beyond_precision_are_normalized_not_rejected() {
        let padded = Decimal::from_str("1.5000000000").unwrap();
        let money = Money::new(padded, usd()).expect("trailing zeros are representable");
        assert_eq!(money.amount(), Decimal::from_str("1.5").unwrap());
    }

    #[test]
    fn magnitude_beyond_storage_range_is_rejected() {
        let huge = Decimal::from_str("1000000000000").unwrap();
        assert!(matches!(
            Money::new(huge, usd()),
            Err(MoneyError::MagnitudeTooLarge)
        ));
        assert!(Money::new(Decimal::from_str("999999999999.99999999").unwrap(), usd()).is_ok());
    }

    #[test]
    fn cross_currency_arithmetic_is_an_error_not_a_number() {
        let a = Money::new(Decimal::from(10), usd()).unwrap();
        let b = Money::new(Decimal::from(10), eur()).unwrap();
        assert!(matches!(
            a.checked_add(&b),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
        assert!(matches!(
            a.checked_lt(&b),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn canonical_form_ignores_trailing_zeros() {
        let a = Money::new(Decimal::from_str("10").unwrap(), usd()).unwrap();
        let b = Money::new(Decimal::from_str("10.00").unwrap(), usd()).unwrap();
        assert_eq!(a.canonical(), b.canonical());
    }

    #[test]
    fn amount_serializes_as_a_string_never_a_json_number() {
        let m = Money::new(Decimal::from_str("10.25").unwrap(), usd()).unwrap();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"amount":"10.25","currency":"USD"}"#);
    }
}
