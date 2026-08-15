use std::time::Duration;

#[cfg(test)]
use std::sync::Mutex;

use rusqlite::ErrorCode;

use super::query_needs_repair;
use super::schema::CatalogSchema;
use super::*;
#[cfg(test)]
use super::{
    INITIALIZE_INTERRUPTION, REPLACEMENT_INITIALIZATION_FAILURE, REPLACEMENT_INTERRUPTION,
};

impl Catalog {
    pub(crate) fn open(path: PathBuf, domain: SessionDomain) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let replacement_pending = replacement_marker_exists(&path)?;
        let header = sqlite_header(&path)?;
        let catalog_was_missing = header == SqliteHeader::Missing;
        let invalid_header = header == SqliteHeader::Invalid;
        if catalog_was_missing || invalid_header {
            // A missing/invalid index can be initialized into a valid empty
            // SQLite database before its in-database marker is written. Make
            // the repair obligation durable first so that crash window cannot
            // hide existing JSONL sources on the next launch.
            persist_replacement_marker(&path)?;
        }
        if invalid_header {
            remove_sqlite_sidecars(&path);
        }

        match Self::open_connection(&path)
            .and_then(|connection| Self::initialize(connection, path.clone(), domain))
        {
            Ok(mut catalog) => {
                #[cfg(test)]
                if take_initialize_interruption(&catalog.path) {
                    // Leave the external marker in place exactly as a process
                    // crash would. The next open must not mistake this newly
                    // initialized but empty projection for a healthy index.
                    return Err(CatalogError::Io(std::io::Error::other(
                        "injected interruption after catalog initialization",
                    )));
                }
                if catalog_was_missing || invalid_header || replacement_pending {
                    catalog.mark_repair_required()?;
                }
                Ok(catalog)
            }
            Err(first_error) if invalid_header || is_rebuildable_open_error(&first_error) => {
                // This filesystem marker is durable before the old index is
                // moved. It closes the crash window where a newly created,
                // current-schema but empty index could otherwise be accepted
                // on the next launch before the SQLite marker was written.
                persist_replacement_marker(&path)?;
                // The index is disposable. Preserve a corrupt file for
                // diagnosis, then rebuild an empty catalog from the source log.
                if path.exists() {
                    remove_sqlite_sidecars(&path);
                    let backup = path.with_extension("sqlite.corrupt");
                    let _ = std::fs::remove_file(&backup);
                    std::fs::rename(&path, backup)?;
                    sync_parent_directory(&path)?;
                }
                #[cfg(test)]
                if take_replacement_interruption(&path) {
                    return Err(CatalogError::Io(std::io::Error::other(
                        "injected interruption after catalog backup",
                    )));
                }
                #[cfg(test)]
                if take_replacement_initialization_failure(&path) {
                    return Err(CatalogError::Io(std::io::Error::other(
                        "injected replacement catalog initialization failure",
                    )));
                }
                let mut catalog = Self::open_connection(&path)
                    .and_then(|connection| Self::initialize(connection, path, domain))?;
                #[cfg(test)]
                if take_initialize_interruption(&catalog.path) {
                    return Err(CatalogError::Io(std::io::Error::other(
                        "injected interruption after catalog initialization",
                    )));
                }
                // The replacement index is valid but empty. Persist the repair
                // obligation before publishing it so a crash or ordinary
                // restart cannot make intact JSONL sources unreachable.
                catalog.mark_repair_required()?;
                Ok(catalog)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub(crate) fn interrupt_replacement_after_backup_for_test(path: PathBuf) {
        let fault = REPLACEMENT_INTERRUPTION.get_or_init(|| Mutex::new(None));
        *fault
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
    }

    #[cfg(test)]
    pub(crate) fn fail_replacement_initialization_for_test(path: PathBuf) {
        let fault = REPLACEMENT_INITIALIZATION_FAILURE.get_or_init(|| Mutex::new(None));
        *fault
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
    }

    fn open_connection(path: &Path) -> Result<Connection, CatalogError> {
        // The metadata check in `sqlite_header` gives callers a precise error,
        // while SQLite's NOFOLLOW flag closes the replacement race between
        // that check and this read-write open.
        // Resolve only the parent: config roots may legitimately contain a
        // symlinked ancestor (macOS exposes `/var` this way), but the final
        // catalog name must remain unresolved for NOFOLLOW to protect it.
        let parent = path
            .parent()
            .ok_or_else(|| CatalogError::UnsafePath(path.to_path_buf()))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| CatalogError::UnsafePath(path.to_path_buf()))?;
        let nofollow_path = std::fs::canonicalize(parent)?.join(file_name);
        let connection = Connection::open_with_flags(
            nofollow_path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        // A projection intent is acknowledged before its JSONL mutation is
        // published. WAL + NORMAL may lose that committed intent on power
        // failure while the separately fsynced source survives, leaving an
        // intact conversation undiscoverable. FULL makes the write-ahead
        // obligation durable; later projection loss remains safely repairable.
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }

    fn initialize(
        connection: Connection,
        path: PathBuf,
        domain: SessionDomain,
    ) -> Result<Self, CatalogError> {
        let schema = CatalogSchema::for_domain(domain);
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // There is no on-disk schema migration path. A catalog is a disposable
        // projection, so any version other than the empty database or the
        // current schema is rebuilt from the JSONL source by `open`.
        if version != 0 && version != CATALOG_SCHEMA_VERSION {
            return Err(CatalogError::UnsupportedVersion(version));
        }
        if version == 0 {
            if has_application_tables(&connection)? {
                return Err(CatalogError::Corrupt(
                    "schema version 0 contains an existing catalog table".to_string(),
                ));
            }
            schema.create(&connection)?;
        }
        if version == CATALOG_SCHEMA_VERSION {
            schema.validate(&connection)?;
        }
        let needs_repair = query_needs_repair(&connection)?;
        let identity = catalog_file_identity(&path)?;
        Ok(Self {
            path,
            domain,
            connection,
            identity,
            needs_repair,
            #[cfg(test)]
            full_projection_writes: 0,
            #[cfg(test)]
            incremental_projection_writes: 0,
        })
    }
}

#[cfg(test)]
impl Catalog {
    pub(crate) fn interrupt_initialize_after_open_for_test(path: PathBuf) {
        let fault = INITIALIZE_INTERRUPTION.get_or_init(|| Mutex::new(None));
        *fault
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
    }
}

#[cfg(test)]
fn take_initialize_interruption(path: &Path) -> bool {
    let fault = INITIALIZE_INTERRUPTION.get_or_init(|| Mutex::new(None));
    let mut target = fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if target.as_deref() != Some(path) {
        return false;
    }
    target.take();
    true
}

#[cfg(test)]
fn take_replacement_interruption(path: &Path) -> bool {
    let fault = REPLACEMENT_INTERRUPTION.get_or_init(|| Mutex::new(None));
    let mut target = fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if target.as_deref() != Some(path) {
        return false;
    }
    target.take();
    true
}

#[cfg(test)]
fn take_replacement_initialization_failure(path: &Path) -> bool {
    let fault = REPLACEMENT_INITIALIZATION_FAILURE.get_or_init(|| Mutex::new(None));
    let mut target = fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if target.as_deref() != Some(path) {
        return false;
    }
    target.take();
    true
}

fn is_rebuildable_open_error(error: &CatalogError) -> bool {
    match error {
        CatalogError::Corrupt(_) | CatalogError::UnsupportedVersion(_) => true,
        CatalogError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) => matches!(
            code.code,
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
        ),
        CatalogError::Sqlite(_)
        | CatalogError::Io(_)
        | CatalogError::UnsafePath(_)
        | CatalogError::ReplacedDuringOperation
        | CatalogError::DomainMismatch { .. }
        | CatalogError::StoreShuttingDown
        | CatalogError::StorePoisoned => false,
    }
}

fn remove_sqlite_sidecars(path: &Path) {
    let wal = path.with_extension("sqlite-wal");
    let shm = path.with_extension("sqlite-shm");
    let _ = std::fs::remove_file(wal);
    let _ = std::fs::remove_file(shm);
}

fn replacement_marker_path(index_path: &Path) -> PathBuf {
    index_path.with_extension("sqlite.repair-required")
}

fn replacement_marker_exists(index_path: &Path) -> Result<bool, CatalogError> {
    let marker = replacement_marker_path(index_path);
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(CatalogError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "catalog repair marker is not a regular file: {}",
                    marker.display()
                ),
            )))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CatalogError::Io(error)),
    }
}

fn persist_replacement_marker(index_path: &Path) -> Result<(), CatalogError> {
    let marker = replacement_marker_path(index_path);
    if replacement_marker_exists(index_path)? {
        std::fs::File::open(&marker)?.sync_all()?;
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker)?;
    std::io::Write::write_all(&mut file, b"repair-required\n")?;
    file.sync_all()?;
    sync_parent_directory(&marker)?;
    Ok(())
}

pub(super) fn clear_replacement_marker(index_path: &Path) -> Result<(), CatalogError> {
    let marker = replacement_marker_path(index_path);
    match std::fs::remove_file(&marker) {
        Ok(()) => sync_parent_directory(&marker).map_err(CatalogError::from),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CatalogError::Io(error)),
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SqliteHeader {
    Missing,
    Valid,
    Invalid,
}

fn sqlite_header(path: &Path) -> Result<SqliteHeader, CatalogError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(CatalogError::UnsafePath(path.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SqliteHeader::Missing);
        }
        Err(error) => return Err(CatalogError::Io(error)),
    }
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return Err(CatalogError::Io(error)),
    };
    let mut header = [0_u8; 16];
    match std::io::Read::read_exact(&mut file, &mut header) {
        Ok(()) if header == *b"SQLite format 3\0" => Ok(SqliteHeader::Valid),
        Ok(()) => Ok(SqliteHeader::Invalid),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Ok(SqliteHeader::Invalid)
        }
        Err(error) => Err(CatalogError::Io(error)),
    }
}

fn has_application_tables(connection: &Connection) -> Result<bool, CatalogError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema
                WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
            )",
            [],
            |row| row.get(0),
        )
        .map_err(CatalogError::from)
}
