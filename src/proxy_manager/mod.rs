//! Account-specific proxy management. Persisted bindings fail closed.
pub mod config;
mod crypto;
mod manager;
mod mihomo;
mod model;
pub mod router;
mod scheduler;
mod store;
mod subscription;
pub use manager::{Action, ProxyManager, SubscriptionInput};
pub(crate) use model::id;
pub use model::{Binding, Node, Operation, Snapshot, Subscription};
pub(crate) fn error(code: &'static str, message: impl Into<String>) -> crate::error::GatewayError {
    crate::error::GatewayError::Admission {
        status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
        code,
        message: message.into(),
    }
}
