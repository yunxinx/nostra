use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt as _;

use thiserror::Error;

use crate::paths;

use super::{
    CatalogError, ChatMessageRead, ChatMessageReferenceStore, ChatMessageSearchCursor,
    ChatMessageSearchPage, ChatMessageSearchQuery, ChatReferenceError, EntryId, JsonlLoader,
    JsonlRecorder, ProjectCatalogPage, ProjectCatalogQuery, ProjectIdentity, ProjectSessionStore,
    ResolvedSessionState, SessionBranchPreview, SessionBranchTreeSnapshot, SessionCatalogStore,
    SessionDomain, SessionEntry, SessionEntryKind, SessionError, SessionFlushStore, SessionHeader,
    SessionId, SessionLifecycleStore, SessionReadStore, SessionSummary, SessionTreeSnapshot,
    SessionTreeStore,
    catalog::{
        Catalog, CatalogPage, CatalogQuery, CatalogRepairProjection, ProjectionIntent,
        RepairReport, SessionProjection,
    },
    reference::{
        ChatMessageUnavailableReason, message_from_entry, preview_from_node, unavailable,
        validate_reference,
    },
    resolve_session, session_branch_preview, session_branch_tree_snapshot, session_tree_snapshot,
};

mod capabilities;
mod deletion;
mod lifecycle;
mod repair;
mod source;

#[cfg(test)]
mod tests;

#[derive(Debug, Error)]
pub enum LocalStoreError {
    #[error("session data root is unavailable")]
    DataRootUnavailable,
    #[error("session store domain mismatch: expected `{expected}`, got `{actual}`")]
    DomainMismatch {
        expected: SessionDomain,
        actual: SessionDomain,
    },
    #[error("session `{0}` does not exist in the local store")]
    SessionNotFound(SessionId),
    #[error("multiple source files claim session `{0}`")]
    AmbiguousSessionSource(SessionId),
    #[error("session source path is outside the authorized store boundary: {0}")]
    UnsafeSourcePath(PathBuf),
    #[error("session operation failed: {0}")]
    Session(#[from] SessionError),
    #[error("session catalog operation failed: {0}")]
    Catalog(#[from] CatalogError),
    #[error("failed to enumerate session files: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out waiting for the domain persistence lock")]
    OperationLockTimeout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalStoreConfig {
    pub data_root: PathBuf,
    pub domain: SessionDomain,
    pub max_open_handles: usize,
}

impl LocalStoreConfig {
    #[must_use]
    pub fn new(data_root: impl Into<PathBuf>, domain: SessionDomain) -> Self {
        Self {
            data_root: data_root.into(),
            domain,
            max_open_handles: 16,
        }
    }

    pub fn default_for(domain: SessionDomain) -> Result<Self, LocalStoreError> {
        let root = paths::nostra_config_dir().ok_or(LocalStoreError::DataRootUnavailable)?;
        Ok(Self::new(root, domain))
    }

    #[must_use]
    pub fn with_max_open_handles(mut self, max_open_handles: usize) -> Self {
        self.max_open_handles = max_open_handles.max(1);
        self
    }

    #[must_use]
    pub fn storage_root(&self) -> PathBuf {
        self.data_root.join(self.domain.prefix())
    }

    #[must_use]
    pub fn sessions_root(&self) -> PathBuf {
        self.storage_root().join("sessions")
    }

    fn staging_root(&self) -> PathBuf {
        self.storage_root().join("staging")
    }

    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.storage_root().join("index.sqlite")
    }
}

struct LocalHandle {
    header: SessionHeader,
    path: PathBuf,
    recorder: JsonlRecorder,
    entries: Vec<SessionEntry>,
    projection: SessionProjection,
    /// A JSONL commit can succeed while its SQLite transaction fails. The
    /// next write must replace the entire disposable projection before using
    /// incremental node insertion again.
    catalog_dirty: bool,
    /// Exact write-ahead markers owned by this handle. A successful source-
    /// derived projection clears only these keys in the same SQLite commit;
    /// concurrent processes retain their independent recovery obligations.
    projection_intents: Vec<ProjectionIntent>,
    source_stamp: Option<source::SourceStamp>,
    last_used: u64,
}

const DOMAIN_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const DOMAIN_LOCK_RETRY: Duration = Duration::from_millis(10);

struct DomainLock(File);

impl Drop for DomainLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub struct LocalSessionStore {
    config: LocalStoreConfig,
    source_boundary: source::SourceBoundary,
    staging_boundary: source::SourceBoundary,
    catalog: Catalog,
    handles: HashMap<SessionId, LocalHandle>,
    access_counter: u64,
    #[cfg(test)]
    faults: LocalStoreTestFaults,
}

impl Drop for LocalSessionStore {
    fn drop(&mut self) {
        for handle in self.handles.values_mut() {
            if handle.recorder.has_pending()
                && source::authorize_retained_source(
                    &self.source_boundary,
                    &handle.path,
                    handle.source_stamp.as_ref(),
                )
                .is_err()
            {
                handle.recorder.abandon_pending_after_authority_loss();
            }
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct LocalStoreTestFaults {
    after_create_stage_crash: bool,
    after_create_intent: bool,
    after_create_publish: bool,
    after_append_commit: bool,
    after_leaf_commit: bool,
    after_delete_commit: bool,
}

impl LocalSessionStore {
    pub fn open(config: LocalStoreConfig) -> Result<Self, LocalStoreError> {
        let sessions_root = config.sessions_root();
        source::prepare_durable_directory_chain(&sessions_root)?;
        let source_boundary = source::SourceBoundary::open(sessions_root)?;
        let staging_root = config.staging_root();
        source::prepare_durable_directory_chain(&staging_root)?;
        let staging_boundary = source::SourceBoundary::open(staging_root)?;
        {
            // Cleanup is independent from the disposable SQLite projection.
            // Run it first so a broken catalog cannot retain plaintext left by
            // a crashed prepublication create.
            let _lock = acquire_domain_lock_for_config(&config)?;
            source::cleanup_abandoned_create_stages(&staging_boundary)?;
        }
        let catalog = Catalog::open(config.index_path(), config.domain)?;
        Ok(Self {
            config,
            source_boundary,
            staging_boundary,
            catalog,
            handles: HashMap::new(),
            access_counter: 0,
            #[cfg(test)]
            faults: LocalStoreTestFaults::default(),
        })
    }

    pub fn open_default(domain: SessionDomain) -> Result<Self, LocalStoreError> {
        Self::open(LocalStoreConfig::default_for(domain)?)
    }

    pub fn repair_if_needed(&mut self) -> Result<Option<RepairReport>, LocalStoreError> {
        self.catalog.refresh_after_external_replacement()?;
        if !self.catalog.needs_repair() {
            return Ok(None);
        }
        self.repair().map(Some)
    }

    #[must_use]
    pub fn config(&self) -> &LocalStoreConfig {
        &self.config
    }

    #[must_use]
    pub fn catalog_path(&self) -> &Path {
        self.catalog.path()
    }

    #[must_use]
    pub fn open_handle_count(&self) -> usize {
        self.handles.len()
    }

    #[cfg(test)]
    fn projection_write_counts(&self) -> (u64, u64) {
        self.catalog.projection_write_counts()
    }

    #[cfg(test)]
    fn catalog_synchronous_level_for_test(&self) -> Result<i64, CatalogError> {
        self.catalog.synchronous_level()
    }

    pub fn list(&self, query: CatalogQuery) -> Result<CatalogPage, LocalStoreError> {
        Ok(self.catalog.list(&query)?)
    }

    /// Read one catalog row without opening or scanning its JSONL source.
    pub fn get_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, LocalStoreError> {
        Ok(self.catalog.get(session_id)?)
    }

    fn source_path_for_summary(&self, summary: &SessionSummary) -> PathBuf {
        let mut directory = self.config.sessions_root();
        if let Some(project) = &summary.project {
            directory = directory.join(format!("--{}--", project.project_id));
        }
        directory.join(format!(
            "{}_{}.jsonl",
            summary.created_at.max(0),
            summary.session_id
        ))
    }

    fn source_candidate_id(&self, path: &Path) -> Option<SessionId> {
        let session_id = session_id_from_source_filename(path)?;
        if session_id.domain() != self.config.domain {
            return None;
        }
        let parent = path.parent()?;
        match self.config.domain {
            SessionDomain::Chat => (parent == self.config.sessions_root()).then_some(session_id),
            SessionDomain::Agent => {
                let bucket = parent.file_name()?.to_str()?;
                let project_id = bucket.strip_prefix("--")?.strip_suffix("--")?;
                let valid_bucket = project_id.starts_with("project-")
                    && parent.parent() == Some(self.config.sessions_root().as_path());
                valid_bucket.then_some(session_id)
            }
        }
    }

    fn acquire_domain_lock(&self) -> Result<DomainLock, LocalStoreError> {
        acquire_domain_lock_for_config(&self.config)
    }

    fn ensure_handle(&mut self, session_id: &SessionId) -> Result<(), LocalStoreError> {
        if let Some((path, expected_stamp, has_pending)) =
            self.handles.get(session_id).map(|handle| {
                (
                    handle.path.clone(),
                    handle.source_stamp.clone(),
                    handle.recorder.has_pending(),
                )
            })
        {
            // A retained recorder owns an open file descriptor, but the path
            // must remain authorized for every new mutation. Otherwise moving
            // that inode outside the store and replacing its name with a
            // symlink would let the old descriptor bypass the path boundary.
            source::authorize_existing_source(&self.source_boundary, &path)?;
            if !source::retained_source_identity_matches(&path, expected_stamp.as_ref()) {
                if has_pending {
                    if let Some(mut handle) = self.handles.remove(session_id) {
                        handle.recorder.abandon_pending_after_authority_loss();
                    }
                    return Err(LocalStoreError::UnsafeSourcePath(path));
                }
                // With no exact retry batch, the replacement cannot receive
                // stale facts. Drop the old descriptor and validate the current
                // canonical source from its header before the next mutation.
                self.handles.remove(session_id);
            }
        }
        if !self.handles.contains_key(session_id) {
            let summary = self
                .catalog
                .get(session_id)?
                .ok_or_else(|| LocalStoreError::SessionNotFound(session_id.clone()))?;
            // `jsonl_path` is disposable index metadata, not a filesystem
            // capability. Always derive the source location from validated
            // session identity fields before opening a recorder.
            let path = self.source_path_for_summary(&summary);
            let path = source::authorize_existing_source(&self.source_boundary, &path)?;
            let loaded = JsonlLoader::load(&path)?;
            let header = loaded.header()?.clone();
            self.validate_header(session_id, &header)?;
            let recorder = JsonlRecorder::open(&path)?;
            let projection = SessionProjection::from_entries(&header, &loaded.entries)
                .map_err(session_io_error)?;
            let source_stamp = source_stamp(&path);
            self.handles.insert(
                session_id.clone(),
                LocalHandle {
                    header,
                    path,
                    recorder,
                    entries: loaded.entries,
                    projection,
                    // A handle may be opened after an external writer changed
                    // the source while this process had no handle. Reconcile
                    // its disposable projection on the first mutation.
                    catalog_dirty: true,
                    projection_intents: Vec::new(),
                    source_stamp,
                    last_used: 0,
                },
            );
        }
        // An external writer may have advanced the source since this handle
        // was opened. Flush any local pending batch, then reopen from the
        // source so subsequent writes use a fresh parent graph.
        let stale = self
            .handles
            .get(session_id)
            .is_some_and(|handle| source_stamp(&handle.path) != handle.source_stamp);
        if stale {
            if self
                .handles
                .get(session_id)
                .is_some_and(|handle| handle.recorder.has_pending())
            {
                let handle = self
                    .handles
                    .get_mut(session_id)
                    .ok_or_else(|| LocalStoreError::SessionNotFound(session_id.clone()))?;
                // Finish the local queue before replacing the writer with a
                // snapshot that includes an external append. This keeps the
                // recorder's ordering contract while avoiding a stale parent
                // graph on the next write.
                handle.recorder.flush()?;
            }
            self.handles.remove(session_id);
            return self.ensure_handle(session_id);
        }
        self.access_counter = self.access_counter.saturating_add(1);
        if let Some(handle) = self.handles.get_mut(session_id) {
            handle.last_used = self.access_counter;
        }
        // The caller is about to use this handle. Under pressure, evicting
        // the only clean candidate here could remove the very handle we just
        // ensured when every other handle still carries a dirty projection.
        self.evict_handles_except(Some(session_id));
        Ok(())
    }

    fn evict_handles(&mut self) {
        self.evict_handles_except(None);
    }

    fn evict_handles_except(&mut self, protected: Option<&SessionId>) {
        while self.handles.len() > self.config.max_open_handles.max(1) {
            let candidate = self
                .handles
                .iter()
                .filter(|(_, handle)| !handle.recorder.has_pending() && !handle.catalog_dirty)
                .filter(|(id, _)| protected != Some(*id))
                .min_by_key(|(_, handle)| handle.last_used)
                .map(|(id, _)| id.clone());
            let Some(candidate) = candidate else {
                break;
            };
            self.handles.remove(&candidate);
        }
    }

    fn validate_header(
        &self,
        session_id: &SessionId,
        header: &SessionHeader,
    ) -> Result<(), LocalStoreError> {
        if header.domain != self.config.domain {
            return Err(LocalStoreError::DomainMismatch {
                expected: self.config.domain,
                actual: header.domain,
            });
        }
        if header.session_id != session_id.clone() {
            return Err(LocalStoreError::Session(SessionError::SessionIdMismatch {
                expected: session_id.clone(),
                actual: header.session_id.clone(),
            }));
        }
        Ok(())
    }

    fn reload_entries(
        source_boundary: &source::SourceBoundary,
        path: &Path,
    ) -> Result<Vec<SessionEntry>, LocalStoreError> {
        source::authorize_existing_source(source_boundary, path)?;
        Ok(JsonlLoader::load(path)?.entries)
    }

    fn load_header_and_entries_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(SessionHeader, Vec<SessionEntry>), SessionError> {
        if let Some(handle) = self.handles.get(session_id) {
            source::authorize_existing_source(&self.source_boundary, &handle.path)
                .map_err(local_store_session_error)?;
            if source_stamp(&handle.path) == handle.source_stamp {
                return Ok((handle.header.clone(), handle.entries.clone()));
            }
        }
        let summary = self
            .catalog
            .get(session_id)
            .map_err(session_io_error)?
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        // Reads use the same authorized path derivation as mutation. A stale
        // or tampered SQLite path can never redirect transcript access.
        let path = self.source_path_for_summary(&summary);
        let path = source::authorize_existing_source(&self.source_boundary, &path)
            .map_err(local_store_session_error)?;
        let loaded = JsonlLoader::load(path)?;
        let header = loaded.header()?;
        if header.domain != self.config.domain {
            return Err(SessionError::DomainMismatch {
                header: self.config.domain,
                id: header.domain,
            });
        }
        if header.session_id != *session_id {
            return Err(SessionError::SessionIdMismatch {
                expected: session_id.clone(),
                actual: header.session_id.clone(),
            });
        }
        Ok((header.clone(), loaded.entries))
    }

    fn load_entries_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        Ok(self.load_header_and_entries_for_session(session_id)?.1)
    }

    fn flush_handle(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        self.ensure_handle(session_id)
            .map_err(local_store_session_error)?;
        let source_boundary = self.source_boundary.clone();
        let catalog = &mut self.catalog;
        let handle = self
            .handles
            .get_mut(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        handle.recorder.flush()?;
        handle.entries = Self::reload_entries(&source_boundary, &handle.path)
            .map_err(local_store_session_error)?;
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

    fn source_path_for_header(&self, header: &SessionHeader) -> PathBuf {
        let mut directory = self.config.sessions_root();
        if let Some(project) = &header.project {
            directory = directory.join(format!("--{}--", project.project_id));
        }
        directory.join(format!(
            "{}_{}.jsonl",
            header.created_at.max(0),
            header.session_id
        ))
    }

    #[cfg(test)]
    fn fail_next_append_for_test(&mut self, session_id: &SessionId) {
        if let Some(handle) = self.handles.get(session_id) {
            let _ = handle.recorder.fail_next_append_for_test();
        }
    }

    #[cfg(test)]
    fn fail_next_append_after_write_for_test(&mut self, session_id: &SessionId) {
        if let Some(handle) = self.handles.get(session_id) {
            let _ = handle.recorder.fail_next_append_after_write_for_test();
        }
    }

    #[cfg(test)]
    fn fail_next_set_leaf_after_write_for_test(&mut self, session_id: &SessionId) {
        if let Some(handle) = self.handles.get(session_id) {
            let _ = handle.recorder.fail_next_set_leaf_after_write_for_test();
        }
    }

    #[cfg(test)]
    fn crash_after_create_stage_for_test(&mut self) {
        self.faults.after_create_stage_crash = true;
    }

    #[cfg(test)]
    fn fail_after_create_publish_for_test(&mut self) {
        self.faults.after_create_publish = true;
    }

    #[cfg(test)]
    fn fail_after_create_intent_for_test(&mut self) {
        self.faults.after_create_intent = true;
    }

    #[cfg(test)]
    fn fail_after_append_commit_for_test(&mut self) {
        self.faults.after_append_commit = true;
    }

    #[cfg(test)]
    fn fail_after_leaf_commit_for_test(&mut self) {
        self.faults.after_leaf_commit = true;
    }

    #[cfg(test)]
    fn fail_after_delete_commit_for_test(&mut self) {
        self.faults.after_delete_commit = true;
    }

    #[cfg(test)]
    fn fail_next_directory_sync_for_test(&self, path: PathBuf) {
        source::fail_next_directory_sync_for_test(path);
    }
}

fn acquire_domain_lock_for_config(
    config: &LocalStoreConfig,
) -> Result<DomainLock, LocalStoreError> {
    let path = config.storage_root().join("store.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let started = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(DomainLock(file)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= DOMAIN_LOCK_TIMEOUT {
                    return Err(LocalStoreError::OperationLockTimeout);
                }
                thread::sleep(DOMAIN_LOCK_RETRY);
            }
            Err(error) => return Err(LocalStoreError::Io(error)),
        }
    }
}

fn collect_jsonl_paths(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut pending = vec![root.to_path_buf()];
    let mut result = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut items = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        items.sort_by_key(std::fs::DirEntry::file_name);
        for item in items {
            let file_type = item.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let path = item.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
            {
                result.push(path);
            }
        }
    }
    result.sort();
    Ok(result)
}

fn session_id_from_source_filename(path: &Path) -> Option<SessionId> {
    let stem = path.file_stem()?.to_str()?;
    let (created_at, session_id) = stem.rsplit_once('_')?;
    let created_at = created_at.parse::<i64>().ok()?;
    if created_at < 0 {
        return None;
    }
    session_id.parse().ok()
}

fn session_io_error<E>(error: E) -> SessionError
where
    E: std::error::Error + Send + Sync + 'static,
{
    SessionError::Io {
        // Keep the typed catalog/store error as the io::Error source instead
        // of flattening it to a diagnostic string at the trait boundary.
        source: std::io::Error::other(error),
    }
}

fn local_store_session_error(error: LocalStoreError) -> SessionError {
    match error {
        LocalStoreError::Session(error) => error,
        other => session_io_error(other),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn source_stamp(path: &Path) -> Option<source::SourceStamp> {
    source::source_stamp(path)
}
