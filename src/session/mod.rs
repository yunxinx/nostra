//! UI-independent session facts, JSONL persistence, and tree resolution.
//!
//! A session file is the append-only source of truth.  The catalog and any
//! future database projection are deliberately outside this module so that the
//! same domain model can be used by the Chat and Agent stores.

mod domain;
mod error;
mod jsonl;
mod memory;
mod tree;

pub use crate::llm::{FinishReason, Usage};
pub use domain::{
    BranchSummary, CURRENT_FORMAT_VERSION, ChatMessageRef, Compaction, ConfigChange, EntryId, Leaf,
    MessageEntry, ProjectIdentity, Reference, SafeError, SafeErrorCategory, SessionDomain,
    SessionEntry, SessionEntryKind, SessionHeader, SessionId, TurnResult, TurnStatus,
};
pub use error::{DiagnosticKind, JsonlDiagnostic, SessionError};
pub use jsonl::{JsonlLoad, JsonlLoader, JsonlWriter};
pub use memory::{
    InMemorySessionStore, SessionFlushStore, SessionLifecycleStore, SessionStore, SessionTreeStore,
};
pub use tree::{
    ResolvedContextItem, ResolvedMessage, ResolvedSessionState, resolve_session,
    validate_session_entries,
};

pub(crate) use tree::validate_appended_kind;
