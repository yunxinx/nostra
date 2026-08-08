use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use super::{
    DiagnosticKind, EntryId, JsonlDiagnostic, SessionEntry, SessionEntryKind, SessionError,
    SessionHeader, validate_appended_kind, validate_session_entries,
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
            if bytes.is_empty() {
                valid_bytes = consumed_bytes;
                ends_with_newline = has_newline;
                line_number += 1;
                continue;
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

        validate_session_entries(&entries)?;
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

/// Append-only JSONL writer.  A writer always keeps the in-memory id set in
/// sync with the file, preventing a duplicate from being appended accidentally.
pub struct JsonlWriter {
    path: PathBuf,
    file: BufWriter<File>,
    known_ids: HashSet<EntryId>,
    leaf: EntryId,
    domain: super::SessionDomain,
}

impl JsonlWriter {
    pub fn create(path: impl Into<PathBuf>, header: SessionHeader) -> Result<Self, SessionError> {
        header.validate()?;
        let domain = header.domain;
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(SessionError::io)?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(SessionError::io)?;
        let header_entry = SessionEntry::header(header);
        let mut writer = Self {
            path,
            file: BufWriter::new(file),
            known_ids: HashSet::new(),
            leaf: header_entry.id.clone(),
            domain,
        };
        writer.write_entries(std::slice::from_ref(&header_entry))?;
        Ok(writer)
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
        let known_ids = loaded
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        Ok(Self {
            path,
            file: BufWriter::new(file),
            known_ids,
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
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let mut parent = self.leaf.clone();
        let mut entries: Vec<SessionEntry> = Vec::with_capacity(kinds.len());
        for kind in kinds {
            if matches!(kind, SessionEntryKind::Header(_)) {
                return Err(SessionError::InvalidEntryKind);
            }
            let entry = SessionEntry::new(EntryId::new(), Some(parent.clone()), kind);
            parent = match &entry.kind {
                SessionEntryKind::Leaf(leaf) => {
                    leaf.target_id.clone().unwrap_or_else(|| entry.id.clone())
                }
                _ => entry.id.clone(),
            };
            entries.push(entry);
        }
        let ids = entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        self.write_entries(&entries)?;
        self.leaf = parent;
        Ok(ids)
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
            self.leaf = match &last.kind {
                SessionEntryKind::Leaf(leaf) => {
                    leaf.target_id.clone().unwrap_or_else(|| last.id.clone())
                }
                _ => last.id.clone(),
            };
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_entries(&mut self, entries: &[SessionEntry]) -> Result<(), SessionError> {
        let mut batch_ids = self.known_ids.clone();
        for entry in entries {
            if self.known_ids.is_empty() {
                if !matches!(entry.kind, SessionEntryKind::Header(_)) {
                    return Err(SessionError::MissingHeader);
                }
            } else if matches!(entry.kind, SessionEntryKind::Header(_)) {
                return Err(SessionError::InvalidEntryKind);
            }
            if !matches!(entry.kind, SessionEntryKind::Header(_)) {
                validate_appended_kind(&entry.kind, &batch_ids, self.domain)?;
            }
            if let Some(parent) = &entry.parent_id
                && !batch_ids.contains(parent)
            {
                return Err(SessionError::DanglingParent(parent.clone()));
            }
            if !batch_ids.insert(entry.id.clone()) {
                return Err(SessionError::DuplicateId(entry.id.clone()));
            }
        }
        for entry in entries {
            let line = serde_json::to_string(entry).map_err(|source| SessionError::Serialize {
                entry_id: entry.id.clone(),
                source,
            })?;
            self.file
                .write_all(line.as_bytes())
                .map_err(SessionError::io)?;
            self.file.write_all(b"\n").map_err(SessionError::io)?;
        }
        self.file.flush().map_err(SessionError::io)?;
        self.file.get_ref().sync_all().map_err(SessionError::io)?;
        self.known_ids = batch_ids;
        Ok(())
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
        let loaded = JsonlLoader::load(&path).expect("corrupt line is diagnosed");
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
}
