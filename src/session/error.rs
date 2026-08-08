use std::{fmt, io};

use thiserror::Error;

use super::{EntryId, SessionDomain, SessionId};

/// A non-fatal line-level issue found while scanning a JSONL file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonlDiagnostic {
    pub line: usize,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    InvalidJson,
    InvalidUtf8,
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidJson => "invalid JSON",
            Self::InvalidUtf8 => "invalid UTF-8",
        })
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("failed to access session storage: {source}")]
    Io {
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize session entry `{entry_id}`: {source}")]
    Serialize {
        entry_id: EntryId,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to parse session line {line}: {source}")]
    ParseLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("session has no header entry")]
    MissingHeader,
    #[error("session header must be the first entry")]
    HeaderNotFirst,
    #[error("session contains more than one header entry")]
    DuplicateHeader,
    #[error("session header uses unsupported format version {0}")]
    UnsupportedVersion(u16),
    #[error("session header domain `{header}` does not match session id domain `{id}`")]
    DomainMismatch {
        header: SessionDomain,
        id: SessionDomain,
    },
    #[error("chat sessions cannot have a project identity")]
    ChatHasProject,
    #[error("agent sessions require a project identity")]
    AgentMissingProject,
    #[error("invalid {kind} `{value}`: {reason}")]
    InvalidIdentifier {
        kind: &'static str,
        value: String,
        reason: &'static str,
    },
    #[error("duplicate entry id `{0}`")]
    DuplicateId(EntryId),
    #[error("entry references missing parent `{0}`")]
    DanglingParent(EntryId),
    #[error("non-header entry `{0}` has no parent")]
    MissingParent(EntryId),
    #[error("entry graph contains a parent cycle")]
    CycleDetected,
    #[error("entry `{0}` is not a valid leaf")]
    LeafNotFound(EntryId),
    #[error("session `{0}` does not exist")]
    SessionNotFound(SessionId),
    #[error("session `{0}` already exists")]
    SessionAlreadyExists(SessionId),
    #[error("compaction references invalid first kept entry `{0}`")]
    InvalidCompactionTarget(EntryId),
    #[error("branch summary references invalid source entry `{0}`")]
    InvalidBranchTarget(EntryId),
    #[error("chat message reference must point to a chat session")]
    ReferenceSourceNotChat,
    #[error("chat message references are only valid in agent sessions")]
    ReferenceOutsideAgent,
    #[error("session entry kind is not allowed in this position")]
    InvalidEntryKind,
}

impl SessionError {
    pub(crate) fn io(source: io::Error) -> Self {
        Self::Io { source }
    }
}
