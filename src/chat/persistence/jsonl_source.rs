//! Cursor-shaped transcript source backed by the session entry index.
//!
//! The source holds an immutable snapshot of the resolved active path's entry
//! metadata plus the shared session-store capability. Opening a conversation
//! loads only the tail page; `load_before` reads earlier pages by entry id,
//! deserializing just that page's JSONL lines. Errors fail the page so the
//! open path can fall back to a full `ResolvedStateSource` load.

use std::sync::Arc;

use crate::session::{EntryId, EntryKindTag, PathEntryRecord, SessionId, SessionTreeStore};

use super::super::transcript::{
    TranscriptCursor, TranscriptPage, TranscriptSource, Turn, allocate_turn_id,
};
/// A transcript source that pages an opened session by entry id through
/// `SessionTreeStore::read_entries`. The path snapshot is taken once at
/// construction; appends during a live session only extend the tail, which
/// the model already holds, so the prefix stays immutable for paging.
pub(crate) struct JsonlOffsetSource {
    session_id: SessionId,
    store: Arc<dyn SessionTreeStore + Send + Sync>,
    path: Vec<PathEntryRecord>,
}

impl JsonlOffsetSource {
    /// Build the source from a resolved entry path. The store handle is the
    /// shared send-safe service capability; reads must not open a recorder or
    /// mutate catalog state.
    pub(crate) fn new(
        session_id: SessionId,
        store: Arc<dyn SessionTreeStore + Send + Sync>,
        path: Vec<PathEntryRecord>,
    ) -> Self {
        Self {
            session_id,
            store,
            path,
        }
    }

    /// Entry metadata of the resolved path, ordered from header to leaf.
    #[must_use]
    pub(crate) fn path(&self) -> &[PathEntryRecord] {
        &self.path
    }

    fn message_records(&self) -> Vec<&PathEntryRecord> {
        self.path
            .iter()
            .filter(|record| record.kind == EntryKindTag::Message)
            .collect()
    }

    fn page_of(
        &self,
        records: &[&PathEntryRecord],
        at_path_start: bool,
    ) -> Result<TranscriptPage, String> {
        if records.is_empty() {
            return Ok(TranscriptPage {
                turns: Vec::new(),
                cursor_before: None,
            });
        }
        let entry_ids: Vec<EntryId> = records
            .iter()
            .map(|record| record.entry_id.clone())
            .collect();
        // Reading a page is bounded by the page size: only the requested
        // entries are deserialized, never the whole session file.
        let entries = self
            .store
            .read_entries(&self.session_id, &entry_ids)
            .map_err(|error| error.to_string())?;
        let messages = entries
            .into_iter()
            .map(|entry| match entry.kind {
                crate::session::SessionEntryKind::Message(message) => Ok((entry.id, message)),
                _ => Err(format!(
                    "entry `{}` in the message page is not a message entry",
                    entry.id
                )),
            })
            .collect::<Result<Vec<_>, String>>()?;

        // The deserialized entry ids must mirror the requested page; the
        // store already enforces this per entry, so a difference here means
        // the index moved between resolution and read.
        for (entry_id, (loaded_id, _)) in entry_ids.iter().zip(&messages) {
            if entry_id != loaded_id {
                return Err(format!("entry page id mismatch for `{entry_id}`"));
            }
        }

        // Placeholder ids: `Transcript::load` / `prepend` adopt pages through
        // the model counters, so page-local ids never reach the model.
        let mut next_turn_id = 1;
        let mut next_part_id = 1;
        let turns = messages
            .into_iter()
            .map(|(_, message)| {
                let turn_id = allocate_turn_id(&mut next_turn_id);
                Turn::from_llm(message.message, turn_id, &mut next_part_id)
            })
            .collect();
        let cursor_before = (!at_path_start)
            .then(|| records.first())
            .flatten()
            .map(|record| TranscriptCursor {
                entry_id: record.entry_id.clone(),
            });
        Ok(TranscriptPage {
            turns,
            cursor_before,
        })
    }

    fn load_window(&self, start: usize, end: usize) -> TranscriptPage {
        let records = self.message_records();
        let end = end.min(records.len());
        let start = start.min(end);
        match self.page_of(&records[start..end], start == 0) {
            Ok(page) => page,
            // Fail closed with an empty page and a preserved cursor: callers
            // of `load_before` stop prepending, and the open path treats a
            // first-page failure as its signal to fall back to a full load.
            Err(_error) => TranscriptPage {
                turns: Vec::new(),
                cursor_before: Some(TranscriptCursor {
                    entry_id: records[start].entry_id.clone(),
                }),
            },
        }
    }
}

impl TranscriptSource for JsonlOffsetSource {
    fn total_hint(&self) -> Option<usize> {
        Some(
            self.path
                .iter()
                .filter(|record| record.kind == EntryKindTag::Message)
                .count(),
        )
    }

    fn load_tail(&self, turns: usize) -> TranscriptPage {
        let len = self.message_records().len();
        let start = if turns == usize::MAX {
            0
        } else {
            len.saturating_sub(turns)
        };
        self.load_window(start, len)
    }

    fn load_before(&self, cursor: &TranscriptCursor, turns: usize) -> TranscriptPage {
        let records = self.message_records();
        let Some(position) = records
            .iter()
            .position(|record| record.entry_id == cursor.entry_id)
        else {
            // The path no longer contains the cursor's entry (for example
            // after a rewind while an older source was retained). Return an
            // empty page and keep the cursor so the loader stops instead of
            // skipping content.
            return TranscriptPage {
                turns: Vec::new(),
                cursor_before: Some(cursor.clone()),
            };
        };
        let start = if turns == usize::MAX {
            0
        } else {
            position.saturating_sub(turns)
        };
        self.load_window(start, position)
    }
}

#[cfg(test)]
mod tests {
    use crate::llm::{ContentBlock, Message as LlmMessage, Role as LlmRole};
    use crate::session::{
        EntryId, EntryKindTag, LocalSessionStore, LocalStoreConfig, MessageEntry,
        ResolvedSessionState, SessionDomain, SessionEntryKind, SessionFlushStore, SessionHeader,
        SessionLifecycleStore, SessionReadStore, SessionStores, Usage,
    };

    use super::*;

    fn message(text: &str, turn: usize) -> SessionEntryKind {
        SessionEntryKind::Message(MessageEntry {
            message: LlmMessage {
                role: if turn % 2 == 0 {
                    LlmRole::User
                } else {
                    LlmRole::Assistant
                },
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
            turn_id: Some(format!("turn-{turn}")),
            model: None,
            usage: Usage::default(),
        })
    }

    fn prose_text(turn: &Turn) -> String {
        turn.parts
            .iter()
            .find_map(|part| match &part.source {
                crate::chat::transcript::PartSource::Prose { text, .. } => Some(text.to_string()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Seed a local Chat session with `count` message entries and return a
    /// `JsonlOffsetSource` over the shared read handle.
    fn seeded_source(count: usize) -> (JsonlOffsetSource, ResolvedSessionState) {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path().to_path_buf(), SessionDomain::Chat);
        let mut store = LocalSessionStore::open(config.clone()).expect("open local store");
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let session_id = header.session_id.clone();
        let entries: Vec<SessionEntryKind> = (0..count)
            .map(|index| message(&format!("message {index}"), index))
            .collect();
        store
            .create_session_with_entries(header, entries)
            .expect("create session");
        store.shutdown().expect("shutdown writer");

        let stores = SessionStores::with_chat_store(
            LocalSessionStore::open(config.clone()).expect("reopen store"),
        );
        let handle = stores.chat().expect("chat capability").tree_read_handle();
        let path = handle
            .load_entry_index(&session_id, None)
            .expect("load entry index");
        let source = JsonlOffsetSource::new(session_id.clone(), handle, path);

        // A separate read-only handle provides the authoritative full load
        // for the parity comparison.
        let reader = LocalSessionStore::open(config.clone()).expect("open reader");
        let state = reader.load_session(&session_id, None).expect("full load");
        // Leak the tempdir: the paged reads must outlive the seeding scope.
        std::mem::forget(root);
        (source, state)
    }

    #[test]
    fn load_tail_and_load_before_page_the_full_path_in_order() {
        let (source, state) = seeded_source(10);
        assert_eq!(source.total_hint(), Some(10));

        let tail = source.load_tail(4);
        assert_eq!(tail.turns.len(), 4);
        assert_eq!(prose_text(&tail.turns[0]), "message 6");
        let cursor = tail.cursor_before.clone().expect("earlier page remains");

        // Walk backwards to the path start.
        let mut collected: Vec<String> = tail.turns.iter().map(prose_text).collect();
        let mut cursor = cursor;
        loop {
            let page = source.load_before(&cursor, 4);
            if page.turns.is_empty() {
                break;
            }
            let mut page_texts: Vec<String> = page.turns.iter().map(prose_text).collect();
            page_texts.extend(collected);
            collected = page_texts;
            let Some(next) = page.cursor_before else {
                break;
            };
            cursor = next;
        }
        // Paged content must match the full-load order entry for entry.
        assert_eq!(collected.len(), state.messages.len());
        for (text, resolved) in collected.iter().zip(&state.messages) {
            assert_eq!(text, &message_text_of(resolved));
        }

        // A full-range tail load has no earlier page.
        let earliest = source.load_tail(usize::MAX);
        assert_eq!(earliest.turns.len(), state.messages.len());
        assert!(earliest.cursor_before.is_none());
    }

    fn message_text_of(resolved: &crate::session::ResolvedMessage) -> String {
        resolved
            .message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.to_string()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn unknown_cursor_returns_an_empty_page_and_keeps_the_cursor() {
        let (source, _state) = seeded_source(4);
        let stale = TranscriptCursor {
            entry_id: EntryId::new(),
        };
        let page = source.load_before(&stale, 2);
        assert!(page.turns.is_empty());
        assert_eq!(page.cursor_before, Some(stale));
    }

    #[test]
    fn path_snapshot_contains_only_active_path_metadata() {
        let (source, _state) = seeded_source(5);
        let path = source.path();
        assert_eq!(path.len(), 6);
        assert_eq!(path[0].kind, EntryKindTag::Header);
        assert!(
            path[1..]
                .iter()
                .all(|record| record.kind == EntryKindTag::Message)
        );
        assert!(path.iter().all(|record| record.byte_len.is_some()));
    }
}
