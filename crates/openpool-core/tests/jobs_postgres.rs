use std::env;

use openpool_core::domain::JobId;
use openpool_core::jobs::{JobDisposition, JobStore, NewJob};
use openpool_core::persistence_sqlx::Persistence;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires a PostgreSQL DATABASE_URL"]
async fn expired_job_lease_is_reclaimed_without_duplicate_enqueue() {
    let persistence = Persistence::connect(&env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    persistence.migrate().await.unwrap();
    sqlx::query("TRUNCATE jobs")
        .execute(persistence.pool())
        .await
        .unwrap();
    let jobs = JobStore::new(persistence.pool().clone());
    let job = NewJob {
        id: JobId::from(Uuid::new_v4()),
        kind: "draw_raffle".into(),
        deduplication_key: "raffle-1".into(),
        payload: json!({"raffle_id": "raffle-1"}),
        max_attempts: 2,
        available_at: OffsetDateTime::now_utc(),
    };
    assert!(jobs.enqueue(job).await.unwrap());
    assert!(
        !jobs
            .enqueue(NewJob {
                id: JobId::from(Uuid::new_v4()),
                kind: "draw_raffle".into(),
                deduplication_key: "raffle-1".into(),
                payload: json!({}),
                max_attempts: 2,
                available_at: OffsetDateTime::now_utc()
            })
            .await
            .unwrap()
    );
    let claimed = jobs.claim("worker-a", -1).await.unwrap().unwrap();
    let reclaimed = jobs.claim("worker-b", 30).await.unwrap().unwrap();
    assert_eq!(claimed.id, reclaimed.id);
    assert_eq!(
        jobs.fail_or_retry(&reclaimed, "worker-b", 0, "temporary")
            .await
            .unwrap(),
        JobDisposition::Dead
    );
}
