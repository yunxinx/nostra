use super::*;

impl LocalSessionStore {
    pub fn delete_session(&mut self, session_id: &SessionId) -> Result<(), LocalStoreError> {
        let _lock = self.acquire_domain_lock()?;
        // Deletion must account for every self-consistent source that can
        // resurrect this identity during repair. Checking only the active
        // handle or catalog row would leave a second canonical source behind.
        let source_candidate = self.find_unique_source_path(session_id)?;
        let summary = self.catalog.get(session_id)?;
        let handle = self.handles.get(session_id).map(|handle| {
            (
                handle.header.clone(),
                handle.path.clone(),
                handle.projection_intents.clone(),
            )
        });
        let (path, mut projection_intents) = match (handle, summary.as_ref()) {
            (None, None) => {
                let Some(path) = source_candidate else {
                    return Ok(());
                };
                let loaded = JsonlLoader::load(&path)?;
                let header = loaded.header()?.clone();
                self.validate_header(session_id, &header)?;
                let expected = self.source_path_for_header(&header);
                if path != expected {
                    return Err(LocalStoreError::UnsafeSourcePath(path));
                }
                (expected, Vec::new())
            }
            (Some((header, path, intents)), summary) => {
                let expected = self.source_path_for_header(&header);
                if path != expected {
                    return Err(LocalStoreError::UnsafeSourcePath(path));
                }
                if let Some(observed) = source_candidate
                    && observed != expected
                {
                    return Err(LocalStoreError::UnsafeSourcePath(observed));
                }
                if let Some(summary) = summary {
                    let catalog_expected = self.source_path_for_summary(summary);
                    if summary.jsonl_path != catalog_expected || catalog_expected != expected {
                        return Err(LocalStoreError::UnsafeSourcePath(
                            summary.jsonl_path.clone(),
                        ));
                    }
                }
                (expected, intents)
            }
            (None, Some(summary)) => {
                let catalog_expected = self.source_path_for_summary(summary);
                if summary.jsonl_path != catalog_expected {
                    return Err(LocalStoreError::UnsafeSourcePath(
                        summary.jsonl_path.clone(),
                    ));
                }
                let path = if let Some(observed) = source_candidate {
                    match JsonlLoader::load(&observed).and_then(|loaded| loaded.header().cloned()) {
                        Ok(header) => {
                            self.validate_header(session_id, &header)?;
                            let authoritative = self.source_path_for_header(&header);
                            if observed != authoritative {
                                return Err(LocalStoreError::UnsafeSourcePath(observed));
                            }
                            authoritative
                        }
                        Err(_) if observed == catalog_expected => {
                            // Permanent deletion is authorized by the unique,
                            // canonical in-root path. Requiring a readable
                            // transcript here would make a corrupt conversation
                            // impossible for its owner to remove.
                            observed
                        }
                        Err(error) => return Err(LocalStoreError::Session(error)),
                    }
                } else {
                    // SQLite identity fields are only a hint when no handle is
                    // open. Prefer the validated JSONL header so a consistent
                    // catalog tamper cannot make permanent deletion unlink a
                    // fabricated missing path and leave the real source to be
                    // resurrected by repair.
                    catalog_expected
                };
                (path, Vec::new())
            }
        };
        let (path, durability_parent) = self.contained_delete_target(&path)?;
        // Record the recovery obligation before removing the authoritative
        // file. A crash after unlinking can then prune the stale catalog row
        // on the next open instead of leaving an unreachable conversation.
        projection_intents.push(self.catalog.begin_projection_intent(session_id)?);
        self.handles.remove(session_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalStoreError::Io(error)),
        }
        // NotFound may be a retry after an earlier unlink whose directory sync
        // failed. Re-run the barrier before clearing the catalog obligation;
        // otherwise a crash could resurrect the source without a discoverable
        // row or repair intent.
        source::sync_directory(&durability_parent)?;
        #[cfg(test)]
        if std::mem::take(&mut self.faults.after_delete_commit) {
            return Err(LocalStoreError::Io(std::io::Error::other(
                "injected interruption after session source deletion",
            )));
        }
        self.catalog
            .delete_session_with_intents(session_id, &projection_intents)?;
        Ok(())
    }

    fn find_unique_source_path(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, LocalStoreError> {
        source::authorize_sessions_root(&self.source_boundary)?;
        let suffix = format!("_{session_id}.jsonl");
        let mut found = None;
        for path in collect_jsonl_paths(&self.config.sessions_root())? {
            let matches_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix));
            if !matches_name {
                continue;
            }
            let path = source::authorize_existing_source(&self.source_boundary, &path)?;
            if found.is_some() {
                return Err(LocalStoreError::AmbiguousSessionSource(session_id.clone()));
            }
            found = Some(path);
        }
        Ok(found)
    }

    fn contained_delete_target(&self, path: &Path) -> Result<(PathBuf, PathBuf), LocalStoreError> {
        source::authorize_delete_target(&self.source_boundary, path)
            .map(source::AuthorizedDeleteTarget::into_parts)
    }
}
