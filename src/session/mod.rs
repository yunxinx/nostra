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
mod reference;
mod service;
mod tree;

pub use crate::llm::{FinishReason, Usage};
pub use catalog::{
    CatalogCursor, CatalogError, CatalogPage, CatalogQuery, ProjectCatalogCursor,
    ProjectCatalogPage, ProjectCatalogQuery, ProjectSummary, RepairReport, SessionSummary,
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
    SessionEntry, SessionEntryKind, SessionHeader, SessionId, TranscriptReplay, TurnResult,
    TurnStatus,
};
pub use error::{DiagnosticKind, JsonlDiagnostic, SessionError};
pub use jsonl::{JsonlLoad, JsonlLoader, JsonlWriter};
pub use local::{LocalSessionStore, LocalStoreConfig, LocalStoreError};
pub use memory::{
    InMemorySessionStore, ProjectSessionStore, SessionCatalogStore, SessionFlushStore,
    SessionLifecycleStore, SessionReadStore, SessionStore, SessionTreeStore,
};
pub use reference::{
    AgentChatReferenceTool, ChatMessagePreview, ChatMessageRead, ChatMessageReferenceStore,
    ChatMessageSearchCursor, ChatMessageSearchPage, ChatMessageSearchQuery, ChatMessageUnavailable,
    ChatMessageUnavailableReason, ChatReferenceError, MAX_REFERENCE_MESSAGE_BYTES,
    ReferencedContentBlock, ReferencedMessage,
};
pub use service::{
    SessionStores, SessionStoresError, SharedAgentProjectStore, SharedChatReferenceStore,
    SharedSessionCatalog, SharedSessionStore,
};
pub use tree::{
    ResolvedContextItem, ResolvedMessage, ResolvedSessionState, ResolvedTranscriptReplay,
    ResolvedTurnResult, SessionBranchPreview, SessionBranchSummary, SessionBranchTreeNode,
    SessionBranchTreeSnapshot, SessionTreeBranchChoice, SessionTreeRow, SessionTreeRowKind,
    SessionTreeSnapshot, resolve_session, session_branch_preview, session_branch_tree_snapshot,
    session_tree_snapshot, validate_session_entries,
};

pub(crate) use recorder::JsonlRecorder;
pub(crate) use service::SessionOperationGuard;
pub(crate) use tree::AppendValidationState;
