use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::llm::{ModelSelection, Role};

use super::{
    DEFAULT_PAGE_SIZE, EntryId, PROJECTION_INTENT_PREFIX, ProjectIdentity, SessionDomain, SessionId,
};

#[derive(Clone, Debug)]
pub(crate) struct MessageNodeRow {
    pub session_id: SessionId,
    pub entry_id: EntryId,
    pub timestamp: i64,
    pub role: Role,
    pub preview: Option<String>,
    pub session_title: Option<String>,
    pub session_created_at: i64,
}

/// A durable marker for one source mutation whose disposable catalog update
/// has not yet been committed. Each operation gets its own key so concurrent
/// writers cannot clear one another's recovery obligation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionIntent {
    pub(super) key: String,
}

impl ProjectionIntent {
    pub(super) fn new(session_id: &SessionId) -> Self {
        Self {
            key: format!("{PROJECTION_INTENT_PREFIX}{session_id}:{}", Uuid::now_v7()),
        }
    }
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("failed to access session catalog: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("failed to access session catalog file: {0}")]
    Io(#[from] std::io::Error),
    #[error("session catalog contains invalid data: {0}")]
    Corrupt(String),
    #[error("unsupported session catalog schema version {0}")]
    UnsupportedVersion(i64),
    #[error("session catalog path is not a regular file: {0}")]
    UnsafePath(PathBuf),
    #[error("session catalog was replaced while an operation was in progress")]
    ReplacedDuringOperation,
    #[error("catalog is scoped to `{expected}`, but request targeted `{actual}`")]
    DomainMismatch {
        expected: SessionDomain,
        actual: SessionDomain,
    },
    #[error("session catalog is shutting down and cannot accept another operation")]
    StoreShuttingDown,
    #[error("session catalog lock is poisoned after an interrupted operation")]
    StorePoisoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCursor {
    pub created_at: i64,
    pub session_id: SessionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogQuery {
    pub project_id: Option<String>,
    pub cursor: Option<CatalogCursor>,
    pub limit: usize,
    /// `None` returns every session. `Some(true)` is the favorite group;
    /// `Some(false)` is the time-ordered timeline that excludes favorites.
    pub favorited: Option<bool>,
}

impl CatalogQuery {
    #[must_use]
    pub fn first_page() -> Self {
        Self {
            project_id: None,
            cursor: None,
            limit: DEFAULT_PAGE_SIZE,
            favorited: None,
        }
    }

    #[must_use]
    pub fn timeline_first_page() -> Self {
        Self {
            favorited: Some(false),
            ..Self::first_page()
        }
    }

    #[must_use]
    pub fn favorites() -> Self {
        Self {
            favorited: Some(true),
            limit: super::MAX_FAVORITES,
            ..Self::first_page()
        }
    }

    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            ..Self::first_page()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub domain: SessionDomain,
    pub project: Option<ProjectIdentity>,
    /// Missing titles remain semantic absence. The GUI supplies any localized
    /// placeholder at render time instead of persisting presentation text.
    pub title: Option<String>,
    pub preview: Option<String>,
    pub model: Option<ModelSelection>,
    pub total_tokens: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub favorited: bool,
    /// Local implementation detail used to locate the append-only source.
    /// Product callers restore through a typed store capability rather than
    /// opening this path themselves.
    pub(crate) jsonl_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogPage {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<CatalogCursor>,
}

/// Keyset cursor for project enumeration.  Projects are ordered by
/// `(updated_at DESC, project_id DESC)` so a cursor names the last project
/// already returned on the previous page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCatalogCursor {
    pub updated_at: i64,
    pub project_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectCatalogQuery {
    pub cursor: Option<ProjectCatalogCursor>,
    pub limit: usize,
}

impl ProjectCatalogQuery {
    #[must_use]
    pub fn first_page() -> Self {
        Self {
            cursor: None,
            limit: DEFAULT_PAGE_SIZE,
        }
    }

    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            ..Self::first_page()
        }
    }
}

/// Read-only summary of one persisted Agent project.  The catalog derives
/// `session_count` and `last_updated_at` from the project's session rows;
/// the GUI never reads JSONL or the `projects` table directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSummary {
    pub project_id: String,
    pub display_name: String,
    pub canonical_path: PathBuf,
    /// Number of Agent sessions currently bound to this project.  A project
    /// with zero sessions remains listed until its durable facts are gone.
    pub session_count: usize,
    /// Most recent `updated_at` across the project's sessions, or the
    /// project row's own `updated_at` when no session remains.
    pub last_updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectCatalogPage {
    pub projects: Vec<ProjectSummary>,
    pub next_cursor: Option<ProjectCatalogCursor>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepairReport {
    pub scanned: usize,
    pub rebuilt: usize,
    pub removed: usize,
    pub issues: Vec<String>,
}
