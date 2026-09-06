use std::collections::HashSet;

use thiserror::Error;

use crate::llm::ModelSelection;

use super::{
    CatalogCursor, CatalogError, CatalogQuery, EntryId, EntryKindTag, SessionCatalogStore,
    SessionDomain, SessionEntry, SessionEntryKind, SessionId, SessionSummary, SessionTreeStore,
};

/// One selected Chat session, resolved lazily: catalog metadata plus the
/// model the restored conversation should seed from. Transcript bodies are
/// loaded separately by the windowed transcript open path, never here.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedChatSession {
    pub summary: SessionSummary,
    pub model: Option<ModelSelection>,
}

#[derive(Debug, Error)]
pub enum ChatSessionCatalogError {
    #[error("the Chat catalog has not loaded its initial page")]
    NotInitialized,
    #[error("Chat catalog pagination cursor did not advance")]
    CursorDidNotAdvance,
    #[error("session `{session_id}` is not a Chat session")]
    NotChatSession { session_id: SessionId },
    #[error("Chat session `{session_id}` is not present in the catalog")]
    SessionNotFound { session_id: SessionId },
    #[error("Chat catalog operation failed: {0}")]
    Catalog(#[from] CatalogError),
}

/// UI-independent state holder for the Chat session directory.
///
/// It deliberately stops at typed catalog DTOs. A future GPUI sidebar owns
/// rendering, scroll triggers, and the selected ChatView.
pub struct ChatSessionCatalogController<S> {
    store: S,
    summaries: Vec<SessionSummary>,
    next_cursor: Option<CatalogCursor>,
    initial_loaded: bool,
    selected_session_id: Option<SessionId>,
}

impl<S> ChatSessionCatalogController<S>
where
    S: SessionCatalogStore + SessionTreeStore,
{
    #[must_use]
    pub fn new(store: S) -> Self {
        Self {
            store,
            summaries: Vec::new(),
            next_cursor: None,
            initial_loaded: false,
            selected_session_id: None,
        }
    }

    #[must_use]
    pub fn summaries(&self) -> &[SessionSummary] {
        &self.summaries
    }

    #[must_use]
    pub fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    #[must_use]
    pub fn initial_loaded(&self) -> bool {
        self.initial_loaded
    }

    #[must_use]
    pub fn selected_session_id(&self) -> Option<&SessionId> {
        self.selected_session_id.as_ref()
    }

    /// Load the first product page exactly once. A repeated call preserves
    /// the current snapshot instead of silently resetting pagination state.
    pub fn load_initial(&mut self) -> Result<bool, ChatSessionCatalogError> {
        if self.initial_loaded {
            return Ok(false);
        }

        let page = self
            .store
            .list_sessions(SessionDomain::Chat, CatalogQuery::first_page())?;
        let summaries = deduplicate(page.sessions);
        self.summaries = summaries;
        self.next_cursor = page.next_cursor;
        self.initial_loaded = true;
        Ok(true)
    }

    /// Append the next keyset page. No cursor means the whole directory is
    /// already loaded and therefore remains an inexpensive no-op.
    pub fn load_more(&mut self) -> Result<bool, ChatSessionCatalogError> {
        if !self.initial_loaded {
            return Err(ChatSessionCatalogError::NotInitialized);
        }
        let Some(cursor) = self.next_cursor.clone() else {
            return Ok(false);
        };

        let page = self.store.list_sessions(
            SessionDomain::Chat,
            CatalogQuery {
                cursor: Some(cursor.clone()),
                ..CatalogQuery::first_page()
            },
        )?;
        if page.next_cursor.as_ref() == Some(&cursor) {
            return Err(ChatSessionCatalogError::CursorDidNotAdvance);
        }

        let existing = self
            .summaries
            .iter()
            .map(|summary| summary.session_id.clone())
            .collect::<HashSet<_>>();
        let additions = deduplicate(page.sessions)
            .into_iter()
            .filter(|summary| !existing.contains(&summary.session_id))
            .collect::<Vec<_>>();
        let added = !additions.is_empty();
        self.summaries.extend(additions);
        self.next_cursor = page.next_cursor;
        Ok(added)
    }

    /// Select one Chat session lazily. This reads the catalog row and resolves
    /// the latest model through the entry index (one small targeted read per
    /// entry); it never loads or deserializes transcript message bodies. A
    /// successful select is the only transition that changes
    /// `selected_session_id`. An unusable entry index degrades `model` to
    /// `None` instead of failing: the transcript open path owns the fallback
    /// decision (R6).
    pub fn select(
        &mut self,
        session_id: &SessionId,
    ) -> Result<SelectedChatSession, ChatSessionCatalogError> {
        if session_id.domain() != SessionDomain::Chat {
            return Err(ChatSessionCatalogError::NotChatSession {
                session_id: session_id.clone(),
            });
        }
        let summary = self.store.get_session_summary(session_id)?.ok_or_else(|| {
            ChatSessionCatalogError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        if summary.domain != SessionDomain::Chat {
            return Err(ChatSessionCatalogError::NotChatSession {
                session_id: session_id.clone(),
            });
        }
        let model = latest_model(&self.store, session_id);
        self.selected_session_id = Some(session_id.clone());
        Ok(SelectedChatSession { summary, model })
    }
}

/// Resolve the model the restored conversation should seed from: the last
/// `ConfigChange` on the resolved active path, otherwise the latest message
/// entry that carries one. Both use bounded targeted reads; every error is a
/// disposable-index signal and degrades to `None`.
fn latest_model<S>(store: &S, session_id: &SessionId) -> Option<ModelSelection>
where
    S: SessionTreeStore,
{
    let path = store.load_entry_index(session_id, None).ok()?;
    for record in path.iter().rev() {
        if record.kind != EntryKindTag::ConfigChange {
            continue;
        }
        return read_one(store, session_id, &record.entry_id).and_then(|entry| match entry.kind {
            SessionEntryKind::ConfigChange(change) => Some(change.model),
            _ => None,
        });
    }
    // Every durable Chat turn writes the model onto its message entries, so
    // this walk stops at the first read in practice.
    for record in path.iter().rev() {
        if record.kind != EntryKindTag::Message {
            continue;
        }
        if let Some(model) =
            read_one(store, session_id, &record.entry_id).and_then(|entry| match entry.kind {
                SessionEntryKind::Message(message) => message.model,
                _ => None,
            })
        {
            return Some(model);
        }
    }
    None
}

fn read_one<S>(store: &S, session_id: &SessionId, entry_id: &EntryId) -> Option<SessionEntry>
where
    S: SessionTreeStore,
{
    store
        .read_entries(session_id, std::slice::from_ref(entry_id))
        .ok()
        .and_then(|entries| entries.into_iter().next())
}

fn deduplicate(summaries: Vec<SessionSummary>) -> Vec<SessionSummary> {
    let mut seen = HashSet::with_capacity(summaries.len());
    summaries
        .into_iter()
        .filter(|summary| seen.insert(summary.session_id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        llm::{ContentBlock, Message, ModelSelection, ProviderMetadata, Role, Usage},
        session::{
            CatalogPage, ConfigChange, EntryId, InMemorySessionStore, LocalSessionStore,
            LocalStoreConfig, MessageEntry, PathEntryRecord, ProjectIdentity, SessionBranchPreview,
            SessionBranchTreeSnapshot, SessionEntry, SessionEntryKind, SessionError,
            SessionFlushStore, SessionLifecycleStore, SessionStore, SessionStores,
            SessionTreeSnapshot,
        },
    };

    fn model(id: &str) -> ModelSelection {
        ModelSelection {
            profile_id: "profile".into(),
            model_id: id.into(),
        }
    }

    fn chat_message(text: &str, turn_model: Option<ModelSelection>) -> SessionEntryKind {
        SessionEntryKind::Message(MessageEntry {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                    provider_metadata: ProviderMetadata::default(),
                }],
                provider_metadata: ProviderMetadata::default(),
            },
            turn_id: None,
            model: turn_model,
            usage: Usage::default(),
        })
    }

    fn create_chat_session<S: SessionStore>(
        store: &mut S,
        created_at: i64,
        text: &str,
    ) -> SessionId {
        let mut header = super::super::SessionHeader::new(SessionDomain::Chat, None);
        header.created_at = created_at;
        let session_id = header.session_id.clone();
        store.create_session(header).expect("create chat session");
        store
            .append(
                &session_id,
                vec![chat_message(text, Some(model("seed-model")))],
            )
            .expect("append chat message");
        session_id
    }

    /// A wrapper that counts entry reads so laziness assertions can prove the
    /// controller never deserializes transcript bodies wholesale.
    struct CountingTreeStore {
        inner: InMemorySessionStore,
        entries_read: Arc<AtomicUsize>,
    }

    impl CountingTreeStore {
        fn wrap(inner: InMemorySessionStore) -> (Self, Arc<AtomicUsize>) {
            let entries_read = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    inner,
                    entries_read: Arc::clone(&entries_read),
                },
                entries_read,
            )
        }
    }

    impl SessionCatalogStore for CountingTreeStore {
        fn list_sessions(
            &self,
            domain: SessionDomain,
            query: CatalogQuery,
        ) -> Result<CatalogPage, CatalogError> {
            self.inner.list_sessions(domain, query)
        }

        fn get_session_summary(
            &self,
            session_id: &SessionId,
        ) -> Result<Option<SessionSummary>, CatalogError> {
            self.inner.get_session_summary(session_id)
        }
    }

    impl SessionTreeStore for CountingTreeStore {
        fn set_leaf(
            &mut self,
            session_id: &SessionId,
            leaf: Option<&EntryId>,
        ) -> Result<(), SessionError> {
            self.inner.set_leaf(session_id, leaf)
        }

        fn load_session_tree(
            &self,
            session_id: &SessionId,
        ) -> Result<SessionTreeSnapshot, SessionError> {
            self.inner.load_session_tree(session_id)
        }

        fn load_session_tree_for_leaf(
            &self,
            session_id: &SessionId,
            leaf: &EntryId,
        ) -> Result<SessionTreeSnapshot, SessionError> {
            self.inner.load_session_tree_for_leaf(session_id, leaf)
        }

        fn load_branch_preview(
            &self,
            session_id: &SessionId,
            branch_root: &EntryId,
        ) -> Result<SessionBranchPreview, SessionError> {
            self.inner.load_branch_preview(session_id, branch_root)
        }

        fn load_branch_tree(
            &self,
            session_id: &SessionId,
        ) -> Result<SessionBranchTreeSnapshot, SessionError> {
            self.inner.load_branch_tree(session_id)
        }

        fn load_entry_index(
            &self,
            session_id: &SessionId,
            leaf: Option<&EntryId>,
        ) -> Result<Vec<PathEntryRecord>, SessionError> {
            self.inner.load_entry_index(session_id, leaf)
        }

        fn read_entries(
            &self,
            session_id: &SessionId,
            entry_ids: &[EntryId],
        ) -> Result<Vec<SessionEntry>, SessionError> {
            self.entries_read
                .fetch_add(entry_ids.len(), Ordering::SeqCst);
            self.inner.read_entries(session_id, entry_ids)
        }

        fn invalidate_entry_index(&mut self, _session_id: &SessionId) -> Result<(), SessionError> {
            Ok(())
        }
    }

    fn exercise_pagination<S>(mut store: S)
    where
        S: SessionStore + SessionCatalogStore,
    {
        for index in 0..35 {
            create_chat_session(&mut store, index, &format!("chat {index}"));
        }
        let mut controller = ChatSessionCatalogController::new(store);
        assert!(!controller.initial_loaded());
        assert!(controller.load_initial().expect("load initial"));
        assert_eq!(controller.summaries().len(), 30);
        assert!(controller.has_more());
        assert!(
            controller
                .summaries()
                .windows(2)
                .all(|pair| pair[0].created_at >= pair[1].created_at)
        );
        assert!(!controller.load_initial().expect("repeat initial"));

        assert!(controller.load_more().expect("load more"));
        assert_eq!(controller.summaries().len(), 35);
        assert!(!controller.has_more());
        let ids = controller
            .summaries()
            .iter()
            .map(|summary| summary.session_id.clone())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), 35);
        assert!(!controller.load_more().expect("exhausted load more"));
    }

    #[test]
    fn memory_catalog_pages_in_creation_order_without_duplicates() {
        exercise_pagination(InMemorySessionStore::new());
    }

    fn exercise_exact_page_boundary<S>(mut store: S)
    where
        S: SessionStore + SessionCatalogStore,
    {
        for index in 0..30 {
            create_chat_session(&mut store, index, &format!("chat {index}"));
        }
        let mut controller = ChatSessionCatalogController::new(store);
        controller.load_initial().expect("load initial");
        assert_eq!(controller.summaries().len(), 30);
        assert!(!controller.has_more());
        assert!(!controller.load_more().expect("exhausted load more"));
    }

    #[test]
    fn exact_first_page_does_not_advertise_an_empty_second_page() {
        exercise_exact_page_boundary(InMemorySessionStore::new());

        let root = tempfile::tempdir().expect("temporary root");
        let local =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open local store");
        exercise_exact_page_boundary(local);
    }

    #[test]
    fn local_catalog_pages_in_creation_order_without_duplicates() {
        let root = tempfile::tempdir().expect("temporary root");
        let store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open local store");
        exercise_pagination(store);
    }

    #[test]
    fn catalog_is_empty_without_creating_a_draft_session() {
        let mut controller = ChatSessionCatalogController::new(InMemorySessionStore::new());
        assert!(matches!(
            controller.load_more(),
            Err(ChatSessionCatalogError::NotInitialized)
        ));
        assert!(controller.load_initial().expect("load initial"));
        assert!(controller.summaries().is_empty());
        assert!(
            controller
                .store
                .list_sessions(SessionDomain::Chat, CatalogQuery::first_page())
                .expect("catalog remains empty")
                .sessions
                .is_empty()
        );
        assert!(controller.selected_session_id().is_none());
    }

    #[test]
    fn selection_is_lazy_and_rejects_non_chat_sessions_without_state_changes() {
        let mut store = InMemorySessionStore::new();
        let chat_id = create_chat_session(&mut store, 10, "remember this");
        let agent_header = super::super::SessionHeader::new(
            SessionDomain::Agent,
            Some(ProjectIdentity::new("/tmp/project", "project")),
        );
        let agent_id = agent_header.session_id.clone();
        store.create_session(agent_header).expect("create agent");

        let mut controller = ChatSessionCatalogController::new(store);
        controller.load_initial().expect("load initial");
        assert!(controller.selected_session_id().is_none());
        let selected = controller.select(&chat_id).expect("select chat");
        assert_eq!(selected.summary.session_id, chat_id);
        assert_eq!(selected.summary.preview.as_deref(), Some("remember this"));
        assert_eq!(selected.model.as_ref(), Some(&model("seed-model")));
        assert_eq!(controller.selected_session_id(), Some(&chat_id));

        assert!(matches!(
            controller.select(&agent_id),
            Err(ChatSessionCatalogError::NotChatSession { .. })
        ));
        assert_eq!(controller.selected_session_id(), Some(&chat_id));
    }

    #[test]
    fn select_resolves_the_model_with_bounded_targeted_reads() {
        let mut store = InMemorySessionStore::new();
        let chat_id = create_chat_session(&mut store, 10, "first");
        for index in 1..6 {
            store
                .append(
                    &chat_id,
                    vec![chat_message(
                        &format!("message {index}"),
                        Some(model("seed-model")),
                    )],
                )
                .expect("append message");
        }
        store
            .append(
                &chat_id,
                vec![SessionEntryKind::ConfigChange(ConfigChange {
                    model: model("latest-model"),
                    system_prompt: None,
                })],
            )
            .expect("append config change");

        let (counting, reads) = CountingTreeStore::wrap(store);
        let mut controller = ChatSessionCatalogController::new(counting);
        let selected = controller.select(&chat_id).expect("select chat");
        assert_eq!(selected.model.as_ref(), Some(&model("latest-model")));
        // One targeted entry read resolves the model: no transcript body is
        // deserialized even though the session carries six messages.
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn select_falls_back_to_the_latest_message_model_without_a_config_change() {
        let mut store = InMemorySessionStore::new();
        let chat_id = create_chat_session(&mut store, 10, "only");
        let (counting, reads) = CountingTreeStore::wrap(store);
        let mut controller = ChatSessionCatalogController::new(counting);
        let selected = controller.select(&chat_id).expect("select chat");
        assert_eq!(selected.model.as_ref(), Some(&model("seed-model")));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_missing_catalog_row_keeps_the_prior_selection() {
        let mut store = InMemorySessionStore::new();
        let healthy_id = create_chat_session(&mut store, 1, "healthy");
        let missing_id = super::super::SessionHeader::new(SessionDomain::Chat, None).session_id;

        let mut controller = ChatSessionCatalogController::new(store);
        controller.load_initial().expect("load catalog");
        controller.select(&healthy_id).expect("select healthy");
        assert!(matches!(
            controller.select(&missing_id),
            Err(ChatSessionCatalogError::SessionNotFound { .. })
        ));
        assert_eq!(controller.selected_session_id(), Some(&healthy_id));
    }

    #[test]
    fn initial_page_is_catalog_only_for_local_storage() {
        let root = tempfile::tempdir().expect("temporary root");
        let mut store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open local store");
        let _id = create_chat_session(&mut store, 1, "catalog only");
        store.shutdown().expect("shutdown seed handles");

        let mut controller = ChatSessionCatalogController::new(store);
        assert_eq!(controller.store.open_handle_count(), 0);
        controller.load_initial().expect("load initial");
        assert_eq!(controller.store.open_handle_count(), 0);
    }

    #[test]
    fn shared_catalog_and_turn_stores_observe_the_same_session_facts() {
        let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
        let mut turns = stores.chat().expect("shared turn store");
        let session_id = create_chat_session(&mut turns, 1, "shared session");

        let mut catalog =
            ChatSessionCatalogController::new(stores.chat_catalog().expect("shared catalog"));
        catalog.load_initial().expect("load initial");
        assert_eq!(catalog.summaries().len(), 1);
        let selected = catalog.select(&session_id).expect("select");
        assert_eq!(selected.summary.session_id, session_id);
        assert_eq!(selected.model.as_ref(), Some(&model("seed-model")));
    }
}
