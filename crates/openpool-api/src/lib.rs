//! HTTP contract for public raffle reads, organizer configuration, and invoice collection.
use std::sync::Arc;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, patch, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use openpool_domain::{BasisPoints, InvoiceId, OrganizerId, PayoutSplit, RaffleId, Sats};
use openpool_payments::{CollectionRequest, PaymentProvider};
use openpool_payments_mavapay::Mavapay;
use openpool_persistence_sqlx::{
    NewCollectionInvoice, NewDraftRaffle, NewOidcLoginAttempt, NewOidcSession, NewOrganizer,
    Persistence,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

#[derive(Clone)]
pub struct ApiState {
    pub persistence: Persistence,
    pub payment_provider: Arc<dyn PaymentProvider>,
    pub address_cipher: AddressCipher,
    pub mavapay_webhook_secret: Option<Arc<[u8]>>,
    pub development_identity_enabled: bool,
    pub oidc: Option<Arc<openpool_oidc::OidcAdapter>>,
    pub secure_session_cookie: bool,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        list_raffles,
        get_raffle,
        get_pool,
        list_entries,
        create_invoice,
        get_invoice
    ),
    components(schemas(
        openpool_contract::PublicRaffle,
        openpool_contract::PublicInvoice,
        openpool_contract::PublicPool,
        openpool_contract::PublicEntry,
        openpool_contract::EntryPage,
        openpool_contract::Problem,
        InvoiceRequest
    )),
    info(
        title = "OpenPool API",
        version = "1.0.0",
        description = "OPENPOOL-1 technical-staging API"
    )
)]
struct ApiDoc;

#[derive(Clone)]
pub struct AddressCipher(Aes256Gcm);

impl AddressCipher {
    pub fn from_base64(value: &str) -> Result<Self, ApiStartupError> {
        let bytes = STANDARD
            .decode(value)
            .map_err(|_| ApiStartupError::InvalidEncryptionKey)?;
        Self::from_key_bytes(&bytes).ok_or(ApiStartupError::InvalidEncryptionKey)
    }

    pub fn from_key_bytes(value: &[u8]) -> Option<Self> {
        (value.len() == 32).then(|| Self(Aes256Gcm::new_from_slice(value).expect("32-byte key")))
    }

    fn encrypt(&self, value: &str) -> Result<Vec<u8>, ApiError> {
        let mut nonce = [0_u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .0
            .encrypt(Nonce::from_slice(&nonce), value.as_bytes())
            .map_err(|_| ApiError::internal("could not encrypt payment address"))?;
        Ok([
            b"OPENPOOL-AEAD-V1\0".as_slice(),
            nonce.as_slice(),
            ciphertext.as_slice(),
        ]
        .concat())
    }

    /// Decryption is intentionally available only to authorized server-side worker code.
    /// The version marker makes future key/cipher rotation explicit rather than heuristic.
    pub fn decrypt(&self, value: &[u8]) -> Result<String, ApiStartupError> {
        const PREFIX: &[u8] = b"OPENPOOL-AEAD-V1\0";
        if !value.starts_with(PREFIX) || value.len() <= PREFIX.len() + 12 {
            return Err(ApiStartupError::UnsupportedCiphertext);
        }
        let (nonce, ciphertext) = value[PREFIX.len()..].split_at(12);
        let plaintext = self
            .0
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| ApiStartupError::UnsupportedCiphertext)?;
        String::from_utf8(plaintext).map_err(|_| ApiStartupError::UnsupportedCiphertext)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiStartupError {
    #[error("ADDRESS_ENCRYPTION_KEY must be base64-encoded 32-byte key material")]
    InvalidEncryptionKey,
    #[error("encrypted payment address uses an unsupported key or format")]
    UnsupportedCiphertext,
}
pub fn health_router(_service: &'static str) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(health))
}
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .route("/v1/openapi.json", get(openapi))
        .route("/auth/login", get(oidc_login))
        .route("/auth/callback", get(oidc_callback))
        .route("/auth/logout", post(oidc_logout))
        .route("/v1/raffles", get(list_raffles))
        .route("/v1/raffles/{id}", get(get_raffle))
        .route("/v1/raffles/{id}/pool", get(get_pool))
        .route("/v1/raffles/{id}/entries", get(list_entries))
        .route("/v1/raffles/{id}/proof", get(get_proof))
        .route("/v1/raffles/{id}/proof/metadata", get(get_proof_metadata))
        .route("/v1/raffles/{id}/result", get(get_result))
        .route("/v1/refunds/{id}", get(get_refund))
        .route("/v1/raffles/{id}/verify", post(verify_proof))
        .route("/v1/raffles/{id}/invoices", post(create_invoice))
        .route("/v1/invoices/{id}", get(get_invoice))
        .route("/v1/webhooks/mavapay", post(receive_mavapay_webhook))
        .route("/v1/organizers", post(create_organizer))
        .route("/v1/organizers/me/raffles", get(list_organizer_raffles))
        .route("/v1/organizers/me/raffles", post(create_raffle))
        .route("/v1/organizers/me/raffles/{id}", patch(update_raffle))
        .route("/v1/operator/jobs", get(list_operator_jobs))
        .route("/v1/operator/jobs/{id}/retry", post(retry_operator_job))
        .route("/v1/raffles/{id}/schedule", post(schedule))
        .route("/v1/raffles/{id}/open", post(open))
        .route("/v1/raffles/{id}/cancel", post(cancel))
        .fallback(get(web_home))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}
#[derive(Serialize)]
struct Health {
    status: &'static str,
}
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn oidc_login(State(s): State<ApiState>) -> Result<Redirect, ApiError> {
    let oidc = s
        .oidc
        .as_deref()
        .ok_or_else(|| ApiError::unavailable("OIDC is not configured"))?;
    let start = oidc
        .begin_authorization()
        .map_err(|error| ApiError::unavailable(error.to_string()))?;
    s.persistence
        .create_oidc_login_attempt(NewOidcLoginAttempt {
            state: start.state,
            nonce: start.nonce,
            pkce_verifier: start.pkce_verifier,
            expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(10),
        })
        .await?;
    Ok(Redirect::temporary(&start.authorization_url))
}

#[derive(Deserialize)]
struct OidcCallback {
    code: String,
    state: String,
}

async fn oidc_callback(
    State(s): State<ApiState>,
    axum::extract::Query(callback): axum::extract::Query<OidcCallback>,
) -> Result<Response, ApiError> {
    let oidc = s
        .oidc
        .as_deref()
        .ok_or_else(|| ApiError::unavailable("OIDC is not configured"))?;
    let attempt = s
        .persistence
        .consume_oidc_login_attempt(&callback.state)
        .await?
        .ok_or_else(|| ApiError::forbidden("OIDC state is invalid or expired"))?;
    let identity = oidc
        .exchange_code(callback.code, attempt.pkce_verifier, attempt.nonce)
        .await
        .map_err(|error| ApiError::forbidden(format!("OIDC callback was rejected: {error}")))?;
    let session_id = Uuid::new_v4();
    let csrf_token = Uuid::new_v4().to_string();
    s.persistence
        .create_oidc_session(NewOidcSession {
            id: session_id,
            subject: oidc_subject_id(&identity.subject),
            roles: serde_json::to_value(identity.roles)
                .map_err(|_| ApiError::internal("could not store OIDC roles"))?,
            csrf_token_hash: hash(csrf_token.as_bytes()).0.to_vec(),
            expires_at: OffsetDateTime::now_utc() + time::Duration::hours(8),
        })
        .await?;
    let secure = if s.secure_session_cookie {
        "; Secure"
    } else {
        ""
    };
    let mut response = Redirect::to("/organizer").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "openpool_session={session_id}; HttpOnly; SameSite=Lax; Path=/; Max-Age=28800{secure}"
        ))
        .map_err(|_| ApiError::internal("could not set session cookie"))?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "openpool_csrf={csrf_token}; SameSite=Lax; Path=/; Max-Age=28800{secure}"
        ))
        .map_err(|_| ApiError::internal("could not set CSRF cookie"))?,
    );
    Ok(response)
}

async fn oidc_logout(State(s): State<ApiState>, headers: HeaderMap) -> Result<Response, ApiError> {
    // Logout is a state-changing authenticated request. Requiring the synchronizer token makes
    // a cross-site form unable to terminate a user's session.
    validate_csrf(&headers, &s).await?;
    if !s.development_identity_enabled {
        s.persistence
            .delete_oidc_session(session_id_from_headers(&headers)?)
            .await?;
    }
    let secure = if s.secure_session_cookie {
        "; Secure"
    } else {
        ""
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    for cookie in ["openpool_session", "openpool_csrf"] {
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&format!(
                "{cookie}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{secure}"
            ))
            .map_err(|_| ApiError::internal("could not clear session cookie"))?,
        );
    }
    Ok(response)
}

async fn web_home() -> Html<String> {
    Html(openpool_web::render_home())
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}
#[utoipa::path(
    get,
    path = "/v1/raffles",
    responses((status = 200, body = [openpool_contract::PublicRaffle]))
)]
async fn list_raffles(
    State(s): State<ApiState>,
) -> Result<Json<Vec<openpool_contract::PublicRaffle>>, ApiError> {
    Ok(Json(
        s.persistence
            .list_public_raffles()
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}
#[utoipa::path(
    get,
    path = "/v1/raffles/{id}",
    params(("id" = Uuid, Path, description = "Raffle UUID")),
    responses((status = 200, body = openpool_contract::PublicRaffle), (status = 404, body = openpool_contract::Problem))
)]
async fn get_raffle(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<openpool_contract::PublicRaffle>, ApiError> {
    Ok(Json(
        s.persistence
            .public_raffle(RaffleId::from(id))
            .await?
            .into(),
    ))
}
#[utoipa::path(get, path = "/v1/raffles/{id}/pool", params(("id" = Uuid, Path)), responses((status = 200, body = openpool_contract::PublicPool)))]
async fn get_pool(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<openpool_contract::PublicPool>, ApiError> {
    Ok(Json(s.persistence.public_pool(RaffleId::from(id)).await?))
}
#[derive(Deserialize)]
struct EntryQuery {
    cursor: Option<u64>,
    limit: Option<u32>,
}
#[utoipa::path(get, path = "/v1/raffles/{id}/entries", params(("id" = Uuid, Path)), responses((status = 200, body = openpool_contract::EntryPage)))]
async fn list_entries(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<EntryQuery>,
) -> Result<Json<openpool_contract::EntryPage>, ApiError> {
    Ok(Json(
        s.persistence
            .public_entries(
                RaffleId::from(id),
                query.cursor.unwrap_or(0),
                query.limit.unwrap_or(50).clamp(1, 100),
            )
            .await?,
    ))
}

/// Convenience wrapper only: browser verification uses the same verifier through WASM and
/// does not need this endpoint or any private API access.
async fn verify_proof(
    Path(id): Path<Uuid>,
    Json(document): Json<openpool_protocol::ProofDocument>,
) -> Result<Json<openpool_verifier::VerificationResult>, ApiError> {
    if document.payload.raffle_id.as_uuid() != id {
        return Err(ApiError::bad(
            "proof raffle_id does not match the request path",
        ));
    }
    Ok(Json(openpool_verifier::verify(&document)))
}
async fn get_proof(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<openpool_protocol::ProofDocument>, ApiError> {
    Ok(Json(s.persistence.public_proof(RaffleId::from(id)).await?))
}
async fn get_proof_metadata(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<openpool_contract::PublicProofMetadata>, ApiError> {
    Ok(Json(
        s.persistence
            .public_proof_metadata(RaffleId::from(id))
            .await?,
    ))
}
async fn get_result(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<openpool_contract::PublicResult>, ApiError> {
    Ok(Json(s.persistence.public_result(RaffleId::from(id)).await?))
}
async fn get_refund(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<openpool_contract::PublicRefund>, ApiError> {
    Ok(Json(s.persistence.public_refund(id).await?))
}
#[derive(Deserialize)]
struct OrganizerRequest {
    display_name: String,
    lightning_address: String,
}
async fn create_organizer(
    headers: HeaderMap,
    State(s): State<ApiState>,
    Json(body): Json<OrganizerRequest>,
) -> Result<StatusCode, ApiError> {
    validate_csrf(&headers, &s).await?;
    if body.display_name.trim().is_empty() || body.lightning_address.trim().is_empty() {
        return Err(ApiError::bad(
            "display_name and lightning_address are required",
        ));
    }
    let id = identity(&headers, &s).await?;
    s.persistence
        .create_organizer(NewOrganizer {
            id,
            display_name: body.display_name,
            lightning_address_ciphertext: s.address_cipher.encrypt(&body.lightning_address)?,
        })
        .await?;
    Ok(StatusCode::CREATED)
}
#[derive(Deserialize)]
struct RaffleRequest {
    name: String,
    entry_price_sats: u64,
    start_time: String,
    end_time: String,
    winner_bps: u16,
    organizer_bps: u16,
    platform_bps: u16,
}

#[derive(Deserialize, ToSchema)]
struct InvoiceRequest {
    lightning_address: String,
    ticket_count: u64,
}

#[derive(Deserialize)]
struct MavapayWebhook {
    event_id: String,
    invoice_id: Uuid,
}

async fn receive_mavapay_webhook(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    if body.len() > 64 * 1024 {
        return Err(ApiError::bad("webhook body exceeds the 64 KiB limit"));
    }
    let signature = headers
        .get("x-mavapay-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("mavapay signature is required"))?;
    let secret = s
        .mavapay_webhook_secret
        .as_deref()
        .ok_or_else(|| ApiError::unavailable("mavapay webhook verification is not configured"))?;
    if !Mavapay::verify_webhook_signature(secret, &body, signature) {
        return Err(ApiError::forbidden("mavapay signature is invalid"));
    }
    let raw_payload: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad("webhook payload must be valid JSON"))?;
    let normalized: MavapayWebhook = serde_json::from_value(raw_payload.clone())
        .map_err(|_| ApiError::bad("webhook event_id and invoice_id are required"))?;
    if normalized.event_id.trim().is_empty() {
        return Err(ApiError::bad("webhook event_id is required"));
    }
    s.persistence
        .record_webhook_and_enqueue(openpool_persistence_sqlx::NewWebhookEvent {
            provider_name: "mavapay".into(),
            provider_event_id: normalized.event_id,
            invoice_id: InvoiceId::from(normalized.invoice_id),
            payload_hash: hash(&body),
            raw_payload,
        })
        .await?;
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(
    post,
    path = "/v1/raffles/{raffle_id}/invoices",
    params(("raffle_id" = Uuid, Path, description = "Open raffle UUID")),
    request_body = InvoiceRequest,
    responses((status = 201, body = openpool_contract::PublicInvoice), (status = 400, body = openpool_contract::Problem), (status = 409, body = openpool_contract::Problem), (status = 502, body = openpool_contract::Problem))
)]
async fn create_invoice(
    State(s): State<ApiState>,
    Path(raffle_id): Path<Uuid>,
    Json(body): Json<InvoiceRequest>,
) -> Result<(StatusCode, Json<openpool_contract::PublicInvoice>), ApiError> {
    if body.lightning_address.trim().is_empty() {
        return Err(ApiError::bad("lightning_address is required"));
    }
    let now = OffsetDateTime::now_utc();
    let invoice_id = InvoiceId::from(Uuid::new_v4());
    let address = body.lightning_address.trim();
    let pending = s
        .persistence
        .create_collection_invoice(NewCollectionInvoice {
            id: invoice_id,
            raffle_id: RaffleId::from(raffle_id),
            participant_public_id: hash(address.as_bytes()),
            payment_reference_hash: hash(invoice_id.as_uuid().as_bytes()),
            payout_address_ciphertext: s.address_cipher.encrypt(address)?,
            ticket_count: body.ticket_count,
            requested_at: now,
            expires_at: now + time::Duration::minutes(15),
        })
        .await?;
    let collection = s
        .payment_provider
        .create_collection(CollectionRequest {
            invoice_id,
            amount_sats: pending.amount_sats,
            expires_at: pending.expires_at,
        })
        .await;
    let collection = match collection {
        Ok(value)
            if value.amount_sats == pending.amount_sats
                && value.expires_at <= pending.expires_at
                && !value.bolt11.is_empty() =>
        {
            value
        }
        Ok(_) => {
            s.persistence.fail_collection_invoice(invoice_id).await?;
            return Err(ApiError::provider(
                "provider returned an invalid collection invoice",
            ));
        }
        Err(_) => {
            s.persistence.fail_collection_invoice(invoice_id).await?;
            return Err(ApiError::provider(
                "payment provider could not create an invoice",
            ));
        }
    };
    s.persistence
        .store_collection_invoice(
            invoice_id,
            &collection.provider_quote_id,
            &collection.provider_payment_hash,
            &collection.bolt11,
            collection.expires_at,
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(s.persistence.public_invoice(invoice_id).await?.into()),
    ))
}

#[utoipa::path(
    get,
    path = "/v1/invoices/{id}",
    params(("id" = Uuid, Path, description = "Invoice UUID")),
    responses((status = 200, body = openpool_contract::PublicInvoice), (status = 404, body = openpool_contract::Problem))
)]
async fn get_invoice(
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<openpool_contract::PublicInvoice>, ApiError> {
    Ok(Json(
        s.persistence
            .public_invoice(InvoiceId::from(id))
            .await?
            .into(),
    ))
}

fn hash(value: &[u8]) -> openpool_protocol::Hash32 {
    openpool_protocol::Hash32::from_bytes(Sha256::digest(value).into())
}

async fn list_organizer_raffles(
    headers: HeaderMap,
    State(s): State<ApiState>,
) -> Result<Json<Vec<openpool_contract::PublicRaffle>>, ApiError> {
    let organizer_id = identity(&headers, &s).await?;
    if !s.persistence.organizer_exists(organizer_id).await? {
        return Err(ApiError::forbidden("active organizer required"));
    }
    Ok(Json(
        s.persistence
            .organizer_raffles(organizer_id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

async fn list_operator_jobs(
    headers: HeaderMap,
    State(s): State<ApiState>,
) -> Result<Json<Vec<openpool_contract::OperatorJob>>, ApiError> {
    require_operator(&headers, &s).await?;
    Ok(Json(s.persistence.operator_jobs().await?))
}

async fn retry_operator_job(
    headers: HeaderMap,
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    validate_csrf(&headers, &s).await?;
    require_operator(&headers, &s).await?;
    if s.persistence.retry_dead_job(id).await? {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::bad("only dead jobs can be retried"))
    }
}

async fn create_raffle(
    headers: HeaderMap,
    State(s): State<ApiState>,
    Json(body): Json<RaffleRequest>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    validate_csrf(&headers, &s).await?;
    let organizer_id = identity(&headers, &s).await?;
    if !s.persistence.organizer_exists(organizer_id).await? {
        return Err(ApiError::forbidden("active organizer required"));
    }
    let raffle = validated_raffle(body, RaffleId::from(Uuid::new_v4()), organizer_id)?;
    let raffle_id = raffle.id;
    s.persistence.create_draft_raffle(raffle).await?;
    Ok((
        StatusCode::CREATED,
        Json(IdResponse {
            id: raffle_id.as_uuid(),
        }),
    ))
}
async fn update_raffle(
    headers: HeaderMap,
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(body): Json<RaffleRequest>,
) -> Result<StatusCode, ApiError> {
    validate_csrf(&headers, &s).await?;
    let organizer_id = identity(&headers, &s).await?;
    if !s.persistence.organizer_exists(organizer_id).await? {
        return Err(ApiError::forbidden("active organizer required"));
    }
    s.persistence
        .update_raffle(
            organizer_id,
            validated_raffle(body, RaffleId::from(id), organizer_id)?,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn validated_raffle(
    body: RaffleRequest,
    id: RaffleId,
    organizer_id: OrganizerId,
) -> Result<NewDraftRaffle, ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad("name is required"));
    }
    if body.entry_price_sats == 0 {
        return Err(ApiError::bad("entry_price_sats must be positive"));
    }
    let start_time = parse_time(&body.start_time)?;
    let end_time = parse_time(&body.end_time)?;
    if end_time <= start_time {
        return Err(ApiError::bad("end_time must be after start_time"));
    }
    let payout_split = PayoutSplit::new(
        BasisPoints::new(body.winner_bps).map_err(ApiError::bad)?,
        BasisPoints::new(body.organizer_bps).map_err(ApiError::bad)?,
        BasisPoints::new(body.platform_bps).map_err(ApiError::bad)?,
    )
    .map_err(ApiError::bad)?;
    Ok(NewDraftRaffle {
        id,
        organizer_id,
        name: body.name.trim().to_owned(),
        entry_price_sats: Sats::new(body.entry_price_sats),
        start_time,
        end_time,
        randomness_delay_blocks: 6,
        payout_split,
    })
}
async fn schedule(
    headers: HeaderMap,
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    transition(&headers, &s, id, "SCHEDULED").await
}
async fn open(
    headers: HeaderMap,
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    transition(&headers, &s, id, "OPEN").await
}
async fn cancel(
    headers: HeaderMap,
    State(s): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    transition(&headers, &s, id, "CANCELLED").await
}
async fn transition(
    headers: &HeaderMap,
    state: &ApiState,
    raffle_id: Uuid,
    status: &str,
) -> Result<StatusCode, ApiError> {
    validate_csrf(headers, state).await?;
    state
        .persistence
        .transition_raffle(
            identity(headers, state).await?,
            RaffleId::from(raffle_id),
            status,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn identity(headers: &HeaderMap, state: &ApiState) -> Result<OrganizerId, ApiError> {
    if state.development_identity_enabled {
        return headers
            .get("x-dev-organizer-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Uuid::parse_str(v).ok())
            .map(OrganizerId::from)
            .ok_or_else(|| ApiError::forbidden("x-dev-organizer-id UUID is required"));
    }
    let session_id = session_id_from_headers(headers)?;
    let session = state
        .persistence
        .oidc_session(session_id)
        .await?
        .ok_or_else(|| ApiError::forbidden("OIDC session is invalid or expired"))?;
    let roles: Vec<String> = serde_json::from_value(session.roles)
        .map_err(|_| ApiError::forbidden("OIDC session roles are invalid"))?;
    if !roles.iter().any(|role| role == "organizer") {
        return Err(ApiError::forbidden("organizer role is required"));
    }
    Ok(OrganizerId::from(session.subject))
}

async fn require_operator(headers: &HeaderMap, state: &ApiState) -> Result<(), ApiError> {
    if state.development_identity_enabled {
        return headers
            .get("x-dev-operator")
            .and_then(|value| value.to_str().ok())
            .filter(|value| *value == "true")
            .map(|_| ())
            .ok_or_else(|| ApiError::forbidden("x-dev-operator: true is required"));
    }
    let session = state
        .persistence
        .oidc_session(session_id_from_headers(headers)?)
        .await?
        .ok_or_else(|| ApiError::forbidden("OIDC session is invalid or expired"))?;
    let roles: Vec<String> = serde_json::from_value(session.roles)
        .map_err(|_| ApiError::forbidden("OIDC session roles are invalid"))?;
    if roles.iter().any(|role| role == "operator") {
        Ok(())
    } else {
        Err(ApiError::forbidden("operator role is required"))
    }
}

async fn validate_csrf(headers: &HeaderMap, state: &ApiState) -> Result<(), ApiError> {
    if state.development_identity_enabled {
        return Ok(());
    }
    let session_id = session_id_from_headers(headers)?;
    let token = headers
        .get("x-openpool-csrf")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("CSRF token is required"))?;
    let session = state
        .persistence
        .oidc_session(session_id)
        .await?
        .ok_or_else(|| ApiError::forbidden("OIDC session is invalid or expired"))?;
    if session.csrf_token_hash != hash(token.as_bytes()).0.to_vec() {
        return Err(ApiError::forbidden("CSRF token is invalid"));
    }
    Ok(())
}

fn session_id_from_headers(headers: &HeaderMap) -> Result<Uuid, ApiError> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| {
            cookie
                .split(';')
                .map(str::trim)
                .find_map(|part| part.strip_prefix("openpool_session="))
        })
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| ApiError::forbidden("OIDC session cookie is required"))
}

fn oidc_subject_id(subject: &str) -> Uuid {
    let digest = Sha256::digest(subject.as_bytes());
    Uuid::from_slice(&digest[..16]).expect("SHA-256 has at least 16 bytes")
}
fn parse_time(value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| ApiError::bad("timestamps must be RFC3339"))
}
#[derive(Serialize)]
struct IdResponse {
    id: Uuid,
}
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}
impl ApiError {
    fn bad(error: impl ToString) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "validation_error",
            message: error.to_string(),
        }
    }
    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            message: message.into(),
        }
    }
    fn provider(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "provider_error",
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "dependency_unavailable",
            message: message.into(),
        }
    }
}
impl From<openpool_persistence_sqlx::PersistenceError> for ApiError {
    fn from(value: openpool_persistence_sqlx::PersistenceError) -> Self {
        let (status, code) = if matches!(
            value,
            openpool_persistence_sqlx::PersistenceError::NotFound(_)
        ) {
            (StatusCode::NOT_FOUND, "not_found")
        } else {
            (StatusCode::CONFLICT, "conflict")
        };
        Self {
            status,
            code,
            message: value.to_string(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(openpool_contract::Problem {
                status: self.status.as_u16(),
                code: self.code.into(),
                detail: self.message,
                request_id: Uuid::new_v4(),
            }),
        )
            .into_response()
    }
}
