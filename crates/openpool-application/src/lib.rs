//! Application ports and use-case boundaries. HTTP, UI, and worker adapters depend inward on this crate.

use async_trait::async_trait;
use openpool_contract::{PublicInvoice, PublicRaffle};
use openpool_domain::{InvoiceId, RaffleId};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Organizer,
    Operator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    pub subject: Uuid,
    pub roles: Vec<Role>,
}

impl Principal {
    pub fn permits(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }
}

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("authentication is required")]
    Unauthenticated,
    #[error("the caller is not allowed to perform this action")]
    Forbidden,
    #[error("requested resource does not exist")]
    NotFound,
    #[error("the requested state transition is invalid")]
    Conflict,
    #[error("input is invalid: {0}")]
    Validation(String),
    #[error("dependency is unavailable")]
    Unavailable,
    #[error("unexpected application failure")]
    Internal,
}

#[async_trait]
pub trait PublicRaffleReader: Send + Sync {
    async fn list_raffles(&self) -> Result<Vec<PublicRaffle>, ApplicationError>;
    async fn raffle(&self, raffle_id: RaffleId) -> Result<PublicRaffle, ApplicationError>;
    async fn invoice(&self, invoice_id: InvoiceId) -> Result<PublicInvoice, ApplicationError>;
}

#[async_trait]
pub trait IdentityProvider: Send + Sync {
    async fn principal_from_bearer(&self, bearer: &str) -> Result<Principal, ApplicationError>;
}
