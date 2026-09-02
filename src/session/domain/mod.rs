use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::{Uuid, Version};

use crate::llm::{FinishReason, Message, ModelSelection, Usage};

use super::SessionError;

pub const CURRENT_FORMAT_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDomain {
    Chat,
    Agent,
}

impl SessionDomain {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for SessionDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.prefix())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId {
    domain: SessionDomain,
    uuid: Uuid,
}

impl SessionId {
    #[must_use]
    pub fn new(domain: SessionDomain) -> Self {
        Self {
            domain,
            uuid: Uuid::now_v7(),
        }
    }

    pub fn domain(&self) -> SessionDomain {
        self.domain
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.domain.prefix(), self.uuid)
    }
}

impl FromStr for SessionId {
    type Err = SessionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (prefix, uuid_text) =
            value
                .split_once('-')
                .ok_or_else(|| SessionError::InvalidIdentifier {
                    kind: "session id",
                    value: value.to_string(),
                    reason: "expected `<chat|agent>-<uuid-v7>`",
                })?;
        let domain = match prefix {
            "chat" => SessionDomain::Chat,
            "agent" => SessionDomain::Agent,
            _ => {
                return Err(SessionError::InvalidIdentifier {
                    kind: "session id",
                    value: value.to_string(),
                    reason: "unknown domain prefix",
                });
            }
        };
        let uuid = Uuid::parse_str(uuid_text).map_err(|_| SessionError::InvalidIdentifier {
            kind: "session id",
            value: value.to_string(),
            reason: "UUID is malformed",
        })?;
        if uuid.get_version() != Some(Version::SortRand) {
            return Err(SessionError::InvalidIdentifier {
                kind: "session id",
                value: value.to_string(),
                reason: "UUID must be version 7",
            });
        }
        Ok(Self { domain, uuid })
    }
}

impl Serialize for SessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(Uuid);

impl EntryId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for EntryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for EntryId {
    type Err = SessionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value).map_err(|_| SessionError::InvalidIdentifier {
            kind: "entry id",
            value: value.to_string(),
            reason: "UUID is malformed",
        })?;
        if uuid.get_version() != Some(Version::SortRand) {
            return Err(SessionError::InvalidIdentifier {
                kind: "entry id",
                value: value.to_string(),
                reason: "UUID must be version 7",
            });
        }
        Ok(Self(uuid))
    }
}

impl Serialize for EntryId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EntryId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectIdentity {
    pub project_id: String,
    pub canonical_path: PathBuf,
    pub display_name: String,
}

impl ProjectIdentity {
    #[must_use]
    pub fn new(canonical_path: impl Into<PathBuf>, display_name: impl Into<String>) -> Self {
        let canonical_path = canonical_path.into();
        Self {
            project_id: format!("project-{}", Uuid::now_v7()),
            canonical_path: std::fs::canonicalize(&canonical_path).unwrap_or(canonical_path),
            display_name: display_name.into(),
        }
    }

    /// Rehydrate a project identity selected by the caller without changing
    /// its stable storage bucket. The location fields may change when a
    /// project is moved or renamed; `project_id` remains its durable owner.
    pub fn from_parts(
        project_id: impl Into<String>,
        canonical_path: impl Into<PathBuf>,
        display_name: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let canonical_path = canonical_path.into();
        let identity = Self {
            project_id: project_id.into(),
            canonical_path: std::fs::canonicalize(&canonical_path).unwrap_or(canonical_path),
            display_name: display_name.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        let Some(uuid_text) = self.project_id.strip_prefix("project-") else {
            return Err(SessionError::InvalidIdentifier {
                kind: "project id",
                value: self.project_id.clone(),
                reason: "expected `project-<uuid-v7>`",
            });
        };
        let uuid = Uuid::parse_str(uuid_text).map_err(|_| SessionError::InvalidIdentifier {
            kind: "project id",
            value: self.project_id.clone(),
            reason: "UUID is malformed",
        })?;
        if uuid.get_version() != Some(Version::SortRand) {
            return Err(SessionError::InvalidIdentifier {
                kind: "project id",
                value: self.project_id.clone(),
                reason: "UUID must be version 7",
            });
        }
        if self.canonical_path.as_os_str().is_empty() {
            return Err(SessionError::InvalidIdentifier {
                kind: "project path",
                value: String::new(),
                reason: "path must not be empty",
            });
        }
        if self.display_name.trim().is_empty() {
            return Err(SessionError::InvalidIdentifier {
                kind: "project display name",
                value: self.display_name.clone(),
                reason: "display name must not be empty",
            });
        }
        Ok(())
    }
}

impl Serialize for ProjectIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            project_id: &'a str,
            canonical_path: &'a Path,
            display_name: &'a str,
        }
        Wire {
            project_id: &self.project_id,
            canonical_path: &self.canonical_path,
            display_name: &self.display_name,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProjectIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            project_id: String,
            canonical_path: PathBuf,
            display_name: String,
        }
        let Wire {
            project_id,
            canonical_path,
            display_name,
        } = Wire::deserialize(deserializer)?;
        let identity = Self {
            project_id,
            canonical_path,
            display_name,
        };
        identity.validate().map_err(serde::de::Error::custom)?;
        Ok(identity)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub format_version: u16,
    pub session_id: SessionId,
    pub domain: SessionDomain,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_system_prompt: Option<String>,
}

impl SessionHeader {
    #[must_use]
    pub fn new(domain: SessionDomain, project: Option<ProjectIdentity>) -> Self {
        Self {
            format_version: CURRENT_FORMAT_VERSION,
            session_id: SessionId::new(domain),
            domain,
            created_at: now_millis(),
            project,
            initial_model: None,
            initial_system_prompt: None,
        }
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        if self.format_version != CURRENT_FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion(self.format_version));
        }
        if self.domain != self.session_id.domain() {
            return Err(SessionError::DomainMismatch {
                header: self.domain,
                id: self.session_id.domain(),
            });
        }
        match (self.domain, self.project.as_ref()) {
            (SessionDomain::Chat, Some(_)) => return Err(SessionError::ChatHasProject),
            (SessionDomain::Agent, None) => return Err(SessionError::AgentMissingProject),
            (_, Some(project)) => project.validate()?,
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: EntryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<EntryId>,
    pub timestamp: i64,
    pub kind: SessionEntryKind,
}

impl SessionEntry {
    #[must_use]
    pub fn header(header: SessionHeader) -> Self {
        Self {
            id: EntryId::new(),
            parent_id: None,
            timestamp: header.created_at,
            kind: SessionEntryKind::Header(header),
        }
    }

    #[must_use]
    pub fn new(id: EntryId, parent_id: Option<EntryId>, kind: SessionEntryKind) -> Self {
        Self::with_timestamp(id, parent_id, now_millis(), kind)
    }

    #[must_use]
    pub fn with_timestamp(
        id: EntryId,
        parent_id: Option<EntryId>,
        timestamp: i64,
        kind: SessionEntryKind,
    ) -> Self {
        Self {
            id,
            parent_id,
            timestamp,
            kind,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SessionEntryKind {
    Header(SessionHeader),
    Message(MessageEntry),
    TurnResult(TurnResult),
    ConfigChange(ConfigChange),
    FavoriteChange(FavoriteChange),
    Compaction(Compaction),
    BranchSummary(BranchSummary),
    TranscriptReplay(TranscriptReplay),
    Reference(Reference),
    Leaf(Leaf),
}

/// Session-level favorite flag. Last write in the file wins; it is not
/// branch-scoped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteChange {
    pub favorited: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageEntry {
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SafeError>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeError {
    pub category: SafeErrorCategory,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}

impl SafeError {
    /// Project a gateway failure onto the fields safe for a durable transcript.
    /// The gateway's raw upstream body is intentionally not reachable here.
    #[must_use]
    pub fn from_gateway(error: &crate::llm::GatewayError) -> Self {
        let category = match error.kind {
            crate::llm::ErrorKind::Configuration => SafeErrorCategory::Configuration,
            crate::llm::ErrorKind::Transport => SafeErrorCategory::Transport,
            crate::llm::ErrorKind::Http => SafeErrorCategory::Http,
            crate::llm::ErrorKind::Protocol => SafeErrorCategory::Protocol,
            crate::llm::ErrorKind::Provider => SafeErrorCategory::Provider,
        };
        Self {
            category,
            message: error.safe_message().to_string(),
            provider_code: error.provider_code.clone(),
            retryable: error.retryable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeErrorCategory {
    Configuration,
    Transport,
    Http,
    Protocol,
    Provider,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChange {
    pub model: ModelSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    pub summary: String,
    pub first_kept_entry_id: EntryId,
    pub tokens_before: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSummary {
    pub from_id: EntryId,
    pub summary: String,
}

/// A typed, transcript-only activity snapshot for future Agent tools.
///
/// These facts are intentionally plain data rather than handles to a process,
/// terminal emulator, or tool runtime. They restore visible activity without
/// making the persistence layer responsible for executing it again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptReplay {
    ToolCall {
        call_id: String,
        tool_name: String,
        arguments: Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    ToolActivity {
        title: String,
        #[serde(default)]
        content: String,
    },
    TerminalSnapshot {
        terminal_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default)]
        content: String,
    },
}

impl TranscriptReplay {
    pub fn validate(&self) -> Result<(), SessionError> {
        let invalid = |kind, value, reason| SessionError::InvalidIdentifier {
            kind,
            value,
            reason,
        };
        match self {
            Self::ToolCall {
                call_id, tool_name, ..
            } => {
                if call_id.trim().is_empty() {
                    return Err(invalid(
                        "tool call id",
                        call_id.clone(),
                        "must not be empty",
                    ));
                }
                if tool_name.trim().is_empty() {
                    return Err(invalid("tool name", tool_name.clone(), "must not be empty"));
                }
            }
            Self::ToolResult { call_id, .. } if call_id.trim().is_empty() => {
                return Err(invalid(
                    "tool call id",
                    call_id.clone(),
                    "must not be empty",
                ));
            }
            Self::ToolActivity { title, .. } if title.trim().is_empty() => {
                return Err(invalid(
                    "tool activity title",
                    title.clone(),
                    "must not be empty",
                ));
            }
            Self::TerminalSnapshot { terminal_id, .. } if terminal_id.trim().is_empty() => {
                return Err(invalid(
                    "terminal id",
                    terminal_id.clone(),
                    "must not be empty",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    #[must_use]
    pub fn preview(&self) -> Option<String> {
        match self {
            Self::ToolCall { tool_name, .. } => Some(tool_name.clone()),
            Self::ToolResult { content, .. } => {
                (!content.trim().is_empty()).then(|| content.clone())
            }
            Self::ToolActivity { title, content } => {
                if !content.trim().is_empty() {
                    Some(content.clone())
                } else {
                    Some(title.clone())
                }
            }
            Self::TerminalSnapshot { title, content, .. } => title
                .as_ref()
                .filter(|title| !title.trim().is_empty())
                .cloned()
                .or_else(|| (!content.trim().is_empty()).then(|| content.clone())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatMessageRef {
    pub session_id: SessionId,
    pub entry_id: EntryId,
}

impl ChatMessageRef {
    pub fn new(session_id: SessionId, entry_id: EntryId) -> Result<Self, SessionError> {
        if session_id.domain() != SessionDomain::Chat {
            return Err(SessionError::ReferenceSourceNotChat);
        }
        Ok(Self {
            session_id,
            entry_id,
        })
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        if self.session_id.domain() != SessionDomain::Chat {
            return Err(SessionError::ReferenceSourceNotChat);
        }
        Ok(())
    }
}

/// A Chat-session reference held by an Agent draft. Unlike [`ChatMessageRef`],
/// this names the whole conversation (session id only) so a later tool can
/// search that chat. It is not a JSONL `Reference` fact and is not serialized
/// into session entries.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChatSessionRef {
    pub session_id: SessionId,
}

impl ChatSessionRef {
    pub fn new(session_id: SessionId) -> Result<Self, SessionError> {
        if session_id.domain() != SessionDomain::Chat {
            return Err(SessionError::ReferenceSourceNotChat);
        }
        Ok(Self { session_id })
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        if self.session_id.domain() != SessionDomain::Chat {
            return Err(SessionError::ReferenceSourceNotChat);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub source: ChatMessageRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaf {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<EntryId>,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
