use super::super::JsonlWriter;
use super::*;

impl SessionLifecycleStore for LocalSessionStore {
    fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError> {
        self.create_session_with_entries(header, Vec::new())
            .map(|(session_id, _)| session_id)
    }

    fn create_session_with_entries(
        &mut self,
        header: SessionHeader,
        entries: Vec<SessionEntryKind>,
    ) -> Result<(SessionId, Vec<EntryId>), SessionError> {
        if header.domain != self.config.domain {
            return Err(SessionError::DomainMismatch {
                header: self.config.domain,
                id: header.domain,
            });
        }
        header.validate()?;
        let _lock = self
            .acquire_domain_lock()
            .map_err(local_store_session_error)?;
        if self.handles.contains_key(&header.session_id)
            || self
                .catalog
                .get(&header.session_id)
                .map_err(session_io_error)?
                .is_some()
        {
            return Err(SessionError::SessionAlreadyExists(header.session_id));
        }
        let path = self.source_path_for_header(&header);
        let parent = path.parent().ok_or_else(|| {
            SessionError::io(std::io::Error::other(
                "session source path has no parent directory",
            ))
        })?;
        // Agent buckets are direct children of the sessions root. Refuse a
        // pre-existing symlink before any temporary file can carry facts into
        // a directory outside Nostra's storage boundary.
        source::prepare_create_parent(&self.source_boundary, parent)
            .map_err(local_store_session_error)?;
        let temporary = source::create_session_stage(&self.staging_boundary)
            .map_err(local_store_session_error)?;
        let (file, temporary_path) = temporary.into_parts();
        let (mut staged, initial_entries) = JsonlWriter::create_on_file(
            temporary_path.to_path_buf(),
            file,
            header.clone(),
            entries,
        )?;
        staged.flush()?;
        drop(staged);
        #[cfg(test)]
        if std::mem::take(&mut self.faults.after_create_stage_crash) {
            // Simulate an actual process loss: `process::exit` deliberately
            // skips TempPath::drop, leaving the staged plaintext on disk.
            std::process::exit(86);
        }
        // The marker must commit before the complete source becomes visible.
        // If the process stops after publication but before the catalog
        // transaction, reopening can discover and repair that exact gap.
        let projection_intent = self
            .catalog
            .begin_projection_intent(&header.session_id)
            .map_err(session_io_error)?;
        #[cfg(test)]
        if std::mem::take(&mut self.faults.after_create_intent) {
            return Err(SessionError::io(std::io::Error::other(
                "injected interruption after create projection intent",
            )));
        }
        // Publish only a complete header + initial-facts file. A validation or
        // write failure above leaves the temporary path private and removable;
        // the final namespace never exposes a header-only session.
        if let Err(error) = temporary_path.persist_noclobber(&path) {
            // The no-clobber install did not publish this operation's source,
            // so its write-ahead marker no longer represents recoverable work.
            // Clear only this exact intent; unrelated writers keep theirs.
            self.catalog
                .cancel_projection_intent(&projection_intent)
                .map_err(session_io_error)?;
            return Err(SessionError::io(error.error));
        }
        source::sync_directory(parent).map_err(local_store_session_error)?;
        source::sync_staging_directory(&self.staging_boundary)
            .map_err(local_store_session_error)?;
        #[cfg(test)]
        if std::mem::take(&mut self.faults.after_create_publish) {
            return Err(SessionError::io(std::io::Error::other(
                "injected interruption after session source publication",
            )));
        }

        let recorder = JsonlRecorder::open(&path)?;
        let entry_ids = initial_entries
            .iter()
            .skip(1)
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        self.handles.insert(
            header.session_id.clone(),
            LocalHandle {
                header: header.clone(),
                path: path.clone(),
                recorder,
                entries: initial_entries.clone(),
                projection: SessionProjection::from_entries(&header, &initial_entries)
                    .map_err(session_io_error)?,
                catalog_dirty: true,
                projection_intents: vec![projection_intent],
                source_stamp: source_stamp(&path),
                last_used: 0,
            },
        );
        let projection = self
            .handles
            .get(&header.session_id)
            .ok_or_else(|| SessionError::SessionNotFound(header.session_id.clone()))?
            .projection
            .clone();
        let intents = self
            .handles
            .get(&header.session_id)
            .ok_or_else(|| SessionError::SessionNotFound(header.session_id.clone()))?
            .projection_intents
            .clone();
        let result =
            self.catalog
                .upsert_projection_with_intents(&header, &projection, &path, &intents);
        if result.is_ok()
            && let Some(handle) = self.handles.get_mut(&header.session_id)
        {
            handle.catalog_dirty = false;
            handle.projection_intents.clear();
        }
        result.map_err(session_io_error)?;
        self.evict_handles();
        Ok((header.session_id, entry_ids))
    }

    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let _lock = self
            .acquire_domain_lock()
            .map_err(local_store_session_error)?;
        self.ensure_handle(session_id)
            .map_err(local_store_session_error)?;
        let source_boundary = self.source_boundary.clone();
        let requested_count = entries.len();
        let projection_intent = self
            .catalog
            .begin_projection_intent(session_id)
            .map_err(session_io_error)?;
        let catalog = &mut self.catalog;
        let handle = self
            .handles
            .get_mut(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        let projection_was_dirty = handle.catalog_dirty;
        handle.projection_intents.push(projection_intent);
        handle.catalog_dirty = true;
        // The recorder validates the entire request, including any exact
        // pending tail, before retrying or writing bytes. Deterministic graph
        // errors therefore return here without poisoning its retry queue.
        let appended = match handle.recorder.append_batch(entries) {
            Ok(appended) => appended,
            Err(error) => {
                // Retrying an older pending batch can commit it before the
                // current batch fails. Re-read the source on this exceptional
                // path so the handle and catalog do not omit facts that are
                // already durable.
                let pending_remains = handle.recorder.has_pending();
                handle.catalog_dirty = true;
                if let Ok(reloaded) = Self::reload_entries(&source_boundary, &handle.path)
                    && let Ok(projection) =
                        SessionProjection::from_entries(&handle.header, &reloaded)
                {
                    handle.entries = reloaded;
                    handle.projection = projection;
                    handle.source_stamp = source_stamp(&handle.path);
                    let result = if pending_remains {
                        // Recorder Drop performs an ordered best-effort
                        // shutdown and may still make this exact batch durable
                        // after `append` returns. Reconcile the facts currently
                        // visible in JSONL without clearing the persistent
                        // obligation for that later write.
                        catalog.upsert_projection_with_intents(
                            &handle.header,
                            &handle.projection,
                            &handle.path,
                            &[],
                        )
                    } else {
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
                }
                return Err(error);
            }
        };
        let current_batch_start = appended.len().checked_sub(requested_count).ok_or_else(|| {
            SessionError::io(std::io::Error::other(
                "session recorder returned fewer entries than requested",
            ))
        })?;
        let ids = appended[current_batch_start..]
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        let has_leaf_change = appended
            .iter()
            .any(|entry| matches!(entry.kind, SessionEntryKind::Leaf(_)));
        if current_batch_start == 0 {
            handle.entries.extend(appended.iter().cloned());
        } else {
            // A recovered batch may already be present in the handle because
            // the earlier caller lost only the write result and its error path
            // reloaded JSONL. Reload this exceptional path instead of blindly
            // extending with the recorder's recovered prefix and duplicating
            // exact facts in the in-memory graph.
            handle.entries = Self::reload_entries(&source_boundary, &handle.path)
                .map_err(local_store_session_error)?;
        }
        handle.source_stamp = source_stamp(&handle.path);
        #[cfg(test)]
        if std::mem::take(&mut self.faults.after_append_commit) {
            handle.catalog_dirty = true;
            return Err(SessionError::io(std::io::Error::other(
                "injected interruption after session source append",
            )));
        }

        let catalog_result = if projection_was_dirty || has_leaf_change {
            match SessionProjection::from_entries(&handle.header, &handle.entries) {
                Ok(projection) => {
                    handle.projection = projection;
                    catalog.upsert_projection_with_intents(
                        &handle.header,
                        &handle.projection,
                        &handle.path,
                        &handle.projection_intents,
                    )
                }
                Err(error) => Err(error),
            }
        } else {
            match handle.projection.append_entries(&appended) {
                Ok(appended_messages) => catalog.append_projection_with_intents(
                    &handle.header,
                    &handle.projection,
                    &appended_messages,
                    &handle.path,
                    &handle.projection_intents,
                ),
                Err(error) => Err(error),
            }
        };
        handle.catalog_dirty = catalog_result.is_err();
        catalog_result.map_err(session_io_error)?;
        handle.projection_intents.clear();
        Ok(ids)
    }

    fn delete_session(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        LocalSessionStore::delete_session(self, session_id).map_err(local_store_session_error)
    }
}
