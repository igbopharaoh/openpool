//! SQLx-backed durable state. This crate deliberately owns SQL and transactions, not HTTP.

use openpool_protocol::{
    BasisPoints, EntryId, InvoiceId, OrganizerId, PayoutSplit, RaffleId, Sats, TicketRange,
};
use openpool_protocol::{
    BitcoinFacts, DrawInput, DrawResult, EntryData, FreezeFacts, Hash32, LedgerEntry,
    PROTOCOL_VERSION, ProofDocument, ProofPayload, ZERO_HASH, draw, entries_root,
};
use serde_json::json;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

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

    /// Persists an observed canonical block and preserves displaced candidates as reorg
    /// evidence. Any canonical descendants are invalidated too: their parent can no longer be
    /// part of the selected chain. The caller supplies facts from an independently selected
    /// canonical source (Esplora/Core); this method never invents block data.
    pub async fn record_canonical_block(
        &self,
        height: u64,
        block_hash: Hash32,
        previous_hash: Hash32,
        block_time: OffsetDateTime,
    ) -> Result<(), PersistenceError> {
        let height = as_i64(height)?;
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT block_hash FROM bitcoin_blocks WHERE height = $1 AND is_canonical FOR UPDATE",
        )
        .bind(height)
        .fetch_optional(&mut *tx)
        .await?;
        if existing.as_deref() != Some(block_hash.0.as_slice()) {
            // If this height changed, all later canonical records are descendants of the
            // displaced chain from our point of view and must be re-observed.
            sqlx::query("UPDATE bitcoin_blocks SET is_canonical = false WHERE is_canonical AND height >= $1")
                .bind(height).execute(&mut *tx).await?;
        }
        sqlx::query(
            "INSERT INTO bitcoin_blocks (height, block_hash, previous_hash, block_time, is_canonical)
             VALUES ($1, $2, $3, $4, true)
             ON CONFLICT (block_hash) DO UPDATE SET previous_hash = EXCLUDED.previous_hash,
                 block_time = EXCLUDED.block_time, is_canonical = true, observed_at = now()",
        )
        .bind(height)
        .bind(block_hash.0.to_vec())
        .bind(previous_hash.0.to_vec())
        .bind(block_time)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn canonical_block_matches(
        &self,
        height: u64,
        block_hash: Hash32,
    ) -> Result<bool, PersistenceError> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM bitcoin_blocks WHERE height = $1 AND block_hash = $2 AND is_canonical)",
        )
        .bind(as_i64(height)?)
        .bind(block_hash.0.to_vec())
        .fetch_one(&self.pool)
        .await?)
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

    /// Creates the internal record before calling an external collection provider.
    /// Until a BOLT11 invoice is stored, this record cannot be presented or settled.
    pub async fn create_collection_invoice(
        &self,
        invoice: NewCollectionInvoice,
    ) -> Result<PendingCollectionInvoice, PersistenceError> {
        let mut transaction = self.pool.begin().await?;
        let raffle = sqlx::query(
            "SELECT entry_price_sats, end_time, status FROM raffles WHERE id = $1 FOR UPDATE",
        )
        .bind(invoice.raffle_id.as_uuid())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(PersistenceError::NotFound("raffle"))?;
        if raffle.get::<String, _>("status") != "OPEN" {
            return Err(PersistenceError::RaffleNotOpen);
        }
        let end_time = raffle.get::<OffsetDateTime, _>("end_time");
        if end_time <= invoice.requested_at {
            return Err(PersistenceError::RaffleClosed);
        }
        if invoice.ticket_count == 0 {
            return Err(PersistenceError::InvalidTicketCount);
        }
        let expires_at = invoice.expires_at.min(end_time);
        let amount_sats = as_u64(raffle.get::<i64, _>("entry_price_sats"))?
            .checked_mul(invoice.ticket_count)
            .ok_or(PersistenceError::IntegerOverflow)?;
        sqlx::query("INSERT INTO invoices (id, raffle_id, participant_public_id, payment_reference_hash, amount_sats, ticket_count, payout_address_ciphertext, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(invoice.id.as_uuid())
            .bind(invoice.raffle_id.as_uuid())
            .bind(invoice.participant_public_id.0.to_vec())
            .bind(invoice.payment_reference_hash.0.to_vec())
            .bind(as_i64(amount_sats)?)
            .bind(as_i64(invoice.ticket_count)?)
            .bind(invoice.payout_address_ciphertext)
            .bind(expires_at)
            .execute(&mut *transaction)
            .await?;
        write_audit(
            &mut transaction,
            invoice.raffle_id.as_uuid(),
            "invoice.collection_requested",
            json!({"invoice_id": invoice.id.as_uuid(), "amount_sats": amount_sats}),
        )
        .await?;
        transaction.commit().await?;
        Ok(PendingCollectionInvoice {
            id: invoice.id,
            amount_sats: Sats::new(amount_sats),
            expires_at,
        })
    }

    pub async fn store_collection_invoice(
        &self,
        invoice_id: InvoiceId,
        provider_quote_id: &str,
        provider_payment_hash: &str,
        bolt11: &str,
        expires_at: OffsetDateTime,
    ) -> Result<(), PersistenceError> {
        let result = sqlx::query("UPDATE invoices SET provider_quote_id = $2, provider_payment_hash = $3, bolt11 = $4, expires_at = $5 WHERE id = $1 AND status = 'pending' AND bolt11 IS NULL")
            .bind(invoice_id.as_uuid()).bind(provider_quote_id).bind(provider_payment_hash).bind(bolt11).bind(expires_at)
            .execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(PersistenceError::InvoiceNotEligible);
        }
        Ok(())
    }

    pub async fn fail_collection_invoice(
        &self,
        invoice_id: InvoiceId,
    ) -> Result<(), PersistenceError> {
        sqlx::query("UPDATE invoices SET status = 'collection_failed' WHERE id = $1 AND status = 'pending' AND bolt11 IS NULL")
            .bind(invoice_id.as_uuid()).execute(&self.pool).await?;
        Ok(())
    }

    /// A provider-confirmed failed or expired collection can no longer become an entry.
    /// This is intentionally separate from collection-creation failure: it preserves the
    /// issued invoice while making close reconciliation terminal and auditable.
    pub async fn mark_collection_uncollectible(
        &self,
        invoice_id: InvoiceId,
    ) -> Result<(), PersistenceError> {
        sqlx::query(
            "UPDATE invoices SET status = 'collection_failed' WHERE id = $1 AND status = 'pending'",
        )
        .bind(invoice_id.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn public_invoice(
        &self,
        invoice_id: InvoiceId,
    ) -> Result<PublicInvoice, PersistenceError> {
        let row = sqlx::query("SELECT i.id, i.amount_sats, i.ticket_count, i.status, i.bolt11, i.expires_at, e.ticket_start, e.ticket_end FROM invoices i LEFT JOIN entries e ON e.invoice_id = i.id WHERE i.id = $1")
            .bind(invoice_id.as_uuid()).fetch_optional(&self.pool).await?.ok_or(PersistenceError::NotFound("invoice"))?;
        Ok(PublicInvoice {
            id: InvoiceId::from(row.get::<Uuid, _>("id")),
            amount_sats: Sats::new(as_u64(row.get("amount_sats"))?),
            ticket_count: as_u64(row.get("ticket_count"))?,
            status: row.get("status"),
            bolt11: row.get("bolt11"),
            expires_at: row.get("expires_at"),
            ticket_start: row
                .try_get::<Option<i64>, _>("ticket_start")?
                .map(as_u64)
                .transpose()?,
            ticket_end: row
                .try_get::<Option<i64>, _>("ticket_end")?
                .map(as_u64)
                .transpose()?,
        })
    }

    /// Returns the provider reference required for a worker-side authoritative lookup.
    /// It deliberately never returns the encrypted payout address or provider payload.
    pub async fn collection_payment_reference(
        &self,
        invoice_id: InvoiceId,
    ) -> Result<Option<String>, PersistenceError> {
        let row = sqlx::query("SELECT status, provider_payment_hash FROM invoices WHERE id = $1")
            .bind(invoice_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(PersistenceError::NotFound("invoice"))?;
        if row.get::<String, _>("status") != "pending" {
            return Ok(None);
        }
        Ok(row.try_get("provider_payment_hash")?)
    }

    /// Stores a verified immutable provider receipt and schedules exactly one reconciliation job.
    pub async fn record_webhook_and_enqueue(
        &self,
        event: NewWebhookEvent,
    ) -> Result<bool, PersistenceError> {
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO webhook_events (id, provider_name, provider_event_id, payload_hash, raw_payload, signature_valid) VALUES ($1, $2, $3, $4, $5, true) ON CONFLICT (provider_name, provider_event_id) DO NOTHING")
            .bind(Uuid::new_v4())
            .bind(&event.provider_name)
            .bind(&event.provider_event_id)
            .bind(event.payload_hash.0.to_vec())
            .bind(&event.raw_payload)
            .execute(&mut *transaction)
            .await?
            .rows_affected() == 1;
        if inserted {
            sqlx::query("INSERT INTO jobs (id, kind, deduplication_key, payload) VALUES ($1, 'payment.reconcile', $2, $3) ON CONFLICT (kind, deduplication_key) DO NOTHING")
                .bind(Uuid::new_v4())
                .bind(format!("{}:{}", event.provider_name, event.provider_event_id))
                .bind(json!({"provider": event.provider_name, "event_id": event.provider_event_id, "invoice_id": event.invoice_id.as_uuid()}))
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(inserted)
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
        let empty = row.get::<i64, _>("total_tickets") == 0;
        let status = if empty {
            "CANCELLED"
        } else {
            "RANDOMNESS_PENDING"
        };
        sqlx::query("UPDATE raffles SET status = $2, entries_root = $3, frozen_at = now(), updated_at = now() WHERE id = $1")
            .bind(raffle_id.as_uuid()).bind(status).bind(root.0.to_vec()).execute(&mut *transaction).await?;
        write_audit(
            &mut transaction,
            raffle_id.as_uuid(),
            if empty {
                "raffle.cancelled_empty"
            } else {
                "raffle.frozen"
            },
            json!({"entries_root": root.to_hex(), "entry_count": count}),
        )
        .await?;
        write_outbox(
            &mut transaction,
            raffle_id.as_uuid(),
            if empty {
                "raffle.cancelled_empty"
            } else {
                "raffle.frozen"
            },
            json!({"entries_root": root.to_hex(), "entry_count": count}),
        )
        .await?;
        if !empty {
            sqlx::query("INSERT INTO jobs (id, kind, deduplication_key, payload) VALUES ($1, 'raffle.draw', $2, $3) ON CONFLICT (kind, deduplication_key) DO NOTHING")
                .bind(Uuid::new_v4()).bind(raffle_id.as_uuid().to_string()).bind(json!({"raffle_id": raffle_id.as_uuid()})).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(root)
    }

    pub async fn frozen_draw_context(
        &self,
        raffle_id: RaffleId,
    ) -> Result<FrozenDrawContext, PersistenceError> {
        let row = sqlx::query("SELECT end_time, randomness_delay_blocks FROM raffles WHERE id = $1 AND status = 'RANDOMNESS_PENDING'")
            .bind(raffle_id.as_uuid()).fetch_optional(&self.pool).await?.ok_or(PersistenceError::InvalidStatus)?;
        Ok(FrozenDrawContext {
            end_time: row.get("end_time"),
            randomness_delay_blocks: row.get("randomness_delay_blocks"),
        })
    }

    /// Starts close processing for every raffle whose configured cutoff has passed.
    /// The status transition and durable job insert share one transaction so a crash cannot
    /// leave an eligible raffle closed without work being scheduled.
    pub async fn schedule_due_closes(&self) -> Result<u64, PersistenceError> {
        let mut tx = self.pool.begin().await?;
        let ids = sqlx::query_scalar::<_, Uuid>("UPDATE raffles SET status = 'CLOSING', updated_at = now() WHERE status = 'OPEN' AND end_time <= now() RETURNING id")
            .fetch_all(&mut *tx).await?;
        for id in &ids {
            sqlx::query("INSERT INTO jobs (id, kind, deduplication_key, payload) VALUES ($1, 'raffle.close', $2, $3) ON CONFLICT (kind, deduplication_key) DO NOTHING")
                .bind(Uuid::new_v4()).bind(id.to_string()).bind(json!({"raffle_id": id})).execute(&mut *tx).await?;
            write_audit(&mut tx, *id, "raffle.closing", json!({})).await?;
        }
        tx.commit().await?;
        Ok(ids.len() as u64)
    }

    pub async fn pending_invoices(&self, raffle_id: RaffleId) -> Result<i64, PersistenceError> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM invoices WHERE raffle_id = $1 AND status = 'pending'",
        )
        .bind(raffle_id.as_uuid())
        .fetch_one(&self.pool)
        .await?)
    }

    /// Stores exactly one deterministic draw and creates the corresponding logical payout work.
    /// The unique draw constraint and deterministic idempotency keys make concurrent draw jobs safe.
    pub async fn create_draw_and_payouts(
        &self,
        raffle_id: RaffleId,
        close_height: u64,
        close_hash: Hash32,
        randomness_height: u64,
        randomness_hash: Hash32,
    ) -> Result<DrawResult, PersistenceError> {
        let mut tx = self.pool.begin().await?;
        let raffle = sqlx::query("SELECT status, entries_root, total_tickets, total_pool_sats FROM raffles WHERE id = $1 FOR UPDATE")
            .bind(raffle_id.as_uuid()).fetch_one(&mut *tx).await?;
        if raffle.get::<String, _>("status") != "RANDOMNESS_PENDING" {
            return Err(PersistenceError::InvalidStatus);
        }
        let root = hash_from_bytes(
            raffle
                .try_get::<Option<Vec<u8>>, _>("entries_root")?
                .ok_or(PersistenceError::InvalidStatus)?,
        )?;
        let rows = sqlx::query("SELECT id, raffle_id, entry_index, participant_public_id, payment_reference_hash, amount_sats, ticket_start, ticket_end, settled_at, previous_entry_hash, entry_hash FROM entries WHERE raffle_id = $1 ORDER BY entry_index")
            .bind(raffle_id.as_uuid()).fetch_all(&mut *tx).await?;
        let entries = rows
            .into_iter()
            .map(ledger_entry_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let result = draw(
            &entries,
            DrawInput {
                raffle_id,
                entries_root: root,
                close_block_height: close_height,
                randomness_block_height: randomness_height,
                randomness_block_hash: randomness_hash,
                total_tickets: as_u64(raffle.get("total_tickets"))?,
            },
        )
        .map_err(PersistenceError::Protocol)?;
        let split = sqlx::query("SELECT winner_bps, organizer_bps, platform_bps FROM payout_splits WHERE raffle_id = $1").bind(raffle_id.as_uuid()).fetch_one(&mut *tx).await?;
        let split = PayoutSplit::new(
            BasisPoints::new(split.get::<i32, _>("winner_bps") as u16)
                .map_err(PersistenceError::Domain)?,
            BasisPoints::new(split.get::<i32, _>("organizer_bps") as u16)
                .map_err(PersistenceError::Domain)?,
            BasisPoints::new(split.get::<i32, _>("platform_bps") as u16)
                .map_err(PersistenceError::Domain)?,
        )
        .map_err(PersistenceError::Domain)?;
        let amounts = split.allocate(Sats::new(as_u64(raffle.get("total_pool_sats"))?));
        sqlx::query("INSERT INTO draws (id, raffle_id, entries_root, randomness_block_hash, winning_ticket, winner_entry_id) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(Uuid::new_v4()).bind(raffle_id.as_uuid()).bind(root.0.to_vec()).bind(randomness_hash.0.to_vec()).bind(as_i64(result.winning_ticket)?).bind(result.winner_entry_id.as_uuid()).execute(&mut *tx).await?;
        for (recipient, amount) in [
            ("winner", amounts.winner),
            ("organizer", amounts.organizer),
            ("platform", amounts.platform),
        ] {
            if amount.as_u64() > 0 {
                sqlx::query("INSERT INTO payouts (id, raffle_id, recipient_type, idempotency_key, amount_sats) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (idempotency_key) DO NOTHING")
                .bind(Uuid::new_v4()).bind(raffle_id.as_uuid()).bind(recipient).bind(format!("openpool:{}:{}", raffle_id.as_uuid(), recipient)).bind(as_i64(amount.as_u64())?).execute(&mut *tx).await?;
            }
        }
        sqlx::query("INSERT INTO jobs (id, kind, deduplication_key, payload) VALUES ($1, 'payout.execute', $2, $3) ON CONFLICT (kind, deduplication_key) DO NOTHING")
            .bind(Uuid::new_v4()).bind(raffle_id.as_uuid().to_string()).bind(json!({"raffle_id": raffle_id.as_uuid()})).execute(&mut *tx).await?;
        sqlx::query("UPDATE raffles SET status = 'PAYOUT_PENDING', close_block_height = $2, close_block_hash = $3, randomness_block_height = $4, randomness_block_hash = $5, updated_at = now() WHERE id = $1")
            .bind(raffle_id.as_uuid()).bind(as_i64(close_height)?).bind(close_hash.0.to_vec()).bind(as_i64(randomness_height)?).bind(randomness_hash.0.to_vec()).execute(&mut *tx).await?;
        write_audit(&mut tx, raffle_id.as_uuid(), "draw.created", json!({"winning_ticket": result.winning_ticket, "winner_entry_id": result.winner_entry_id.as_uuid()})).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Constructs the canonical terminal proof exclusively from immutable database facts.
    pub async fn generate_terminal_proof(
        &self,
        raffle_id: RaffleId,
    ) -> Result<ProofDocument, PersistenceError> {
        let mut tx = self.pool.begin().await?;
        let raffle = sqlx::query("SELECT status, entries_root, entry_chain_head, total_tickets, total_pool_sats, close_block_height, randomness_block_height, randomness_block_hash FROM raffles WHERE id = $1 FOR UPDATE")
            .bind(raffle_id.as_uuid()).fetch_one(&mut *tx).await?;
        if raffle.get::<String, _>("status") != "PAID_OUT" {
            return Err(PersistenceError::InvalidStatus);
        }
        let rows = sqlx::query("SELECT id, raffle_id, entry_index, participant_public_id, payment_reference_hash, amount_sats, ticket_start, ticket_end, settled_at, previous_entry_hash, entry_hash FROM entries WHERE raffle_id = $1 ORDER BY entry_index").bind(raffle_id.as_uuid()).fetch_all(&mut *tx).await?;
        let entries = rows
            .into_iter()
            .map(ledger_entry_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let root = hash_from_bytes(
            raffle
                .try_get::<Option<Vec<u8>>, _>("entries_root")?
                .ok_or(PersistenceError::InvalidStatus)?,
        )?;
        let head = raffle
            .try_get::<Option<Vec<u8>>, _>("entry_chain_head")?
            .map(hash_from_bytes)
            .transpose()?
            .unwrap_or(ZERO_HASH);
        let close_height = as_u64(raffle.get::<i64, _>("close_block_height"))?;
        let randomness_height = as_u64(raffle.get::<i64, _>("randomness_block_height"))?;
        let randomness_hash = hash_from_bytes(
            raffle
                .try_get::<Option<Vec<u8>>, _>("randomness_block_hash")?
                .ok_or(PersistenceError::InvalidStatus)?,
        )?;
        let result = draw(
            &entries,
            DrawInput {
                raffle_id,
                entries_root: root,
                close_block_height: close_height,
                randomness_block_height: randomness_height,
                randomness_block_hash: randomness_hash,
                total_tickets: as_u64(raffle.get("total_tickets"))?,
            },
        )
        .map_err(PersistenceError::Protocol)?;
        let split = sqlx::query("SELECT winner_bps, organizer_bps, platform_bps FROM payout_splits WHERE raffle_id = $1").bind(raffle_id.as_uuid()).fetch_one(&mut *tx).await?;
        let split = PayoutSplit::new(
            BasisPoints::new(split.get::<i32, _>("winner_bps") as u16)
                .map_err(PersistenceError::Domain)?,
            BasisPoints::new(split.get::<i32, _>("organizer_bps") as u16)
                .map_err(PersistenceError::Domain)?,
            BasisPoints::new(split.get::<i32, _>("platform_bps") as u16)
                .map_err(PersistenceError::Domain)?,
        )
        .map_err(PersistenceError::Domain)?;
        let payload = ProofPayload {
            protocol_version: PROTOCOL_VERSION.into(),
            raffle_id,
            payout_split: split,
            entries,
            freeze: FreezeFacts {
                entry_count: sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM entries WHERE raffle_id = $1",
                )
                .bind(raffle_id.as_uuid())
                .fetch_one(&mut *tx)
                .await? as u64,
                total_tickets: as_u64(raffle.get("total_tickets"))?,
                total_pool_sats: Sats::new(as_u64(raffle.get("total_pool_sats"))?),
                entry_chain_head: head,
                entries_root: root,
            },
            bitcoin: BitcoinFacts {
                close_block_height: close_height,
                randomness_block_height: randomness_height,
                randomness_block_hash: randomness_hash,
            },
            draw: result,
            payouts: split.allocate(Sats::new(as_u64(raffle.get("total_pool_sats"))?)),
        };
        let proof = ProofDocument::create(payload).map_err(PersistenceError::Protocol)?;
        if !openpool_verifier::verify(&proof).is_verified() {
            return Err(PersistenceError::ProofInvalid);
        }
        sqlx::query("INSERT INTO proofs (id, raffle_id, protocol_version, proof_json, proof_hash) VALUES ($1, $2, $3, $4, $5)")
            .bind(Uuid::new_v4()).bind(raffle_id.as_uuid()).bind(PROTOCOL_VERSION).bind(serde_json::to_value(&proof).map_err(|e| PersistenceError::ProofEncoding(e.to_string()))?).bind(proof.proof_hash.0.to_vec()).execute(&mut *tx).await?;
        write_audit(
            &mut tx,
            raffle_id.as_uuid(),
            "proof.generated",
            json!({"proof_hash": proof.proof_hash.to_hex()}),
        )
        .await?;
        tx.commit().await?;
        Ok(proof)
    }

    pub async fn public_proof(
        &self,
        raffle_id: RaffleId,
    ) -> Result<ProofDocument, PersistenceError> {
        let value = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT proof_json FROM proofs WHERE raffle_id = $1 AND protocol_version = $2",
        )
        .bind(raffle_id.as_uuid())
        .bind(PROTOCOL_VERSION)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(PersistenceError::NotFound("proof"))?;
        serde_json::from_value(value)
            .map_err(|error| PersistenceError::ProofEncoding(error.to_string()))
    }

    pub async fn public_proof_metadata(
        &self,
        raffle_id: RaffleId,
    ) -> Result<crate::contract::PublicProofMetadata, PersistenceError> {
        let row = sqlx::query("SELECT proof_hash, storage_uri, storage_version_id, storage_etag, published_at FROM proofs WHERE raffle_id = $1 AND protocol_version = $2")
            .bind(raffle_id.as_uuid()).bind(PROTOCOL_VERSION).fetch_optional(&self.pool).await?
            .ok_or(PersistenceError::NotFound("proof"))?;
        Ok(crate::contract::PublicProofMetadata {
            raffle_id: raffle_id.as_uuid(),
            proof_hash: hash_from_bytes(row.get("proof_hash"))?.to_hex(),
            storage_uri: row.get("storage_uri"),
            storage_version_id: row.get("storage_version_id"),
            storage_etag: row.get("storage_etag"),
            published_at: row.get("published_at"),
        })
    }

    /// Commits immutable-object identity only after a successful Object-Lock publication. A
    /// retry may repeat the exact values but can never replace a version already recorded.
    pub async fn record_proof_publication(
        &self,
        raffle_id: RaffleId,
        storage_uri: &str,
        version_id: &str,
        etag: &str,
    ) -> Result<(), PersistenceError> {
        let result = sqlx::query(
            "UPDATE proofs SET storage_uri = $3, storage_version_id = $4, storage_etag = $5, published_at = now()
             WHERE raffle_id = $1 AND protocol_version = $2 AND storage_uri IS NULL",
        )
        .bind(raffle_id.as_uuid())
        .bind(PROTOCOL_VERSION)
        .bind(storage_uri)
        .bind(version_id)
        .bind(etag)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            return Ok(());
        }
        let existing = sqlx::query("SELECT storage_uri, storage_version_id, storage_etag FROM proofs WHERE raffle_id = $1 AND protocol_version = $2")
            .bind(raffle_id.as_uuid()).bind(PROTOCOL_VERSION).fetch_optional(&self.pool).await?
            .ok_or(PersistenceError::NotFound("proof"))?;
        if existing.get::<Option<String>, _>("storage_uri").as_deref() == Some(storage_uri)
            && existing
                .get::<Option<String>, _>("storage_version_id")
                .as_deref()
                == Some(version_id)
            && existing.get::<Option<String>, _>("storage_etag").as_deref() == Some(etag)
        {
            Ok(())
        } else {
            Err(PersistenceError::ProofAlreadyPublished)
        }
    }

    pub async fn public_result(
        &self,
        raffle_id: RaffleId,
    ) -> Result<crate::contract::PublicResult, PersistenceError> {
        let raffle = sqlx::query("SELECT status FROM raffles WHERE id = $1")
            .bind(raffle_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(PersistenceError::NotFound("raffle"))?;
        let winning_ticket =
            sqlx::query_scalar::<_, i64>("SELECT winning_ticket FROM draws WHERE raffle_id = $1")
                .bind(raffle_id.as_uuid())
                .fetch_optional(&self.pool)
                .await?
                .map(as_u64)
                .transpose()?;
        let payout_statuses = sqlx::query_scalar::<_, String>(
            "SELECT status FROM payouts WHERE raffle_id = $1 ORDER BY recipient_type",
        )
        .bind(raffle_id.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        let proof_available = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM proofs WHERE raffle_id = $1 AND protocol_version = $2)",
        )
        .bind(raffle_id.as_uuid())
        .bind(PROTOCOL_VERSION)
        .fetch_one(&self.pool)
        .await?;
        Ok(crate::contract::PublicResult {
            raffle_id: raffle_id.as_uuid(),
            status: raffle.get("status"),
            winning_ticket,
            payout_statuses,
            proof_available,
        })
    }

    /// Records refund eligibility without touching the immutable entry ledger.
    pub async fn create_refund_eligibility(
        &self,
        raffle_id: RaffleId,
        invoice_id: InvoiceId,
        reason: &str,
    ) -> Result<Uuid, PersistenceError> {
        let id = Uuid::new_v4();
        let result = sqlx::query("INSERT INTO refunds (id, raffle_id, invoice_id, reason) VALUES ($1, $2, $3, $4) ON CONFLICT (invoice_id, reason) DO NOTHING")
            .bind(id).bind(raffle_id.as_uuid()).bind(invoice_id.as_uuid()).bind(reason).execute(&self.pool).await?;
        if result.rows_affected() == 1 {
            return Ok(id);
        }
        sqlx::query_scalar("SELECT id FROM refunds WHERE invoice_id = $1 AND reason = $2")
            .bind(invoice_id.as_uuid())
            .bind(reason)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }

    /// Converts a post-cutoff settled invoice into an idempotent refund obligation.
    /// It never mutates entries, frozen totals, or draw facts.
    pub async fn mark_late_settlement_for_refund(
        &self,
        invoice_id: InvoiceId,
    ) -> Result<Uuid, PersistenceError> {
        let mut tx = self.pool.begin().await?;
        let invoice =
            sqlx::query("SELECT raffle_id, status FROM invoices WHERE id = $1 FOR UPDATE")
                .bind(invoice_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(PersistenceError::NotFound("invoice"))?;
        let raffle_id: Uuid = invoice.get("raffle_id");
        if invoice.get::<String, _>("status") == "pending" {
            sqlx::query("UPDATE invoices SET status = 'ineligible' WHERE id = $1")
                .bind(invoice_id.as_uuid())
                .execute(&mut *tx)
                .await?;
        }
        let refund_id = Uuid::new_v4();
        sqlx::query("INSERT INTO refunds (id, raffle_id, invoice_id, reason) VALUES ($1, $2, $3, 'late_settlement') ON CONFLICT (invoice_id, reason) DO NOTHING")
            .bind(refund_id).bind(raffle_id).bind(invoice_id.as_uuid()).execute(&mut *tx).await?;
        let id = sqlx::query_scalar(
            "SELECT id FROM refunds WHERE invoice_id = $1 AND reason = 'late_settlement'",
        )
        .bind(invoice_id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        write_audit(
            &mut tx,
            raffle_id,
            "invoice.late_settlement",
            json!({"invoice_id": invoice_id.as_uuid(), "refund_id": id}),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn public_refund(
        &self,
        refund_id: Uuid,
    ) -> Result<crate::contract::PublicRefund, PersistenceError> {
        let row = sqlx::query("SELECT id, status, reason FROM refunds WHERE id = $1")
            .bind(refund_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(PersistenceError::NotFound("refund"))?;
        Ok(crate::contract::PublicRefund {
            id: row.get("id"),
            status: row.get("status"),
            reason: row.get("reason"),
        })
    }

    pub async fn create_oidc_session(
        &self,
        session: NewOidcSession,
    ) -> Result<(), PersistenceError> {
        sqlx::query("INSERT INTO oidc_sessions (id, subject, roles, csrf_token_hash, expires_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(session.id).bind(session.subject).bind(session.roles).bind(session.csrf_token_hash).bind(session.expires_at).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oidc_session(&self, id: Uuid) -> Result<Option<OidcSession>, PersistenceError> {
        let row = sqlx::query("SELECT subject, roles, csrf_token_hash, expires_at FROM oidc_sessions WHERE id = $1 AND expires_at > now()").bind(id).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(OidcSession {
                subject: row.get("subject"),
                roles: row.get("roles"),
                csrf_token_hash: row.get("csrf_token_hash"),
                expires_at: row.get("expires_at"),
            })
        })
        .transpose()
    }

    pub async fn delete_oidc_session(&self, id: Uuid) -> Result<(), PersistenceError> {
        sqlx::query("DELETE FROM oidc_sessions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn operator_jobs(
        &self,
    ) -> Result<Vec<crate::contract::OperatorJob>, PersistenceError> {
        let rows = sqlx::query(
            "SELECT id, kind, status, attempts, max_attempts, last_error FROM jobs ORDER BY updated_at DESC LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::contract::OperatorJob {
                id: row.get("id"),
                kind: row.get("kind"),
                status: row.get("status"),
                attempts: row.get("attempts"),
                max_attempts: row.get("max_attempts"),
                last_error: row.get("last_error"),
            })
            .collect())
    }

    /// Requeue only a terminally dead job. Running jobs retain their lease and queued jobs retain
    /// their normal ordering, so an operator cannot duplicate an external payment effect.
    pub async fn retry_dead_job(&self, id: Uuid) -> Result<bool, PersistenceError> {
        let result = sqlx::query(
            "UPDATE jobs SET status = 'queued', attempts = 0, available_at = now(), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, updated_at = now() WHERE id = $1 AND status = 'dead'",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn create_oidc_login_attempt(
        &self,
        attempt: NewOidcLoginAttempt,
    ) -> Result<(), PersistenceError> {
        sqlx::query("INSERT INTO oidc_login_attempts (state, nonce, pkce_verifier, expires_at) VALUES ($1, $2, $3, $4)")
            .bind(attempt.state).bind(attempt.nonce).bind(attempt.pkce_verifier).bind(attempt.expires_at).execute(&self.pool).await?;
        Ok(())
    }

    /// Consumes the state exactly once; stale callbacks cannot be replayed.
    pub async fn consume_oidc_login_attempt(
        &self,
        state: &str,
    ) -> Result<Option<OidcLoginAttempt>, PersistenceError> {
        let row = sqlx::query("DELETE FROM oidc_login_attempts WHERE state = $1 AND expires_at > now() RETURNING nonce, pkce_verifier")
            .bind(state).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| OidcLoginAttempt {
            nonce: row.get("nonce"),
            pkce_verifier: row.get("pkce_verifier"),
        }))
    }

    /// Atomically claims one retryable payout before any provider call.
    pub async fn claim_payout(
        &self,
        raffle_id: RaffleId,
        worker_id: &str,
    ) -> Result<Option<ClaimedPayout>, PersistenceError> {
        let row = sqlx::query("WITH candidate AS (SELECT id FROM payouts WHERE raffle_id = $1 AND (status IN ('pending', 'failed') OR (status = 'processing' AND lease_expires_at < now())) ORDER BY CASE WHEN status = 'processing' THEN 0 ELSE 1 END, created_at FOR UPDATE SKIP LOCKED LIMIT 1) UPDATE payouts SET status = 'processing', failure_reason = NULL, lease_owner = $2, lease_expires_at = now() + interval '60 seconds' WHERE id = (SELECT id FROM candidate) RETURNING id, raffle_id, recipient_type, amount_sats, idempotency_key")
            .bind(raffle_id.as_uuid()).bind(worker_id).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(ClaimedPayout {
                id: row.get("id"),
                raffle_id: RaffleId::from(row.get::<Uuid, _>("raffle_id")),
                recipient_type: row.get("recipient_type"),
                amount_sats: Sats::new(as_u64(row.get("amount_sats"))?),
                idempotency_key: row.get("idempotency_key"),
            })
        })
        .transpose()
    }

    pub async fn payout_recipient_ciphertext(
        &self,
        payout: &ClaimedPayout,
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        let sql = match payout.recipient_type.as_str() {
            "winner" => {
                "SELECT i.payout_address_ciphertext FROM draws d JOIN entries e ON e.id = d.winner_entry_id JOIN invoices i ON i.id = e.invoice_id WHERE d.raffle_id = $1"
            }
            "organizer" => {
                "SELECT o.lightning_address_ciphertext FROM raffles r JOIN organizers o ON o.id = r.organizer_id WHERE r.id = $1"
            }
            "platform" => return Ok(None),
            _ => return Err(PersistenceError::InvalidStatus),
        };
        sqlx::query_scalar(sql)
            .bind(payout.raffle_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn settle_payout(
        &self,
        payout_id: Uuid,
        worker_id: &str,
        provider_reference: &str,
    ) -> Result<bool, PersistenceError> {
        let mut tx = self.pool.begin().await?;
        let raffle_id: Uuid = sqlx::query_scalar("UPDATE payouts SET status = 'settled', provider_reference = $3, settled_at = now(), lease_owner = NULL, lease_expires_at = NULL WHERE id = $1 AND status = 'processing' AND lease_owner = $2 RETURNING raffle_id")
            .bind(payout_id).bind(worker_id).bind(provider_reference).fetch_one(&mut *tx).await?;
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM payouts WHERE raffle_id = $1 AND status != 'settled'",
        )
        .bind(raffle_id)
        .fetch_one(&mut *tx)
        .await?;
        if remaining == 0 {
            sqlx::query("UPDATE raffles SET status = 'PAID_OUT', updated_at = now() WHERE id = $1 AND status = 'PAYOUT_PENDING'").bind(raffle_id).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO jobs (id, kind, deduplication_key, payload) VALUES ($1, 'proof.generate', $2, $3) ON CONFLICT (kind, deduplication_key) DO NOTHING")
                .bind(Uuid::new_v4()).bind(raffle_id.to_string()).bind(json!({"raffle_id": raffle_id})).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(remaining == 0)
    }

    pub async fn fail_payout(
        &self,
        payout_id: Uuid,
        worker_id: &str,
        reason: &str,
    ) -> Result<(), PersistenceError> {
        sqlx::query("UPDATE payouts SET status = 'failed', failure_reason = $3, lease_owner = NULL, lease_expires_at = NULL WHERE id = $1 AND status = 'processing' AND lease_owner = $2").bind(payout_id).bind(worker_id).bind(reason).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn organizer_exists(
        &self,
        organizer_id: OrganizerId,
    ) -> Result<bool, PersistenceError> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM organizers WHERE id = $1 AND status = 'active')",
        )
        .bind(organizer_id.as_uuid())
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn transition_raffle(
        &self,
        organizer_id: OrganizerId,
        raffle_id: RaffleId,
        status: &str,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query("UPDATE raffles SET status = $3, updated_at = now() WHERE id = $1 AND organizer_id = $2 AND ((status = 'DRAFT' AND $3 = 'SCHEDULED') OR (status = 'SCHEDULED' AND $3 = 'OPEN') OR (status IN ('DRAFT', 'SCHEDULED', 'OPEN') AND $3 = 'CANCELLED'))")
            .bind(raffle_id.as_uuid()).bind(organizer_id.as_uuid()).bind(status).execute(&mut *transaction).await?;
        if result.rows_affected() != 1 {
            return Err(PersistenceError::InvalidStatus);
        }
        write_audit(
            &mut transaction,
            raffle_id.as_uuid(),
            "raffle.status_changed",
            json!({"status": status, "actor_id": organizer_id.as_uuid()}),
        )
        .await?;
        write_outbox(
            &mut transaction,
            raffle_id.as_uuid(),
            "raffle.status_changed",
            json!({"status": status}),
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn update_raffle(
        &self,
        organizer_id: OrganizerId,
        raffle: NewDraftRaffle,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query("UPDATE raffles SET name = $3, entry_price_sats = $4, start_time = $5, end_time = $6, randomness_delay_blocks = $7, updated_at = now() WHERE id = $1 AND organizer_id = $2 AND status IN ('DRAFT', 'SCHEDULED')")
            .bind(raffle.id.as_uuid()).bind(organizer_id.as_uuid()).bind(raffle.name).bind(as_i64(raffle.entry_price_sats.as_u64())?).bind(raffle.start_time).bind(raffle.end_time).bind(raffle.randomness_delay_blocks).execute(&mut *transaction).await?;
        if result.rows_affected() != 1 {
            return Err(PersistenceError::InvalidStatus);
        }
        sqlx::query("UPDATE payout_splits SET winner_bps = $2, organizer_bps = $3, platform_bps = $4 WHERE raffle_id = $1")
            .bind(raffle.id.as_uuid())
            .bind(i32::from(raffle.payout_split.winner.as_u16()))
            .bind(i32::from(raffle.payout_split.organizer.as_u16()))
            .bind(i32::from(raffle.payout_split.platform.as_u16()))
            .execute(&mut *transaction).await?;
        write_audit(
            &mut transaction,
            raffle.id.as_uuid(),
            "raffle.updated",
            json!({"actor_id": organizer_id.as_uuid()}),
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn list_public_raffles(&self) -> Result<Vec<PublicRaffle>, PersistenceError> {
        let rows = sqlx::query("SELECT id, name, entry_price_sats, start_time, end_time, status, total_pool_sats, total_tickets, winner_bps, organizer_bps, platform_bps FROM raffles JOIN payout_splits ON payout_splits.raffle_id = raffles.id ORDER BY created_at DESC")
            .fetch_all(&self.pool).await?;
        rows.into_iter().map(public_raffle_from_row).collect()
    }

    pub async fn public_raffle(
        &self,
        raffle_id: RaffleId,
    ) -> Result<PublicRaffle, PersistenceError> {
        let row = sqlx::query("SELECT id, name, entry_price_sats, start_time, end_time, status, total_pool_sats, total_tickets, winner_bps, organizer_bps, platform_bps FROM raffles JOIN payout_splits ON payout_splits.raffle_id = raffles.id WHERE raffles.id = $1")
            .bind(raffle_id.as_uuid()).fetch_optional(&self.pool).await?.ok_or(PersistenceError::NotFound("raffle"))?;
        public_raffle_from_row(row)
    }

    pub async fn organizer_raffles(
        &self,
        organizer_id: OrganizerId,
    ) -> Result<Vec<PublicRaffle>, PersistenceError> {
        let rows = sqlx::query("SELECT id, name, entry_price_sats, start_time, end_time, status, total_pool_sats, total_tickets, winner_bps, organizer_bps, platform_bps FROM raffles JOIN payout_splits ON payout_splits.raffle_id = raffles.id WHERE organizer_id = $1 ORDER BY created_at DESC")
            .bind(organizer_id.as_uuid())
            .fetch_all(&self.pool).await?;
        rows.into_iter().map(public_raffle_from_row).collect()
    }

    pub async fn public_pool(
        &self,
        raffle_id: RaffleId,
    ) -> Result<crate::contract::PublicPool, PersistenceError> {
        let row = sqlx::query(
            "SELECT total_pool_sats, total_tickets, entry_chain_head FROM raffles WHERE id = $1",
        )
        .bind(raffle_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(PersistenceError::NotFound("raffle"))?;
        Ok(crate::contract::PublicPool {
            raffle_id: raffle_id.as_uuid(),
            total_pool_sats: as_u64(row.get("total_pool_sats"))?,
            total_tickets: as_u64(row.get("total_tickets"))?,
            entry_chain_head: row
                .try_get::<Option<Vec<u8>>, _>("entry_chain_head")?
                .map(hex::encode),
        })
    }

    pub async fn public_entries(
        &self,
        raffle_id: RaffleId,
        cursor: u64,
        limit: u32,
    ) -> Result<crate::contract::EntryPage, PersistenceError> {
        let rows = sqlx::query("SELECT entry_index, participant_public_id, payment_reference_hash, amount_sats, ticket_start, ticket_end, settled_at, entry_hash FROM entries WHERE raffle_id = $1 AND entry_index >= $2 ORDER BY entry_index LIMIT $3")
            .bind(raffle_id.as_uuid()).bind(as_i64(cursor)?).bind(i64::from(limit + 1)).fetch_all(&self.pool).await?;
        let has_more = rows.len() > limit as usize;
        let mut entries = Vec::new();
        for row in rows.into_iter().take(limit as usize) {
            entries.push(crate::contract::PublicEntry {
                entry_index: as_u64(row.get("entry_index"))?,
                participant_public_id: hex::encode(row.get::<Vec<u8>, _>("participant_public_id")),
                payment_reference_hash: hex::encode(
                    row.get::<Vec<u8>, _>("payment_reference_hash"),
                ),
                amount_sats: as_u64(row.get("amount_sats"))?,
                ticket_start: as_u64(row.get("ticket_start"))?,
                ticket_end: as_u64(row.get("ticket_end"))?,
                settled_at: row.get("settled_at"),
                entry_hash: hex::encode(row.get::<Vec<u8>, _>("entry_hash")),
            });
        }
        let next_cursor = has_more.then(|| cursor + limit as u64);
        Ok(crate::contract::EntryPage {
            entries,
            next_cursor,
        })
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
pub struct NewCollectionInvoice {
    pub id: InvoiceId,
    pub raffle_id: RaffleId,
    pub participant_public_id: Hash32,
    pub payment_reference_hash: Hash32,
    pub payout_address_ciphertext: Vec<u8>,
    pub ticket_count: u64,
    pub requested_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}
pub struct NewWebhookEvent {
    pub provider_name: String,
    pub provider_event_id: String,
    pub invoice_id: InvoiceId,
    pub payload_hash: Hash32,
    pub raw_payload: serde_json::Value,
}
pub struct PendingCollectionInvoice {
    pub id: InvoiceId,
    pub amount_sats: Sats,
    pub expires_at: OffsetDateTime,
}
pub struct FrozenDrawContext {
    pub end_time: OffsetDateTime,
    pub randomness_delay_blocks: i32,
}
pub struct ClaimedPayout {
    pub id: Uuid,
    pub raffle_id: RaffleId,
    pub recipient_type: String,
    pub amount_sats: Sats,
    pub idempotency_key: String,
}
pub struct NewOidcSession {
    pub id: Uuid,
    pub subject: Uuid,
    pub roles: serde_json::Value,
    pub csrf_token_hash: Vec<u8>,
    pub expires_at: OffsetDateTime,
}
pub struct OidcSession {
    pub subject: Uuid,
    pub roles: serde_json::Value,
    pub csrf_token_hash: Vec<u8>,
    pub expires_at: OffsetDateTime,
}
pub struct NewOidcLoginAttempt {
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub expires_at: OffsetDateTime,
}
pub struct OidcLoginAttempt {
    pub nonce: String,
    pub pkce_verifier: String,
}
#[derive(serde::Serialize)]
pub struct PublicRaffle {
    pub id: RaffleId,
    pub name: String,
    pub entry_price_sats: Sats,
    pub start_time: OffsetDateTime,
    pub end_time: OffsetDateTime,
    pub status: String,
    pub total_pool_sats: Sats,
    pub total_tickets: u64,
    pub winner_bps: i32,
    pub organizer_bps: i32,
    pub platform_bps: i32,
}
impl From<PublicRaffle> for crate::contract::PublicRaffle {
    fn from(value: PublicRaffle) -> Self {
        Self {
            id: value.id.as_uuid(),
            name: value.name,
            entry_price_sats: value.entry_price_sats.as_u64(),
            start_time: value.start_time,
            end_time: value.end_time,
            status: value.status,
            total_pool_sats: value.total_pool_sats.as_u64(),
            total_tickets: value.total_tickets,
            winner_bps: value
                .winner_bps
                .try_into()
                .expect("database payout split is validated"),
            organizer_bps: value
                .organizer_bps
                .try_into()
                .expect("database payout split is validated"),
            platform_bps: value
                .platform_bps
                .try_into()
                .expect("database payout split is validated"),
        }
    }
}
#[derive(serde::Serialize)]
pub struct PublicInvoice {
    pub id: InvoiceId,
    pub amount_sats: Sats,
    pub ticket_count: u64,
    pub status: String,
    pub bolt11: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub ticket_start: Option<u64>,
    pub ticket_end: Option<u64>,
}
impl From<PublicInvoice> for crate::contract::PublicInvoice {
    fn from(value: PublicInvoice) -> Self {
        Self {
            id: value.id.as_uuid(),
            amount_sats: value.amount_sats.as_u64(),
            ticket_count: value.ticket_count,
            status: value.status,
            bolt11: value.bolt11,
            expires_at: value.expires_at,
            ticket_start: value.ticket_start,
            ticket_end: value.ticket_end,
        }
    }
}
fn public_raffle_from_row(row: sqlx::postgres::PgRow) -> Result<PublicRaffle, PersistenceError> {
    Ok(PublicRaffle {
        id: RaffleId::from(row.get::<Uuid, _>("id")),
        name: row.get("name"),
        entry_price_sats: Sats::new(as_u64(row.get("entry_price_sats"))?),
        start_time: row.get("start_time"),
        end_time: row.get("end_time"),
        status: row.get("status"),
        total_pool_sats: Sats::new(as_u64(row.get("total_pool_sats"))?),
        total_tickets: as_u64(row.get("total_tickets"))?,
        winner_bps: row.get("winner_bps"),
        organizer_bps: row.get("organizer_bps"),
        platform_bps: row.get("platform_bps"),
    })
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
    Domain(#[from] openpool_protocol::DomainError),
    #[error(transparent)]
    Protocol(#[from] openpool_protocol::ProtocolError),
    #[error("generated proof did not pass the native verifier")]
    ProofInvalid,
    #[error("proof was already published with different immutable object metadata")]
    ProofAlreadyPublished,
    #[error("proof encoding failed: {0}")]
    ProofEncoding(String),
    #[error("{0} was not found")]
    NotFound(&'static str),
    #[error("invoice is not eligible for settlement")]
    InvoiceNotEligible,
    #[error("raffle is not open")]
    RaffleNotOpen,
    #[error("raffle is closed for new invoices")]
    RaffleClosed,
    #[error("ticket count must be positive")]
    InvalidTicketCount,
    #[error("raffle is not in the required state")]
    InvalidStatus,
    #[error("integer conversion or addition overflowed")]
    IntegerOverflow,
    #[error("database hash is not 32 bytes")]
    InvalidHashLength,
}
