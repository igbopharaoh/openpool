//! Durable PostgreSQL job claiming and retry primitives.

use openpool_domain::JobId;
use serde_json::Value;
use sqlx::{PgPool, Row};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub struct JobStore {
    pool: PgPool,
}
impl JobStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn enqueue(&self, job: NewJob) -> Result<bool, JobError> {
        let result = sqlx::query("INSERT INTO jobs (id, kind, deduplication_key, payload, max_attempts, available_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (kind, deduplication_key) DO NOTHING")
            .bind(job.id.as_uuid()).bind(job.kind).bind(job.deduplication_key).bind(job.payload).bind(job.max_attempts).bind(job.available_at)
            .execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn claim(
        &self,
        worker_id: &str,
        lease_seconds: i64,
    ) -> Result<Option<ClaimedJob>, JobError> {
        let row = sqlx::query("WITH candidate AS (SELECT id FROM jobs WHERE (status = 'queued' AND available_at <= now()) OR (status = 'running' AND lease_expires_at < now()) ORDER BY available_at, created_at FOR UPDATE SKIP LOCKED LIMIT 1) UPDATE jobs SET status = 'running', attempts = attempts + 1, lease_owner = $1, lease_expires_at = now() + ($2 * interval '1 second'), updated_at = now() WHERE id = (SELECT id FROM candidate) RETURNING id, kind, deduplication_key, payload, attempts, max_attempts, lease_expires_at")
            .bind(worker_id).bind(lease_seconds).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(ClaimedJob {
                id: JobId::from(row.get::<Uuid, _>("id")),
                kind: row.get("kind"),
                deduplication_key: row.get("deduplication_key"),
                payload: row.get("payload"),
                attempts: row.get("attempts"),
                max_attempts: row.get("max_attempts"),
                lease_expires_at: row.get("lease_expires_at"),
            })
        })
        .transpose()
    }

    pub async fn succeed(&self, job_id: JobId, worker_id: &str) -> Result<(), JobError> {
        let result = sqlx::query("UPDATE jobs SET status = 'succeeded', lease_owner = NULL, lease_expires_at = NULL, updated_at = now() WHERE id = $1 AND status = 'running' AND lease_owner = $2")
            .bind(job_id.as_uuid()).bind(worker_id).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(JobError::LeaseLost);
        }
        Ok(())
    }

    pub async fn fail_or_retry(
        &self,
        job: &ClaimedJob,
        worker_id: &str,
        retry_after_seconds: i64,
        error: &str,
    ) -> Result<JobDisposition, JobError> {
        let status = if job.attempts >= job.max_attempts {
            "dead"
        } else {
            "queued"
        };
        let result = sqlx::query("UPDATE jobs SET status = $3, available_at = now() + ($4 * interval '1 second'), lease_owner = NULL, lease_expires_at = NULL, last_error = $5, updated_at = now() WHERE id = $1 AND status = 'running' AND lease_owner = $2")
            .bind(job.id.as_uuid()).bind(worker_id).bind(status).bind(retry_after_seconds).bind(error).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(JobError::LeaseLost);
        }
        Ok(if status == "dead" {
            JobDisposition::Dead
        } else {
            JobDisposition::Retried
        })
    }
}

/// Claims committed outbox events using the same lease semantics as background jobs.
pub struct OutboxStore {
    pool: PgPool,
}

impl OutboxStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn claim(
        &self,
        consumer: &str,
        lease_seconds: i64,
    ) -> Result<Option<ClaimedOutboxEvent>, JobError> {
        let row = sqlx::query("WITH candidate AS (SELECT id FROM outbox_events WHERE delivered_at IS NULL AND (lease_expires_at IS NULL OR lease_expires_at < now()) ORDER BY occurred_at FOR UPDATE SKIP LOCKED LIMIT 1) UPDATE outbox_events SET lease_owner = $1, lease_expires_at = now() + ($2 * interval '1 second'), attempts = attempts + 1 WHERE id = (SELECT id FROM candidate) RETURNING id, aggregate_type, aggregate_id, event_type, payload")
            .bind(consumer).bind(lease_seconds).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(ClaimedOutboxEvent {
                id: row.get("id"),
                aggregate_type: row.get("aggregate_type"),
                aggregate_id: row.get("aggregate_id"),
                event_type: row.get("event_type"),
                payload: row.get("payload"),
            })
        })
        .transpose()
    }

    pub async fn mark_delivered(&self, event_id: Uuid, consumer: &str) -> Result<(), JobError> {
        let result = sqlx::query("UPDATE outbox_events SET delivered_at = now(), lease_owner = NULL, lease_expires_at = NULL WHERE id = $1 AND lease_owner = $2 AND delivered_at IS NULL")
            .bind(event_id).bind(consumer).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(JobError::LeaseLost);
        }
        Ok(())
    }
}

pub struct NewJob {
    pub id: JobId,
    pub kind: String,
    pub deduplication_key: String,
    pub payload: Value,
    pub max_attempts: i32,
    pub available_at: OffsetDateTime,
}
#[derive(Clone, Debug)]
pub struct ClaimedJob {
    pub id: JobId,
    pub kind: String,
    pub deduplication_key: String,
    pub payload: Value,
    pub attempts: i32,
    pub max_attempts: i32,
    pub lease_expires_at: OffsetDateTime,
}
#[derive(Clone, Debug)]
pub struct ClaimedOutboxEvent {
    pub id: Uuid,
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub event_type: String,
    pub payload: Value,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobDisposition {
    Retried,
    Dead,
}
#[derive(Debug, Error)]
pub enum JobError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("job lease is not owned by this worker")]
    LeaseLost,
}
