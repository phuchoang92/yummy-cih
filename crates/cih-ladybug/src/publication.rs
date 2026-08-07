//! Durable file-backed authoritative publication records for Ladybug.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use cih_core::RepositoryId;
use cih_graph_store::publication::{
    CurrentPublication, GraphPublicationEpoch, GraphPublicationStore, PublicationCasResult,
    PublisherFencingToken,
};
use cih_graph_store::{GraphStoreError, Result};
use serde::{Deserialize, Serialize};

const PUBLICATIONS_DIR: &str = ".publications";
const CURRENT_FILE: &str = "CURRENT";
const LOCK_FILE: &str = ".lock";
const FENCE_FILE: &str = "FENCE";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CurrentPointer {
    epoch: GraphPublicationEpoch,
    fencing_token: PublisherFencingToken,
}

/// Ladybug's publication metadata is separate from its graph files. Epoch
/// records are immutable JSON files; one same-directory atomic `CURRENT`
/// replacement commits both the selected epoch and its fencing token.
pub struct LadybugPublicationStore {
    root: PathBuf,
}

impl LadybugPublicationStore {
    pub fn connect(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(GraphStoreError::InvalidInput(
                "Ladybug publication root must not be empty".into(),
            ));
        }
        Ok(Self { root })
    }

    fn repository_dir(&self, repository_id: &RepositoryId) -> PathBuf {
        self.root
            .join(PUBLICATIONS_DIR)
            .join(repository_id.as_str())
    }

    fn epoch_path(&self, repository_id: &RepositoryId, epoch: &GraphPublicationEpoch) -> PathBuf {
        self.repository_dir(repository_id)
            .join(format!("{}.json", epoch.as_str()))
    }

    fn read_pointer(&self, repository_id: &RepositoryId) -> Result<Option<CurrentPointer>> {
        read_json(
            &self.repository_dir(repository_id).join(CURRENT_FILE),
            "current pointer",
        )
    }

    fn lock_repository(&self, repository_id: &RepositoryId) -> Result<File> {
        let directory = self.repository_dir(repository_id);
        std::fs::create_dir_all(&directory).map_err(|error| {
            GraphStoreError::Backend(format!(
                "create Ladybug publication directory {}: {error}",
                directory.display()
            ))
        })?;
        let path = directory.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                GraphStoreError::Backend(format!(
                    "open Ladybug publication lock {}: {error}",
                    path.display()
                ))
            })?;
        lock.lock().map_err(|error| {
            GraphStoreError::Backend(format!(
                "lock Ladybug publication record {}: {error}",
                path.display()
            ))
        })?;
        Ok(lock)
    }
}

#[async_trait]
impl GraphPublicationStore for LadybugPublicationStore {
    async fn allocate_fencing_token(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<PublisherFencingToken> {
        let _lock = self.lock_repository(repository_id)?;
        let path = self.repository_dir(repository_id).join(FENCE_FILE);
        let current = match std::fs::read_to_string(&path) {
            Ok(value) => value.trim().parse::<u64>().map_err(|error| {
                GraphStoreError::Backend(format!(
                    "parse Ladybug publication fence {}: {error}",
                    path.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(GraphStoreError::Backend(format!(
                    "read Ladybug publication fence {}: {error}",
                    path.display()
                )))
            }
        };
        let next = current.checked_add(1).ok_or_else(|| {
            GraphStoreError::Backend("Ladybug publication fence exhausted".into())
        })?;
        atomic_write(&path, next.to_string().as_bytes())?;
        PublisherFencingToken::new(next)
    }

    async fn current(&self, repository_id: &RepositoryId) -> Result<Option<CurrentPublication>> {
        let Some(pointer) = self.read_pointer(repository_id)? else {
            return Ok(None);
        };
        self.by_epoch(repository_id, &pointer.epoch)
            .await?
            .map(Some)
            .ok_or_else(|| {
                GraphStoreError::Backend(format!(
                    "Ladybug publication CURRENT references missing epoch {}",
                    pointer.epoch
                ))
            })
    }

    async fn by_epoch(
        &self,
        repository_id: &RepositoryId,
        epoch: &GraphPublicationEpoch,
    ) -> Result<Option<CurrentPublication>> {
        let record: Option<CurrentPublication> =
            read_json(&self.epoch_path(repository_id, epoch), "epoch record")?;
        if let Some(record) = record.as_ref() {
            record.validate()?;
            if &record.repository_id != repository_id || &record.epoch != epoch {
                return Err(GraphStoreError::Backend(format!(
                    "Ladybug publication epoch record {} has mismatched identity",
                    epoch
                )));
            }
        }
        Ok(record)
    }

    async fn compare_and_swap(
        &self,
        repository_id: &RepositoryId,
        expected_epoch: Option<&GraphPublicationEpoch>,
        next: &CurrentPublication,
        fencing_token: PublisherFencingToken,
    ) -> Result<PublicationCasResult> {
        next.validate()?;
        if &next.repository_id != repository_id {
            return Err(GraphStoreError::InvalidInput(
                "publication repository_id does not match the CAS repository".into(),
            ));
        }
        if next.previous_epoch.as_ref() != expected_epoch {
            return Err(GraphStoreError::InvalidInput(
                "publication previous_epoch must equal the CAS expected_epoch".into(),
            ));
        }

        let _lock = self.lock_repository(repository_id)?;
        let current = self.read_pointer(repository_id)?;
        let allocated_fence =
            std::fs::read_to_string(self.repository_dir(repository_id).join(FENCE_FILE))
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(0);
        if fencing_token.get() < allocated_fence {
            return Ok(PublicationCasResult::StaleFencingToken {
                current_token: PublisherFencingToken::new(allocated_fence)?,
            });
        }
        if let Some(pointer) = current.as_ref() {
            if fencing_token <= pointer.fencing_token {
                return Ok(PublicationCasResult::StaleFencingToken {
                    current_token: pointer.fencing_token,
                });
            }
        }

        let actual_epoch = current.as_ref().map(|pointer| &pointer.epoch);
        if actual_epoch != expected_epoch {
            return Ok(PublicationCasResult::Conflict {
                current_epoch: actual_epoch.cloned(),
            });
        }

        let record_bytes = serde_json::to_vec(next).map_err(|error| {
            GraphStoreError::Backend(format!("serialize Ladybug publication epoch: {error}"))
        })?;
        write_immutable(&self.epoch_path(repository_id, &next.epoch), &record_bytes)?;

        let pointer = CurrentPointer {
            epoch: next.epoch.clone(),
            fencing_token,
        };
        let pointer_bytes = serde_json::to_vec(&pointer).map_err(|error| {
            GraphStoreError::Backend(format!("serialize Ladybug publication pointer: {error}"))
        })?;
        atomic_write(
            &self.repository_dir(repository_id).join(CURRENT_FILE),
            &pointer_bytes,
        )?;
        Ok(PublicationCasResult::Published)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, label: &str) -> Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(GraphStoreError::Backend(format!(
                "read Ladybug publication {label} {}: {error}",
                path.display()
            )))
        }
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        GraphStoreError::Backend(format!(
            "parse Ladybug publication {label} {}: {error}",
            path.display()
        ))
    })
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = match OpenOptions::new().create_new(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read(path).map_err(|read_error| {
                GraphStoreError::Backend(format!(
                    "read existing Ladybug publication epoch {}: {read_error}",
                    path.display()
                ))
            })?;
            if existing == bytes {
                return Ok(());
            }
            return Err(GraphStoreError::Backend(format!(
                "Ladybug publication epoch collision at {}",
                path.display()
            )));
        }
        Err(error) => {
            return Err(GraphStoreError::Backend(format!(
                "create Ladybug publication epoch {}: {error}",
                path.display()
            )))
        }
    };
    file.write_all(bytes).map_err(|error| {
        GraphStoreError::Backend(format!(
            "write Ladybug publication epoch {}: {error}",
            path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        GraphStoreError::Backend(format!(
            "sync Ladybug publication epoch {}: {error}",
            path.display()
        ))
    })?;
    sync_parent(path)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let directory = path.parent().ok_or_else(|| {
        GraphStoreError::Backend(format!(
            "publication pointer has no parent: {}",
            path.display()
        ))
    })?;
    let temp = directory.join(format!(
        ".CURRENT.tmp-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|error| {
                GraphStoreError::Backend(format!(
                    "create Ladybug publication pointer {}: {error}",
                    temp.display()
                ))
            })?;
        file.write_all(bytes).map_err(|error| {
            GraphStoreError::Backend(format!(
                "write Ladybug publication pointer {}: {error}",
                temp.display()
            ))
        })?;
        file.sync_all().map_err(|error| {
            GraphStoreError::Backend(format!(
                "sync Ladybug publication pointer {}: {error}",
                temp.display()
            ))
        })?;
        atomic_replace(&temp, path).map_err(|error| {
            GraphStoreError::Backend(format!(
                "replace Ladybug publication pointer {}: {error}",
                path.display()
            ))
        })?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        let parent = path.parent().ok_or_else(|| {
            GraphStoreError::Backend(format!(
                "publication path has no parent: {}",
                path.display()
            ))
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                GraphStoreError::Backend(format!(
                    "sync Ladybug publication directory {}: {error}",
                    parent.display()
                ))
            })?;
    }
    Ok(())
}
