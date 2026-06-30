//! Pure domain values and invariants for OPENPOOL.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const BASIS_POINTS_TOTAL: u16 = 10_000;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

uuid_id!(RaffleId);
uuid_id!(EntryId);
uuid_id!(OrganizerId);
uuid_id!(InvoiceId);
uuid_id!(DrawId);
uuid_id!(PayoutId);
uuid_id!(AuditEventId);
uuid_id!(JobId);
uuid_id!(OutboxEventId);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sats(u64);

impl Sats {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BasisPoints(u16);

impl BasisPoints {
    pub fn new(value: u16) -> Result<Self, DomainError> {
        if value > BASIS_POINTS_TOTAL {
            return Err(DomainError::BasisPointsOutOfRange(value));
        }
        Ok(Self(value))
    }

    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayoutSplit {
    pub winner: BasisPoints,
    pub organizer: BasisPoints,
    pub platform: BasisPoints,
}

impl PayoutSplit {
    pub fn new(
        winner: BasisPoints,
        organizer: BasisPoints,
        platform: BasisPoints,
    ) -> Result<Self, DomainError> {
        let total = u32::from(winner.0) + u32::from(organizer.0) + u32::from(platform.0);
        if total != u32::from(BASIS_POINTS_TOTAL) {
            return Err(DomainError::InvalidPayoutSplit { total });
        }
        Ok(Self {
            winner,
            organizer,
            platform,
        })
    }

    pub fn allocate(self, pool: Sats) -> PayoutAmounts {
        debug_assert!(self.is_valid());
        let pool = pool.0;
        let winner =
            (u128::from(pool) * u128::from(self.winner.0) / u128::from(BASIS_POINTS_TOTAL)) as u64;
        let organizer = (u128::from(pool) * u128::from(self.organizer.0)
            / u128::from(BASIS_POINTS_TOTAL)) as u64;
        let platform = pool - winner - organizer;
        PayoutAmounts {
            winner: Sats(winner),
            organizer: Sats(organizer),
            platform: Sats(platform),
        }
    }

    pub const fn is_valid(self) -> bool {
        (self.winner.0 as u32) + (self.organizer.0 as u32) + (self.platform.0 as u32)
            == BASIS_POINTS_TOTAL as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayoutAmounts {
    pub winner: Sats,
    pub organizer: Sats,
    pub platform: Sats,
}

impl PayoutAmounts {
    pub fn total(self) -> Sats {
        Sats(self.winner.0 + self.organizer.0 + self.platform.0)
    }

    pub fn checked_total(self) -> Option<Sats> {
        self.winner
            .0
            .checked_add(self.organizer.0)
            .and_then(|total| total.checked_add(self.platform.0))
            .map(Sats)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RaffleStatus {
    Draft,
    Scheduled,
    Open,
    Closing,
    RandomnessPending,
    DrawReady,
    WinnerSelected,
    PayoutPending,
    PaidOut,
    Cancelled,
    Refunding,
    Refunded,
}

impl RaffleStatus {
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Draft, Self::Scheduled)
                | (Self::Scheduled, Self::Open)
                | (Self::Open, Self::Closing)
                | (Self::Closing, Self::RandomnessPending)
                | (Self::RandomnessPending, Self::DrawReady)
                | (Self::DrawReady, Self::WinnerSelected)
                | (Self::WinnerSelected, Self::PayoutPending)
                | (Self::PayoutPending, Self::PaidOut)
                | (Self::Open, Self::Cancelled)
                | (Self::Closing, Self::Cancelled)
                | (Self::Cancelled, Self::Refunding)
                | (Self::Refunding, Self::Refunded)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketRange {
    pub start: u64,
    pub end: u64,
}

impl TicketRange {
    pub fn new(start: u64, end: u64) -> Result<Self, DomainError> {
        if start >= end {
            return Err(DomainError::InvalidTicketRange { start, end });
        }
        Ok(Self { start, end })
    }

    pub const fn count(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_valid(self) -> bool {
        self.start < self.end
    }

    pub const fn contains(self, ticket: u64) -> bool {
        self.start <= ticket && ticket < self.end
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("basis points value {0} exceeds 10,000")]
    BasisPointsOutOfRange(u16),
    #[error("payout split must total 10,000 basis points, got {total}")]
    InvalidPayoutSplit { total: u32 },
    #[error("ticket range [{start}, {end}) must have start < end")]
    InvalidTicketRange { start: u64, end: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payout_remainder_is_assigned_to_platform() {
        let split = PayoutSplit::new(
            BasisPoints::new(9_500).unwrap(),
            BasisPoints::new(400).unwrap(),
            BasisPoints::new(100).unwrap(),
        )
        .unwrap();

        let amounts = split.allocate(Sats::new(101));
        assert_eq!(amounts.winner, Sats::new(95));
        assert_eq!(amounts.organizer, Sats::new(4));
        assert_eq!(amounts.platform, Sats::new(2));
        assert_eq!(amounts.total(), Sats::new(101));
    }

    #[test]
    fn ticket_range_is_half_open() {
        let range = TicketRange::new(10, 15).unwrap();
        assert!(range.contains(10));
        assert!(range.contains(14));
        assert!(!range.contains(15));
        assert_eq!(range.count(), 5);
    }

    #[test]
    fn only_declared_lifecycle_transitions_are_allowed() {
        assert!(RaffleStatus::Open.can_transition_to(RaffleStatus::Closing));
        assert!(!RaffleStatus::Open.can_transition_to(RaffleStatus::PaidOut));
    }
}
