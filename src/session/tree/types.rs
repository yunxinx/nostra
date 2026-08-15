use std::collections::HashSet;

use crate::llm::{Message, ModelSelection, Usage};

use super::super::{Compaction, ConfigChange, EntryId, Reference, TranscriptReplay, TurnResult};

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMessage {
    pub entry_id: EntryId,
    pub message: Message,
    pub turn_id: Option<String>,
    pub model: Option<ModelSelection>,
    pub usage: Usage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTurnResult {
    pub entry_id: EntryId,
    pub result: TurnResult,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTranscriptReplay {
    pub entry_id: EntryId,
    pub replay: TranscriptReplay,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedContextItem {
    Message(EntryId),
    Reference {
        entry_id: EntryId,
        reference: Reference,
    },
    Summary {
        entry_id: EntryId,
        summary: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSessionState {
    pub leaf_id: EntryId,
    pub path: Vec<EntryId>,
    pub context: Vec<ResolvedContextItem>,
    pub messages: Vec<ResolvedMessage>,
    pub transcript_replays: Vec<ResolvedTranscriptReplay>,
    pub turn_results: Vec<ResolvedTurnResult>,
    pub latest_config: Option<ConfigChange>,
    pub latest_compaction: Option<Compaction>,
}

/// A display-oriented row in the durable session graph. State-only entries
/// such as configuration, compaction, and leaf markers remain in the graph
/// but do not become rows of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTreeRowKind {
    UserMessage,
    AssistantMessage,
    ToolMessage,
    SystemMessage,
    DeveloperMessage,
    ToolActivity,
    TerminalActivity,
    ChatReference,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTreeRow {
    pub entry_id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp: i64,
    pub kind: SessionTreeRowKind,
    /// Missing visible content remains semantic absence. A future GUI chooses
    /// the localized label appropriate for this row kind at render time.
    pub preview: Option<String>,
    pub is_active_path: bool,
    pub is_current: bool,
    pub branch_choices: Vec<SessionTreeBranchChoice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBranchSummary {
    pub branch_root_id: EntryId,
    pub subtree_leaf_id: EntryId,
    pub latest_row_id: EntryId,
    /// Branch summaries do not persist presentation fallback strings.
    pub preview: Option<String>,
    pub is_current: bool,
    pub row_count: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTreeBranchChoice {
    pub branch: SessionBranchSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTreeSnapshot {
    pub rows: Vec<SessionTreeRow>,
    pub current_row_id: Option<EntryId>,
    pub active_row_ids: HashSet<EntryId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBranchTreeNode {
    pub parent_branch_root_id: Option<EntryId>,
    pub branch: SessionBranchSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBranchTreeSnapshot {
    pub nodes: Vec<SessionBranchTreeNode>,
    pub current_branch_root_id: Option<EntryId>,
    pub total_row_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionBranchPreview {
    pub branch: SessionBranchSummary,
    pub common_parent_id: Option<EntryId>,
    pub snapshot: SessionTreeSnapshot,
}
