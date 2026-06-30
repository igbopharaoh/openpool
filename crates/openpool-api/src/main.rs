use std::error::Error;

use std::sync::Arc;

use openpool_api::oidc::{OidcAdapter, OidcSettings};
use openpool_api::{AddressCipher, ApiState, router};
use openpool_core::config::{AppConfig, Environment};
use openpool_core::payments::{FakePaymentProvider, PaymentProvider};
use openpool_core::payments_mavapay::Mavapay;
use openpool_core::persistence_sqlx::Persistence;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    openpool_core::observability::init();
    let config = AppConfig::from_env()?;
    let database_url = std::env::var("DATABASE_URL")?;
    let encryption_key = std::env::var("ADDRESS_ENCRYPTION_KEY")?;
    let persistence = Persistence::connect(&database_url).await?;
    persistence.migrate().await?;
    if std::env::var("OPENPOOL_MIGRATE_ONLY").as_deref() == Ok("true") {
        // The deployment task exits immediately after SQLx has acquired its migration lock and
        // applied all forward-only migrations. It must run before any API/worker rollout.
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(config.api_bind_addr).await?;
    let payment_provider: Arc<dyn PaymentProvider> = match config.environment {
        Environment::Development | Environment::Test => Arc::new(FakePaymentProvider),
        Environment::Staging | Environment::Production => {
            let api_key = std::env::var("MAVAPAY_API_KEY")?;
            let base_url = std::env::var("MAVAPAY_BASE_URL").unwrap_or_default();
            if base_url.is_empty() {
                Arc::new(Mavapay::staging(api_key))
            } else {
                Arc::new(Mavapay::with_base_url(api_key, base_url))
            }
        }
    };
    let mavapay_webhook_secret: Arc<[u8]> = match config.environment {
        Environment::Development | Environment::Test => Arc::from(
            std::env::var("MAVAPAY_WEBHOOK_SECRET")
                .unwrap_or_else(|_| "openpool-dev-webhook".into())
                .into_bytes(),
        ),
        Environment::Staging | Environment::Production => {
            Arc::from(std::env::var("MAVAPAY_WEBHOOK_SECRET")?.into_bytes())
        }
    };
    let oidc = match config.environment {
        Environment::Development | Environment::Test => None,
        Environment::Staging | Environment::Production => Some(Arc::new(
            OidcAdapter::discover(OidcSettings {
                issuer: std::env::var("OIDC_ISSUER")?,
                client_id: std::env::var("OIDC_CLIENT_ID")?,
                client_secret: std::env::var("OIDC_CLIENT_SECRET").ok(),
                redirect_url: std::env::var("OIDC_REDIRECT_URL")?,
            })
            .await?,
        )),
    };
    axum::serve(
        listener,
        router(ApiState {
            persistence,
            payment_provider,
            address_cipher: AddressCipher::from_base64(&encryption_key)?,
            mavapay_webhook_secret: Some(mavapay_webhook_secret),
            development_identity_enabled: matches!(
                config.environment,
                Environment::Development | Environment::Test
            ),
            oidc,
            secure_session_cookie: matches!(
                config.environment,
                Environment::Staging | Environment::Production
            ),
        }),
    )
    .await?;
    Ok(())
}
