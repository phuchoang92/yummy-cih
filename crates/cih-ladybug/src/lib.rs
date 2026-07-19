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
//! moved (in-flight queries keep the old `Arc<Database>` — and, on POSIX, its
//! unlinked file — alive until they finish). The server's forever per-key
//! store cache never needs invalidating.
//!
//! POSIX-only for now: GC deletes version files readers may hold open, which
//! Windows forbids.

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
        // once if GC removed the dir between reading CURRENT and opening.
        for _ in 0..2 {
            let current = match self.read_current() {
                Some(v) => v,
                None => return Ok(None),
            };
            let dir = self.key_dir().join(&current);
            match Self::open_db(&dir, true) {
                Ok(db) => {
                    *state = Some(OpenHandle {
                        version: current,
                        db: db.clone(),
                        writable: false,
                    });
                    return Ok(Some(db));
                }
                Err(_) if !dir.exists() => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// A read-write handle on a version dir owned by this store, creating the
    /// next version (with schema) when none is open yet. Multi-set loads and
    /// the delta path reuse the open handle.
    pub(crate) async fn write_handle(&self) -> Result<Arc<Database>> {
        let mut state = self.state.lock().await;
        if let Some(h) = state.as_ref() {
            if h.writable {
                return Ok(h.db.clone());
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
        // Point CURRENT at the version being built so reads on this key (same
        // store or a later reader) see the load result without a publish —
        // `bulk_load` alone must leave the graph queryable (contract suite).
        self.flip_current(&version)?;
        *state = Some(OpenHandle {
            version,
            db: db.clone(),
            writable: true,
        });
        Ok(db)
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
        std::fs::create_dir_all(dir)
            .map_err(|e| GraphStoreError::Backend(format!("create {}: {e}", dir.display())))?;
        let tmp = dir.join(format!("CURRENT.tmp-{}", std::process::id()));
        std::fs::write(&tmp, format!("{version}\n"))
            .map_err(|e| GraphStoreError::Backend(format!("write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, dir.join("CURRENT"))
            .map_err(|e| GraphStoreError::Backend(format!("flip CURRENT: {e}")))?;
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
    /// immediately previous version, and are older than [`GC_GRACE`].
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
        versions.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
        // Keep the current version and the next-newest one (grace for readers).
        let keep: Vec<&str> = std::iter::once(current.as_str())
            .chain(
                versions
                    .iter()
                    .map(|(_, name)| name.as_str())
                    .filter(|name| *name != current)
                    .take(1),
            )
            .collect();
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
                for p in Self::version_paths(dir, name) {
                    let _ = if p.is_dir() {
                        std::fs::remove_dir_all(&p)
                    } else {
                        std::fs::remove_file(&p)
                    };
                }
            }
        }
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
}
