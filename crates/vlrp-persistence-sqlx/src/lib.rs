//! SQLx-backed durable state. This crate deliberately owns SQL and transactions, not HTTP.

use serde_json::json;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use vlrp_domain::{EntryId, InvoiceId, OrganizerId, PayoutSplit, RaffleId, Sats, TicketRange};
use vlrp_protocol::{EntryData, Hash32, LedgerEntry, ZERO_HASH, entries_root};

#[derive(Clone)]
pub struct Persistence {
    pool: PgPool,
}

impl Persistence {
    pub async fn connect(database_url: &str) -> Result<Self, PersistenceError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<(), PersistenceError> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        Ok(())
    }

    pub async fn create_organizer(&self, organizer: NewOrganizer) -> Result<(), PersistenceError> {
        sqlx::query(
            "INSERT INTO organizers (id, display_name, lightning_address_ciphertext) VALUES ($1, $2, $3)",
        )
        .bind(organizer.id.as_uuid())
        .bind(organizer.display_name)
        .bind(organizer.lightning_address_ciphertext)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_draft_raffle(
        &self,
        raffle: NewDraftRaffle,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("INSERT INTO raffles (id, organizer_id, name, entry_price_sats, start_time, end_time, status, randomness_delay_blocks) VALUES ($1, $2, $3, $4, $5, $6, 'DRAFT', $7)")
            .bind(raffle.id.as_uuid())
            .bind(raffle.organizer_id.as_uuid())
            .bind(raffle.name)
            .bind(as_i64(raffle.entry_price_sats.as_u64())?)
            .bind(raffle.start_time)
            .bind(raffle.end_time)
            .bind(raffle.randomness_delay_blocks)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO payout_splits (raffle_id, winner_bps, organizer_bps, platform_bps) VALUES ($1, $2, $3, $4)")
            .bind(raffle.id.as_uuid())
            .bind(i32::from(raffle.payout_split.winner.as_u16()))
            .bind(i32::from(raffle.payout_split.organizer.as_u16()))
            .bind(i32::from(raffle.payout_split.platform.as_u16()))
            .execute(&mut *transaction)
            .await?;
        write_audit(
            &mut transaction,
            raffle.id.as_uuid(),
            "raffle.created",
            json!({"status": "DRAFT"}),
        )
        .await?;
        write_outbox(
            &mut transaction,
            raffle.id.as_uuid(),
            "raffle.created",
            json!({"raffle_id": raffle.id.as_uuid()}),
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Temporary persistence seam used by Milestone 2 tests. Payment adapters will own invoice
    /// creation in the next payment milestone.
    pub async fn create_pending_invoice(
        &self,
        invoice: NewInvoice,
    ) -> Result<(), PersistenceError> {
        sqlx::query("INSERT INTO invoices (id, raffle_id, participant_public_id, payment_reference_hash, amount_sats, ticket_count) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(invoice.id.as_uuid())
            .bind(invoice.raffle_id.as_uuid())
            .bind(invoice.participant_public_id.0.to_vec())
            .bind(invoice.payment_reference_hash.0.to_vec())
            .bind(as_i64(invoice.amount_sats.as_u64())?)
            .bind(as_i64(invoice.ticket_count)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Locks the invoice and raffle, then makes settlement-to-entry idempotent and atomic.
    pub async fn settle_invoice_and_append_entry(
        &self,
        invoice_id: InvoiceId,
        entry_id: EntryId,
        settled_at: OffsetDateTime,
    ) -> Result<LedgerEntry, PersistenceError> {
        let mut transaction = self.pool.begin().await?;
        let invoice = sqlx::query("SELECT raffle_id, participant_public_id, payment_reference_hash, amount_sats, ticket_count, status FROM invoices WHERE id = $1 FOR UPDATE")
            .bind(invoice_id.as_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(PersistenceError::NotFound("invoice"))?;
        if invoice.get::<String, _>("status") == "settled" {
            let row = sqlx::query("SELECT id, raffle_id, entry_index, participant_public_id, payment_reference_hash, amount_sats, ticket_start, ticket_end, settled_at, previous_entry_hash, entry_hash FROM entries WHERE invoice_id = $1")
                .bind(invoice_id.as_uuid())
                .fetch_one(&mut *transaction)
                .await?;
            transaction.commit().await?;
            return ledger_entry_from_row(row);
        }
        if invoice.get::<String, _>("status") != "pending" {
            return Err(PersistenceError::InvoiceNotEligible);
        }
        let raffle_id = RaffleId::from(invoice.get::<Uuid, _>("raffle_id"));
        let raffle = sqlx::query("SELECT status, total_pool_sats, total_tickets, entry_chain_head FROM raffles WHERE id = $1 FOR UPDATE")
            .bind(raffle_id.as_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(PersistenceError::NotFound("raffle"))?;
        if raffle.get::<String, _>("status") != "OPEN" {
            return Err(PersistenceError::RaffleNotOpen);
        }
        let ticket_start = as_u64(raffle.get::<i64, _>("total_tickets"))?;
        let ticket_count = as_u64(invoice.get::<i64, _>("ticket_count"))?;
        let ticket_end = ticket_start
            .checked_add(ticket_count)
            .ok_or(PersistenceError::IntegerOverflow)?;
        let entry_index =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entries WHERE raffle_id = $1")
                .bind(raffle_id.as_uuid())
                .fetch_one(&mut *transaction)
                .await?;
        let previous_hash = raffle
            .try_get::<Option<Vec<u8>>, _>("entry_chain_head")?
            .map(hash_from_bytes)
            .transpose()?
            .unwrap_or(ZERO_HASH);
        let entry = LedgerEntry::create(
            EntryData {
                raffle_id,
                entry_id,
                entry_index: as_u64(entry_index)?,
                participant_public_id: hash_from_bytes(invoice.get("participant_public_id"))?,
                payment_reference_hash: hash_from_bytes(invoice.get("payment_reference_hash"))?,
                amount_sats: Sats::new(as_u64(invoice.get::<i64, _>("amount_sats"))?),
                ticket_range: TicketRange::new(ticket_start, ticket_end)
                    .map_err(PersistenceError::Domain)?,
                settled_at_unix_ms: settled_at.unix_timestamp_nanos().div_euclid(1_000_000) as i64,
            },
            previous_hash,
        );
        sqlx::query("INSERT INTO entries (id, raffle_id, invoice_id, entry_index, participant_public_id, payment_reference_hash, amount_sats, ticket_start, ticket_end, settled_at, previous_entry_hash, entry_hash) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)")
            .bind(entry.data.entry_id.as_uuid()).bind(raffle_id.as_uuid()).bind(invoice_id.as_uuid())
            .bind(as_i64(entry.data.entry_index)?).bind(entry.data.participant_public_id.0.to_vec()).bind(entry.data.payment_reference_hash.0.to_vec())
            .bind(as_i64(entry.data.amount_sats.as_u64())?).bind(as_i64(ticket_start)?).bind(as_i64(ticket_end)?)
            .bind(settled_at).bind(entry.previous_entry_hash.0.to_vec()).bind(entry.entry_hash.0.to_vec())
            .execute(&mut *transaction).await?;
        let new_pool = as_u64(raffle.get::<i64, _>("total_pool_sats"))?
            .checked_add(entry.data.amount_sats.as_u64())
            .ok_or(PersistenceError::IntegerOverflow)?;
        sqlx::query("UPDATE invoices SET status = 'settled', settled_at = $2 WHERE id = $1")
            .bind(invoice_id.as_uuid())
            .bind(settled_at)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("UPDATE raffles SET total_pool_sats = $2, total_tickets = $3, entry_chain_head = $4, updated_at = now() WHERE id = $1")
            .bind(raffle_id.as_uuid()).bind(as_i64(new_pool)?).bind(as_i64(ticket_end)?).bind(entry.entry_hash.0.to_vec())
            .execute(&mut *transaction).await?;
        write_audit(
            &mut transaction,
            raffle_id.as_uuid(),
            "entry.appended",
            json!({"entry_id": entry_id.as_uuid(), "invoice_id": invoice_id.as_uuid()}),
        )
        .await?;
        write_outbox(
            &mut transaction,
            raffle_id.as_uuid(),
            "entry.appended",
            json!({"entry_id": entry_id.as_uuid()}),
        )
        .await?;
        transaction.commit().await?;
        Ok(entry)
    }

    pub async fn freeze_raffle(&self, raffle_id: RaffleId) -> Result<Hash32, PersistenceError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT status, total_tickets, entry_chain_head FROM raffles WHERE id = $1 FOR UPDATE",
        )
        .bind(raffle_id.as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if row.get::<String, _>("status") != "CLOSING" {
            return Err(PersistenceError::InvalidStatus);
        }
        let count = as_u64(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entries WHERE raffle_id = $1")
                .bind(raffle_id.as_uuid())
                .fetch_one(&mut *transaction)
                .await?,
        )?;
        let chain_head = row
            .try_get::<Option<Vec<u8>>, _>("entry_chain_head")?
            .map(hash_from_bytes)
            .transpose()?
            .unwrap_or(ZERO_HASH);
        let root = entries_root(raffle_id, count, chain_head);
        sqlx::query("UPDATE raffles SET status = 'RANDOMNESS_PENDING', entries_root = $2, frozen_at = now(), updated_at = now() WHERE id = $1")
            .bind(raffle_id.as_uuid()).bind(root.0.to_vec()).execute(&mut *transaction).await?;
        write_audit(
            &mut transaction,
            raffle_id.as_uuid(),
            "raffle.frozen",
            json!({"entries_root": root.to_hex()}),
        )
        .await?;
        write_outbox(
            &mut transaction,
            raffle_id.as_uuid(),
            "raffle.frozen",
            json!({"entries_root": root.to_hex()}),
        )
        .await?;
        transaction.commit().await?;
        Ok(root)
    }
}

pub struct NewOrganizer {
    pub id: OrganizerId,
    pub display_name: String,
    pub lightning_address_ciphertext: Vec<u8>,
}
pub struct NewDraftRaffle {
    pub id: RaffleId,
    pub organizer_id: OrganizerId,
    pub name: String,
    pub entry_price_sats: Sats,
    pub start_time: OffsetDateTime,
    pub end_time: OffsetDateTime,
    pub randomness_delay_blocks: i32,
    pub payout_split: PayoutSplit,
}
pub struct NewInvoice {
    pub id: InvoiceId,
    pub raffle_id: RaffleId,
    pub participant_public_id: Hash32,
    pub payment_reference_hash: Hash32,
    pub amount_sats: Sats,
    pub ticket_count: u64,
}

async fn write_audit(
    transaction: &mut Transaction<'_, Postgres>,
    aggregate_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO audit_events (id, aggregate_type, aggregate_id, event_type, actor_type, payload) VALUES ($1, 'raffle', $2, $3, 'system', $4)")
        .bind(Uuid::new_v4()).bind(aggregate_id).bind(event_type).bind(payload).execute(&mut **transaction).await?;
    Ok(())
}
async fn write_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    aggregate_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO outbox_events (id, aggregate_type, aggregate_id, event_type, payload) VALUES ($1, 'raffle', $2, $3, $4)")
        .bind(Uuid::new_v4()).bind(aggregate_id).bind(event_type).bind(payload).execute(&mut **transaction).await?;
    Ok(())
}
fn hash_from_bytes(value: Vec<u8>) -> Result<Hash32, PersistenceError> {
    Ok(Hash32(
        value
            .try_into()
            .map_err(|_| PersistenceError::InvalidHashLength)?,
    ))
}
fn as_u64(value: i64) -> Result<u64, PersistenceError> {
    u64::try_from(value).map_err(|_| PersistenceError::IntegerOverflow)
}
fn as_i64(value: u64) -> Result<i64, PersistenceError> {
    i64::try_from(value).map_err(|_| PersistenceError::IntegerOverflow)
}
fn ledger_entry_from_row(row: sqlx::postgres::PgRow) -> Result<LedgerEntry, PersistenceError> {
    Ok(LedgerEntry {
        data: EntryData {
            raffle_id: RaffleId::from(row.get::<Uuid, _>("raffle_id")),
            entry_id: EntryId::from(row.get::<Uuid, _>("id")),
            entry_index: as_u64(row.get("entry_index"))?,
            participant_public_id: hash_from_bytes(row.get("participant_public_id"))?,
            payment_reference_hash: hash_from_bytes(row.get("payment_reference_hash"))?,
            amount_sats: Sats::new(as_u64(row.get("amount_sats"))?),
            ticket_range: TicketRange::new(
                as_u64(row.get("ticket_start"))?,
                as_u64(row.get("ticket_end"))?,
            )
            .map_err(PersistenceError::Domain)?,
            settled_at_unix_ms: row
                .get::<OffsetDateTime, _>("settled_at")
                .unix_timestamp_nanos()
                .div_euclid(1_000_000) as i64,
        },
        previous_entry_hash: hash_from_bytes(row.get("previous_entry_hash"))?,
        entry_hash: hash_from_bytes(row.get("entry_hash"))?,
    })
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Domain(#[from] vlrp_domain::DomainError),
    #[error("{0} was not found")]
    NotFound(&'static str),
    #[error("invoice is not eligible for settlement")]
    InvoiceNotEligible,
    #[error("raffle is not open")]
    RaffleNotOpen,
    #[error("raffle is not in the required state")]
    InvalidStatus,
    #[error("integer conversion or addition overflowed")]
    IntegerOverflow,
    #[error("database hash is not 32 bytes")]
    InvalidHashLength,
}
