//! Background open path for one Chat session's paged transcript (design
//! §3.5): select the catalog row lazily, page the tail through the entry
//! index, and fall back to one full load when the index is unusable (R6).

use std::sync::Arc;

use super::JsonlOffsetSource;
use super::restore::{
    ChatSessionRestore, TAIL_PAGE_TURNS, next_turn_seed_from_path, next_turn_seed_from_state,
};
use crate::chat::transcript::{ResolvedStateSource, TranscriptSource as _};
use crate::llm::ModelSelection;
use crate::session::{
    SelectedChatSession, SessionError, SessionId, SessionReadStore, SessionTreeStore,
    SharedSessionStore,
};

/// One fully opened Chat session crossing from the background open task to
/// the workspace: the restore bundle for the view, plus the presentation
/// fields the sidebar row needs (title) and the composer seed (selection).
pub(crate) struct OpenedChatSession {
    pub restore: ChatSessionRestore,
    pub title: Option<String>,
    pub selection: Option<ModelSelection>,
}

/// Everything the background open task can fail with. Catalog errors stop
/// the open; entry-index problems are consumed by the full-load fallback
/// inside [`open_tail_page`] and only surface if that load also fails.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ChatOpenError {
    #[error("chat catalog selection failed: {0}")]
    Catalog(#[from] crate::session::ChatSessionCatalogError),
    #[error("chat transcript load failed: {0}")]
    Storage(#[from] SessionError),
}

/// Open one Chat session's tail page: resolve the entry index, build the
/// paged source, and read only the tail `TAIL_PAGE_TURNS` message entries.
/// Any index failure — missing rows, stale offsets, unreadable source, or a
/// first page that came back empty while content was expected — falls back
/// to a full `load_session` with no paged source (R6) and files a repair
/// intent so the next mutation or flush rebuilds the index.
pub(crate) fn open_tail_page(
    store: &SharedSessionStore,
    session_id: &SessionId,
    selected: &SelectedChatSession,
) -> Result<OpenedChatSession, ChatOpenError> {
    let read_handle = store.clone().tree_read_handle();
    let paged = (|| {
        let path = read_handle.load_entry_index(session_id, None).ok()?;
        let source = Arc::new(JsonlOffsetSource::new(
            session_id.clone(),
            Arc::clone(&read_handle),
            path,
        ));
        let page = source.load_tail(TAIL_PAGE_TURNS);
        // An empty first page with more content expected means the index
        // is unusable; fall back rather than showing nothing.
        if page.turns.is_empty() && source.total_hint().is_some_and(|hint| hint > 0) {
            return None;
        }
        // Turn ids are monotonic, so the path's last message entry names the
        // maximum; its small fact line is one targeted read. A failed read of
        // an existing message entry means the index cannot be trusted to seed
        // the runtime's turn counter either — fall back instead of seeding 0
        // and colliding with durable `turn-<n>` ids on the next send.
        let next_turn_seed = match next_turn_seed_from_path(&read_handle, session_id, source.path())
        {
            Some(seed) => seed,
            None if source.total_hint().is_some_and(|hint| hint > 0) => return None,
            None => 0,
        };
        Some((source, page, next_turn_seed))
    })();
    let restore = match paged {
        Some((source, page, next_turn_seed)) => ChatSessionRestore {
            session_id: session_id.clone(),
            page,
            cursor: None,
            source: Some(source),
            model: selected.model.clone(),
            next_turn_seed,
        },
        None => {
            // R6 fallback: the index is unusable. Mark the projection dirty
            // so the next mutation or flush rebuilds it, then load the whole
            // transcript once.
            if let Err(error) = store.clone().invalidate_entry_index(session_id) {
                crate::logging::warn(
                    "chat.restore",
                    format_args!(
                        "could not file a repair intent for chat session {session_id}: {error}"
                    ),
                );
            }
            let state = store.load_session(session_id, None)?;
            let next_turn_seed = next_turn_seed_from_state(&state);
            let page = ResolvedStateSource::new(state).load_tail(usize::MAX);
            ChatSessionRestore {
                session_id: session_id.clone(),
                page,
                cursor: None,
                source: None,
                model: selected.model.clone(),
                next_turn_seed,
            }
        }
    };
    Ok(OpenedChatSession {
        restore,
        title: selected.summary.title.clone(),
        selection: selected.model.clone(),
    })
}
