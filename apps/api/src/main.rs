use std::error::Error;

use vlrp_api::health_router;
use vlrp_config::AppConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    vlrp_observability::init();
    let config = AppConfig::from_env()?;
    let listener = tokio::net::TcpListener::bind(config.api_bind_addr).await?;
    axum::serve(listener, health_router("api")).await?;
    Ok(())
}
