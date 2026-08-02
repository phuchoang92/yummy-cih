//! LadybugDB (embedded Kùzu-fork, crate `lbug`) adapter for the `GraphStore`
//! port. File-based and in-process — no external service.
//!
//! ## Versioned directories: how an exclusive-lock DB serves two processes
//!
//! LadybugDB enforces one READ_WRITE `Database` *or* many READ_ONLY ones per
//! database, across all processes — so `cih-engine` (writer) and `cih-server`
//! (long-lived reader) can never share one live database. Instead every graph
//! key is a directory of immutable version FILES (this LadybugDB stores a
//! database as a single file, plus a transient `.wal` sidecar) and a pointer:
//!
//! ```text
//! <root>/<key>/CURRENT     one line: the live version name, e.g. "v43"
//! <root>/<key>/v42  v43    LadybugDB database files (previous kept as GC grace)
//! ```
//!
//! Writers build a fresh version file (no lock contention — nobody has it),
//! and `publish_to` is the Redis-RENAME analog: `CHECKPOINT`, close,
//! `fs::rename` the version file into the destination key, then atomically
//! flip `CURRENT`. After the rename, staging and published data share no
//! storage, so the engine's trailing `drop_graph` on staging is harmless —
//! the port guarantee holds structurally.
//!
//! Readers check `CURRENT` before each query and transparently reopen when it
//! moved. On POSIX, GC can unlink an old version while a reader holds it open;
//! on Windows, sharing violations are recorded and retried after readers rotate
//! to the new version. The server's forever per-key store cache never needs
//! invalidating.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cih_graph_store::{GraphStoreError, Result};
use lbug::{Connection, Database, SystemConfig};

mod bulk;
mod convert;
mod query;
mod schema;

/// Keep the previous version around at least this long after a publish, so a
/// reader that read `CURRENT` just before the flip can still open it.
const GC_GRACE: Duration = Duration::from_secs(600);
const GC_PENDING_FILE: &str = ".gc-pending";

pub(crate) struct OpenHandle {
    pub(crate) version: String,
    pub(crate) db: Arc<Database>,
    /// True while this store owns the version dir read-write (bulk load /
    /// upsert). Read paths reuse a writable handle as-is (same process).
    pub(crate) writable: bool,
}

pub struct LadybugStore {
    root: PathBuf,
    key: String,
    state: tokio::sync::Mutex<Option<OpenHandle>>,
    /// (semaphore, acquire timeout) — server-side backpressure; queries are
    /// CPU-bound and in-process, so bounding concurrency still matters.
    limiter: Option<(Arc<tokio::sync::Semaphore>, Duration)>,
}

impl Drop for LadybugStore {
    fn drop(&mut self) {
        // Release this process's reader before the shutdown retry. A sharing
        // violation held by another process remains in .gc-pending for its
        // next connect/read/write retry.
        self.state.get_mut().take();
        Self::gc_versions(&self.key_dir());
    }
}

impl LadybugStore {
    /// Lazy constructor — touches no disk (parity with the other adapters,
    /// and what the hermetic server tests rely on). `root` is a directory
    /// path; an optional `file://` prefix is stripped.
    pub fn connect(root: &str, graph_key: impl Into<String>) -> Result<Self> {
        let root = root.strip_prefix("file://").unwrap_or(root);
        if root.is_empty() {
            return Err(GraphStoreError::Backend(
                "ladybug backend needs a root directory path (got empty url)".into(),
            ));
        }
        Ok(Self {
            root: PathBuf::from(root),
            key: graph_key.into(),
            state: tokio::sync::Mutex::new(None),
            limiter: None,
        })
    }

    pub fn with_query_limit(mut self, max_concurrent: usize, acquire_timeout: Duration) -> Self {
        self.limiter = Some((
            Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1))),
            acquire_timeout,
        ));
        self
    }

    pub(crate) fn key_dir(&self) -> PathBuf {
        self.root.join(&self.key)
    }

    fn current_path(&self) -> PathBuf {
        self.key_dir().join("CURRENT")
    }

    /// The live version name, if this graph exists.
    pub(crate) fn read_current(&self) -> Option<String> {
        let s = std::fs::read_to_string(self.current_path()).ok()?;
        let v = s.trim().to_string();
        (!v.is_empty()).then_some(v)
    }

    fn open_db(dir: &Path, read_only: bool) -> Result<Arc<Database>> {
        let db = Database::new(dir, SystemConfig::default().read_only(read_only)).map_err(|e| {
            GraphStoreError::Backend(format!("ladybug open {}: {e}", dir.display()))
        })?;
        Ok(Arc::new(db))
    }

    /// A database handle suitable for reads, tracking `CURRENT`: `None` when
    /// the graph doesn't exist (queries then return empty results, matching
    /// the auto-created-empty-graph behavior of the Falkor adapter).
    pub(crate) async fn read_handle(&self) -> Result<Option<Arc<Database>>> {
        // A prior Windows GC may have encountered a reader-held version. Retry
        // before selecting CURRENT; failures remain best-effort and cannot make
        // an otherwise healthy graph unreadable.
        Self::gc_versions(&self.key_dir());
        let mut state = self.state.lock().await;
        // A writable handle is this process's own build — always current.
        if let Some(h) = state.as_ref() {
            if h.writable {
                return Ok(Some(h.db.clone()));
            }
        }
        let Some(current) = self.read_current() else {
            return Ok(None);
        };
        if let Some(h) = state.as_ref() {
            if h.version == current {
                return Ok(Some(h.db.clone()));
            }
        }
        // Stale or absent: (re)open READ_ONLY on the current version. Retry
        // if GC removed the file between reading CURRENT and opening, or if a
        // just-renamed publish source still carries a lingering RW lock from
        // an in-flight query on the publishing process (narrow window; the
        // lock dies when that query's Arc drops).
        let mut last_err = None;
        for attempt in 0..3u32 {
            let current = match self.read_current() {
                Some(v) => v,
                None => return Ok(None),
            };
            let path = self.key_dir().join(&current);
            match Self::open_db(&path, true) {
                Ok(db) => {
                    *state = Some(OpenHandle {
                        version: current,
                        db: db.clone(),
                        writable: false,
                    });
                    return Ok(Some(db));
                }
                Err(_) if !path.exists() => continue,
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    /// A read-write handle on a version file owned by this store (returned as
    /// `(version, db)`), creating the next version (with schema) when none is
    /// open yet. Multi-set loads and the delta path reuse the open handle.
    ///
    /// Deliberately does NOT flip `CURRENT`: the new version is empty and
    /// RW-locked, so pointing readers at it mid-build would break them and a
    /// failed load would leave `CURRENT` on a partial graph. Callers flip
    /// after their build step succeeds (`bulk_load_observed` post-checkpoint,
    /// `ensure_schema` after creating the empty schema). Same-store reads
    /// don't need the flip — `read_handle` short-circuits to the writable
    /// handle.
    pub(crate) async fn write_handle(&self) -> Result<(String, Arc<Database>)> {
        Self::gc_versions(&self.key_dir());
        let mut state = self.state.lock().await;
        if let Some(h) = state.as_ref() {
            if h.writable {
                return Ok((h.version.clone(), h.db.clone()));
            }
            // A read handle is open — drop it; we're becoming the writer.
            *state = None;
        }
        let version = self.next_version();
        // Versions are single FILES (this LadybugDB stores a database as one
        // file plus a transient `.wal` sidecar) — create only the key dir and
        // let the engine create the file itself.
        let key_dir = self.key_dir();
        std::fs::create_dir_all(&key_dir)
            .map_err(|e| GraphStoreError::Backend(format!("create {}: {e}", key_dir.display())))?;
        let db = Self::open_db(&key_dir.join(&version), false)?;
        {
            let conn = Connection::new(&db)
                .map_err(|e| GraphStoreError::Backend(format!("ladybug connection: {e}")))?;
            schema::apply_schema(&conn)?;
        }
        *state = Some(OpenHandle {
            version: version.clone(),
            db: db.clone(),
            writable: true,
        });
        Ok((version, db))
    }

    /// Next `v<N>` under this key (existing versions + 1; `v1` for a fresh
    /// key). Sidecars like `v3.wal` don't parse as versions and are ignored.
    fn next_version(&self) -> String {
        format!("v{}", max_version_in(&self.key_dir()) + 1)
    }

    /// The version file plus any sidecars (`v3.wal`, …) — the unit that
    /// publish renames, COW copies, and GC deletes together.
    fn version_paths(dir: &Path, version: &str) -> Vec<PathBuf> {
        let mut out = vec![dir.join(version)];
        let prefix = format!("{version}.");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(&prefix) && !name.ends_with(".csv-tmp") {
                        out.push(dir.join(name));
                    }
                }
            }
        }
        out
    }

    /// Atomically point `CURRENT` at `version` (write tmp + rename).
    pub(crate) fn flip_current(&self, version: &str) -> Result<()> {
        Self::flip_current_in(&self.key_dir(), version)
    }

    fn flip_current_in(dir: &Path, version: &str) -> Result<()> {
        // Unique per process AND per call: two stores in one process flipping
        // the same key dir must not clobber each other's temp file.
        static FLIP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::fs::create_dir_all(dir)
            .map_err(|e| GraphStoreError::Backend(format!("create {}: {e}", dir.display())))?;
        let tmp = dir.join(format!(
            "CURRENT.tmp-{}-{}",
            std::process::id(),
            FLIP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&tmp, format!("{version}\n"))
            .map_err(|e| GraphStoreError::Backend(format!("write {}: {e}", tmp.display())))?;
        let current = dir.join("CURRENT");
        if let Err(error) = atomic_replace(&tmp, &current) {
            let _ = std::fs::remove_file(&tmp);
            return Err(GraphStoreError::Backend(format!("flip CURRENT: {error}")));
        }
        Ok(())
    }

    pub(crate) async fn state_is_writable(&self) -> bool {
        self.state.lock().await.as_ref().is_some_and(|h| h.writable)
    }

    /// Release the open handle without checkpointing (the version dir may
    /// already have been renamed away by a publish).
    pub(crate) async fn discard_handle(&self) {
        self.state.lock().await.take();
    }

    /// Copy-on-write: clone the published version file into the next version
    /// and install a writable handle on the copy. `CURRENT` is NOT flipped —
    /// the caller flips after the delta is applied and checkpointed, so
    /// readers never rotate onto a half-applied copy. No-op when the graph
    /// doesn't exist yet (`write_handle` then creates a fresh one).
    pub(crate) async fn begin_cow_version(&self) -> Result<()> {
        let Some(current) = self.read_current() else {
            return Ok(());
        };
        let mut state = self.state.lock().await;
        state.take(); // drop any read handle; we're becoming the writer
        let dir = self.key_dir();
        let version = self.next_version();
        for src in Self::version_paths(&dir, &current) {
            let name = src.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let dst_name = name.replacen(&current, &version, 1);
            std::fs::copy(&src, dir.join(dst_name)).map_err(|e| {
                GraphStoreError::Backend(format!("copy {current} → {version}: {e}"))
            })?;
        }
        let db = Self::open_db(&dir.join(&version), false)?;
        *state = Some(OpenHandle {
            version,
            db,
            writable: true,
        });
        Ok(())
    }

    /// The Redis-RENAME analog. Checkpoint + close, then move the version dir
    /// into the destination key and atomically flip its `CURRENT`. After the
    /// rename, staging and published data share no storage — the port
    /// guarantee (`drop_graph` on staging is harmless) holds structurally.
    pub(crate) async fn publish_to_impl(&self, dest_key: &str) -> Result<()> {
        let version = match self.close_handle().await? {
            Some(v) => v,
            None => self.read_current().ok_or_else(|| {
                GraphStoreError::Backend(format!(
                    "publish_to: graph '{}' has nothing loaded",
                    self.key
                ))
            })?,
        };
        let dest_dir = self.root.join(dest_key);
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| GraphStoreError::Backend(format!("create {}: {e}", dest_dir.display())))?;
        let next = max_version_in(&dest_dir) + 1;
        let dest_version = format!("v{next}");
        for src in Self::version_paths(&self.key_dir(), &version) {
            let name = src.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let dst_name = name.replacen(&version, &dest_version, 1);
            std::fs::rename(&src, dest_dir.join(dst_name)).map_err(|e| {
                GraphStoreError::Backend(format!(
                    "publish rename {} → {}/{dest_version}: {e}",
                    src.display(),
                    dest_dir.display()
                ))
            })?;
        }
        Self::flip_current_in(&dest_dir, &dest_version)?;
        Self::gc_versions(&dest_dir);
        // Remove the now-empty staging shell (best-effort).
        let _ = std::fs::remove_file(self.current_path());
        let _ = std::fs::remove_dir(self.key_dir());
        Ok(())
    }

    /// Close whatever handle is open, folding the WAL first when writable.
    pub(crate) async fn close_handle(&self) -> Result<Option<String>> {
        let mut state = self.state.lock().await;
        let Some(h) = state.take() else {
            return Ok(None);
        };
        if h.writable {
            let db = h.db.clone();
            run_blocking(move || {
                let conn = Connection::new(&db)
                    .map_err(|e| GraphStoreError::Backend(format!("ladybug connection: {e}")))?;
                conn.query("CHECKPOINT")
                    .map_err(|e| GraphStoreError::Backend(format!("checkpoint: {e}")))?;
                Ok(())
            })
            .await?;
        }
        // `h.db` drops here; the file lock releases once in-flight queries
        // (which cloned the Arc before this call) finish.
        Ok(Some(h.version))
    }

    /// Best-effort GC: delete version files that are neither `CURRENT` nor the
    /// immediately previous version, and are older than [`GC_GRACE`]. Windows
    /// sharing violations are recorded and retried on the next read or write.
    pub(crate) fn gc_versions(dir: &Path) {
        let current = std::fs::read_to_string(dir.join("CURRENT"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let mut versions: Vec<(u64, String)> = std::fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                let n = name.strip_prefix('v')?.parse::<u64>().ok()?;
                Some((n, name))
            })
            .collect();
        // Keep the current version and the most RECENTLY MODIFIED other one
        // (grace for readers) — recency by mtime, not version number, so a
        // stale higher-numbered orphan can't shadow the real previous version.
        versions.sort_unstable_by_key(|(number, name)| {
            (
                std::cmp::Reverse(
                    dir.join(name)
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                ),
                // Windows may assign identical mtimes to versions created in
                // quick succession. Prefer the higher version in that tie so
                // the retained predecessor is deterministic.
                std::cmp::Reverse(*number),
            )
        });
        let keep: Vec<&str> = std::iter::once(current.as_str())
            .chain(
                versions
                    .iter()
                    .map(|(_, name)| name.as_str())
                    .filter(|name| *name != current)
                    .take(1),
            )
            .collect();
        let pending_path = dir.join(GC_PENDING_FILE);
        let pending: std::collections::HashSet<String> = std::fs::read_to_string(&pending_path)
            .ok()
            .into_iter()
            .flat_map(|contents| {
                contents
                    .lines()
                    .filter(|name| is_safe_version_name(name))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut still_pending = std::collections::BTreeSet::new();
        let mut candidates: std::collections::BTreeSet<String> = pending.iter().cloned().collect();
        for (_, name) in &versions {
            if keep.contains(&name.as_str()) {
                continue;
            }
            let path = dir.join(name);
            let old_enough = path
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > GC_GRACE);
            if old_enough {
                candidates.insert(name.clone());
            }
        }
        for name in candidates {
            if keep.contains(&name.as_str()) || !is_safe_version_name(&name) {
                continue;
            }
            let failed = Self::version_paths(dir, &name).into_iter().any(|path| {
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match result {
                    Ok(()) => false,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(_) => true,
                }
            });
            if failed {
                still_pending.insert(name);
            }
        }
        persist_pending_gc(&pending_path, &still_pending);
    }

    /// Execute `f` with a connection on the current readable database, under
    /// the query limiter. `None` handle → `default` (graph doesn't exist).
    pub(crate) async fn with_read_conn<T, F>(&self, default: T, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let _permit = self.acquire_permit().await?;
        let Some(db) = self.read_handle().await? else {
            return Ok(default);
        };
        run_blocking(move || {
            let conn = Connection::new(&db)
                .map_err(|e| GraphStoreError::Backend(format!("ladybug connection: {e}")))?;
            f(&conn)
        })
        .await
    }

    async fn acquire_permit(&self) -> Result<Option<tokio::sync::OwnedSemaphorePermit>> {
        let Some((sem, timeout)) = &self.limiter else {
            return Ok(None);
        };
        match tokio::time::timeout(*timeout, sem.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Some(permit)),
            Ok(Err(_)) => Err(GraphStoreError::Backend("query limiter closed".into())),
            Err(_) => Err(GraphStoreError::Backend(
                "graph store overloaded: timed out waiting for a query slot".into(),
            )),
        }
    }
}

fn is_safe_version_name(name: &str) -> bool {
    name.strip_prefix('v').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn persist_pending_gc(path: &Path, pending: &std::collections::BTreeSet<String>) {
    if pending.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    static PENDING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = parent.join(format!(
        "{GC_PENDING_FILE}.tmp-{}-{}",
        std::process::id(),
        PENDING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let body = pending.iter().cloned().collect::<Vec<_>>().join("\n") + "\n";
    if std::fs::write(&tmp, body).is_ok() && atomic_replace(&tmp, path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(tmp);
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
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

/// Highest `v<N>` (file or dir) directly under `dir`; 0 when none.
fn max_version_in(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix('v')?
                .parse::<u64>()
                .ok()
        })
        .max()
        .unwrap_or(0)
}

/// `spawn_blocking` wrapper for the synchronous lbug API; falls back to
/// running inline on a current-thread runtime without blocking workers
/// (the engine's `block_on` runtime).
pub(crate) async fn run_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::spawn_blocking(f)
                .await
                .map_err(|e| GraphStoreError::Backend(format!("blocking task: {e}")))?
        }
        _ => f(),
    }
}

#[cfg(test)]
mod lib_tests {
    use super::*;

    #[test]
    fn connect_is_lazy_and_strips_file_prefix() {
        let store =
            LadybugStore::connect("file:///tmp/definitely-absent-cih-root", "k").expect("lazy");
        assert_eq!(store.root, PathBuf::from("/tmp/definitely-absent-cih-root"));
        assert!(LadybugStore::connect("", "k").is_err());
    }

    #[test]
    fn next_version_increments_past_existing() {
        let dir = std::env::temp_dir().join(format!("lbver-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("v3"));
        let store = LadybugStore::connect(
            dir.parent().unwrap().to_str().unwrap(),
            dir.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(store.next_version(), "v4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_replace_overwrites_existing_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("CURRENT");
        let next = dir.path().join("CURRENT.tmp");
        std::fs::write(&current, "v1\n").unwrap();
        std::fs::write(&next, "v2\n").unwrap();
        atomic_replace(&next, &current).unwrap();
        assert_eq!(std::fs::read_to_string(current).unwrap(), "v2\n");
        assert!(!next.exists());
    }

    #[test]
    fn gc_keeps_current_and_previous_but_removes_stale_versions() {
        let dir = tempfile::tempdir().unwrap();
        for version in ["v1", "v2", "v3"] {
            std::fs::write(dir.path().join(version), version).unwrap();
        }
        std::fs::write(dir.path().join("CURRENT"), "v3\n").unwrap();
        // Pending entries bypass the ten-minute age gate and model a retry
        // left by a Windows sharing violation.
        std::fs::write(dir.path().join(GC_PENDING_FILE), "v1\n").unwrap();
        LadybugStore::gc_versions(dir.path());
        assert!(!dir.path().join("v1").exists());
        assert!(dir.path().join("v2").exists());
        assert!(dir.path().join("v3").exists());
        assert!(!dir.path().join(GC_PENDING_FILE).exists());
    }

    #[test]
    fn unicode_and_spaced_graph_paths_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("CIH graphs 数据");
        let store = LadybugStore::connect(&root.to_string_lossy(), "repo 日本語").unwrap();
        std::fs::create_dir_all(store.key_dir()).unwrap();
        std::fs::write(store.key_dir().join("v1"), b"fixture").unwrap();
        store.flip_current("v1").unwrap();
        assert_eq!(store.read_current().as_deref(), Some("v1"));
    }

    #[cfg(windows)]
    #[test]
    fn locked_current_replacement_leaves_previous_pointer_readable() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
        };

        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("CURRENT");
        let next = dir.path().join("CURRENT.tmp");
        std::fs::write(&current, "v1\n").unwrap();
        std::fs::write(&next, "v2\n").unwrap();
        let wide: Vec<u16> = current.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        assert!(atomic_replace(&next, &current).is_err());
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "v1\n");
        unsafe { CloseHandle(handle) };
    }

    #[cfg(windows)]
    #[test]
    fn reader_locked_old_version_is_recorded_and_retried() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
        };

        let dir = tempfile::tempdir().unwrap();
        for version in ["v1", "v2", "v3"] {
            std::fs::write(dir.path().join(version), version).unwrap();
        }
        std::fs::write(dir.path().join("CURRENT"), "v3\n").unwrap();
        std::fs::write(dir.path().join(GC_PENDING_FILE), "v1\n").unwrap();
        let locked = dir.path().join("v1");
        let wide: Vec<u16> = locked.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        LadybugStore::gc_versions(dir.path());
        assert!(locked.exists());
        assert!(dir.path().join(GC_PENDING_FILE).exists());
        unsafe { CloseHandle(handle) };
        LadybugStore::gc_versions(dir.path());
        assert!(!locked.exists());
        assert!(!dir.path().join(GC_PENDING_FILE).exists());
    }
}
