use std::env;

use openpool_domain::{BasisPoints, EntryId, InvoiceId, OrganizerId, PayoutSplit, RaffleId, Sats};
use openpool_persistence_sqlx::{NewDraftRaffle, NewInvoice, NewOrganizer, Persistence};
use openpool_protocol::Hash32;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

fn database_url() -> String {
    env::var("DATABASE_URL").expect("DATABASE_URL is required for PostgreSQL integration tests")
}

#[tokio::test]
#[ignore = "requires a PostgreSQL DATABASE_URL"]
async fn migrations_and_concurrent_entry_appends_are_safe() {
    let persistence = Persistence::connect(&database_url()).await.unwrap();
    persistence.migrate().await.unwrap();
    sqlx::query("TRUNCATE audit_events, outbox_events, entries, invoices, payout_splits, raffles, organizers CASCADE")
        .execute(persistence.pool())
        .await
        .unwrap();

    let organizer = OrganizerId::from(Uuid::new_v4());
    persistence
        .create_organizer(NewOrganizer {
            id: organizer,
            display_name: "Test organizer".into(),
            lightning_address_ciphertext: vec![1],
        })
        .await
        .unwrap();
    let raffle = RaffleId::from(Uuid::new_v4());
    let split = PayoutSplit::new(
        BasisPoints::new(9_500).unwrap(),
        BasisPoints::new(400).unwrap(),
        BasisPoints::new(100).unwrap(),
    )
    .unwrap();
    let now = OffsetDateTime::now_utc();
    persistence
        .create_draft_raffle(NewDraftRaffle {
            id: raffle,
            organizer_id: organizer,
            name: "Concurrency raffle".into(),
            entry_price_sats: Sats::new(1_000),
            start_time: now,
            end_time: now + time::Duration::hours(1),
            randomness_delay_blocks: 6,
            payout_split: split,
        })
        .await
        .unwrap();
    sqlx::query("UPDATE raffles SET status = 'OPEN' WHERE id = $1")
        .bind(raffle.as_uuid())
        .execute(persistence.pool())
        .await
        .unwrap();

    let first_invoice = InvoiceId::from(Uuid::new_v4());
    let second_invoice = InvoiceId::from(Uuid::new_v4());
    for (invoice, participant, payment, tickets) in [
        (first_invoice, [1; 32], [11; 32], 2),
        (second_invoice, [2; 32], [12; 32], 3),
    ] {
        persistence
            .create_pending_invoice(NewInvoice {
                id: invoice,
                raffle_id: raffle,
                participant_public_id: Hash32::from_bytes(participant),
                payment_reference_hash: Hash32::from_bytes(payment),
                amount_sats: Sats::new(1_000),
                ticket_count: tickets,
            })
            .await
            .unwrap();
    }
    let left = persistence.clone();
    let right = persistence.clone();
    let (first, second) = tokio::join!(
        left.settle_invoice_and_append_entry(first_invoice, EntryId::from(Uuid::new_v4()), now),
        right.settle_invoice_and_append_entry(second_invoice, EntryId::from(Uuid::new_v4()), now),
    );
    first.unwrap();
    second.unwrap();
    let rows = sqlx::query(
        "SELECT ticket_start, ticket_end FROM entries WHERE raffle_id = $1 ORDER BY ticket_start",
    )
    .bind(raffle.as_uuid())
    .fetch_all(persistence.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<i64, _>("ticket_start"), 0);
    assert_eq!(
        rows[0].get::<i64, _>("ticket_end"),
        rows[1].get::<i64, _>("ticket_start")
    );
    assert_eq!(rows[1].get::<i64, _>("ticket_end"), 5);

    sqlx::query("UPDATE raffles SET status = 'CLOSING' WHERE id = $1")
        .bind(raffle.as_uuid())
        .execute(persistence.pool())
        .await
        .unwrap();
    let root = persistence.freeze_raffle(raffle).await.unwrap();
    assert_ne!(root, Hash32::from_bytes([0; 32]));

    let draw = persistence
        .create_draw_and_payouts(
            raffle,
            100,
            Hash32::from_bytes([3; 32]),
            106,
            Hash32::from_bytes([4; 32]),
        )
        .await
        .unwrap();
    assert!(draw.winning_ticket < 5);
    let payouts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payouts WHERE raffle_id = $1")
        .bind(raffle.as_uuid())
        .fetch_one(persistence.pool())
        .await
        .unwrap();
    assert_eq!(payouts, 3);
    let claimed = persistence
        .claim_payout(raffle, "crashed-worker")
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE payouts SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(claimed.id)
        .execute(persistence.pool())
        .await
        .unwrap();
    let reclaimed = persistence
        .claim_payout(raffle, "recovery-worker")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed.id, claimed.id);
    persistence
        .settle_payout(
            reclaimed.id,
            "recovery-worker",
            "fixture-provider-reference",
        )
        .await
        .unwrap();
    while let Some(payout) = persistence
        .claim_payout(raffle, "test-worker")
        .await
        .unwrap()
    {
        persistence
            .settle_payout(payout.id, "test-worker", "fixture-provider-reference")
            .await
            .unwrap();
    }
    let proof = persistence.generate_terminal_proof(raffle).await.unwrap();
    assert!(openpool_verifier::verify(&proof).is_verified());

    let late_invoice = InvoiceId::from(Uuid::new_v4());
    persistence
        .create_pending_invoice(NewInvoice {
            id: late_invoice,
            raffle_id: raffle,
            participant_public_id: Hash32::from_bytes([9; 32]),
            payment_reference_hash: Hash32::from_bytes([19; 32]),
            amount_sats: Sats::new(1_000),
            ticket_count: 1,
        })
        .await
        .unwrap();
    let refund_id = persistence
        .mark_late_settlement_for_refund(late_invoice)
        .await
        .unwrap();
    let refund = persistence.public_refund(refund_id).await.unwrap();
    assert_eq!(refund.reason, "late_settlement");
    assert_eq!(refund.status, "awaiting_invoice");

    let empty_raffle = RaffleId::from(Uuid::new_v4());
    persistence
        .create_draft_raffle(NewDraftRaffle {
            id: empty_raffle,
            organizer_id: organizer,
            name: "Empty raffle".into(),
            entry_price_sats: Sats::new(1_000),
            start_time: now,
            end_time: now + time::Duration::hours(1),
            randomness_delay_blocks: 1,
            payout_split: split,
        })
        .await
        .unwrap();
    sqlx::query("UPDATE raffles SET status = 'CLOSING' WHERE id = $1")
        .bind(empty_raffle.as_uuid())
        .execute(persistence.pool())
        .await
        .unwrap();
    persistence.freeze_raffle(empty_raffle).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM raffles WHERE id = $1")
        .bind(empty_raffle.as_uuid())
        .fetch_one(persistence.pool())
        .await
        .unwrap();
    assert_eq!(status, "CANCELLED");
    let draws: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'raffle.draw' AND deduplication_key = $1",
    )
    .bind(empty_raffle.as_uuid().to_string())
    .fetch_one(persistence.pool())
    .await
    .unwrap();
    assert_eq!(draws, 0);
}
