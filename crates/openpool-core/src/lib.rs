//! Consolidated server core for OpenPool.
//!
//! Protocol-owned domain primitives are re-exported here for server code. Operational adapters
//! that were previously spread across several tiny crates now live in this crate.

pub mod contract;

#[cfg(feature = "server")]
pub mod bitcoin;
#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod jobs;
#[cfg(feature = "server")]
pub mod observability;
#[cfg(feature = "server")]
pub mod payments;
#[cfg(feature = "server")]
pub mod payments_mavapay;
#[cfg(feature = "server")]
pub mod persistence_sqlx;
#[cfg(feature = "server")]
pub mod proof_storage;
#[cfg(feature = "test-support")]
pub mod test_support;

pub mod domain {
    pub use openpool_protocol::*;
}
