//! Immutable S3-compatible publication for public proof artifacts.
//!
//! Objects are written once under a proof-hash key with S3 Object Lock governance disabled in
//! favour of `COMPLIANCE` retention. A bucket must have versioning and Object Lock enabled before
//! this adapter is configured; the storage service rejects overwrite/precondition failures.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedProof {
    pub uri: String,
    pub version_id: String,
    pub etag: String,
}

#[derive(Debug, Error)]
pub enum ProofStorageError {
    #[error("object store transport failed: {0}")]
    Transport(String),
    #[error("object store rejected immutable publication ({status}): {body}")]
    Rejected { status: u16, body: String },
    #[error("object store response omitted {0}")]
    MissingMetadata(&'static str),
    #[error("proof storage configuration is invalid: {0}")]
    Configuration(String),
}

#[async_trait]
pub trait ProofStore: Send + Sync {
    async fn publish_immutable(
        &self,
        proof_hash_hex: &str,
        canonical_json: &[u8],
    ) -> Result<PublishedProof, ProofStorageError>;
}

#[derive(Clone, Debug)]
pub struct S3ProofStoreSettings {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub retention_days: i64,
}

pub struct S3ProofStore {
    client: reqwest::Client,
    settings: S3ProofStoreSettings,
}

impl S3ProofStore {
    pub fn new(settings: S3ProofStoreSettings) -> Result<Self, ProofStorageError> {
        if settings.endpoint.trim().is_empty()
            || settings.region.trim().is_empty()
            || settings.bucket.trim().is_empty()
            || settings.access_key_id.trim().is_empty()
            || settings.secret_access_key.trim().is_empty()
            || settings.retention_days < 1
        {
            return Err(ProofStorageError::Configuration(
                "endpoint, region, bucket, credentials, and positive retention_days are required"
                    .into(),
            ));
        }
        Ok(Self {
            client: reqwest::Client::new(),
            settings,
        })
    }

    fn object_key(hash: &str) -> Result<String, ProofStorageError> {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ProofStorageError::Configuration(
                "proof hash must be lowercase/uppercase 64-char hex".into(),
            ));
        }
        Ok(format!("proofs/OPENPOOL-1/{hash}/proof.json"))
    }
}

#[async_trait]
impl ProofStore for S3ProofStore {
    async fn publish_immutable(
        &self,
        proof_hash_hex: &str,
        canonical_json: &[u8],
    ) -> Result<PublishedProof, ProofStorageError> {
        let key = Self::object_key(proof_hash_hex)?;
        let endpoint = self.settings.endpoint.trim_end_matches('/');
        let url = format!("{endpoint}/{}/{}", self.settings.bucket, key);
        let now = OffsetDateTime::now_utc();
        let amz_date = now
            .format(&time::macros::format_description!(
                "[year][month][day]T[hour][minute][second]Z"
            ))
            .map_err(|e| ProofStorageError::Configuration(e.to_string()))?;
        let day = now
            .format(&time::macros::format_description!("[year][month][day]"))
            .map_err(|e| ProofStorageError::Configuration(e.to_string()))?;
        let retention = (now + Duration::days(self.settings.retention_days))
            .format(&Rfc3339)
            .map_err(|e| ProofStorageError::Configuration(e.to_string()))?;
        let payload_hash = hex::encode(Sha256::digest(canonical_json));
        let host = reqwest::Url::parse(&url)
            .map_err(|e| ProofStorageError::Configuration(e.to_string()))?
            .host_str()
            .ok_or_else(|| ProofStorageError::Configuration("S3 endpoint has no host".into()))?
            .to_owned();
        let canonical_uri = format!("/{}/{}", self.settings.bucket, key);
        let canonical_headers = format!(
            "content-type:application/json\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\nx-amz-object-lock-mode:COMPLIANCE\nx-amz-object-lock-retain-until-date:{retention}\nx-amz-server-side-encryption:AES256\n"
        );
        let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date;x-amz-object-lock-mode;x-amz-object-lock-retain-until-date;x-amz-server-side-encryption";
        let canonical_request = format!(
            "PUT\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{day}/{}/s3/aws4_request", self.settings.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request))
        );
        let signature = hex::encode(sign_v4(
            &self.settings.secret_access_key,
            &day,
            &self.settings.region,
            "s3",
            &string_to_sign,
        ));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.settings.access_key_id
        );
        let response = self
            .client
            .put(&url)
            .header("content-type", "application/json")
            .header("if-none-match", "*")
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", amz_date)
            .header("x-amz-object-lock-mode", "COMPLIANCE")
            .header("x-amz-object-lock-retain-until-date", retention)
            .header("x-amz-server-side-encryption", "AES256")
            .header("authorization", authorization)
            .body(canonical_json.to_vec())
            .send()
            .await
            .map_err(|e| ProofStorageError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ProofStorageError::Rejected { status, body });
        }
        let headers = response.headers();
        let version_id = headers
            .get("x-amz-version-id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .ok_or(ProofStorageError::MissingMetadata("x-amz-version-id"))?
            .to_owned();
        let etag = headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .ok_or(ProofStorageError::MissingMetadata("etag"))?
            .trim_matches('"')
            .to_owned();
        Ok(PublishedProof {
            uri: format!("s3://{}/{key}", self.settings.bucket),
            version_id,
            etag,
        })
    }
}

fn hmac(key: &[u8], value: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key");
    mac.update(value.as_bytes());
    mac.finalize().into_bytes().to_vec()
}
fn sign_v4(secret: &str, day: &str, region: &str, service: &str, value: &str) -> Vec<u8> {
    let date = hmac(format!("AWS4{secret}").as_bytes(), day);
    let region = hmac(&date, region);
    let service = hmac(&region, service);
    let signing = hmac(&service, "aws4_request");
    hmac(&signing, value)
}
