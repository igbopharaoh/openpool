//! Provider-neutral payment boundary. Providers never leak their payload shapes past here.
use async_trait::async_trait;
use openpool_protocol::{InvoiceId, PayoutId, Sats};
use serde::Deserialize;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Clone, Debug)]
pub struct CollectionRequest {
    pub invoice_id: InvoiceId,
    pub amount_sats: Sats,
    pub expires_at: OffsetDateTime,
}
#[derive(Clone, Debug)]
pub struct CollectionInvoice {
    pub provider_quote_id: String,
    pub provider_payment_hash: String,
    pub bolt11: String,
    pub amount_sats: Sats,
    pub expires_at: OffsetDateTime,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentState {
    Pending,
    Settled,
    Failed,
    Expired,
}
#[derive(Clone, Debug)]
pub struct PaymentStatus {
    pub state: PaymentState,
    pub provider_reference: String,
}
#[derive(Clone, Debug)]
pub struct PayoutRequest {
    pub payout_id: PayoutId,
    pub bolt11: String,
    pub amount_sats: Sats,
    pub idempotency_key: String,
}
#[derive(Clone, Debug)]
pub struct PayoutAttempt {
    pub provider_reference: String,
}
#[derive(Clone, Debug)]
pub struct ResolvedInvoice {
    pub bolt11: String,
    pub amount_sats: Sats,
}
#[derive(Debug, Error)]
pub enum PaymentError {
    #[error("provider rejected request: {0}")]
    Rejected(String),
    #[error("provider transport failed: {0}")]
    Transport(String),
    #[error("provider response was invalid: {0}")]
    InvalidResponse(String),
    #[error("payment provider is not configured")]
    NotConfigured,
}

#[async_trait]
pub trait PaymentProvider: Send + Sync {
    async fn create_collection(
        &self,
        request: CollectionRequest,
    ) -> Result<CollectionInvoice, PaymentError>;
    async fn get_payment(&self, reference: &str) -> Result<PaymentStatus, PaymentError>;
    async fn pay_invoice(&self, request: PayoutRequest) -> Result<PayoutAttempt, PaymentError>;
}

#[async_trait]
pub trait LightningAddressResolver: Send + Sync {
    async fn resolve_exact_amount(
        &self,
        address: &str,
        amount_sats: Sats,
    ) -> Result<ResolvedInvoice, PaymentError>;
}

/// Deterministic local/CI provider. It is intentionally unable to represent real settlement.
pub struct FakePaymentProvider;
pub struct FakeLightningAddressResolver;

/// Resolves Lightning Addresses through LNURL-pay. The callback is deliberately fetched only
/// after the advertised min/max constraints have been checked, so callers cannot accidentally
/// request a different amount from the payout they are about to settle.
pub struct LnurlLightningAddressResolver {
    client: reqwest::Client,
}

impl LnurlLightningAddressResolver {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn well_known_url(address: &str) -> Result<String, PaymentError> {
        let (name, domain) = address.trim().split_once('@').ok_or_else(|| {
            PaymentError::Rejected("Lightning Address must be name@domain".into())
        })?;
        if name.is_empty() || domain.is_empty() || name.contains('/') || domain.contains('/') {
            return Err(PaymentError::Rejected("invalid Lightning Address".into()));
        }
        Ok(format!("https://{domain}/.well-known/lnurlp/{name}"))
    }
}

impl Default for LnurlLightningAddressResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LnurlPayResponse {
    callback: String,
    min_sendable: u64,
    max_sendable: u64,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct LnurlInvoiceResponse {
    pr: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[async_trait]
impl LightningAddressResolver for LnurlLightningAddressResolver {
    async fn resolve_exact_amount(
        &self,
        address: &str,
        amount_sats: Sats,
    ) -> Result<ResolvedInvoice, PaymentError> {
        let endpoint = Self::well_known_url(address)?;
        let response = self
            .client
            .get(endpoint)
            .send()
            .await
            .map_err(|e| PaymentError::Transport(e.to_string()))?
            .error_for_status()
            .map_err(|e| PaymentError::Transport(e.to_string()))?
            .json::<LnurlPayResponse>()
            .await
            .map_err(|e| PaymentError::InvalidResponse(e.to_string()))?;
        if response.status.as_deref() == Some("ERROR") {
            return Err(PaymentError::Rejected(
                response
                    .reason
                    .unwrap_or_else(|| "LNURL service rejected request".into()),
            ));
        }
        let amount_msat = amount_sats
            .as_u64()
            .checked_mul(1_000)
            .ok_or_else(|| PaymentError::Rejected("payout amount is too large".into()))?;
        if amount_msat < response.min_sendable || amount_msat > response.max_sendable {
            return Err(PaymentError::Rejected(
                "Lightning Address does not accept the exact payout amount".into(),
            ));
        }
        let invoice = self
            .client
            .get(&response.callback)
            .query(&[("amount", amount_msat)])
            .send()
            .await
            .map_err(|e| PaymentError::Transport(e.to_string()))?
            .error_for_status()
            .map_err(|e| PaymentError::Transport(e.to_string()))?
            .json::<LnurlInvoiceResponse>()
            .await
            .map_err(|e| PaymentError::InvalidResponse(e.to_string()))?;
        if invoice.status.as_deref() == Some("ERROR") {
            return Err(PaymentError::Rejected(
                invoice
                    .reason
                    .unwrap_or_else(|| "LNURL invoice request was rejected".into()),
            ));
        }
        let bolt11 = invoice
            .pr
            .filter(|pr| !pr.trim().is_empty())
            .ok_or_else(|| {
                PaymentError::InvalidResponse("LNURL response did not contain pr".into())
            })?;
        Ok(ResolvedInvoice {
            bolt11,
            amount_sats,
        })
    }
}

#[async_trait]
impl LightningAddressResolver for FakeLightningAddressResolver {
    async fn resolve_exact_amount(
        &self,
        address: &str,
        amount_sats: Sats,
    ) -> Result<ResolvedInvoice, PaymentError> {
        if address.trim().is_empty() {
            return Err(PaymentError::Rejected(
                "lightning address is required".into(),
            ));
        }
        Ok(ResolvedInvoice {
            bolt11: format!("lntbs{}n1openpoolpayout", amount_sats.as_u64()),
            amount_sats,
        })
    }
}

#[async_trait]
impl PaymentProvider for FakePaymentProvider {
    async fn create_collection(
        &self,
        request: CollectionRequest,
    ) -> Result<CollectionInvoice, PaymentError> {
        Ok(CollectionInvoice {
            provider_quote_id: format!("fake-quote-{}", request.invoice_id.as_uuid()),
            provider_payment_hash: format!("fake-payment-{}", request.invoice_id.as_uuid()),
            bolt11: format!("lntbs{}n1openpool", request.amount_sats.as_u64()),
            amount_sats: request.amount_sats,
            expires_at: request.expires_at,
        })
    }

    async fn get_payment(&self, reference: &str) -> Result<PaymentStatus, PaymentError> {
        Ok(PaymentStatus {
            state: PaymentState::Pending,
            provider_reference: reference.to_owned(),
        })
    }
    async fn pay_invoice(&self, request: PayoutRequest) -> Result<PayoutAttempt, PaymentError> {
        if request.bolt11.trim().is_empty() || request.amount_sats.as_u64() == 0 {
            return Err(PaymentError::Rejected(
                "payout invoice and positive amount are required".into(),
            ));
        }
        Ok(PayoutAttempt {
            provider_reference: format!("fake-payout-{}", request.idempotency_key),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;
    use uuid::Uuid;

    #[tokio::test]
    async fn fake_collection_is_exact_and_unique_per_invoice() {
        let provider = FakePaymentProvider;
        let expires_at = OffsetDateTime::now_utc() + Duration::minutes(10);
        let first = provider
            .create_collection(CollectionRequest {
                invoice_id: InvoiceId::from(Uuid::new_v4()),
                amount_sats: Sats::new(2_100),
                expires_at,
            })
            .await
            .unwrap();
        let second = provider
            .create_collection(CollectionRequest {
                invoice_id: InvoiceId::from(Uuid::new_v4()),
                amount_sats: Sats::new(2_100),
                expires_at,
            })
            .await
            .unwrap();
        assert_eq!(first.amount_sats, Sats::new(2_100));
        assert_eq!(first.expires_at, expires_at);
        assert_ne!(first.provider_quote_id, second.provider_quote_id);
        assert_ne!(first.provider_payment_hash, second.provider_payment_hash);
        assert!(first.bolt11.starts_with("lntbs2100"));
    }
}
