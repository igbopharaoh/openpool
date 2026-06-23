use std::{error::Error, sync::Arc};

use openpool_api::{AddressCipher, health_router};
use openpool_bitcoin::{BitcoinSource, Esplora};
use openpool_config::{AppConfig, Environment};
use openpool_domain::{EntryId, InvoiceId};
use openpool_jobs::{ClaimedJob, JobStore};
use openpool_payments::{
    FakeLightningAddressResolver, FakePaymentProvider, LightningAddressResolver,
    LnurlLightningAddressResolver, PaymentProvider, PaymentState, PayoutRequest,
};
use openpool_payments_mavapay::Mavapay;
use openpool_persistence_sqlx::Persistence;
use openpool_proof_storage::{ProofStore, S3ProofStore, S3ProofStoreSettings};
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    openpool_observability::init();
    let config = AppConfig::from_env()?;
    let database_url = std::env::var("DATABASE_URL")?;
    let persistence = Persistence::connect(&database_url).await?;
    persistence.migrate().await?;
    let provider: Arc<dyn PaymentProvider> = match config.environment {
        Environment::Development | Environment::Test => Arc::new(FakePaymentProvider),
        Environment::Staging | Environment::Production => {
            let key = std::env::var("MAVAPAY_API_KEY")?;
            let url = std::env::var("MAVAPAY_BASE_URL").unwrap_or_default();
            if url.is_empty() {
                Arc::new(Mavapay::staging(key))
            } else {
                Arc::new(Mavapay::with_base_url(key, url))
            }
        }
    };
    let cipher = AddressCipher::from_base64(&std::env::var("ADDRESS_ENCRYPTION_KEY")?)?;
    let resolver: Option<Arc<dyn LightningAddressResolver>> = match config.environment {
        Environment::Development | Environment::Test => {
            Some(Arc::new(FakeLightningAddressResolver))
        }
        Environment::Staging | Environment::Production => {
            Some(Arc::new(LnurlLightningAddressResolver::new()))
        }
    };
    let platform_address = std::env::var("PLATFORM_LIGHTNING_ADDRESS").ok();
    let proof_store: Option<Arc<dyn ProofStore>> = match config.environment {
        Environment::Development | Environment::Test => None,
        Environment::Staging | Environment::Production => {
            Some(Arc::new(S3ProofStore::new(S3ProofStoreSettings {
                endpoint: std::env::var("PROOF_STORAGE_ENDPOINT")?,
                region: std::env::var("PROOF_STORAGE_REGION")?,
                bucket: std::env::var("PROOF_STORAGE_BUCKET")?,
                access_key_id: std::env::var("PROOF_STORAGE_ACCESS_KEY_ID")?,
                secret_access_key: std::env::var("PROOF_STORAGE_SECRET_ACCESS_KEY")?,
                retention_days: std::env::var("PROOF_STORAGE_RETENTION_DAYS")
                    .unwrap_or_else(|_| "2555".into())
                    .parse()?,
            })?))
        }
    };
    let worker_id = format!("worker-{}", Uuid::new_v4());
    let jobs = JobStore::new(persistence.pool().clone());
    let bitcoin: Option<Arc<dyn BitcoinSource>> = std::env::var("ESPLORA_BASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .map(|url| Arc::new(Esplora::new(url)) as Arc<dyn BitcoinSource>);
    let executor = tokio::spawn(run_jobs(
        jobs,
        WorkerServices {
            persistence,
            provider,
            bitcoin,
            resolver,
            cipher,
            platform_address,
            proof_store,
        },
        worker_id,
    ));
    let listener = tokio::net::TcpListener::bind(config.worker_bind_addr).await?;
    axum::serve(listener, health_router("worker")).await?;
    executor.abort();
    Ok(())
}

struct WorkerServices {
    persistence: Persistence,
    provider: Arc<dyn PaymentProvider>,
    bitcoin: Option<Arc<dyn BitcoinSource>>,
    resolver: Option<Arc<dyn LightningAddressResolver>>,
    cipher: AddressCipher,
    platform_address: Option<String>,
    proof_store: Option<Arc<dyn ProofStore>>,
}

async fn run_jobs(jobs: JobStore, services: WorkerServices, worker_id: String) {
    loop {
        if let Err(error) = services.persistence.schedule_due_closes().await {
            tracing::error!(%error, "could not schedule due raffle closes");
        }
        match jobs.claim(&worker_id, 60).await {
            Ok(Some(job)) => {
                let result = reconcile_payment(&services, &worker_id, &job).await;
                match result {
                    Ok(()) => {
                        let _ = jobs.succeed(job.id, &worker_id).await;
                    }
                    Err(error) => {
                        let _ = jobs.fail_or_retry(&job, &worker_id, 15, &error).await;
                    }
                }
            }
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            Err(error) => {
                tracing::error!(%error, "job claim failed");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn reconcile_payment(
    services: &WorkerServices,
    worker_id: &str,
    job: &ClaimedJob,
) -> Result<(), String> {
    let persistence = &services.persistence;
    let provider = services.provider.as_ref();
    let bitcoin = services.bitcoin.as_deref();
    let resolver = services.resolver.as_deref();
    let cipher = &services.cipher;
    let platform_address = services.platform_address.as_deref();
    if job.kind == "raffle.draw" {
        let source = bitcoin.ok_or_else(|| "Bitcoin source is not configured".to_owned())?;
        let raffle_id = job
            .payload
            .get("raffle_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "raffle.draw missing raffle_id".to_owned())?
            .parse::<Uuid>()
            .map(openpool_domain::RaffleId::from)
            .map_err(|_| "invalid raffle_id".to_owned())?;
        let context = persistence
            .frozen_draw_context(raffle_id)
            .await
            .map_err(|e| e.to_string())?;
        let tip = source.tip_height().await.map_err(|e| e.to_string())?;
        let mut height = tip;
        let close = loop {
            let block = source.block_at(height).await.map_err(|e| e.to_string())?;
            if block.time >= context.end_time {
                if height == 0 {
                    return Err("no block exists before raffle cutoff".into());
                }
                height -= 1;
                continue;
            }
            if height == tip {
                return Err(
                    "no canonical close block has been mined after the raffle cutoff".into(),
                );
            }
            break source
                .block_at(height + 1)
                .await
                .map_err(|e| e.to_string())?;
        };
        let randomness_height = close.height
            + u64::try_from(context.randomness_delay_blocks)
                .map_err(|_| "invalid randomness delay".to_owned())?;
        if tip < randomness_height + 1 {
            return Err("randomness block lacks one confirmation".into());
        }
        let randomness = source
            .block_at(randomness_height)
            .await
            .map_err(|e| e.to_string())?;
        // Store both selected facts before the transactional draw. If either selected height
        // later changes, record_canonical_block marks this chain segment non-canonical and a
        // retried draw must acquire fresh facts rather than use a stale hash.
        persistence
            .record_canonical_block(close.height, close.hash, close.previous_hash, close.time)
            .await
            .map_err(|e| e.to_string())?;
        persistence
            .record_canonical_block(
                randomness.height,
                randomness.hash,
                randomness.previous_hash,
                randomness.time,
            )
            .await
            .map_err(|e| e.to_string())?;
        if !persistence
            .canonical_block_matches(close.height, close.hash)
            .await
            .map_err(|e| e.to_string())?
            || !persistence
                .canonical_block_matches(randomness.height, randomness.hash)
                .await
                .map_err(|e| e.to_string())?
        {
            return Err("selected Bitcoin blocks were displaced by a reorg".into());
        }
        persistence
            .create_draw_and_payouts(
                raffle_id,
                close.height,
                close.hash,
                randomness.height,
                randomness.hash,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    if job.kind == "proof.generate" {
        let raffle_id = job
            .payload
            .get("raffle_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "proof.generate missing raffle_id".to_owned())?
            .parse::<Uuid>()
            .map(openpool_domain::RaffleId::from)
            .map_err(|_| "invalid raffle_id".to_owned())?;
        let proof = persistence
            .generate_terminal_proof(raffle_id)
            .await
            .map_err(|e| e.to_string())?;
        let store = services
            .proof_store
            .as_deref()
            .ok_or_else(|| "immutable proof storage is not configured".to_owned())?;
        // Publish the complete public document; its embedded proof hash is independently
        // recomputed by the verifier from the canonical payload.
        let canonical_json = serde_json::to_vec(&proof).map_err(|e| e.to_string())?;
        let publication = store
            .publish_immutable(&proof.proof_hash.to_hex(), &canonical_json)
            .await
            .map_err(|e| e.to_string())?;
        persistence
            .record_proof_publication(
                raffle_id,
                &publication.uri,
                &publication.version_id,
                &publication.etag,
            )
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    if job.kind == "payout.execute" {
        let raffle_id = job
            .payload
            .get("raffle_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "payout.execute missing raffle_id".to_owned())?
            .parse::<Uuid>()
            .map(openpool_domain::RaffleId::from)
            .map_err(|_| "invalid raffle_id".to_owned())?;
        let resolver =
            resolver.ok_or_else(|| "Lightning Address resolver is not configured".to_owned())?;
        while let Some(payout) = persistence
            .claim_payout(raffle_id, worker_id)
            .await
            .map_err(|e| e.to_string())?
        {
            let address = if payout.recipient_type == "platform" {
                platform_address
                    .ok_or_else(|| "platform Lightning address is not configured".to_owned())?
                    .to_owned()
            } else {
                let ciphertext = persistence
                    .payout_recipient_ciphertext(&payout)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "payout recipient has no address".to_owned())?;
                cipher.decrypt(&ciphertext).map_err(|e| e.to_string())?
            };
            let invoice = resolver
                .resolve_exact_amount(&address, payout.amount_sats)
                .await
                .map_err(|e| e.to_string())?;
            if invoice.amount_sats != payout.amount_sats {
                let _ = persistence
                    .fail_payout(payout.id, worker_id, "resolver returned incorrect amount")
                    .await;
                return Err("resolver returned incorrect amount".into());
            }
            match provider
                .pay_invoice(PayoutRequest {
                    payout_id: openpool_domain::PayoutId::from(payout.id),
                    bolt11: invoice.bolt11,
                    amount_sats: payout.amount_sats,
                    idempotency_key: payout.idempotency_key,
                })
                .await
            {
                Ok(attempt) => {
                    persistence
                        .settle_payout(payout.id, worker_id, &attempt.provider_reference)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Err(error) => {
                    let _ = persistence
                        .fail_payout(payout.id, worker_id, &error.to_string())
                        .await;
                    return Err(error.to_string());
                }
            }
        }
        return Ok(());
    }
    if job.kind == "raffle.close" {
        let raffle_id = job
            .payload
            .get("raffle_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "raffle.close missing raffle_id".to_owned())?
            .parse::<Uuid>()
            .map(openpool_domain::RaffleId::from)
            .map_err(|_| "invalid raffle_id".to_owned())?;
        if persistence
            .pending_invoices(raffle_id)
            .await
            .map_err(|e| e.to_string())?
            > 0
        {
            return Err("eligible invoices are still awaiting reconciliation".into());
        }
        persistence
            .freeze_raffle(raffle_id)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    if job.kind != "payment.reconcile" {
        return Ok(());
    }
    let invoice_id = job
        .payload
        .get("invoice_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "payment.reconcile missing invoice_id".to_owned())?
        .parse::<Uuid>()
        .map(InvoiceId::from)
        .map_err(|_| "invalid invoice_id".to_owned())?;
    let Some(reference) = persistence
        .collection_payment_reference(invoice_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    match provider
        .get_payment(&reference)
        .await
        .map_err(|e| e.to_string())?
        .state
    {
        PaymentState::Settled => match persistence
            .settle_invoice_and_append_entry(
                invoice_id,
                EntryId::from(Uuid::new_v4()),
                OffsetDateTime::now_utc(),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(openpool_persistence_sqlx::PersistenceError::RaffleNotOpen) => persistence
                .mark_late_settlement_for_refund(invoice_id)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        },
        PaymentState::Pending => Err("payment is not settled yet".into()),
        PaymentState::Failed | PaymentState::Expired => persistence
            .mark_collection_uncollectible(invoice_id)
            .await
            .map_err(|e| e.to_string()),
    }
}
