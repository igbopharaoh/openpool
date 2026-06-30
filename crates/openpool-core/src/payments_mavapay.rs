//! Mavapay staging adapter. The collection quote fields are isolated here pending doc publication.
use crate::payments::{
    CollectionInvoice, CollectionRequest, PaymentError, PaymentProvider, PaymentState,
    PaymentStatus, PayoutAttempt, PayoutRequest,
};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub struct Mavapay {
    client: Client,
    base_url: String,
    api_key: String,
}
impl Mavapay {
    pub fn staging(api_key: String) -> Self {
        Self {
            client: Client::new(),
            base_url: "https://staging.api.mavapay.co/api/v1".into(),
            api_key,
        }
    }
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
            api_key,
        }
    }

    /// Validates the provider's raw-body HMAC header without parsing or normalizing the payload.
    /// The exact header name is an HTTP concern; callers pass only its value here.
    pub fn verify_webhook_signature(secret: &[u8], raw_body: &[u8], supplied: &str) -> bool {
        let Ok(expected) = hex::decode(supplied) else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
            return false;
        };
        mac.update(raw_body);
        mac.verify_slice(&expected).is_ok()
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QuoteRequest {
    amount: u64,
    source_currency: &'static str,
    target_currency: &'static str,
    payment_method: &'static str,
    payment_currency: &'static str,
    autopayout: bool,
    customer_reference: String,
}
#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Quote {
    id: String,
    invoice: String,
    hash: String,
    expiry: String,
    total_amount_in_source_currency: Option<u64>,
}
#[async_trait]
impl PaymentProvider for Mavapay {
    async fn create_collection(
        &self,
        request: CollectionRequest,
    ) -> Result<CollectionInvoice, PaymentError> {
        let payload = QuoteRequest {
            amount: request.amount_sats.as_u64(),
            source_currency: "BTCSAT",
            target_currency: "BTCSAT",
            payment_method: "LIGHTNING",
            payment_currency: "BTCSAT",
            autopayout: false,
            customer_reference: request.invoice_id.as_uuid().to_string(),
        };
        let response = self
            .client
            .post(format!("{}/quote", self.base_url))
            .header("X-API-KEY", &self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| PaymentError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            return Err(PaymentError::Rejected(
                response.text().await.unwrap_or_default(),
            ));
        }
        let quote: Quote = response
            .json::<Envelope<Quote>>()
            .await
            .map_err(|e| PaymentError::InvalidResponse(e.to_string()))?
            .data;
        let expires_at = OffsetDateTime::parse(&quote.expiry, &Rfc3339)
            .map_err(|e| PaymentError::InvalidResponse(e.to_string()))?;
        Ok(CollectionInvoice {
            provider_quote_id: quote.id,
            provider_payment_hash: quote.hash,
            bolt11: quote.invoice,
            amount_sats: openpool_protocol::Sats::new(
                quote
                    .total_amount_in_source_currency
                    .unwrap_or(request.amount_sats.as_u64()),
            ),
            expires_at,
        })
    }
    async fn get_payment(&self, reference: &str) -> Result<PaymentStatus, PaymentError> {
        let response = self
            .client
            .get(format!("{}/transaction?hash={reference}", self.base_url))
            .header("X-API-KEY", &self.api_key)
            .send()
            .await
            .map_err(|e| PaymentError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            return Err(PaymentError::Rejected(
                response.text().await.unwrap_or_default(),
            ));
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| PaymentError::InvalidResponse(e.to_string()))?;
        let status = value["data"]["status"].as_str().unwrap_or("PENDING");
        let state = match status {
            "SUCCESS" | "PAID" => PaymentState::Settled,
            "FAILED" => PaymentState::Failed,
            "EXPIRED" => PaymentState::Expired,
            _ => PaymentState::Pending,
        };
        Ok(PaymentStatus {
            state,
            provider_reference: reference.into(),
        })
    }
    async fn pay_invoice(&self, _request: PayoutRequest) -> Result<PayoutAttempt, PaymentError> {
        // Mavapay staging payout credentials are a release gate. Keep this operation explicit
        // until the provider enables and documents the withdrawal contract for the account.
        Err(PaymentError::NotConfigured)
    }
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::StatusCode, routing::post};
    use hmac::{Hmac, Mac};
    use time::Duration;
    use uuid::Uuid;

    use super::*;

    async fn fixture_server(status: StatusCode, body: &'static str) -> std::io::Result<String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/quote", post(move || async move { (status, body) })),
            )
            .await
            .unwrap();
        });
        Ok(format!("http://{address}"))
    }

    fn request() -> CollectionRequest {
        CollectionRequest {
            invoice_id: openpool_protocol::InvoiceId::from(Uuid::new_v4()),
            amount_sats: openpool_protocol::Sats::new(100),
            expires_at: OffsetDateTime::now_utc() + Duration::minutes(5),
        }
    }

    #[tokio::test]
    async fn malformed_quote_fixture_is_rejected() {
        let Ok(base_url) = fixture_server(StatusCode::OK, r#"{"data":{}}"#).await else {
            // Some hermetic CI/sandbox environments prohibit local listeners. Networked adapter
            // fixtures run in integration CI; this unit test must not fail before exercising it.
            return;
        };
        let provider = Mavapay::with_base_url("test".into(), base_url);
        assert!(matches!(
            provider.create_collection(request()).await,
            Err(PaymentError::InvalidResponse(_))
        ));
    }

    #[tokio::test]
    async fn declined_quote_fixture_is_rejected() {
        let Ok(base_url) = fixture_server(StatusCode::UNPROCESSABLE_ENTITY, "declined").await
        else {
            return;
        };
        let provider = Mavapay::with_base_url("test".into(), base_url);
        assert!(matches!(
            provider.create_collection(request()).await,
            Err(PaymentError::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn unavailable_provider_is_a_transport_error() {
        let provider = Mavapay::with_base_url("test".into(), "http://127.0.0.1:1".into());
        assert!(matches!(
            provider.create_collection(request()).await,
            Err(PaymentError::Transport(_))
        ));
    }

    #[test]
    fn webhook_signature_uses_the_unchanged_raw_body() {
        let secret = b"webhook-secret";
        let body = br#"{"event_id":"evt-1","invoice_id":"test"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        let signature = hex::encode(mac.finalize().into_bytes());
        assert!(Mavapay::verify_webhook_signature(secret, body, &signature));
        assert!(!Mavapay::verify_webhook_signature(
            secret,
            br#"{ "event_id":"evt-1"}"#,
            &signature
        ));
    }
}
