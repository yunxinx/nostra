use std::{
    fs::{self, Metadata},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
use std::collections::HashMap;

use super::LocalStoreError;

const CREATE_STAGE_PREFIX: &str = ".session-create-";
const CREATE_STAGE_SUFFIX: &str = ".stage";

/// Filesystem authority fixed when a store is opened.
///
/// A lexical path is not a capability: one of its ancestors can be renamed or
/// replaced while the store remains alive. Keeping both the resolved root and
/// its platform file identity lets every later operation reject that drift
/// before it opens, writes, scans, or removes a session source.
#[derive(Clone, Debug)]
pub(super) struct SourceBoundary {
    lexical_root: PathBuf,
    resolved_root: PathBuf,
    identity: FileIdentity,
}

impl SourceBoundary {
    pub(super) fn open(lexical_root: PathBuf) -> Result<Self, LocalStoreError> {
        let metadata = directory_metadata(&lexical_root)?;
        let resolved_root = fs::canonicalize(&lexical_root)?;
        Ok(Self {
            lexical_root,
            resolved_root,
            identity: FileIdentity::from_metadata(&metadata),
        })
    }

    pub(super) fn lexical_root(&self) -> &Path {
        &self.lexical_root
    }

    fn authorize_root(&self) -> Result<&Path, LocalStoreError> {
        let metadata = directory_metadata(&self.lexical_root)?;
        let resolved = fs::canonicalize(&self.lexical_root)?;
        if resolved != self.resolved_root || FileIdentity::from_metadata(&metadata) != self.identity
        {
            return Err(LocalStoreError::UnsafeSourcePath(self.lexical_root.clone()));
        }
        Ok(&self.resolved_root)
    }
}

pub(super) enum AuthorizedDeleteTarget {
    Existing {
        path: PathBuf,
        durability_parent: PathBuf,
    },
    Missing {
        path: PathBuf,
        durability_parent: PathBuf,
    },
}

impl AuthorizedDeleteTarget {
    pub(super) fn into_parts(self) -> (PathBuf, PathBuf) {
        match self {
            Self::Existing {
                path,
                durability_parent,
            }
            | Self::Missing {
                path,
                durability_parent,
            } => (path, durability_parent),
        }
    }

    pub(super) fn durability_parent(&self) -> &Path {
        match self {
            Self::Existing {
                durability_parent, ..
            }
            | Self::Missing {
                durability_parent, ..
            } => durability_parent,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SourceStamp {
    identity: FileIdentity,
    len: u64,
    modified: Option<SystemTime>,
}

pub(super) fn source_stamp(path: &Path) -> Option<SourceStamp> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    Some(SourceStamp {
        identity: FileIdentity::from_metadata(&metadata),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// Revalidate an open handle against the namespace and file identity it was
/// created from. Length and modification time may legitimately change when a
/// second Nostra process appends to the same inode, so only identity loss
/// revokes the recorder's authority to replay an exact pending batch.
pub(super) fn authorize_retained_source(
    boundary: &SourceBoundary,
    path: &Path,
    expected: Option<&SourceStamp>,
) -> Result<(), LocalStoreError> {
    authorize_existing_source(boundary, path)?;
    if !retained_source_identity_matches(path, expected) {
        return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
    }
    Ok(())
}

pub(super) fn retained_source_identity_matches(
    path: &Path,
    expected: Option<&SourceStamp>,
) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    source_stamp(path).is_some_and(|current| current.identity == expected.identity)
}

pub(super) fn authorize_sessions_root(boundary: &SourceBoundary) -> Result<(), LocalStoreError> {
    boundary.authorize_root().map(|_| ())
}

pub(super) fn sync_directory(path: &Path) -> Result<(), LocalStoreError> {
    #[cfg(test)]
    if take_directory_sync_failure(path) {
        return Err(LocalStoreError::Io(std::io::Error::other(
            "injected directory sync failure",
        )));
    }
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

pub(super) fn prepare_durable_directory_chain(path: &Path) -> Result<(), LocalStoreError> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::metadata(current) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(LocalStoreError::UnsafeSourcePath(current.to_path_buf()));
                }
                // A previous attempt may have created this empty directory but
                // failed before syncing its parent. Retrying that one barrier
                // avoids fsyncing established non-empty roots on every open.
                if fs::read_dir(current)?.next().transpose()?.is_none()
                    && let Some(parent) = current.parent()
                {
                    sync_directory(parent)?;
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current.parent().ok_or_else(|| {
                    LocalStoreError::Io(std::io::Error::other(format!(
                        "cannot create a directory chain without an existing ancestor: {}",
                        path.display()
                    )))
                })?;
            }
            Err(error) => return Err(LocalStoreError::Io(error)),
        }
    }

    for directory in missing.into_iter().rev() {
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !fs::metadata(&directory)?.is_dir() {
                    return Err(LocalStoreError::UnsafeSourcePath(directory));
                }
            }
            Err(error) => return Err(LocalStoreError::Io(error)),
        }
        let parent = directory.parent().ok_or_else(|| {
            LocalStoreError::Io(std::io::Error::other(format!(
                "created directory has no parent: {}",
                directory.display()
            )))
        })?;
        // Sync each newly published directory entry before creating the next
        // level. A successful open therefore never places the catalog or fact
        // sources below a namespace prefix that can disappear after a crash.
        sync_directory(parent)?;
    }
    Ok(())
}

pub(super) fn create_session_stage(
    boundary: &SourceBoundary,
) -> Result<tempfile::NamedTempFile, LocalStoreError> {
    let root = boundary.authorize_root()?;
    tempfile::Builder::new()
        .prefix(CREATE_STAGE_PREFIX)
        .suffix(CREATE_STAGE_SUFFIX)
        .tempfile_in(root)
        .map_err(LocalStoreError::Io)
}

pub(super) fn sync_staging_directory(boundary: &SourceBoundary) -> Result<(), LocalStoreError> {
    sync_directory(boundary.authorize_root()?)
}

pub(super) fn cleanup_abandoned_create_stages(
    boundary: &SourceBoundary,
) -> Result<(), LocalStoreError> {
    let root = boundary.authorize_root()?;
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut removed = false;
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(CREATE_STAGE_PREFIX) || !name.ends_with(CREATE_STAGE_SUFFIX) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        fs::remove_file(entry.path())?;
        removed = true;
    }
    if removed {
        // Unlinking the plaintext is not crash-durable until the staging
        // directory itself is synced. A later launch must not resurrect it.
        sync_directory(root)?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn fail_next_directory_sync_for_test(path: PathBuf) {
    let fault = DIRECTORY_SYNC_FAILURE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pending = fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *pending.entry(path).or_insert(0) += 1;
}

pub(super) fn prepare_create_parent(
    boundary: &SourceBoundary,
    parent: &Path,
) -> Result<(), LocalStoreError> {
    let root = boundary.authorize_root()?;
    if parent == boundary.lexical_root() {
        return Ok(());
    }
    if parent.parent() != Some(boundary.lexical_root()) {
        return Err(LocalStoreError::UnsafeSourcePath(parent.to_path_buf()));
    }

    let created = match fs::create_dir(parent) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(LocalStoreError::Io(error)),
    };
    let resolved = authorize_directory(boundary, parent)?;
    if resolved.parent() != Some(root) {
        return Err(LocalStoreError::UnsafeSourcePath(parent.to_path_buf()));
    }
    // The JSONL file is synced inside its project bucket later, but that does
    // not make the bucket's own directory entry durable in `sessions_root`.
    // An empty existing bucket can be the residue of an earlier failed sync,
    // so retry that rare case without fsyncing the root for every Agent turn.
    let needs_parent_sync = created || fs::read_dir(parent)?.next().transpose()?.is_none();
    if needs_parent_sync {
        sync_directory(boundary.lexical_root())?;
    }
    Ok(())
}

pub(super) fn authorize_existing_source(
    boundary: &SourceBoundary,
    path: &Path,
) -> Result<PathBuf, LocalStoreError> {
    let root = boundary.authorize_root()?;
    let parent = path
        .parent()
        .ok_or_else(|| LocalStoreError::UnsafeSourcePath(path.to_path_buf()))?;
    let parent = if parent == boundary.lexical_root() {
        root.to_path_buf()
    } else {
        if parent.parent() != Some(boundary.lexical_root()) {
            return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
        }
        let resolved = authorize_directory(boundary, parent)?;
        if resolved.parent() != Some(root) {
            return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
        }
        resolved
    };

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
    }
    let resolved = fs::canonicalize(path)?;
    if resolved.parent() != Some(parent.as_path()) {
        return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
    }
    // Validation may resolve symlinked ancestors chosen before store open
    // (for example a redirected config directory), but business state and the
    // catalog remain in one lexical namespace. Mixing canonical and lexical
    // spellings would make repair/delete distrust the store's own source.
    Ok(path.to_path_buf())
}

pub(super) fn authorize_delete_target(
    boundary: &SourceBoundary,
    path: &Path,
) -> Result<AuthorizedDeleteTarget, LocalStoreError> {
    let root = boundary.authorize_root()?;
    let parent = path
        .parent()
        .ok_or_else(|| LocalStoreError::UnsafeSourcePath(path.to_path_buf()))?;
    let authorized_parent = if parent == boundary.lexical_root() {
        Some(root.to_path_buf())
    } else {
        if parent.parent() != Some(boundary.lexical_root()) {
            return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
        }
        match fs::symlink_metadata(parent) {
            Ok(_) => {
                let resolved = authorize_directory(boundary, parent)?;
                if resolved.parent() != Some(root) {
                    return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
                }
                Some(resolved)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(LocalStoreError::Io(error)),
        }
    };

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
            }
            let resolved = fs::canonicalize(path)?;
            if authorized_parent.as_deref() != resolved.parent() {
                return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
            }
            Ok(AuthorizedDeleteTarget::Existing {
                path: path.to_path_buf(),
                durability_parent: parent.to_path_buf(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // If an Agent bucket is itself absent, its removal is represented
            // by the sessions-root directory entry. Otherwise the source's
            // immediate parent owns the missing file entry that must be synced
            // before repair or delete clears the catalog obligation.
            let durability_parent = if authorized_parent.is_some() {
                parent.to_path_buf()
            } else {
                boundary.lexical_root().to_path_buf()
            };
            Ok(AuthorizedDeleteTarget::Missing {
                path: path.to_path_buf(),
                durability_parent,
            })
        }
        Err(error) => Err(LocalStoreError::Io(error)),
    }
}

fn authorize_directory(boundary: &SourceBoundary, path: &Path) -> Result<PathBuf, LocalStoreError> {
    let root = boundary.authorize_root()?;
    let metadata = directory_metadata(path)?;
    let resolved = fs::canonicalize(path)?;
    if !resolved.starts_with(root) {
        return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
    }
    // Reading metadata above is not redundant: it rejects a final symlink
    // even when that link resolves back underneath the authorized root.
    let _ = metadata;
    Ok(resolved)
}

fn directory_metadata(path: &Path) -> Result<Metadata, LocalStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(LocalStoreError::UnsafeSourcePath(path.to_path_buf()));
    }
    Ok(metadata)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
    #[cfg(not(any(unix, windows)))]
    created: Option<SystemTime>,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;

            Self {
                volume_serial_number: metadata.volume_serial_number(),
                file_index: metadata.file_index(),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {
                created: metadata.created().ok(),
            }
        }
    }
}

#[cfg(test)]
static DIRECTORY_SYNC_FAILURE: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

#[cfg(test)]
fn take_directory_sync_failure(path: &Path) -> bool {
    let fault = DIRECTORY_SYNC_FAILURE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pending = fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(count) = pending.get_mut(path) else {
        return false;
    };
    *count -= 1;
    if *count == 0 {
        pending.remove(path);
    }
    true
}
