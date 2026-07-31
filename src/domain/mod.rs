//! Domain types. No SQL, no HTTP — just the shape of the ledger.

pub mod money;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub use money::{Currency, Money, MoneyError};

/// Which side of the double entry a posting sits on.
///
/// Sign convention for the whole system: `balance = SUM(debits) - SUM(credits)`
/// ("debit-positive"). A transfer of X from A to B *credits* A and *debits* B.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "entry_direction", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Debit,
    Credit,
}

impl Direction {
    /// The signed contribution of an amount on this side.
    pub fn signed(self, amount: Decimal) -> Decimal {
        match self {
            Direction::Debit => amount,
            Direction::Credit => -amount,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: Uuid,
    pub name: String,
    /// Balance derived from the entry log. Carries its own currency so it can
    /// never be compared against an amount in another one.
    pub balance: Money,
    /// True only for funding/equity accounts, which hold the credit-normal
    /// side that lets money exist without violating invariant #3.
    pub allows_negative_balance: bool,
    pub created_at: DateTime<Utc>,
}

impl Account {
    pub fn currency(&self) -> &Currency {
        self.balance.currency()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub transaction_id: Uuid,
    pub account_id: Uuid,
    pub direction: Direction,
    /// Always positive. The sign lives in `direction`, never here.
    pub amount: Money,
    pub created_at: DateTime<Utc>,
}

impl Entry {
    /// This entry's contribution to its account's balance.
    pub fn signed_amount(&self) -> Decimal {
        self.direction.signed(self.amount.amount())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: Uuid,
    pub idempotency_key: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

/// A transaction together with its postings — the unit that must balance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionDetail {
    #[serde(flatten)]
    pub transaction: Transaction,
    pub entries: Vec<Entry>,
}

impl TransactionDetail {
    /// Invariant #1, checked in the domain independently of the database
    /// trigger that also enforces it. Both exist on purpose: the trigger makes
    /// a violation unwritable, this makes it unmissable in tests.
    pub fn is_balanced(&self) -> bool {
        self.entries.len() >= 2
            && self
                .entries
                .iter()
                .map(|e| e.amount.currency())
                .all(|c| c == self.entries[0].amount.currency())
            && self
                .entries
                .iter()
                .map(Entry::signed_amount)
                .sum::<Decimal>()
                == Decimal::ZERO
    }
}

/// A validated request to move money between two accounts.
#[derive(Debug, Clone)]
pub struct TransferIntent {
    pub from_account_id: Uuid,
    pub to_account_id: Uuid,
    pub amount: Money,
    pub description: String,
}

impl TransferIntent {
    /// SHA-256 over a canonical, field-tagged encoding of the request.
    ///
    /// Not `serde_json::to_vec`: map ordering and number formatting are not
    /// guaranteed stable across versions, and an unstable fingerprint would
    /// turn honest replays into spurious 409s. Field tags and the `v1` prefix
    /// make the encoding unambiguous and versioned.
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"ledger-rs/transfer/v1\n");
        hasher.update(b"from=");
        hasher.update(self.from_account_id.as_bytes());
        hasher.update(b"\nto=");
        hasher.update(self.to_account_id.as_bytes());
        hasher.update(b"\namount=");
        hasher.update(self.amount.canonical().as_bytes());
        hasher.update(b"\ndescription=");
        hasher.update(self.description.as_bytes());
        hasher.update(b"\n");
        hasher.finalize().into()
    }
}

/// Whether a transfer created a new transaction or replayed an existing one.
/// Drives 201 vs 200 at the HTTP edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOutcome {
    Created,
    Replayed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromStr;

    fn usd() -> Currency {
        Currency::try_from("USD".to_string()).unwrap()
    }

    fn intent(amount: &str, description: &str) -> TransferIntent {
        TransferIntent {
            from_account_id: Uuid::from_u128(1),
            to_account_id: Uuid::from_u128(2),
            amount: Money::new(Decimal::from_str(amount).unwrap(), usd()).unwrap(),
            description: description.to_string(),
        }
    }

    #[test]
    fn request_hash_is_stable_across_equivalent_amount_spellings() {
        assert_eq!(
            intent("10", "rent").request_hash(),
            intent("10.00", "rent").request_hash()
        );
    }

    #[test]
    fn request_hash_changes_with_any_field() {
        let base = intent("10", "rent").request_hash();
        assert_ne!(base, intent("10.01", "rent").request_hash());
        assert_ne!(base, intent("10", "rent ").request_hash());

        let mut swapped = intent("10", "rent");
        std::mem::swap(&mut swapped.from_account_id, &mut swapped.to_account_id);
        assert_ne!(base, swapped.request_hash());
    }

    #[test]
    fn field_tags_prevent_boundary_confusion_between_adjacent_fields() {
        // Without delimiters, ("ab", "c") and ("a", "bc") would collide.
        let a = TransferIntent {
            description: "abc".into(),
            ..intent("10", "")
        };
        let b = TransferIntent {
            description: "ab\ndescription=c".into(),
            ..intent("10", "")
        };
        assert_ne!(a.request_hash(), b.request_hash());
    }

    #[test]
    fn balanced_detects_unbalanced_postings() {
        let txn = Transaction {
            id: Uuid::from_u128(9),
            idempotency_key: "k".into(),
            description: "d".into(),
            created_at: Utc::now(),
        };
        let txn_id = txn.id;
        let entry = move |direction, amount: &str| Entry {
            id: Uuid::new_v4(),
            transaction_id: txn_id,
            account_id: Uuid::new_v4(),
            direction,
            amount: Money::new(Decimal::from_str(amount).unwrap(), usd()).unwrap(),
            created_at: Utc::now(),
        };

        let balanced = TransactionDetail {
            transaction: txn.clone(),
            entries: vec![
                entry(Direction::Debit, "10"),
                entry(Direction::Credit, "10"),
            ],
        };
        assert!(balanced.is_balanced());

        let unbalanced = TransactionDetail {
            transaction: txn.clone(),
            entries: vec![entry(Direction::Debit, "10"), entry(Direction::Credit, "9")],
        };
        assert!(!unbalanced.is_balanced());

        let single_sided = TransactionDetail {
            transaction: txn,
            entries: vec![entry(Direction::Debit, "10")],
        };
        assert!(!single_sided.is_balanced());
    }
}
