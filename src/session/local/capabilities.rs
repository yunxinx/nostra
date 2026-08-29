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
            if let Ok(reloaded) = Self::reload_entries(&source_boundary, &handle.path)
                && let Ok(projection) = SessionProjection::from_entries(&handle.header, &reloaded)
            {
                handle.entries = reloaded;
                handle.projection = projection;
                handle.source_stamp = source_stamp(&handle.path);
            }
            let result = if pending_remains {
                catalog.upsert_projection_with_intents(
                    &handle.header,
                    &handle.projection,
                    &handle.path,
                    &[],
                )
            } else {
                // Deterministic validation rejection wrote no fact, so this
                // source-derived projection completes the operation's intent.
                catalog.upsert_projection_with_intents(
                    &handle.header,
                    &handle.projection,
                    &handle.path,
                    &handle.projection_intents,
                )
            };
            handle.catalog_dirty = pending_remains || result.is_err();
            if result.is_ok() && !pending_remains {
                handle.projection_intents.clear();
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
        handle.entries =
            Self::reload_entries(&source_boundary, &handle.path).map_err(session_io_error)?;
        handle.projection = SessionProjection::from_entries(&handle.header, &handle.entries)
            .map_err(session_io_error)?;
        handle.source_stamp = source_stamp(&handle.path);
        let result = catalog.upsert_projection_with_intents(
            &handle.header,
            &handle.projection,
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
