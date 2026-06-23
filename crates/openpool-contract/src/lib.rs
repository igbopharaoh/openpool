//! Stable public DTOs shared by HTTP, SSR, and browser clients.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct PublicRaffle {
    pub id: Uuid,
    pub name: String,
    pub entry_price_sats: u64,
    pub start_time: OffsetDateTime,
    pub end_time: OffsetDateTime,
    pub status: String,
    pub total_pool_sats: u64,
    pub total_tickets: u64,
    pub winner_bps: u16,
    pub organizer_bps: u16,
    pub platform_bps: u16,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct PublicInvoice {
    pub id: Uuid,
    pub amount_sats: u64,
    pub ticket_count: u64,
    pub status: String,
    pub bolt11: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub ticket_start: Option<u64>,
    pub ticket_end: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct PublicPool {
    pub raffle_id: Uuid,
    pub total_pool_sats: u64,
    pub total_tickets: u64,
    pub entry_chain_head: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct PublicEntry {
    pub entry_index: u64,
    pub participant_public_id: String,
    pub payment_reference_hash: String,
    pub amount_sats: u64,
    pub ticket_start: u64,
    pub ticket_end: u64,
    pub settled_at: OffsetDateTime,
    pub entry_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct EntryPage {
    pub entries: Vec<PublicEntry>,
    pub next_cursor: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct PublicResult {
    pub raffle_id: Uuid,
    pub status: String,
    pub winning_ticket: Option<u64>,
    pub payout_statuses: Vec<String>,
    pub proof_available: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct PublicRefund {
    pub id: Uuid,
    pub status: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct OperatorJob {
    pub id: Uuid,
    pub kind: String,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct PublicProofMetadata {
    pub raffle_id: Uuid,
    pub proof_hash: String,
    pub storage_uri: Option<String>,
    pub storage_version_id: Option<String>,
    pub storage_etag: Option<String>,
    pub published_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct Problem {
    pub status: u16,
    pub code: String,
    pub detail: String,
    pub request_id: Uuid,
}
