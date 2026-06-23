//! Proof-only verification for public OPENPOOL-1 proof documents.

use openpool_protocol::{
    BitcoinFacts, DrawInput, PROTOCOL_VERSION, ProofDocument, ProtocolError, draw, proof_hash,
    summarize_ledger,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum VerificationCode {
    ProtocolVersionMismatch,
    LedgerInvalid,
    RaffleIdMismatch,
    FreezeMismatch,
    RandomnessHeightMismatch,
    DrawInvalid,
    PayoutMismatch,
    ProofHashMismatch,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct VerificationIssue {
    pub code: VerificationCode,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct VerificationResult {
    pub issues: Vec<VerificationIssue>,
}

impl VerificationResult {
    pub fn is_verified(&self) -> bool {
        self.issues.is_empty()
    }

    fn push(&mut self, code: VerificationCode, detail: impl Into<String>) {
        self.issues.push(VerificationIssue {
            code,
            detail: detail.into(),
        });
    }
}

pub fn verify(document: &ProofDocument) -> VerificationResult {
    let mut result = VerificationResult::default();
    let payload = &document.payload;

    if payload.protocol_version != PROTOCOL_VERSION {
        result.push(
            VerificationCode::ProtocolVersionMismatch,
            format!(
                "expected protocol version {PROTOCOL_VERSION}, got {}",
                payload.protocol_version
            ),
        );
    }

    let summary = match summarize_ledger(&payload.entries) {
        Ok(summary) => Some(summary),
        Err(error) => {
            result.push(VerificationCode::LedgerInvalid, error.to_string());
            None
        }
    };

    if let Some(summary) = summary {
        if payload
            .entries
            .first()
            .is_some_and(|entry| entry.data.raffle_id != payload.raffle_id)
        {
            result.push(
                VerificationCode::RaffleIdMismatch,
                "proof raffle ID does not match the ledger raffle ID",
            );
        }
        if summary.entry_count != payload.freeze.entry_count
            || summary.total_tickets != payload.freeze.total_tickets
            || summary.total_pool_sats != payload.freeze.total_pool_sats
            || summary.entry_chain_head != payload.freeze.entry_chain_head
            || summary.entries_root != payload.freeze.entries_root
        {
            result.push(
                VerificationCode::FreezeMismatch,
                "freeze facts do not match the recomputed ledger",
            );
        }

        let bitcoin: BitcoinFacts = payload.bitcoin;
        if bitcoin.randomness_block_height <= bitcoin.close_block_height {
            result.push(
                VerificationCode::RandomnessHeightMismatch,
                "randomness block height must be greater than close block height",
            );
        }

        let input = DrawInput {
            raffle_id: payload.raffle_id,
            entries_root: summary.entries_root,
            close_block_height: bitcoin.close_block_height,
            randomness_block_height: bitcoin.randomness_block_height,
            randomness_block_hash: bitcoin.randomness_block_hash,
            total_tickets: summary.total_tickets,
        };
        match draw(&payload.entries, input) {
            Ok(expected) if expected != payload.draw => result.push(
                VerificationCode::DrawInvalid,
                "stored draw does not match recomputed draw",
            ),
            Err(error) => result.push(VerificationCode::DrawInvalid, error.to_string()),
            Ok(_) => {}
        }

        if !payload.payout_split.is_valid() {
            result.push(
                VerificationCode::PayoutMismatch,
                "payout split does not total 10,000 basis points",
            );
        } else if payload.payout_split.allocate(summary.total_pool_sats) != payload.payouts
            || payload.payouts.checked_total() != Some(summary.total_pool_sats)
        {
            result.push(
                VerificationCode::PayoutMismatch,
                "payouts do not reconcile to the frozen pool and split",
            );
        }
    }

    match proof_hash(payload) {
        Ok(expected) if expected != document.proof_hash => result.push(
            VerificationCode::ProofHashMismatch,
            "proof hash does not match canonical proof payload",
        ),
        Err(error) => result.push(VerificationCode::ProofHashMismatch, error.to_string()),
        Ok(_) => {}
    }

    result
}

pub fn verify_or_protocol_error(document: &ProofDocument) -> Result<(), ProtocolError> {
    let result = verify(document);
    if result.is_verified() {
        Ok(())
    } else {
        Err(ProtocolError::ProofSerialization(
            result
                .issues
                .into_iter()
                .map(|issue| issue.detail)
                .collect::<Vec<_>>()
                .join("; "),
        ))
    }
}

#[cfg(test)]
mod tests {
    use openpool_protocol::Hash32;

    use super::*;

    fn fixture(source: &str) -> ProofDocument {
        serde_json::from_str(source).expect("golden fixture is valid proof JSON")
    }

    fn single_entry_fixture() -> ProofDocument {
        fixture(include_str!(
            "../../openpool-test-support/fixtures/valid-single-entry.json"
        ))
    }

    fn multiple_entry_fixture() -> ProofDocument {
        fixture(include_str!(
            "../../openpool-test-support/fixtures/valid-multiple-entry.json"
        ))
    }

    fn has_code(result: &VerificationResult, expected: VerificationCode) -> bool {
        result.issues.iter().any(|issue| issue.code == expected)
    }

    #[test]
    fn accepts_valid_single_and_multiple_entry_fixtures() {
        assert!(verify(&single_entry_fixture()).is_verified());
        assert!(verify(&multiple_entry_fixture()).is_verified());
    }

    #[test]
    fn rejects_corrupted_entry_root_draw_payout_and_hash() {
        let mut entry = multiple_entry_fixture();
        entry.payload.entries[0].data.amount_sats = openpool_domain::Sats::new(9_999);
        let result = verify(&entry);
        assert!(has_code(&result, VerificationCode::LedgerInvalid));

        let mut root = multiple_entry_fixture();
        root.payload.freeze.entries_root = Hash32::from_bytes([7; 32]);
        let result = verify(&root);
        assert!(has_code(&result, VerificationCode::FreezeMismatch));

        let mut draw = multiple_entry_fixture();
        draw.payload.draw.seed = Hash32::from_bytes([8; 32]);
        let result = verify(&draw);
        assert!(has_code(&result, VerificationCode::DrawInvalid));

        let mut payout = multiple_entry_fixture();
        payout.payload.payouts.winner = openpool_domain::Sats::new(1);
        let result = verify(&payout);
        assert!(has_code(&result, VerificationCode::PayoutMismatch));

        let mut proof = multiple_entry_fixture();
        proof.proof_hash = Hash32::from_bytes([9; 32]);
        let result = verify(&proof);
        assert!(has_code(&result, VerificationCode::ProofHashMismatch));

        let mut range = multiple_entry_fixture();
        range.payload.entries[0].data.ticket_range.start = 2;
        range.payload.entries[0].data.ticket_range.end = 1;
        let result = verify(&range);
        assert!(has_code(&result, VerificationCode::LedgerInvalid));

        let mut split = multiple_entry_fixture();
        split.payload.payout_split.platform = openpool_domain::BasisPoints::new(99).unwrap();
        let result = verify(&split);
        assert!(has_code(&result, VerificationCode::PayoutMismatch));

        let mut raffle = multiple_entry_fixture();
        raffle.payload.raffle_id = openpool_domain::RaffleId::from(
            uuid::Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
        );
        let result = verify(&raffle);
        assert!(has_code(&result, VerificationCode::RaffleIdMismatch));
    }
}
