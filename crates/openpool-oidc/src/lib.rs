//! Standards-based OIDC authorization-code + PKCE adapter for OpenPool.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope,
    core::{CoreClient, CoreProviderMetadata},
};
use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_url: String,
}

pub struct OidcAdapter {
    metadata: CoreProviderMetadata,
    settings: OidcSettings,
}

#[derive(Clone, Debug)]
pub struct AuthorizationStart {
    pub authorization_url: String,
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedIdentity {
    pub subject: String,
    pub roles: Vec<String>,
}

impl OidcAdapter {
    pub async fn discover(settings: OidcSettings) -> Result<Self, OidcError> {
        let issuer = IssuerUrl::new(settings.issuer.clone()).map_err(OidcError::Configuration)?;
        let http_client = reqwest::Client::new();
        let metadata = CoreProviderMetadata::discover_async(issuer, &http_client)
            .await
            .map_err(|error| OidcError::Discovery(error.to_string()))?;
        Ok(Self { metadata, settings })
    }

    pub fn begin_authorization(&self) -> Result<AuthorizationStart, OidcError> {
        let verifier =
            PkceCodeVerifier::new(format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()));
        let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
        let client = CoreClient::from_provider_metadata(
            self.metadata.clone(),
            ClientId::new(self.settings.client_id.clone()),
            self.settings.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.settings.redirect_url.clone())
                .map_err(OidcError::Configuration)?,
        );
        let (url, state, nonce) = client
            .authorize_url(
                openidconnect::core::CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("openid".into()))
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge)
            .url();
        Ok(AuthorizationStart {
            authorization_url: url.to_string(),
            state: state.secret().to_owned(),
            nonce: nonce.secret().to_owned(),
            pkce_verifier: verifier.secret().to_owned(),
        })
    }

    pub async fn exchange_code(
        &self,
        code: String,
        verifier: String,
        nonce: String,
    ) -> Result<AuthenticatedIdentity, OidcError> {
        let http_client = reqwest::Client::new();
        let client = CoreClient::from_provider_metadata(
            self.metadata.clone(),
            ClientId::new(self.settings.client_id.clone()),
            self.settings.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.settings.redirect_url.clone())
                .map_err(OidcError::Configuration)?,
        );
        let request = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|error| OidcError::Exchange(error.to_string()))?;
        let token = request
            .set_pkce_verifier(PkceCodeVerifier::new(verifier))
            .request_async(&http_client)
            .await
            .map_err(|error| OidcError::Exchange(error.to_string()))?;
        let id_token = token
            .extra_fields()
            .id_token()
            .ok_or(OidcError::MissingIdToken)?;
        let claims = id_token
            .claims(&client.id_token_verifier(), &Nonce::new(nonce))
            .map_err(|error| OidcError::TokenValidation(error.to_string()))?;
        let roles = keycloak_roles(&id_token.to_string())?;
        Ok(AuthenticatedIdentity {
            subject: claims.subject().as_str().to_owned(),
            roles,
        })
    }
}

#[derive(Default, Deserialize)]
struct KeycloakClaims {
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    realm_access: RealmAccess,
}
#[derive(Default, Deserialize)]
struct RealmAccess {
    #[serde(default)]
    roles: Vec<String>,
}

fn keycloak_roles(raw_token: &str) -> Result<Vec<String>, OidcError> {
    let payload = raw_token
        .split('.')
        .nth(1)
        .ok_or_else(|| OidcError::TokenValidation("ID token is not a JWT".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| OidcError::TokenValidation(error.to_string()))?;
    let claims: KeycloakClaims = serde_json::from_slice(&bytes)
        .map_err(|error| OidcError::TokenValidation(error.to_string()))?;
    let mut roles = claims.groups;
    roles.extend(claims.realm_access.roles);
    Ok(roles
        .into_iter()
        .filter_map(|role| match role.as_str() {
            "organizer" | "openpool-organizer" => Some("organizer".into()),
            "operator" | "openpool-operator" => Some("operator".into()),
            _ => None,
        })
        .collect())
}

#[derive(Debug, Error)]
pub enum OidcError {
    #[error("OIDC configuration is invalid: {0}")]
    Configuration(url::ParseError),
    #[error("OIDC discovery failed: {0}")]
    Discovery(String),
    #[error("OIDC token exchange failed: {0}")]
    Exchange(String),
    #[error("OIDC token response did not include an ID token")]
    MissingIdToken,
    #[error("OIDC ID token validation failed: {0}")]
    TokenValidation(String),
}

#[cfg(test)]
mod tests {
    use super::keycloak_roles;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    #[test]
    fn maps_only_supported_keycloak_roles() {
        let claims = URL_SAFE_NO_PAD.encode(
            r#"{"groups":["openpool-organizer","ignored"],"realm_access":{"roles":["operator"]}}"#,
        );
        assert_eq!(
            keycloak_roles(&format!("x.{claims}.y")).unwrap(),
            vec!["organizer", "operator"]
        );
    }
}
