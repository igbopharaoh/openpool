use std::env;
use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use hmac::{Hmac, Mac};
use openpool_api::{ApiState, router};
use openpool_core::payments::FakePaymentProvider;
use openpool_core::persistence_sqlx::Persistence;
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;
use uuid::Uuid;

fn database_url() -> String {
    env::var("DATABASE_URL").expect("DATABASE_URL is required for PostgreSQL integration tests")
}

async fn call(app: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

fn request(method: &str, uri: &str, organizer_id: Uuid, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-dev-organizer-id", organizer_id.to_string())
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a PostgreSQL DATABASE_URL"]
async fn open_raffle_creates_an_exact_pending_invoice_without_an_entry() {
    let persistence = Persistence::connect(&database_url()).await.unwrap();
    persistence.migrate().await.unwrap();
    sqlx::query("TRUNCATE audit_events, outbox_events, jobs, webhook_events, entries, invoices, payout_splits, raffles, organizers CASCADE")
        .execute(persistence.pool())
        .await
        .unwrap();
    let app = router(ApiState {
        persistence: persistence.clone(),
        payment_provider: Arc::new(FakePaymentProvider),
        address_cipher: openpool_api::AddressCipher::from_key_bytes(&[7; 32]).unwrap(),
        mavapay_webhook_secret: Some(Arc::from(&b"test-webhook"[..])),
        development_identity_enabled: true,
        oidc: None,
        secure_session_cookie: false,
    });
    let organizer_id = Uuid::new_v4();

    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            "/v1/organizers",
            organizer_id,
            json!({
                "display_name": "Phase Four", "lightning_address": "phase4@example.com"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, created) = call(
        app.clone(),
        request(
            "POST",
            "/v1/organizers/me/raffles",
            organizer_id,
            json!({
                "name": "Public read raffle", "entry_price_sats": 1000,
                "start_time": "2030-01-01T00:00:00Z", "end_time": "2030-01-02T00:00:00Z",
                "winner_bps": 9500, "organizer_bps": 400, "platform_bps": 100
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let raffle_id = created["id"].as_str().unwrap();

    for action in ["schedule", "open"] {
        let (status, _) = call(
            app.clone(),
            request(
                "POST",
                &format!("/v1/raffles/{raffle_id}/{action}"),
                organizer_id,
                json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (status, public) = call(
        app.clone(),
        Request::builder()
            .uri(format!("/v1/raffles/{raffle_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(public["name"], "Public read raffle");
    assert_eq!(public["status"], "OPEN");
    assert_eq!(public["total_pool_sats"], 0);
    assert!(public.get("lightning_address").is_none());

    let (status, invoice) = call(
        app.clone(),
        request(
            "POST",
            &format!("/v1/raffles/{raffle_id}/invoices"),
            organizer_id,
            json!({"lightning_address": "participant@example.com", "ticket_count": 2}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(invoice["amount_sats"], 2000);
    assert_eq!(invoice["status"], "pending");
    assert!(invoice["bolt11"].as_str().unwrap().starts_with("lntbs2000"));
    let entry_count: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entries WHERE raffle_id = $1")
            .bind(Uuid::parse_str(raffle_id).unwrap())
            .fetch_one(persistence.pool())
            .await
            .unwrap();
    assert_eq!(entry_count, 0);
    let ciphertext: Vec<u8> = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT payout_address_ciphertext FROM invoices WHERE id = $1",
    )
    .bind(Uuid::parse_str(invoice["id"].as_str().unwrap()).unwrap())
    .fetch_one(persistence.pool())
    .await
    .unwrap();
    assert_ne!(ciphertext, b"participant@example.com");

    let raw_webhook =
        json!({"event_id": format!("evt-{raffle_id}"), "invoice_id": invoice["id"]}).to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"test-webhook").unwrap();
    mac.update(raw_webhook.as_bytes());
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/v1/webhooks/mavapay")
            .header("content-type", "application/json")
            .header(
                "x-mavapay-signature",
                hex::encode(mac.finalize().into_bytes()),
            )
            .body(Body::from(raw_webhook.clone()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = call(
        app,
        Request::builder()
            .method("POST")
            .uri("/v1/webhooks/mavapay")
            .header("content-type", "application/json")
            .header("x-mavapay-signature", {
                let mut mac = Hmac::<Sha256>::new_from_slice(b"test-webhook").unwrap();
                mac.update(raw_webhook.as_bytes());
                hex::encode(mac.finalize().into_bytes())
            })
            .body(Body::from(raw_webhook))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let jobs: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'payment.reconcile'")
            .fetch_one(persistence.pool())
            .await
            .unwrap();
    assert_eq!(jobs, 1);
}
