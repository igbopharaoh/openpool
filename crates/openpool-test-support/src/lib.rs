//! Deterministic OPENPOOL-1 fixtures shared by integration tests.

use openpool_domain::{BasisPoints, EntryId, PayoutSplit, RaffleId, Sats, TicketRange};
use openpool_protocol::{
    BitcoinFacts, DrawInput, EntryData, FreezeFacts, Hash32, LedgerEntry, PROTOCOL_VERSION,
    ProofDocument, ProofPayload, ZERO_HASH, draw,
};
use uuid::Uuid;

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("fixture UUID is valid")
}

pub fn valid_multiple_entry_proof() -> ProofDocument {
    let raffle_id = RaffleId::from(uuid("11111111-1111-1111-1111-111111111111"));
    let first = LedgerEntry::create(
        EntryData {
            raffle_id,
            entry_id: EntryId::from(uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")),
            entry_index: 0,
            participant_public_id: Hash32::from_bytes([1; 32]),
            payment_reference_hash: Hash32::from_bytes([11; 32]),
            amount_sats: Sats::new(2_000),
            ticket_range: TicketRange::new(0, 2).unwrap(),
            settled_at_unix_ms: 1_700_000_000_000,
        },
        ZERO_HASH,
    );
    let second = LedgerEntry::create(
        EntryData {
            raffle_id,
            entry_id: EntryId::from(uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")),
            entry_index: 1,
            participant_public_id: Hash32::from_bytes([2; 32]),
            payment_reference_hash: Hash32::from_bytes([12; 32]),
            amount_sats: Sats::new(3_000),
            ticket_range: TicketRange::new(2, 5).unwrap(),
            settled_at_unix_ms: 1_700_000_000_001,
        },
        first.entry_hash,
    );
    let entries = vec![first, second];
    let summary = openpool_protocol::summarize_ledger(&entries).unwrap();
    let split = PayoutSplit::new(
        BasisPoints::new(9_500).unwrap(),
        BasisPoints::new(400).unwrap(),
        BasisPoints::new(100).unwrap(),
    )
    .unwrap();
    let bitcoin = BitcoinFacts {
        close_block_height: 900_000,
        randomness_block_height: 900_006,
        randomness_block_hash: Hash32::from_bytes([42; 32]),
    };
    let draw = draw(
        &entries,
        DrawInput {
            raffle_id,
            entries_root: summary.entries_root,
            close_block_height: bitcoin.close_block_height,
            randomness_block_height: bitcoin.randomness_block_height,
            randomness_block_hash: bitcoin.randomness_block_hash,
            total_tickets: summary.total_tickets,
        },
    )
    .unwrap();
    ProofDocument::create(ProofPayload {
        protocol_version: PROTOCOL_VERSION.to_owned(),
        raffle_id,
        payout_split: split,
        entries,
        freeze: FreezeFacts {
            entry_count: summary.entry_count,
            total_tickets: summary.total_tickets,
            total_pool_sats: summary.total_pool_sats,
            entry_chain_head: summary.entry_chain_head,
            entries_root: summary.entries_root,
        },
        bitcoin,
        draw,
        payouts: split.allocate(summary.total_pool_sats),
    })
    .unwrap()
}

pub fn valid_single_entry_proof() -> ProofDocument {
    let mut document = valid_multiple_entry_proof();
    document.payload.entries.truncate(1);
    let entry = document.payload.entries[0].clone();
    let summary = openpool_protocol::summarize_ledger(&document.payload.entries).unwrap();
    document.payload.freeze = FreezeFacts {
        entry_count: summary.entry_count,
        total_tickets: summary.total_tickets,
        total_pool_sats: summary.total_pool_sats,
        entry_chain_head: summary.entry_chain_head,
        entries_root: summary.entries_root,
    };
    document.payload.draw = draw(
        &[entry],
        DrawInput {
            raffle_id: document.payload.raffle_id,
            entries_root: summary.entries_root,
            close_block_height: document.payload.bitcoin.close_block_height,
            randomness_block_height: document.payload.bitcoin.randomness_block_height,
            randomness_block_hash: document.payload.bitcoin.randomness_block_hash,
            total_tickets: summary.total_tickets,
        },
    )
    .unwrap();
    document.payload.payouts = document
        .payload
        .payout_split
        .allocate(document.payload.freeze.total_pool_sats);
    ProofDocument::create(document.payload).unwrap()
}
