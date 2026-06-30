//! Canonical Bitcoin observation boundary used by draw workers.
use async_trait::async_trait;
use openpool_protocol::Hash32;
use serde::Deserialize;
use std::collections::BTreeMap;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub height: u64,
    pub hash: Hash32,
    pub previous_hash: Hash32,
    pub time: OffsetDateTime,
}
#[derive(Debug, Error)]
pub enum BitcoinError {
    #[error("bitcoin source unavailable: {0}")]
    Transport(String),
    #[error("bitcoin source returned invalid data: {0}")]
    Invalid(String),
}
#[async_trait]
pub trait BitcoinSource: Send + Sync {
    async fn tip_height(&self) -> Result<u64, BitcoinError>;
    async fn block_at(&self, height: u64) -> Result<Block, BitcoinError>;
}

/// Deterministic fixture source for local workflows and reorg tests. It is deliberately
/// explicit: production code must choose an external canonical source instead.
pub struct FixtureSource {
    blocks: BTreeMap<u64, Block>,
}
impl FixtureSource {
    pub fn new(blocks: impl IntoIterator<Item = Block>) -> Self {
        Self {
            blocks: blocks.into_iter().map(|b| (b.height, b)).collect(),
        }
    }
}
#[async_trait]
impl BitcoinSource for FixtureSource {
    async fn tip_height(&self) -> Result<u64, BitcoinError> {
        self.blocks
            .last_key_value()
            .map(|(height, _)| *height)
            .ok_or_else(|| BitcoinError::Invalid("fixture chain is empty".into()))
    }
    async fn block_at(&self, height: u64) -> Result<Block, BitcoinError> {
        self.blocks
            .get(&height)
            .cloned()
            .ok_or_else(|| BitcoinError::Invalid(format!("fixture block {height} is absent")))
    }
}

pub struct Esplora {
    base_url: String,
    client: reqwest::Client,
}
impl Esplora {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: reqwest::Client::new(),
        }
    }
}
#[derive(Deserialize)]
struct EsploraBlock {
    id: String,
    height: u64,
    previousblockhash: String,
    timestamp: i64,
}
#[async_trait]
impl BitcoinSource for Esplora {
    async fn tip_height(&self) -> Result<u64, BitcoinError> {
        self.client
            .get(format!("{}/blocks/tip/height", self.base_url))
            .send()
            .await
            .map_err(|e| BitcoinError::Transport(e.to_string()))?
            .error_for_status()
            .map_err(|e| BitcoinError::Transport(e.to_string()))?
            .text()
            .await
            .map_err(|e| BitcoinError::Transport(e.to_string()))?
            .parse()
            .map_err(|_| BitcoinError::Invalid("tip height".into()))
    }
    async fn block_at(&self, height: u64) -> Result<Block, BitcoinError> {
        let hash = self
            .client
            .get(format!("{}/block-height/{}", self.base_url, height))
            .send()
            .await
            .map_err(|e| BitcoinError::Transport(e.to_string()))?
            .error_for_status()
            .map_err(|e| BitcoinError::Transport(e.to_string()))?
            .text()
            .await
            .map_err(|e| BitcoinError::Transport(e.to_string()))?;
        let b: EsploraBlock = self
            .client
            .get(format!("{}/block/{}", self.base_url, hash.trim()))
            .send()
            .await
            .map_err(|e| BitcoinError::Transport(e.to_string()))?
            .error_for_status()
            .map_err(|e| BitcoinError::Transport(e.to_string()))?
            .json()
            .await
            .map_err(|e| BitcoinError::Invalid(e.to_string()))?;
        Ok(Block {
            height: b.height,
            hash: parse_hash(&b.id)?,
            previous_hash: parse_hash(&b.previousblockhash)?,
            time: OffsetDateTime::from_unix_timestamp(b.timestamp)
                .map_err(|_| BitcoinError::Invalid("timestamp".into()))?,
        })
    }
}
fn parse_hash(value: &str) -> Result<Hash32, BitcoinError> {
    let bytes = hex::decode(value).map_err(|_| BitcoinError::Invalid("block hash".into()))?;
    if bytes.len() != 32 {
        return Err(BitcoinError::Invalid("block hash length".into()));
    }
    let mut result = [0; 32];
    result.copy_from_slice(&bytes);
    Ok(Hash32::from_bytes(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fixture_source_is_deterministic() {
        let block = Block {
            height: 100,
            hash: Hash32::from_bytes([2; 32]),
            previous_hash: Hash32::from_bytes([1; 32]),
            time: OffsetDateTime::UNIX_EPOCH,
        };
        let source = FixtureSource::new([block.clone()]);
        assert_eq!(source.tip_height().await.unwrap(), 100);
        assert_eq!(source.block_at(100).await.unwrap(), block);
        assert!(source.block_at(101).await.is_err());
    }
}
