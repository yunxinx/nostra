//! UI-independent session facts, JSONL persistence, tree resolution, and local
//! Chat/Agent stores.
//!
//! A session file is the append-only source of truth. The SQLite catalog is a
//! disposable projection owned by the local store, so the same domain model
//! remains usable by the in-memory implementation and future adapters.

mod catalog;
mod chat;
mod chat_catalog;
mod domain;
mod error;
mod jsonl;
mod local;
mod memory;
mod recorder;
mod service;
mod tree;

pub use crate::llm::{FinishReason, Usage};
pub use catalog::{
    CatalogCursor, CatalogError, CatalogPage, CatalogQuery, RepairReport, SessionSummary,
};
pub use chat::{
    ChatSessionController, ChatSessionControllerError, ChatTurnStart, ChatTurnTerminal,
};
pub use chat_catalog::{
    ChatSessionCatalogController, ChatSessionCatalogError, SelectedChatSession,
};
pub use domain::{
    BranchSummary, CURRENT_FORMAT_VERSION, ChatMessageRef, Compaction, ConfigChange, EntryId, Leaf,
    MessageEntry, ProjectIdentity, Reference, SafeError, SafeErrorCategory, SessionDomain,
    SessionEntry, SessionEntryKind, SessionHeader, SessionId, TurnResult, TurnStatus,
};
pub use error::{DiagnosticKind, JsonlDiagnostic, SessionError};
pub use jsonl::{JsonlLoad, JsonlLoader, JsonlWriter};
pub use local::{LocalSessionStore, LocalStoreConfig, LocalStoreError};
pub use memory::{
    InMemorySessionStore, SessionCatalogStore, SessionFlushStore, SessionLifecycleStore,
    SessionStore, SessionTreeStore,
};
pub use service::{SessionStores, SharedSessionStore};
pub use tree::{
    ResolvedContextItem, ResolvedMessage, ResolvedSessionState, ResolvedTurnResult,
    resolve_session, validate_session_entries,
};

pub(crate) use recorder::JsonlRecorder;
pub(crate) use tree::validate_appended_kind;
