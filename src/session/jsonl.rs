use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use super::{
    AppendValidationState, DiagnosticKind, EntryId, JsonlDiagnostic, SessionEntry,
    SessionEntryKind, SessionError, SessionHeader, validate_session_entries,
};

/// Parsed session facts and non-fatal diagnostics emitted by the loader.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonlLoad {
    pub entries: Vec<SessionEntry>,
    pub diagnostics: Vec<JsonlDiagnostic>,
    pub truncated_tail: bool,
    pub valid_bytes: u64,
    pub ends_with_newline: bool,
}

impl JsonlLoad {
    pub fn header(&self) -> Result<&SessionHeader, SessionError> {
        validate_session_entries(&self.entries)
    }
}

/// Append-only JSONL reader.
pub struct JsonlLoader;

impl JsonlLoader {
    pub fn load(path: impl AsRef<Path>) -> Result<JsonlLoad, SessionError> {
        let loaded = Self::scan(path)?;
        if let Some(diagnostic) = loaded.diagnostics.first() {
            return Err(SessionError::CorruptLine {
                line: diagnostic.line,
                kind: diagnostic.kind,
                message: diagnostic.message.clone(),
            });
        }
        Ok(loaded)
    }

    /// Scan a source for repair diagnostics without treating a complete
    /// malformed line as a valid omission from authoritative history.
    pub(crate) fn scan(path: impl AsRef<Path>) -> Result<JsonlLoad, SessionError> {
        let file = File::open(path).map_err(SessionError::io)?;
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut diagnostics = Vec::new();
        let mut seen = HashSet::new();
        let mut line_number = 1;
        let mut bytes = Vec::new();
        let mut consumed_bytes = 0_u64;
        let mut valid_bytes = 0_u64;
        let mut ends_with_newline = false;
        let mut truncated_tail = false;

        loop {
            bytes.clear();
            let read = reader
                .read_until(b'\n', &mut bytes)
                .map_err(SessionError::io)?;
            if read == 0 {
                break;
            }
            let line_start = consumed_bytes;
            consumed_bytes = consumed_bytes.saturating_add(read as u64);
            let has_newline = bytes.last() == Some(&b'\n');
            if has_newline {
                bytes.pop();
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
            }
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(_error) if !has_newline => {
                    truncated_tail = true;
                    valid_bytes = line_start;
                    break;
                }
                Err(error) => {
                    diagnostics.push(JsonlDiagnostic {
                        line: line_number,
                        kind: DiagnosticKind::InvalidUtf8,
                        message: error.to_string(),
                    });
                    valid_bytes = consumed_bytes;
                    ends_with_newline = true;
                    line_number += 1;
                    continue;
                }
            };

            match serde_json::from_str::<SessionEntry>(text) {
                Ok(entry) => {
                    if !seen.insert(entry.id.clone()) {
                        return Err(SessionError::DuplicateId(entry.id));
                    }
                    entries.push(entry);
                    valid_bytes = consumed_bytes;
                    ends_with_newline = has_newline;
                }
                Err(_error) if !has_newline => {
                    truncated_tail = true;
                    valid_bytes = line_start;
                    break;
                }
                Err(error) => {
                    diagnostics.push(JsonlDiagnostic {
                        line: line_number,
                        kind: DiagnosticKind::InvalidJson,
                        message: error.to_string(),
                    });
                    valid_bytes = consumed_bytes;
                    ends_with_newline = has_newline;
                }
            }
            line_number += 1;
        }

        // A complete malformed record is the primary corruption boundary.
        // Validating the surviving entries first can turn a broken header into
        // `MissingHeader`, or a skipped middle record into `DanglingParent`,
        // and thereby hide the exact physical line that repair must report.
        // Strict callers reject the diagnostic immediately; explicit repair
        // may inspect the surviving prefix but never projects it as complete.
        if diagnostics.is_empty() {
            validate_session_entries(&entries)?;
        }
        Ok(JsonlLoad {
            entries,
            diagnostics,
            truncated_tail,
            valid_bytes,
            ends_with_newline,
        })
    }

    pub fn load_entries(path: impl AsRef<Path>) -> Result<Vec<SessionEntry>, SessionError> {
        Ok(Self::load(path)?.entries)
    }
}

/// Append-only JSONL writer. The compact validation index is advanced only
/// after a batch reaches stable storage, so failed writes cannot make later
/// validation observe facts that are not durable.
pub struct JsonlWriter {
    path: PathBuf,
    file: File,
    validation: AppendValidationState,
    leaf: EntryId,
    domain: super::SessionDomain,
}

impl JsonlWriter {
    pub fn create(path: impl Into<PathBuf>, header: SessionHeader) -> Result<Self, SessionError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(SessionError::io)?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(SessionError::io)?;
        Self::create_on_file(path, file, header, Vec::new()).map(|(writer, _)| writer)
    }

    pub(crate) fn create_on_file(
        path: PathBuf,
        file: File,
        header: SessionHeader,
        initial: Vec<SessionEntryKind>,
    ) -> Result<(Self, Vec<SessionEntry>), SessionError> {
        header.validate()?;
        let domain = header.domain;
        let header_entry = SessionEntry::header(header);
        let mut writer = Self {
            path,
            file,
            validation: AppendValidationState::default(),
            leaf: header_entry.id.clone(),
            domain,
        };
        writer.write_entries(std::slice::from_ref(&header_entry))?;
        let mut entries = vec![header_entry];
        let initial = writer.prepare_batch_entries(initial)?;
        writer.append_entries(&initial)?;
        entries.extend(initial);
        Ok((writer, entries))
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let loaded = JsonlLoader::load(&path)?;
        let domain = loaded.header()?.domain;
        let leaf = loaded
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                SessionEntryKind::Leaf(leaf) => {
                    leaf.target_id.clone().or_else(|| Some(entry.id.clone()))
                }
                _ => Some(entry.id.clone()),
            })
            .ok_or(SessionError::MissingHeader)?;
        if loaded.truncated_tail {
            let repair_file = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(SessionError::io)?;
            repair_file
                .set_len(loaded.valid_bytes)
                .map_err(SessionError::io)?;
            repair_file.sync_all().map_err(SessionError::io)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(SessionError::io)?;
        if loaded.valid_bytes > 0 && !loaded.ends_with_newline {
            file.write_all(b"\n").map_err(SessionError::io)?;
            file.sync_all().map_err(SessionError::io)?;
        }
        let validation = AppendValidationState::from_entries(&loaded.entries)?;
        Ok(Self {
            path,
            file,
            validation,
            leaf,
            domain,
        })
    }

    pub fn append(&mut self, kind: SessionEntryKind) -> Result<EntryId, SessionError> {
        if matches!(kind, SessionEntryKind::Header(_)) {
            return Err(SessionError::InvalidEntryKind);
        }
        let entry = SessionEntry::new(EntryId::new(), Some(self.leaf.clone()), kind);
        self.write_entries(std::slice::from_ref(&entry))?;
        self.leaf = match &entry.kind {
            SessionEntryKind::Leaf(leaf) => {
                leaf.target_id.clone().unwrap_or_else(|| entry.id.clone())
            }
            _ => entry.id.clone(),
        };
        Ok(entry.id)
    }

    pub fn append_batch(
        &mut self,
        kinds: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError> {
        Ok(self
            .append_batch_entries(kinds)?
            .into_iter()
            .map(|entry| entry.id)
            .collect())
    }

    pub fn append_batch_entries(
        &mut self,
        kinds: Vec<SessionEntryKind>,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        let entries = self.prepare_batch_entries(kinds)?;
        self.append_entries(&entries)?;
        Ok(entries)
    }

    pub(crate) fn prepare_batch_entries(
        &self,
        kinds: Vec<SessionEntryKind>,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        self.prepare_batch_after(kinds, &[])
    }

    pub(crate) fn prepare_batch_after(
        &self,
        kinds: Vec<SessionEntryKind>,
        preceding: &[SessionEntry],
    ) -> Result<Vec<SessionEntry>, SessionError> {
        let mut validation = self.validation.clone();
        let mut parent = self.leaf.clone();
        for entry in preceding {
            parent = leaf_after_entry(entry);
            if validation.contains(&entry.id) {
                continue;
            }
            let validated = validation.validate_entries(std::iter::once(entry), self.domain)?;
            validation.commit(validated);
        }

        let mut entries = Vec::with_capacity(kinds.len());
        for kind in kinds {
            if matches!(kind, SessionEntryKind::Header(_)) {
                return Err(SessionError::InvalidEntryKind);
            }
            let entry = SessionEntry::new(EntryId::new(), Some(parent), kind);
            parent = leaf_after_entry(&entry);
            entries.push(entry);
        }
        // A caller-visible result can be lost after `append_entries` already
        // advanced the writer's validation state. Overlay only the pending
        // entries not yet represented there, then validate the new request
        // against that logical tail. The exact pending batch is still kept for
        // byte-for-byte reconciliation before either batch is acknowledged.
        validation.validate_entries(entries.iter(), self.domain)?;
        Ok(entries)
    }

    pub fn set_leaf(&mut self, target: Option<&EntryId>) -> Result<EntryId, SessionError> {
        self.append(SessionEntryKind::Leaf(super::Leaf {
            target_id: target.cloned(),
        }))
    }

    pub fn append_entries(&mut self, entries: &[SessionEntry]) -> Result<(), SessionError> {
        if entries.is_empty() {
            return Ok(());
        }
        self.write_entries(entries)?;
        if let Some(last) = entries.last() {
            self.leaf = leaf_after_entry(last);
        }
        Ok(())
    }

    pub(crate) fn reconcile_entries(
        &mut self,
        expected: &[SessionEntry],
    ) -> Result<(), SessionError> {
        if expected.is_empty() {
            return Ok(());
        }

        let path = self.path.clone();
        // Reopening also removes an interrupted trailing line. Reconciliation
        // then compares durable facts by identity and content before deciding
        // whether any exact suffix is still missing.
        let mut reopened = Self::open(path.clone())?;
        let loaded = JsonlLoader::load(&path)?;
        let first = loaded
            .entries
            .iter()
            .position(|entry| entry.id == expected[0].id);

        let mut result = if let Some(start) = first {
            let mut matched = 0;
            for (offset, exact) in expected.iter().enumerate() {
                let Some(actual) = loaded.entries.get(start + offset) else {
                    break;
                };
                if actual.id != exact.id || actual != exact {
                    break;
                }
                matched += 1;
            }

            let conflict = expected[matched..]
                .iter()
                .find(|exact| loaded.entries.iter().any(|actual| actual.id == exact.id));
            if let Some(conflict) = conflict {
                Err(SessionError::ExactBatchConflict(conflict.id.clone()))
            } else if matched < expected.len() && start + matched != loaded.entries.len() {
                Err(SessionError::ExactBatchConflict(
                    expected[matched].id.clone(),
                ))
            } else {
                reopened.append_entries(&expected[matched..])
            }
        } else if let Some(conflict) = expected
            .iter()
            .find(|exact| loaded.entries.iter().any(|actual| actual.id == exact.id))
        {
            Err(SessionError::ExactBatchConflict(conflict.id.clone()))
        } else {
            reopened.append_entries(expected)
        };

        if result.is_ok() {
            // A prior write may have returned an error after making the exact
            // bytes readable but before proving them stable. Even when no
            // suffix is missing, reconciliation must cross a fresh sync_all
            // barrier before the recorder is allowed to forget this batch.
            result = reopened.flush();
        }
        *self = reopened;
        result
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush buffered bytes and force the source log to stable storage.
    pub fn flush(&mut self) -> Result<(), SessionError> {
        self.file.flush().map_err(SessionError::io)?;
        #[cfg(test)]
        if take_file_sync_failure(&self.path) {
            return Err(SessionError::io(std::io::Error::other(
                "injected session source sync failure",
            )));
        }
        self.file.sync_all().map_err(SessionError::io)
    }

    fn write_entries(&mut self, entries: &[SessionEntry]) -> Result<(), SessionError> {
        let validated = self
            .validation
            .validate_entries(entries.iter(), self.domain)?;
        let mut encoded = Vec::new();
        for entry in entries {
            let line = serde_json::to_string(entry).map_err(|source| SessionError::Serialize {
                entry_id: entry.id.clone(),
                source,
            })?;
            encoded.extend_from_slice(line.as_bytes());
            encoded.push(b'\n');
        }
        // Serialize the whole exact batch before touching the file. A failed
        // write can therefore leave only kernel-visible bytes, which the
        // recorder can safely reopen and reconcile without a hidden buffer
        // flushing again during Drop.
        self.file.write_all(&encoded).map_err(SessionError::io)?;
        self.flush()?;
        self.validation.commit(validated);
        Ok(())
    }
}

#[cfg(test)]
static FILE_SYNC_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

#[cfg(test)]
fn fail_next_file_sync_for_test(path: PathBuf) {
    let failures = FILE_SYNC_FAILURES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut failures = failures
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *failures.entry(path).or_insert(0) += 1;
}

#[cfg(test)]
fn take_file_sync_failure(path: &Path) -> bool {
    let failures = FILE_SYNC_FAILURES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut failures = failures
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(remaining) = failures.get_mut(path) else {
        return false;
    };
    *remaining = remaining.saturating_sub(1);
    if *remaining == 0 {
        failures.remove(path);
    }
    true
}

fn leaf_after_entry(entry: &SessionEntry) -> EntryId {
    match &entry.kind {
        SessionEntryKind::Leaf(leaf) => leaf.target_id.clone().unwrap_or_else(|| entry.id.clone()),
        _ => entry.id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use super::*;
    use crate::llm::{Message, Role, Usage};
    use crate::session::MessageEntry;

    fn chat_header() -> SessionHeader {
        SessionHeader::new(crate::session::SessionDomain::Chat, None)
    }

    fn message(text: &str) -> SessionEntryKind {
        SessionEntryKind::Message(MessageEntry {
            message: Message {
                role: Role::User,
                content: vec![crate::llm::ContentBlock::Text {
                    text: text.to_string(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
            turn_id: None,
            model: None,
            usage: Usage::default(),
        })
    }

    #[test]
    fn writer_roundtrips_and_loads_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let mut writer = JsonlWriter::create(&path, chat_header()).expect("create");
        let first = writer.append(message("hello")).expect("append");
        let second = writer.append(message("world")).expect("append");
        let loaded = JsonlLoader::load(&path).expect("load");
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.entries[1].id, first);
        assert_eq!(loaded.entries[2].id, second);
        assert_eq!(loaded.entries[2].parent_id.as_ref(), Some(&first));
        assert!(loaded.diagnostics.is_empty());
    }

    #[test]
    fn loader_ignores_only_truncated_tail_and_reports_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let header = SessionEntry::header(chat_header());
        let user = SessionEntry::new(EntryId::new(), Some(header.id.clone()), message("hello"));
        fs::write(
            &path,
            format!(
                "{}\n{}\n{{\"partial\"",
                serde_json::to_string(&header).expect("header"),
                serde_json::to_string(&user).expect("user")
            ),
        )
        .expect("write");
        let loaded = JsonlLoader::load(&path).expect("truncated tail is recoverable");
        assert_eq!(loaded.entries.len(), 2);

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        file.write_all(b"\n{not-json}\n").expect("corrupt line");
        let loaded = JsonlLoader::scan(&path).expect("corrupt line is diagnosed");
        assert_eq!(loaded.diagnostics.len(), 2);
    }

    #[test]
    fn duplicate_ids_are_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let header = SessionEntry::header(chat_header());
        let encoded = serde_json::to_string(&header).expect("header");
        fs::write(&path, format!("{encoded}\n{encoded}\n")).expect("write");
        assert!(matches!(
            JsonlLoader::load(&path),
            Err(SessionError::DuplicateId(_))
        ));
    }

    #[test]
    fn writer_persists_leaf_selection_and_rejects_unknown_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let mut writer = JsonlWriter::create(&path, chat_header()).expect("create");
        let first = writer.append(message("first")).expect("append");
        let leaf_fact = writer.set_leaf(Some(&first)).expect("set leaf");
        assert!(matches!(
            writer.set_leaf(Some(&EntryId::new())),
            Err(SessionError::LeafNotFound(_))
        ));
        let loaded = JsonlLoader::load(&path).expect("load");
        assert_eq!(
            loaded.entries.last().map(|entry| &entry.id),
            Some(&leaf_fact)
        );
        assert!(matches!(
            loaded.entries.last().map(|entry| &entry.kind),
            Some(SessionEntryKind::Leaf(_))
        ));
    }

    #[test]
    fn reopening_writer_truncates_an_interrupted_tail_before_appending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let first = {
            let mut writer = JsonlWriter::create(&path, chat_header()).expect("create");
            writer.append(message("first")).expect("append")
        };
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open")
            .write_all(br#"{"id":"interrupted""#)
            .expect("append partial tail");

        let second = JsonlWriter::open(&path)
            .expect("reopen")
            .append(message("second"))
            .expect("append after repair");
        let loaded = JsonlLoader::load(&path).expect("load repaired file");
        assert_eq!(loaded.entries.len(), 3);
        assert!(loaded.diagnostics.is_empty());
        assert!(!loaded.truncated_tail);
        assert_eq!(loaded.entries[2].id, second);
        assert_eq!(loaded.entries[2].parent_id.as_ref(), Some(&first));
    }

    #[test]
    fn exact_reconciliation_syncs_a_fully_visible_batch_before_acknowledging_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let mut writer = JsonlWriter::create(&path, chat_header()).expect("create");
        let expected = writer
            .prepare_batch_entries(vec![message("visible but not yet proven durable")])
            .expect("prepare exact batch");
        for entry in &expected {
            serde_json::to_writer(&mut writer.file, entry).expect("write exact entry");
            writer.file.write_all(b"\n").expect("write newline");
        }

        fail_next_file_sync_for_test(path.clone());
        let result = writer.reconcile_entries(&expected);
        // Consume an unobserved test fault on the broken implementation so it
        // cannot leak into another concurrently scheduled test.
        let _ = writer.flush();

        assert!(
            result.is_err(),
            "reconciliation must not clear the retry batch before sync_all succeeds"
        );
    }
}
