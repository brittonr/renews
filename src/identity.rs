//! Which iroh endpoint is allowed to act as an NNTP account.

use async_trait::async_trait;

/// Lookup from an account name to the endpoint that may use it.
#[async_trait]
pub trait UserEndpoint: Send + Sync {
    /// Endpoint id bound to `username`, if the account is key-bound.
    async fn endpoint_for_user(&self, username: &str) -> anyhow::Result<Option<String>>;
}
