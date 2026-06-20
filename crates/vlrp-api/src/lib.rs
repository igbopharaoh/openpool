//! HTTP routes intentionally limited to health/readiness in Milestone 1.

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

#[derive(Clone)]
struct HealthState {
    service: &'static str,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

pub fn health_router(service: &'static str) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .with_state(HealthState { service })
}

async fn health(State(state): State<HealthState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: state.service,
    })
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn health_and_readiness_are_available() {
        let app = health_router("api");
        for uri in ["/healthz", "/readyz"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }
}
