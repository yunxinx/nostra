use std::{error::Error as StdError, fmt, io};

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

#[derive(Debug)]
pub struct SessionMaintenanceFailures(Vec<(String, SessionError)>);

impl SessionMaintenanceFailures {
    pub(crate) fn new(failures: Vec<(String, SessionError)>) -> Self {
        Self(failures)
    }
}

impl fmt::Display for SessionMaintenanceFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (target, error)) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{target}: {error}")?;
        }
        Ok(())
    }
}

impl StdError for SessionMaintenanceFailures {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.0
            .first()
            .map(|(_, error)| error as &(dyn StdError + 'static))
    }
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
    #[error("session source contains {kind} at complete line {line}: {message}")]
    CorruptLine {
        line: usize,
        kind: DiagnosticKind,
        message: String,
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
    #[error(
        "agent session `{session_id}` belongs to project `{actual}`, not requested project `{expected}`"
    )]
    ProjectMismatch {
        session_id: SessionId,
        expected: String,
        actual: String,
    },
    #[error("invalid {kind} `{value}`: {reason}")]
    InvalidIdentifier {
        kind: &'static str,
        value: String,
        reason: &'static str,
    },
    #[error("duplicate entry id `{0}`")]
    DuplicateId(EntryId),
    #[error("durable session entry `{0}` conflicts with the exact retry batch")]
    ExactBatchConflict(EntryId),
    #[error("entry references missing parent `{0}`")]
    DanglingParent(EntryId),
    #[error("entry `{entry_id}` references later parent `{parent_id}`")]
    ParentOutOfOrder {
        entry_id: EntryId,
        parent_id: EntryId,
    },
    #[error("non-header entry `{0}` has no parent")]
    MissingParent(EntryId),
    #[error("entry graph contains a parent cycle")]
    CycleDetected,
    #[error("entry `{0}` is not a valid leaf")]
    LeafNotFound(EntryId),
    #[error("entry `{0}` is not a selectable branch root")]
    BranchNotFound(EntryId),
    #[error("session `{0}` does not exist")]
    SessionNotFound(SessionId),
    #[error("session `{0}` already exists")]
    SessionAlreadyExists(SessionId),
    #[error("session file header id `{actual}` does not match requested session `{expected}`")]
    SessionIdMismatch {
        expected: SessionId,
        actual: SessionId,
    },
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
    #[error("invalid transcript replay entry: {0}")]
    InvalidTranscriptReplay(String),
    #[error("session store lock is poisoned after an interrupted operation")]
    StorePoisoned,
    #[error("session storage is shutting down and cannot accept another operation")]
    StoreShuttingDown,
    #[error("timed out while waiting for session storage to shut down")]
    ShutdownTimeout,
    #[error("timed out while waiting for session store {operation}")]
    MaintenanceTimeout { operation: &'static str },
    #[error("session store {operation} failed: {failures}")]
    Maintenance {
        operation: &'static str,
        #[source]
        failures: SessionMaintenanceFailures,
    },
}

impl SessionError {
    pub(crate) fn io(source: io::Error) -> Self {
        Self::Io { source }
    }

    pub(crate) fn maintenance(
        operation: &'static str,
        failures: Vec<(String, SessionError)>,
    ) -> Self {
        Self::Maintenance {
            operation,
            failures: SessionMaintenanceFailures::new(failures),
        }
    }
}
