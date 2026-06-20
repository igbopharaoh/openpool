//! Canonical VLRP-1 encodings, hashes, draw calculation, and public proof model.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;
use vlrp_domain::{EntryId, PayoutAmounts, PayoutSplit, RaffleId, Sats, TicketRange};

pub const PROTOCOL_VERSION: &str = "VLRP-1";
pub const ZERO_HASH: Hash32 = Hash32([0; 32]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Hash32(pub [u8; 32]);

impl Hash32 {
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_hex(value: &str) -> Result<Self, ProtocolError> {
        let decoded = hex::decode(value).map_err(|_| ProtocolError::InvalidHashEncoding)?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| ProtocolError::InvalidHashEncoding)?;
        Ok(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl Serialize for Hash32 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash32 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryData {
    pub raffle_id: RaffleId,
    pub entry_id: EntryId,
    pub entry_index: u64,
    pub participant_public_id: Hash32,
    pub payment_reference_hash: Hash32,
    pub amount_sats: Sats,
    pub ticket_range: TicketRange,
    pub settled_at_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    #[serde(flatten)]
    pub data: EntryData,
    pub previous_entry_hash: Hash32,
    pub entry_hash: Hash32,
}

impl LedgerEntry {
    pub fn create(data: EntryData, previous_entry_hash: Hash32) -> Self {
        let entry_hash = entry_hash(&data, previous_entry_hash);
        Self {
            data,
            previous_entry_hash,
            entry_hash,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSummary {
    pub entry_count: u64,
    pub total_tickets: u64,
    pub total_pool_sats: Sats,
    pub entry_chain_head: Hash32,
    pub entries_root: Hash32,
}

pub fn entry_hash(data: &EntryData, previous_entry_hash: Hash32) -> Hash32 {
    let mut digest = Sha256::new();
    digest.update(b"VLRP-ENTRY-V1\0");
    digest.update(data.raffle_id.as_bytes());
    digest.update(data.entry_index.to_be_bytes());
    digest.update(data.participant_public_id.as_bytes());
    digest.update(data.payment_reference_hash.as_bytes());
    digest.update(data.amount_sats.as_u64().to_be_bytes());
    digest.update(data.ticket_range.count().to_be_bytes());
    digest.update(data.ticket_range.start.to_be_bytes());
    digest.update(data.ticket_range.end.to_be_bytes());
    digest.update(data.settled_at_unix_ms.to_be_bytes());
    digest.update(previous_entry_hash.as_bytes());
    Hash32(digest.finalize().into())
}

pub fn entries_root(raffle_id: RaffleId, entry_count: u64, entry_chain_head: Hash32) -> Hash32 {
    let mut digest = Sha256::new();
    digest.update(b"VLRP-ENTRIES-V1\0");
    digest.update(raffle_id.as_bytes());
    digest.update(entry_count.to_be_bytes());
    digest.update(entry_chain_head.as_bytes());
    Hash32(digest.finalize().into())
}

pub fn summarize_ledger(entries: &[LedgerEntry]) -> Result<LedgerSummary, ProtocolError> {
    let Some(first) = entries.first() else {
        return Err(ProtocolError::EmptyLedger);
    };
    let raffle_id = first.data.raffle_id;
    let mut expected_ticket_start = 0;
    let mut previous_hash = ZERO_HASH;
    let mut total_pool_sats = 0_u64;

    for (position, entry) in entries.iter().enumerate() {
        let expected_index =
            u64::try_from(position).map_err(|_| ProtocolError::EntryCountOverflow)?;
        if entry.data.raffle_id != raffle_id {
            return Err(ProtocolError::MixedRaffleIds);
        }
        if entry.data.entry_index != expected_index {
            return Err(ProtocolError::NonConsecutiveEntryIndex {
                expected: expected_index,
                actual: entry.data.entry_index,
            });
        }
        if !entry.data.ticket_range.is_valid() {
            return Err(ProtocolError::InvalidTicketCount {
                index: expected_index,
            });
        }
        if entry.data.ticket_range.start != expected_ticket_start {
            return Err(ProtocolError::NonContiguousTicketRange {
                expected: expected_ticket_start,
                actual: entry.data.ticket_range.start,
            });
        }
        if entry.previous_entry_hash != previous_hash {
            return Err(ProtocolError::PreviousHashMismatch {
                index: expected_index,
            });
        }
        if entry.entry_hash != entry_hash(&entry.data, previous_hash) {
            return Err(ProtocolError::EntryHashMismatch {
                index: expected_index,
            });
        }
        total_pool_sats = total_pool_sats
            .checked_add(entry.data.amount_sats.as_u64())
            .ok_or(ProtocolError::SatsOverflow)?;
        expected_ticket_start = entry.data.ticket_range.end;
        previous_hash = entry.entry_hash;
    }

    let entry_count =
        u64::try_from(entries.len()).map_err(|_| ProtocolError::EntryCountOverflow)?;
    Ok(LedgerSummary {
        entry_count,
        total_tickets: expected_ticket_start,
        total_pool_sats: Sats::new(total_pool_sats),
        entry_chain_head: previous_hash,
        entries_root: entries_root(raffle_id, entry_count, previous_hash),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawInput {
    pub raffle_id: RaffleId,
    pub entries_root: Hash32,
    pub close_block_height: u64,
    pub randomness_block_height: u64,
    pub randomness_block_hash: Hash32,
    pub total_tickets: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawResult {
    pub seed: Hash32,
    pub winning_ticket: u64,
    pub winner_entry_id: EntryId,
    pub winner_ticket_range: TicketRange,
}

pub fn draw_seed(input: DrawInput) -> Hash32 {
    let mut digest = Sha256::new();
    digest.update(b"VLRP-DRAW-V1\0");
    digest.update(input.raffle_id.as_bytes());
    digest.update(input.entries_root.as_bytes());
    digest.update(input.close_block_height.to_be_bytes());
    digest.update(input.randomness_block_height.to_be_bytes());
    digest.update(input.randomness_block_hash.as_bytes());
    Hash32(digest.finalize().into())
}

pub fn winning_ticket(seed: Hash32, total_tickets: u64) -> Result<u64, ProtocolError> {
    if total_tickets == 0 {
        return Err(ProtocolError::ZeroTickets);
    }
    let mut remainder = 0_u128;
    let modulus = u128::from(total_tickets);
    for byte in seed.0 {
        remainder = (remainder * 256 + u128::from(byte)) % modulus;
    }
    Ok(remainder as u64)
}

pub fn draw(entries: &[LedgerEntry], input: DrawInput) -> Result<DrawResult, ProtocolError> {
    let summary = summarize_ledger(entries)?;
    if summary.total_tickets != input.total_tickets {
        return Err(ProtocolError::TotalTicketsMismatch {
            expected: summary.total_tickets,
            actual: input.total_tickets,
        });
    }
    if summary.entries_root != input.entries_root {
        return Err(ProtocolError::EntriesRootMismatch);
    }
    let seed = draw_seed(input);
    let ticket = winning_ticket(seed, input.total_tickets)?;
    let winner = entries
        .iter()
        .find(|entry| entry.data.ticket_range.contains(ticket))
        .ok_or(ProtocolError::WinningEntryNotFound(ticket))?;
    Ok(DrawResult {
        seed,
        winning_ticket: ticket,
        winner_entry_id: winner.data.entry_id,
        winner_ticket_range: winner.data.ticket_range,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreezeFacts {
    pub entry_count: u64,
    pub total_tickets: u64,
    pub total_pool_sats: Sats,
    pub entry_chain_head: Hash32,
    pub entries_root: Hash32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitcoinFacts {
    pub close_block_height: u64,
    pub randomness_block_height: u64,
    pub randomness_block_hash: Hash32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofPayload {
    pub protocol_version: String,
    pub raffle_id: RaffleId,
    pub payout_split: PayoutSplit,
    pub entries: Vec<LedgerEntry>,
    pub freeze: FreezeFacts,
    pub bitcoin: BitcoinFacts,
    pub draw: DrawResult,
    pub payouts: PayoutAmounts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDocument {
    pub payload: ProofPayload,
    pub proof_hash: Hash32,
}

impl ProofDocument {
    pub fn create(payload: ProofPayload) -> Result<Self, ProtocolError> {
        let proof_hash = proof_hash(&payload)?;
        Ok(Self {
            payload,
            proof_hash,
        })
    }
}

pub fn canonical_proof_json(payload: &ProofPayload) -> Result<Vec<u8>, ProtocolError> {
    serde_jcs::to_vec(payload).map_err(|error| ProtocolError::ProofSerialization(error.to_string()))
}

pub fn proof_hash(payload: &ProofPayload) -> Result<Hash32, ProtocolError> {
    let canonical_json = canonical_proof_json(payload)?;
    let mut digest = Sha256::new();
    digest.update(canonical_json);
    Ok(Hash32(digest.finalize().into()))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("hash must be a 32-byte hexadecimal string")]
    InvalidHashEncoding,
    #[error("ledger cannot be empty")]
    EmptyLedger,
    #[error("ledger contains entries from different raffles")]
    MixedRaffleIds,
    #[error("entry index must be consecutive: expected {expected}, got {actual}")]
    NonConsecutiveEntryIndex { expected: u64, actual: u64 },
    #[error("ticket ranges must be contiguous: expected start {expected}, got {actual}")]
    NonContiguousTicketRange { expected: u64, actual: u64 },
    #[error("entry {index} previous hash does not match the chain")]
    PreviousHashMismatch { index: u64 },
    #[error("entry {index} hash does not match canonical input")]
    EntryHashMismatch { index: u64 },
    #[error("entry {index} has an invalid ticket count")]
    InvalidTicketCount { index: u64 },
    #[error("satoshi total overflowed u64")]
    SatsOverflow,
    #[error("entry count overflowed u64")]
    EntryCountOverflow,
    #[error("winner selection requires at least one ticket")]
    ZeroTickets,
    #[error("ledger has {expected} tickets but draw input has {actual}")]
    TotalTicketsMismatch { expected: u64, actual: u64 },
    #[error("draw input entries root does not match the ledger")]
    EntriesRootMismatch,
    #[error("no entry owns winning ticket {0}")]
    WinningEntryNotFound(u64),
    #[error("proof serialization failed: {0}")]
    ProofSerialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use vlrp_domain::{BasisPoints, PayoutSplit};

    fn id(value: &str) -> Uuid {
        Uuid::parse_str(value).unwrap()
    }

    fn entry(index: u64, start: u64, end: u64, previous: Hash32) -> LedgerEntry {
        LedgerEntry::create(
            EntryData {
                raffle_id: RaffleId::from(id("11111111-1111-1111-1111-111111111111")),
                entry_id: EntryId::from(id(if index == 0 {
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
                } else {
                    "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
                })),
                entry_index: index,
                participant_public_id: Hash32::from_bytes([index as u8 + 1; 32]),
                payment_reference_hash: Hash32::from_bytes([index as u8 + 11; 32]),
                amount_sats: Sats::new(1_000),
                ticket_range: TicketRange::new(start, end).unwrap(),
                settled_at_unix_ms: 1_700_000_000_000 + index as i64,
            },
            previous,
        )
    }

    #[test]
    fn ledger_and_draw_are_deterministic() {
        let first = entry(0, 0, 2, ZERO_HASH);
        let second = entry(1, 2, 5, first.entry_hash);
        let entries = vec![first, second];
        let summary = summarize_ledger(&entries).unwrap();
        assert_eq!(summary.total_tickets, 5);
        assert_eq!(summary.total_pool_sats, Sats::new(2_000));

        let input = DrawInput {
            raffle_id: entries[0].data.raffle_id,
            entries_root: summary.entries_root,
            close_block_height: 900_000,
            randomness_block_height: 900_006,
            randomness_block_hash: Hash32::from_bytes([42; 32]),
            total_tickets: summary.total_tickets,
        };
        let result = draw(&entries, input).unwrap();
        assert_eq!(
            entries[0].entry_hash.to_hex(),
            "09c09a3287b7437beb6b0abbb706a6626b8e2e3115d8db1cbfb0a4f796cfa08b"
        );
        assert_eq!(
            entries[1].entry_hash.to_hex(),
            "d28e8306a68a9fabff06347ef6f6429685aa4d965bb138e7b0912eea3c1d7b85"
        );
        assert_eq!(
            summary.entries_root.to_hex(),
            "9a0f40b075459ab73691eaae8e34abf1dc1dd4eb292fc627f901d050bab975ca"
        );
        assert_eq!(
            result.seed.to_hex(),
            "d806ca35d92d8f562e013210618fdaf38f44ef924be4ab621370c90702526034"
        );
        assert_eq!(result.winning_ticket, 1);
        assert_eq!(result.winner_entry_id, entries[0].data.entry_id);
    }

    #[test]
    fn proof_hash_is_stable_for_the_same_payload() {
        let entry = entry(0, 0, 1, ZERO_HASH);
        let summary = summarize_ledger(std::slice::from_ref(&entry)).unwrap();
        let split = PayoutSplit::new(
            BasisPoints::new(9_500).unwrap(),
            BasisPoints::new(400).unwrap(),
            BasisPoints::new(100).unwrap(),
        )
        .unwrap();
        let draw = draw(
            std::slice::from_ref(&entry),
            DrawInput {
                raffle_id: entry.data.raffle_id,
                entries_root: summary.entries_root,
                close_block_height: 900_000,
                randomness_block_height: 900_006,
                randomness_block_hash: Hash32::from_bytes([42; 32]),
                total_tickets: 1,
            },
        )
        .unwrap();
        let payload = ProofPayload {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            raffle_id: entry.data.raffle_id,
            payout_split: split,
            entries: vec![entry],
            freeze: FreezeFacts {
                entry_count: summary.entry_count,
                total_tickets: summary.total_tickets,
                total_pool_sats: summary.total_pool_sats,
                entry_chain_head: summary.entry_chain_head,
                entries_root: summary.entries_root,
            },
            bitcoin: BitcoinFacts {
                close_block_height: 900_000,
                randomness_block_height: 900_006,
                randomness_block_hash: Hash32::from_bytes([42; 32]),
            },
            draw,
            payouts: split.allocate(summary.total_pool_sats),
        };
        assert_eq!(proof_hash(&payload).unwrap(), proof_hash(&payload).unwrap());
    }
}
