//! Shared session-domain services and capability façades.
//!
//! The implementation is split by responsibility: the core owns locking and
//! lifecycle barriers, capabilities expose narrowly scoped traits, and domains
//! coordinate independent Chat/Agent availability.

mod capabilities;
mod core;
mod domains;
#[cfg(test)]
mod tests;

pub use capabilities::{
    ConversationContext, SharedAgentProjectStore, SharedChatReferenceStore, SharedSessionCatalog,
    SharedSessionStore,
};
pub use domains::{SessionStores, SessionStoresError};

pub(crate) use core::SessionOperationGuard;
