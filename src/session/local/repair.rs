use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use super::*;

impl LocalSessionStore {
    pub fn repair(&mut self) -> Result<RepairReport, LocalStoreError> {
        let _lock = self.acquire_domain_lock()?;
        // A recorder may still own an exact batch that is not visible in the
        // source file yet. Reconcile every active writer before a catalog-wide
        // scan; otherwise repair could clear its durable intent and recorder
        // shutdown could append the fact afterward with no recovery marker.
        self.drain_handles_for_repair_locked()?;
        self.repair_locked()
    }

    pub(super) fn repair_locked(&mut self) -> Result<RepairReport, LocalStoreError> {
        // Persist intent before scanning. If enumeration, parsing, or the final
        // transaction fails, the next process still knows the disposable
        // projection must be reconciled from JSONL.
        self.catalog.mark_repair_required()?;
        source::authorize_sessions_root(&self.source_boundary)?;
        let mut report = RepairReport::default();
        let existing_rows = self.catalog.source_rows()?;
        let existing_session_ids = existing_rows
            .iter()
            .map(|summary| summary.session_id.clone())
            .collect::<HashSet<_>>();
        let intent_session_ids = self.catalog.projection_intent_session_ids()?;
        let mut projections = Vec::new();
        let mut observed_sources = HashMap::<SessionId, Vec<PathBuf>>::new();
        let mut source_candidate_ids = HashSet::<SessionId>::new();
        let mut unresolved_session_ids = HashSet::<SessionId>::new();
        let paths = collect_jsonl_paths(&self.config.sessions_root())?;
        for path in paths {
            report.scanned += 1;
            let candidate_session_id = self.source_candidate_id(&path);
            if let Some(session_id) = candidate_session_id.as_ref() {
                // A recognizable final filename is enough to keep an intent.
                // Its contents may be temporarily unreadable, so requiring a
                // parsed header here would misclassify recoverable work as an
                // abandoned pre-publication create.
                source_candidate_ids.insert(session_id.clone());
            }
            // Directory enumeration is only a snapshot. Re-authorize the
            // candidate immediately before reading it so a replaced bucket or
            // source cannot turn repair into an out-of-boundary file read.
            let path = match source::authorize_existing_source(&self.source_boundary, &path) {
                Ok(path) => path,
                Err(error) => {
                    report.issues.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let loaded = match JsonlLoader::scan(&path) {
                Ok(loaded) => loaded,
                Err(error) => {
                    if let Some(session_id) = candidate_session_id.as_ref() {
                        unresolved_session_ids.insert(session_id.clone());
                    }
                    report.issues.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            for diagnostic in &loaded.diagnostics {
                report.issues.push(format!(
                    "{}:{} {}: {}",
                    path.display(),
                    diagnostic.line,
                    diagnostic.kind,
                    diagnostic.message
                ));
            }
            if loaded.truncated_tail {
                report.issues.push(format!(
                    "{}: interrupted trailing JSONL entry",
                    path.display()
                ));
            }
            let header = match loaded.header() {
                Ok(header) if header.domain == self.config.domain => header.clone(),
                Ok(header) => {
                    report.issues.push(format!(
                        "{}: expected domain `{}`, got `{}`",
                        path.display(),
                        self.config.domain,
                        header.domain
                    ));
                    continue;
                }
                Err(error) => {
                    if let Some(session_id) = candidate_session_id.as_ref() {
                        unresolved_session_ids.insert(session_id.clone());
                    }
                    report.issues.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            source_candidate_ids.insert(header.session_id.clone());
            let expected = self.source_path_for_header(&header);
            if path != expected {
                // A valid payload at an arbitrary filename is not a usable
                // session source: normal restore derives this exact path from
                // the header identity and intentionally ignores catalog text.
                // Rejecting it here prevents repair from publishing a list row
                // that no read path can subsequently open.
                report.issues.push(format!(
                    "{}: session source does not match its canonical identity path",
                    path.display()
                ));
                continue;
            }
            observed_sources
                .entry(header.session_id.clone())
                .or_default()
                .push(path.clone());
            let session_id = header.session_id.clone();
            if !loaded.diagnostics.is_empty() || loaded.truncated_tail {
                // The valid header still proves that this identity's source
                // exists. Preserve its last trusted row and retain a precise
                // retry obligation until the complete file can be projected.
                unresolved_session_ids.insert(session_id);
                continue;
            }
            match CatalogRepairProjection::from_entries(header, &loaded.entries, path.clone()) {
                Ok(projection) => projections.push(projection),
                Err(error) => {
                    unresolved_session_ids.insert(session_id);
                    report.issues.push(format!("{}: {error}", path.display()));
                }
            }
        }
        let ambiguous_session_ids = observed_sources
            .iter()
            .filter_map(|(session_id, paths)| (paths.len() > 1).then_some(session_id.clone()))
            .collect::<HashSet<_>>();
        for session_id in &ambiguous_session_ids {
            unresolved_session_ids.insert(session_id.clone());
            projections.retain(|projection| projection.session_id() != session_id);
            report.issues.push(format!(
                "multiple canonical source files claim session `{session_id}`"
            ));
        }
        let mut missing_session_ids = Vec::new();
        let rebuilt_sources = projections
            .iter()
            .map(|projection| {
                (
                    projection.session_id().clone(),
                    projection.jsonl_path().to_path_buf(),
                )
            })
            .collect::<HashMap<_, _>>();
        for summary in existing_rows {
            let expected = self.source_path_for_summary(&summary);
            if summary.jsonl_path != expected {
                report.issues.push(format!(
                    "{}: catalog source path does not match the authorized session path",
                    summary.jsonl_path.display()
                ));
            }
            // The authoritative source for this identity was validated during
            // the same scan. Stale catalog path fields must not make the later
            // missing-source pass delete that row. Still report any catalog
            // identity drift above so repair remains observable instead of
            // silently laundering the untrusted projection metadata.
            if observed_sources.contains_key(&summary.session_id) {
                if let Some(rebuilt_source) = rebuilt_sources.get(&summary.session_id)
                    && summary.jsonl_path != *rebuilt_source
                    && summary.jsonl_path == expected
                {
                    report.issues.push(format!(
                        "{}: catalog identity does not match the authoritative session source",
                        summary.jsonl_path.display()
                    ));
                }
                continue;
            }
            if let Some(rebuilt_source) = rebuilt_sources.get(&summary.session_id) {
                if summary.jsonl_path != *rebuilt_source && summary.jsonl_path == expected {
                    report.issues.push(format!(
                        "{}: catalog identity does not match the authoritative session source",
                        summary.jsonl_path.display()
                    ));
                }
                continue;
            }
            if summary.jsonl_path != expected {
                continue;
            }
            match source::authorize_delete_target(&self.source_boundary, &expected) {
                Ok(source::AuthorizedDeleteTarget::Existing { .. }) => {}
                Ok(target @ source::AuthorizedDeleteTarget::Missing { .. }) => {
                    // A missing source can be the result of an interrupted
                    // permanent delete. Durably confirm the containing
                    // directory entry before removing its last catalog row.
                    source::sync_directory(target.durability_parent())?;
                    missing_session_ids.push(summary.session_id);
                }
                Err(error) => report
                    .issues
                    .push(format!("{}: {error}", expected.display())),
            }
        }
        report.rebuilt = projections.len();
        let intent_session_id_set = intent_session_ids.iter().cloned().collect::<HashSet<_>>();
        let mut durability_parents = Vec::new();
        for projection in &projections {
            if !intent_session_id_set.contains(projection.session_id()) {
                continue;
            }
            // A create intent can survive publication when the directory
            // fsync itself fails. Re-establish that namespace barrier before
            // the catalog transaction clears the intent; otherwise a crash
            // could retain the projection after its source entry disappears.
            let source_path =
                source::authorize_existing_source(&self.source_boundary, projection.jsonl_path())?;
            let parent = source_path.parent().ok_or_else(|| {
                LocalStoreError::Io(std::io::Error::other(format!(
                    "session source has no durability parent: {}",
                    source_path.display()
                )))
            })?;
            durability_parents.push(parent.to_path_buf());
        }
        durability_parents.sort();
        durability_parents.dedup();
        for parent in durability_parents {
            source::sync_directory(&parent)?;
        }
        let mut unresolved_session_ids = unresolved_session_ids.into_iter().collect::<Vec<_>>();
        unresolved_session_ids.sort();
        let abandoned_session_ids = intent_session_ids
            .into_iter()
            .filter(|session_id| {
                !existing_session_ids.contains(session_id)
                    && !source_candidate_ids.contains(session_id)
            })
            .collect::<Vec<_>>();
        // All catalog mutations, including repair completion, share one
        // transaction. A failure cannot expose a mix of old and new rows.
        report.removed = self.catalog.apply_repair(
            &projections,
            &missing_session_ids,
            &unresolved_session_ids,
            &abandoned_session_ids,
            &now_millis().to_string(),
        )?;
        Ok(report)
    }
}
