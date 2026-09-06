use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use super::super::ChatMessageRef;
use super::*;

impl SessionReadStore for LocalSessionStore {
    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError> {
        resolve_session(&self.load_entries_for_session(session_id)?, leaf)
    }
}

impl SessionTreeStore for LocalSessionStore {
    fn set_leaf(
        &mut self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<(), SessionError> {
        let _lock = self
            .acquire_domain_lock()
            .map_err(local_store_session_error)?;
        self.ensure_handle(session_id)
            .map_err(local_store_session_error)?;
        let source_boundary = self.source_boundary.clone();
        let projection_intent = self
            .catalog
            .begin_projection_intent(session_id)
            .map_err(session_io_error)?;
        let catalog = &mut self.catalog;
        let handle = self
            .handles
            .get_mut(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        handle.projection_intents.push(projection_intent);
        handle.catalog_dirty = true;
        if let Err(error) = handle.recorder.set_leaf(leaf) {
            let pending_remains = handle.recorder.has_pending();
            // A write can fail after a complete Leaf fact reaches JSONL. Read
            // the source again before deciding which projection is safe to
            // publish; an exact batch still pending may also be persisted by
            // recorder shutdown after this method returns.
            if let Ok(loaded) = Self::reload_load(&source_boundary, &handle.path)
                && let Ok(projection) =
                    SessionProjection::from_entries(&handle.header, &loaded.entries)
                && let Ok(entry_rows) = entry_index_rows(&loaded.entries, &loaded.entry_ranges)
            {
                handle.entries = loaded.entries;
                handle.projection = projection;
                handle.source_stamp = source_stamp(&handle.path);
                let result = if pending_remains {
                    catalog.upsert_projection_with_intents(
                        &handle.header,
                        &handle.projection,
                        &entry_rows,
                        &handle.path,
                        &[],
                    )
                } else {
                    // Deterministic validation rejection wrote no fact, so this
                    // source-derived projection completes the operation's intent.
                    catalog.upsert_projection_with_intents(
                        &handle.header,
                        &handle.projection,
                        &entry_rows,
                        &handle.path,
                        &handle.projection_intents,
                    )
                };
                handle.catalog_dirty = pending_remains || result.is_err();
                if result.is_ok() && !pending_remains {
                    handle.projection_intents.clear();
                }
            }
            return Err(error);
        }
        #[cfg(test)]
        if std::mem::take(&mut self.faults.after_leaf_commit) {
            handle.catalog_dirty = true;
            return Err(SessionError::io(std::io::Error::other(
                "injected interruption after session leaf publication",
            )));
        }
        let loaded = Self::reload_load(&source_boundary, &handle.path).map_err(session_io_error)?;
        handle.entries = loaded.entries;
        handle.projection = SessionProjection::from_entries(&handle.header, &handle.entries)
            .map_err(session_io_error)?;
        handle.source_stamp = source_stamp(&handle.path);
        let entry_rows =
            entry_index_rows(&handle.entries, &loaded.entry_ranges).map_err(session_io_error)?;
        let result = catalog.upsert_projection_with_intents(
            &handle.header,
            &handle.projection,
            &entry_rows,
            &handle.path,
            &handle.projection_intents,
        );
        handle.catalog_dirty = result.is_err();
        result.map_err(session_io_error)?;
        handle.projection_intents.clear();
        Ok(())
    }

    fn load_session_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_tree_snapshot(&entries, None)
    }

    fn load_session_tree_for_leaf(
        &self,
        session_id: &SessionId,
        leaf: &EntryId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_tree_snapshot(&entries, Some(leaf))
    }

    fn load_branch_preview(
        &self,
        session_id: &SessionId,
        branch_root: &EntryId,
    ) -> Result<SessionBranchPreview, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_branch_preview(&entries, branch_root)
    }

    fn load_branch_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionBranchTreeSnapshot, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_branch_tree_snapshot(&entries, None)
    }

    fn load_entry_index(
        &self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<Vec<PathEntryRecord>, SessionError> {
        if session_id.domain() != self.config.domain {
            return Err(SessionError::DomainMismatch {
                header: self.config.domain,
                id: session_id.domain(),
            });
        }
        let rows = self
            .catalog
            .entry_index_rows(session_id)
            .map_err(session_io_error)?;
        if rows.is_empty() {
            // Distinguish a deleted session from a missing index projection;
            // both must fail closed so callers take the full-load fallback.
            self.catalog
                .get(session_id)
                .map_err(session_io_error)?
                .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
            return Err(SessionError::EntryIndexMissing(session_id.clone()));
        }
        let records = rows
            .into_iter()
            .map(|row| PathEntryRecord {
                entry_id: row.entry_id,
                parent_id: row.parent_id,
                kind: row.kind,
                byte_offset: Some(row.byte_offset),
                byte_len: Some(row.byte_len),
                timestamp: row.timestamp,
            })
            .collect::<Vec<_>>();
        let source_path = self.authorized_source_path_for_read(session_id)?;
        resolve_entry_index_path(&records, &source_path, leaf, session_id)
    }

    fn read_entries(
        &self,
        session_id: &SessionId,
        entry_ids: &[EntryId],
    ) -> Result<Vec<SessionEntry>, SessionError> {
        if entry_ids.is_empty() {
            return Ok(Vec::new());
        }
        if session_id.domain() != self.config.domain {
            return Err(SessionError::DomainMismatch {
                header: self.config.domain,
                id: session_id.domain(),
            });
        }
        let rows = self
            .catalog
            .entry_index_rows_for_entries(session_id, entry_ids)
            .map_err(session_io_error)?;
        #[cfg(test)]
        super::READ_ENTRIES_PROBE.fetch_add(entry_ids.len(), std::sync::atomic::Ordering::Relaxed);
        let path = self.authorized_source_path_for_read(session_id)?;
        let mut file = fs::File::open(&path).map_err(SessionError::io)?;
        let mismatch = |entry_id: &EntryId| SessionError::EntryIndexMismatch {
            session_id: session_id.clone(),
            entry_id: entry_id.clone(),
        };
        entry_ids
            .iter()
            .zip(rows)
            .map(|(entry_id, row)| {
                let row = row.ok_or_else(|| mismatch(entry_id))?;
                // The index is a disposable projection: the deserialized fact
                // must prove it is exactly the requested entry, otherwise the
                // offsets are stale and the caller must fall back.
                let text = read_entry_line(&mut file, row.byte_offset, row.byte_len)
                    .map_err(|_| mismatch(entry_id))?;
                let entry: SessionEntry =
                    serde_json::from_str(&text).map_err(|_| mismatch(entry_id))?;
                if entry.id != *entry_id {
                    return Err(mismatch(entry_id));
                }
                Ok(entry)
            })
            .collect()
    }

    fn invalidate_entry_index(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        if session_id.domain() != self.config.domain {
            return Err(SessionError::DomainMismatch {
                header: self.config.domain,
                id: session_id.domain(),
            });
        }
        // A retained handle keeps the disposable projection the next mutation
        // would extend incrementally; force that extension to become a full
        // rebuild instead. A handle that does not exist yet always starts
        // dirty, so there is nothing to mark.
        if let Some(handle) = self.handles.get_mut(session_id) {
            handle.catalog_dirty = true;
        }
        Ok(())
    }
}

impl SessionFlushStore for LocalSessionStore {
    fn flush(&mut self) -> Result<(), SessionError> {
        let _lock = self
            .acquire_domain_lock()
            .map_err(local_store_session_error)?;
        self.flush_locked()
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        let _lock = self
            .acquire_domain_lock()
            .map_err(local_store_session_error)?;
        self.flush_locked()?;
        self.handles.clear();
        Ok(())
    }
}

impl LocalSessionStore {
    /// Resolve the authorized JSONL source for a read-only entry-index
    /// operation. An open handle is used only while its file identity still
    /// matches; otherwise the path is derived from validated session identity
    /// like every other local read, never from catalog path text alone.
    fn authorized_source_path_for_read(
        &self,
        session_id: &SessionId,
    ) -> Result<PathBuf, SessionError> {
        if let Some(handle) = self.handles.get(session_id) {
            source::authorize_existing_source(&self.source_boundary, &handle.path)
                .map_err(local_store_session_error)?;
            if source_stamp(&handle.path) == handle.source_stamp {
                return Ok(handle.path.clone());
            }
        }
        let summary = self
            .catalog
            .get(session_id)
            .map_err(session_io_error)?
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        let path = self.source_path_for_summary(&summary);
        source::authorize_existing_source(&self.source_boundary, &path)
            .map_err(local_store_session_error)
    }

    fn flush_locked(&mut self) -> Result<(), SessionError> {
        self.flush_handles_locked("flush")?;
        if self.catalog.needs_repair() {
            self.repair_locked().map_err(local_store_session_error)?;
        }
        Ok(())
    }

    pub(super) fn flush_handles_locked(
        &mut self,
        operation: &'static str,
    ) -> Result<(), SessionError> {
        let session_ids = self.handles.keys().cloned().collect::<Vec<_>>();
        let mut failures = Vec::new();
        for session_id in session_ids {
            let missing_without_pending = self.handles.get(&session_id).is_some_and(|handle| {
                !handle.recorder.has_pending()
                    && matches!(
                        source::authorize_delete_target(&self.source_boundary, &handle.path,),
                        Ok(source::AuthorizedDeleteTarget::Missing { .. })
                    )
            });
            if missing_without_pending {
                // An external unlink can leave an idle recorder holding only
                // an unreachable file descriptor. Drop that handle so the
                // catalog-wide repair can durably confirm the missing
                // directory entry and remove its stale projection. A recorder
                // with an exact pending batch must still fail instead.
                self.handles.remove(&session_id);
                continue;
            }
            if let Err(error) = self.flush_handle(&session_id) {
                failures.push((session_id.to_string(), error));
            }
        }
        if !failures.is_empty() {
            if failures.len() == 1 {
                if let Some((_, error)) = failures.pop() {
                    return Err(error);
                }
            }
            return Err(SessionError::maintenance(operation, failures));
        }
        Ok(())
    }

    pub(super) fn drain_handles_for_repair_locked(&mut self) -> Result<(), SessionError> {
        let session_ids = self.handles.keys().cloned().collect::<Vec<_>>();
        let mut failures = Vec::new();
        for session_id in session_ids {
            let (path, source_stamp, has_pending) = self
                .handles
                .get(&session_id)
                .map(|handle| {
                    (
                        handle.path.clone(),
                        handle.source_stamp.clone(),
                        handle.recorder.has_pending(),
                    )
                })
                .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
            if let Err(error) = source::authorize_retained_source(
                &self.source_boundary,
                &path,
                source_stamp.as_ref(),
            ) {
                if has_pending {
                    if let Some(mut handle) = self.handles.remove(&session_id) {
                        handle.recorder.abandon_pending_after_authority_loss();
                    }
                    failures.push((session_id.to_string(), local_store_session_error(error)));
                } else {
                    // With no retry batch, dropping the stale descriptor cannot
                    // publish new facts. The repair scan will report the unsafe
                    // namespace and preserve the last trusted catalog row.
                    self.handles.remove(&session_id);
                }
                continue;
            }
            let result = self
                .handles
                .get(&session_id)
                .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?
                .recorder
                .flush();
            match result {
                Ok(()) => {
                    // Repair must inspect the source independently. Retaining
                    // a cached projection would either hide a complete corrupt
                    // line or let a later append cross that corruption after
                    // the scan deliberately preserved the last trusted row.
                    self.handles.remove(&session_id);
                }
                Err(error) => failures.push((session_id.to_string(), error)),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else if failures.len() == 1 {
            if let Some((_, error)) = failures.pop() {
                Err(error)
            } else {
                Ok(())
            }
        } else {
            Err(SessionError::maintenance("repair", failures))
        }
    }
}

impl SessionCatalogStore for LocalSessionStore {
    fn list_sessions(
        &self,
        domain: SessionDomain,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        if domain != self.config.domain {
            return Err(CatalogError::DomainMismatch {
                expected: self.config.domain,
                actual: domain,
            });
        }
        self.catalog.list(&query)
    }

    fn get_session_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, CatalogError> {
        if session_id.domain() != self.config.domain {
            return Err(CatalogError::DomainMismatch {
                expected: self.config.domain,
                actual: session_id.domain(),
            });
        }
        self.catalog.get(session_id)
    }
}

impl ProjectSessionStore for LocalSessionStore {
    fn list_project_sessions(
        &self,
        project_id: &str,
        mut query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        if self.config.domain != SessionDomain::Agent {
            return Err(CatalogError::DomainMismatch {
                expected: self.config.domain,
                actual: SessionDomain::Agent,
            });
        }
        query.project_id = Some(project_id.to_string());
        self.catalog.list(&query)
    }

    fn load_project_session(
        &self,
        project_id: &str,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError> {
        if session_id.domain() != SessionDomain::Agent {
            return Err(SessionError::DomainMismatch {
                header: SessionDomain::Agent,
                id: session_id.domain(),
            });
        }
        let (header, entries) = self.load_header_and_entries_for_session(session_id)?;
        let actual = header
            .project
            .as_ref()
            .ok_or(SessionError::AgentMissingProject)?;
        if actual.project_id != project_id {
            return Err(SessionError::ProjectMismatch {
                session_id: session_id.clone(),
                expected: project_id.to_string(),
                actual: actual.project_id.clone(),
            });
        }
        resolve_session(&entries, leaf)
    }

    fn get_project_identity(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectIdentity>, CatalogError> {
        if self.config.domain != SessionDomain::Agent {
            return Err(CatalogError::DomainMismatch {
                expected: SessionDomain::Agent,
                actual: self.config.domain,
            });
        }
        self.catalog.get_project_identity(project_id)
    }

    fn list_projects(
        &self,
        query: super::ProjectCatalogQuery,
    ) -> Result<super::ProjectCatalogPage, CatalogError> {
        if self.config.domain != SessionDomain::Agent {
            return Err(CatalogError::DomainMismatch {
                expected: SessionDomain::Agent,
                actual: self.config.domain,
            });
        }
        self.catalog.list_projects(query)
    }
}

impl ChatMessageReferenceStore for LocalSessionStore {
    fn search_chat_messages(
        &self,
        query: ChatMessageSearchQuery,
    ) -> Result<ChatMessageSearchPage, ChatReferenceError> {
        if self.config.domain != SessionDomain::Chat {
            return Err(ChatReferenceError::Catalog(CatalogError::DomainMismatch {
                expected: SessionDomain::Chat,
                actual: self.config.domain,
            }));
        }
        let limit = query.bounded_limit();
        let folded_query = query.text.to_lowercase();
        let mut rows = self
            .catalog
            .search_message_nodes(&folded_query, query.cursor.as_ref(), limit)
            .map_err(ChatReferenceError::Catalog)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = has_more
            .then(|| {
                rows.last().map(|row| ChatMessageSearchCursor {
                    timestamp: row.timestamp,
                    session_id: row.session_id.clone(),
                    entry_id: row.entry_id.clone(),
                })
            })
            .flatten();
        let messages = rows
            .into_iter()
            .map(|row| {
                preview_from_node(
                    row.session_id,
                    row.entry_id,
                    row.timestamp,
                    row.session_title,
                    row.session_created_at,
                    row.role,
                    row.preview,
                )
            })
            .collect();
        Ok(ChatMessageSearchPage {
            messages,
            next_cursor,
        })
    }

    fn read_chat_message(
        &self,
        reference: &ChatMessageRef,
    ) -> Result<ChatMessageRead, ChatReferenceError> {
        validate_reference(reference)?;
        if self.config.domain != SessionDomain::Chat {
            return Err(ChatReferenceError::Catalog(CatalogError::DomainMismatch {
                expected: SessionDomain::Chat,
                actual: self.config.domain,
            }));
        }
        let summary = self
            .catalog
            .get(&reference.session_id)
            .map_err(ChatReferenceError::Catalog)?
            .ok_or_else(|| unavailable(reference, ChatMessageUnavailableReason::SessionDeleted))?;
        // Exact reference reads re-open the JSONL source of truth, but the
        // catalog's path text is not authority to choose that source.
        let path = self.source_path_for_summary(&summary);
        let path = match source::authorize_existing_source(&self.source_boundary, &path) {
            Ok(path) => path,
            // Authorization runs before the JSONL loader so it can reject
            // symlinks. Preserve the public deletion semantic when the only
            // failure is that the identity-derived source no longer exists.
            Err(LocalStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(unavailable(
                    reference,
                    ChatMessageUnavailableReason::SessionDeleted,
                ));
            }
            Err(_) => {
                return Err(unavailable(
                    reference,
                    ChatMessageUnavailableReason::SourceCorrupt,
                ));
            }
        };
        let loaded = JsonlLoader::load(&path).map_err(|error| match error {
            SessionError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
                unavailable(reference, ChatMessageUnavailableReason::SessionDeleted)
            }
            _ => unavailable(reference, ChatMessageUnavailableReason::SourceCorrupt),
        })?;
        if !loaded.diagnostics.is_empty() || loaded.truncated_tail {
            return Err(unavailable(
                reference,
                ChatMessageUnavailableReason::SourceCorrupt,
            ));
        }
        match loaded.header() {
            Ok(header) if header.session_id == reference.session_id => {}
            _ => {
                return Err(unavailable(
                    reference,
                    ChatMessageUnavailableReason::SourceCorrupt,
                ));
            }
        }
        let active = resolve_session(&loaded.entries, None)
            .map_err(|_| unavailable(reference, ChatMessageUnavailableReason::SourceCorrupt))?;
        if !active.path.iter().any(|id| id == &reference.entry_id) {
            return Err(unavailable(
                reference,
                ChatMessageUnavailableReason::MessageDeleted,
            ));
        }
        let entry = loaded
            .entries
            .iter()
            .find(|entry| entry.id == reference.entry_id)
            .ok_or_else(|| unavailable(reference, ChatMessageUnavailableReason::MessageDeleted))?;
        message_from_entry(reference, &summary, entry)
    }
}

/// Resolve the active entry path from catalog index rows without
/// deserializing message bodies. `records` must be in durable append order.
/// `Leaf` rows carry no target column, so their small fact line is read from
/// the source on demand (design: keep the PRD R1 column set unchanged).
fn resolve_entry_index_path(
    records: &[PathEntryRecord],
    source_path: &Path,
    requested_leaf: Option<&EntryId>,
    session_id: &SessionId,
) -> Result<Vec<PathEntryRecord>, SessionError> {
    let by_id: HashMap<&EntryId, &PathEntryRecord> = records
        .iter()
        .map(|record| (&record.entry_id, record))
        .collect();
    let effective_leaf = match requested_leaf {
        Some(leaf_id) => leaf_id.clone(),
        None => {
            let last = records
                .last()
                .ok_or_else(|| SessionError::EntryIndexMissing(session_id.clone()))?;
            match last.kind {
                EntryKindTag::Leaf => leaf_target_at(source_path, last, session_id)?
                    .unwrap_or_else(|| last.entry_id.clone()),
                _ => last.entry_id.clone(),
            }
        }
    };
    let leaf_id = match by_id.get(&effective_leaf) {
        Some(record) if record.kind == EntryKindTag::Leaf => {
            leaf_target_at(source_path, record, session_id)?.unwrap_or(effective_leaf)
        }
        Some(_) => effective_leaf,
        None => return Err(SessionError::LeafNotFound(effective_leaf)),
    };
    let mut path = Vec::new();
    let mut current = Some(leaf_id);
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            return Err(SessionError::CycleDetected);
        }
        let record = by_id
            .get(&id)
            .copied()
            .ok_or_else(|| SessionError::LeafNotFound(id.clone()))?;
        current = record.parent_id.clone();
        path.push(record.clone());
    }
    path.reverse();
    match path.first() {
        Some(first) if first.kind == EntryKindTag::Header => Ok(path),
        _ => Err(SessionError::MissingHeader),
    }
}

/// Read one exact JSONL line by byte range. Reading through `take` keeps the
/// allocation bounded by the bytes that actually exist, so a corrupt catalog
/// length can never request an unbounded buffer.
fn read_entry_line(
    file: &mut fs::File,
    byte_offset: u64,
    byte_len: u64,
) -> Result<String, SessionError> {
    file.seek(SeekFrom::Start(byte_offset))
        .map_err(SessionError::io)?;
    let mut text = String::new();
    file.take(byte_len)
        .read_to_string(&mut text)
        .map_err(SessionError::io)?;
    if text.len() as u64 != byte_len {
        return Err(SessionError::io(std::io::Error::other(
            "session entry index range is not fully readable",
        )));
    }
    Ok(text)
}

/// Resolve `Leaf.target_id` for one index row through a targeted read of the
/// original fact line. Any mismatch between the line and the index is a typed
/// index error, so the caller falls back instead of trusting the projection.
fn leaf_target_at(
    source_path: &Path,
    record: &PathEntryRecord,
    session_id: &SessionId,
) -> Result<Option<EntryId>, SessionError> {
    let mut file = fs::File::open(source_path).map_err(SessionError::io)?;
    let mismatch = || SessionError::EntryIndexMismatch {
        session_id: session_id.clone(),
        entry_id: record.entry_id.clone(),
    };
    let text = read_entry_line(
        &mut file,
        record.byte_offset.unwrap_or(0),
        record.byte_len.unwrap_or(0),
    )
    .map_err(|_| mismatch())?;
    let entry: SessionEntry = serde_json::from_str(&text).map_err(|_| mismatch())?;
    match entry.kind {
        SessionEntryKind::Leaf(leaf) if entry.id == record.entry_id => Ok(leaf.target_id),
        _ => Err(mismatch()),
    }
}
