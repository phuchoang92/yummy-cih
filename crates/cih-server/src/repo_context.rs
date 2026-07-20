//! Repository identity and infrastructure resolution.
//!
//! Registry lookup is deliberately fresh on every request. Expensive graph
//! connections and search-state construction are cached independently by their
//! stable normalized keys.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cih_embed::EmbedStore;
use cih_graph_store::GraphStore;

use crate::app_error::AppError;
use crate::search::{SearchCache, SearchState};
use crate::single_flight::SingleFlight;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepoSelector {
    Default,
    NameOrPath(String),
}

impl RepoSelector {
    pub(crate) fn from_wire(value: &str) -> Self {
        if value.trim().is_empty() {
            Self::Default
        } else {
            Self::NameOrPath(value.trim().to_string())
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedRepo {
    pub(crate) registry_entry: cih_core::RegistryEntry,
    pub(crate) canonical_path: PathBuf,
    pub(crate) versioned_artifacts_dir: Option<PathBuf>,
    pub(crate) community_artifacts_dir: Option<PathBuf>,
}

impl ResolvedRepo {
    pub(crate) fn from_entry(entry: cih_core::RegistryEntry) -> Self {
        let canonical_path = normalize_path(Path::new(&entry.path));
        let versioned_artifacts_dir = nonempty_path(&entry.artifacts_dir).map(normalize_path);
        let community_artifacts_dir = entry
            .community_artifacts_dir
            .as_deref()
            .and_then(nonempty_path)
            .map(normalize_path);
        Self {
            registry_entry: entry,
            canonical_path,
            versioned_artifacts_dir,
            community_artifacts_dir,
        }
    }

    pub(crate) fn graph_key(&self) -> &str {
        &self.registry_entry.graph_key
    }

    pub(crate) fn artifacts_root(&self) -> Option<PathBuf> {
        self.versioned_artifacts_dir
            .as_deref()
            .map(unversioned_artifacts_dir)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RepoCatalogSnapshot {
    primary_graph_key: String,
    registry: Arc<cih_core::Registry>,
    groups: Arc<cih_core::GroupRegistry>,
}

impl RepoCatalogSnapshot {
    fn new(
        primary_graph_key: String,
        registry: Arc<cih_core::Registry>,
        groups: Arc<cih_core::GroupRegistry>,
    ) -> Self {
        Self {
            primary_graph_key,
            registry,
            groups,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        primary_graph_key: String,
        registry: cih_core::Registry,
        groups: cih_core::GroupRegistry,
    ) -> Self {
        Self::new(primary_graph_key, Arc::new(registry), Arc::new(groups))
    }

    pub(crate) fn resolve(&self, selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
        resolve_entry(&self.registry, &selector, &self.primary_graph_key)
            .cloned()
            .map(ResolvedRepo::from_entry)
    }

    pub(crate) fn registry(&self) -> &cih_core::Registry {
        &self.registry
    }

    pub(crate) fn groups(&self) -> &cih_core::GroupRegistry {
        &self.groups
    }
}

#[derive(Clone)]
pub(crate) struct RepoContext {
    pub(crate) repo: ResolvedRepo,
    pub(crate) store: Arc<dyn GraphStore>,
    pub(crate) search: SearchState,
}

#[async_trait]
pub(crate) trait RepoContextProvider: Send + Sync {
    fn catalog_snapshot(&self) -> RepoCatalogSnapshot;

    fn resolve_repo(&self, selector: RepoSelector) -> Result<ResolvedRepo, AppError>;

    async fn resolve(&self, selector: RepoSelector) -> Result<Arc<RepoContext>, AppError>;
}

trait RepoCatalog: Send + Sync {
    fn resolve(
        &self,
        selector: &RepoSelector,
        primary_graph_key: &str,
    ) -> Result<cih_core::RegistryEntry, AppError>;

    fn snapshot(&self) -> (Arc<cih_core::Registry>, Arc<cih_core::GroupRegistry>);
}

struct RegistryRepoCatalog;

impl RepoCatalog for RegistryRepoCatalog {
    fn resolve(
        &self,
        selector: &RepoSelector,
        primary_graph_key: &str,
    ) -> Result<cih_core::RegistryEntry, AppError> {
        let registry = cih_core::Registry::load_cached();
        resolve_entry(&registry, selector, primary_graph_key).cloned()
    }

    fn snapshot(&self) -> (Arc<cih_core::Registry>, Arc<cih_core::GroupRegistry>) {
        (
            cih_core::Registry::load_cached(),
            cih_core::GroupRegistry::load_cached(),
        )
    }
}

fn resolve_entry<'a>(
    registry: &'a cih_core::Registry,
    selector: &RepoSelector,
    primary_graph_key: &str,
) -> Result<&'a cih_core::RegistryEntry, AppError> {
    if registry.entries.is_empty() {
        return Err(AppError::InvalidInput {
            field: "repo",
            message: "no repos in registry; run `cih-engine analyze <repo>` first".into(),
        });
    }
    match selector {
        RepoSelector::Default => registry
            .entries
            .iter()
            .find(|entry| entry.graph_key == primary_graph_key)
            .ok_or_else(|| AppError::InvalidInput {
                field: "repo",
                message: format!(
                    "no repo registered for graph_key '{primary_graph_key}'; \
                     pass `repo` explicitly"
                ),
            }),
        RepoSelector::NameOrPath(value) => registry.find(value).ok_or_else(|| AppError::NotFound {
            entity: "repo",
            key: value.clone(),
        }),
    }
}

#[async_trait]
trait RepoInfrastructure: Send + Sync {
    async fn connect_graph(&self, graph_key: &str) -> Result<Arc<dyn GraphStore>, AppError>;
    fn create_search(&self, artifacts_root: Option<PathBuf>) -> SearchState;
}

struct LocalRepoInfrastructure {
    backend: String,
    falkor_url: String,
    store_limits: (usize, Duration),
    embed_store: Option<Arc<EmbedStore>>,
    search_cache: SearchCache,
}

#[async_trait]
impl RepoInfrastructure for LocalRepoInfrastructure {
    async fn connect_graph(&self, graph_key: &str) -> Result<Arc<dyn GraphStore>, AppError> {
        let store = cih_store_factory::connect_store(
            &self.backend,
            &self.falkor_url,
            graph_key,
            &cih_store_factory::StoreOptions {
                query_limit: Some(self.store_limits),
            },
        )
        .map_err(|error| AppError::Unavailable {
            dependency: "graph store",
            message: format!("graph '{graph_key}': {error}"),
            retryable: true,
        })?;
        store
            .ensure_schema()
            .await
            .map_err(|error| AppError::Unavailable {
                dependency: "graph schema",
                message: format!("graph '{graph_key}': {error}"),
                retryable: true,
            })?;
        Ok(store)
    }

    fn create_search(&self, artifacts_root: Option<PathBuf>) -> SearchState {
        SearchState::with_cache(
            artifacts_root,
            self.embed_store.clone(),
            self.search_cache.clone(),
        )
    }
}

pub(crate) struct DefaultRepoContextProvider {
    primary_graph_key: String,
    catalog: Arc<dyn RepoCatalog>,
    infrastructure: Arc<dyn RepoInfrastructure>,
    graphs: SingleFlight<Arc<dyn GraphStore>, AppError>,
    searches: SingleFlight<SearchState>,
}

impl DefaultRepoContextProvider {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn production(
        primary_graph_key: String,
        primary_store: Arc<dyn GraphStore>,
        primary_search: SearchState,
        primary_artifacts_root: Option<PathBuf>,
        backend: String,
        falkor_url: String,
        store_limits: (usize, Duration),
        embed_store: Option<Arc<EmbedStore>>,
        search_cache: SearchCache,
    ) -> Self {
        let search_key = search_cache_key(
            primary_artifacts_root.as_deref().map(normalize_path),
            &primary_graph_key,
        );
        Self::with_parts(
            primary_graph_key.clone(),
            Arc::new(RegistryRepoCatalog),
            Arc::new(LocalRepoInfrastructure {
                backend,
                falkor_url,
                store_limits,
                embed_store,
                search_cache,
            }),
            [(primary_graph_key, primary_store)],
            [(search_key, primary_search)],
        )
    }

    fn with_parts(
        primary_graph_key: String,
        catalog: Arc<dyn RepoCatalog>,
        infrastructure: Arc<dyn RepoInfrastructure>,
        graphs: impl IntoIterator<Item = (String, Arc<dyn GraphStore>)>,
        searches: impl IntoIterator<Item = (String, SearchState)>,
    ) -> Self {
        Self {
            primary_graph_key,
            catalog,
            infrastructure,
            graphs: SingleFlight::with(graphs),
            searches: SingleFlight::with(searches),
        }
    }
}

#[async_trait]
impl RepoContextProvider for DefaultRepoContextProvider {
    fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
        let (registry, groups) = self.catalog.snapshot();
        RepoCatalogSnapshot::new(self.primary_graph_key.clone(), registry, groups)
    }

    fn resolve_repo(&self, selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
        self.catalog
            .resolve(&selector, &self.primary_graph_key)
            .map(ResolvedRepo::from_entry)
    }

    async fn resolve(&self, selector: RepoSelector) -> Result<Arc<RepoContext>, AppError> {
        let repo = self.resolve_repo(selector)?;
        let graph_key = repo.graph_key().to_string();
        let graph_key_for_init = graph_key.clone();
        let infrastructure = self.infrastructure.clone();
        let store = self
            .graphs
            .get_or_try_init(&graph_key, || async move {
                infrastructure.connect_graph(&graph_key_for_init).await
            })
            .await?;

        let artifacts_root = repo.artifacts_root();
        let search_key = search_cache_key(artifacts_root.clone(), repo.graph_key());
        let infrastructure = self.infrastructure.clone();
        let search = self
            .searches
            .get_or_init(&search_key, || async move {
                infrastructure.create_search(artifacts_root)
            })
            .await;

        Ok(Arc::new(RepoContext {
            repo,
            store,
            search,
        }))
    }
}

fn nonempty_path(value: &str) -> Option<&Path> {
    (!value.trim().is_empty()).then(|| Path::new(value))
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

pub(crate) fn unversioned_artifacts_dir(versioned: &Path) -> PathBuf {
    versioned
        .parent()
        .map(normalize_path)
        .unwrap_or_else(|| normalize_path(versioned))
}

fn search_cache_key(artifacts_root: Option<PathBuf>, graph_key: &str) -> String {
    match artifacts_root {
        Some(root) => format!("artifacts:{}", normalize_path(&root).display()),
        None => format!("no-artifacts:{graph_key}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::RwLock;

    struct TestCatalog {
        entries: RwLock<HashMap<String, cih_core::RegistryEntry>>,
    }

    impl TestCatalog {
        fn new(entries: impl IntoIterator<Item = cih_core::RegistryEntry>) -> Self {
            Self {
                entries: RwLock::new(
                    entries
                        .into_iter()
                        .map(|entry| (entry.name.clone(), entry))
                        .collect(),
                ),
            }
        }

        fn replace(&self, entry: cih_core::RegistryEntry) {
            self.entries
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .insert(entry.name.clone(), entry);
        }
    }

    impl RepoCatalog for TestCatalog {
        fn resolve(
            &self,
            selector: &RepoSelector,
            primary_graph_key: &str,
        ) -> Result<cih_core::RegistryEntry, AppError> {
            let entries = self
                .entries
                .read()
                .unwrap_or_else(|error| error.into_inner());
            let found = match selector {
                RepoSelector::Default => entries
                    .values()
                    .find(|entry| entry.graph_key == primary_graph_key),
                RepoSelector::NameOrPath(value) => entries
                    .values()
                    .find(|entry| entry.name == *value || entry.path == *value),
            };
            found.cloned().ok_or_else(|| AppError::NotFound {
                entity: "repo",
                key: format!("{selector:?}"),
            })
        }

        fn snapshot(&self) -> (Arc<cih_core::Registry>, Arc<cih_core::GroupRegistry>) {
            let entries = self
                .entries
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .values()
                .cloned()
                .collect();
            (
                Arc::new(cih_core::Registry { entries }),
                Arc::new(cih_core::GroupRegistry::default()),
            )
        }
    }

    struct TestInfrastructure {
        store: Arc<dyn GraphStore>,
        graph_calls: AtomicUsize,
        search_calls: AtomicUsize,
        fail_graph_calls: AtomicUsize,
        active_graph_calls: AtomicUsize,
        max_active_graph_calls: AtomicUsize,
        delay: Duration,
    }

    #[async_trait]
    impl RepoInfrastructure for TestInfrastructure {
        async fn connect_graph(&self, _graph_key: &str) -> Result<Arc<dyn GraphStore>, AppError> {
            self.graph_calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active_graph_calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active_graph_calls
                .fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.active_graph_calls.fetch_sub(1, Ordering::SeqCst);
            if self
                .fail_graph_calls
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(AppError::Unavailable {
                    dependency: "test graph",
                    message: "planned failure".into(),
                    retryable: true,
                });
            }
            Ok(self.store.clone())
        }

        fn create_search(&self, artifacts_root: Option<PathBuf>) -> SearchState {
            self.search_calls.fetch_add(1, Ordering::SeqCst);
            SearchState::new(artifacts_root, None)
        }
    }

    fn lazy_store() -> Arc<dyn GraphStore> {
        cih_store_factory::connect_store(
            "falkor",
            "redis://127.0.0.1:6380",
            "repo_context_test",
            &cih_store_factory::StoreOptions::default(),
        )
        .expect("lazy test store")
    }

    fn entry(
        name: &str,
        repo: &Path,
        graph_key: &str,
        artifacts: &Path,
    ) -> cih_core::RegistryEntry {
        cih_core::RegistryEntry {
            name: name.into(),
            path: repo.display().to_string(),
            graph_key: graph_key.into(),
            artifacts_dir: artifacts.display().to_string(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        }
    }

    fn infrastructure(fail_graph_calls: usize, delay: Duration) -> Arc<TestInfrastructure> {
        Arc::new(TestInfrastructure {
            store: lazy_store(),
            graph_calls: AtomicUsize::new(0),
            search_calls: AtomicUsize::new(0),
            fail_graph_calls: AtomicUsize::new(fail_graph_calls),
            active_graph_calls: AtomicUsize::new(0),
            max_active_graph_calls: AtomicUsize::new(0),
            delay,
        })
    }

    fn provider(
        primary_graph_key: &str,
        catalog: Arc<dyn RepoCatalog>,
        infrastructure: Arc<dyn RepoInfrastructure>,
    ) -> DefaultRepoContextProvider {
        DefaultRepoContextProvider::with_parts(
            primary_graph_key.into(),
            catalog,
            infrastructure,
            [],
            [],
        )
    }

    #[test]
    fn versioned_artifacts_dir_maps_to_normalized_parent() {
        assert_eq!(
            unversioned_artifacts_dir(Path::new("/repo/.cih/artifacts/b5bb9fb09e9b7a16")),
            PathBuf::from("/repo/.cih/artifacts")
        );
        assert_ne!(
            unversioned_artifacts_dir(Path::new("/a/.cih/artifacts/deadbeef")),
            unversioned_artifacts_dir(Path::new("/b/.cih/artifacts/deadbeef"))
        );
    }

    #[test]
    fn identity_only_resolution_does_not_initialize_infrastructure() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join(".cih/artifacts/v1");
        std::fs::create_dir_all(&artifacts).unwrap();
        let catalog = Arc::new(TestCatalog::new([entry(
            "repo",
            temp.path(),
            "graph",
            &artifacts,
        )]));
        let infra = infrastructure(0, Duration::ZERO);
        let provider = provider("primary", catalog, infra.clone());

        let repo = provider
            .resolve_repo(RepoSelector::NameOrPath("repo".into()))
            .unwrap();

        assert_eq!(repo.graph_key(), "graph");
        assert_eq!(repo.canonical_path, temp.path().canonicalize().unwrap());
        assert_eq!(infra.graph_calls.load(Ordering::SeqCst), 0);
        assert_eq!(infra.search_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn catalog_snapshot_is_stable_across_later_catalog_refreshes() {
        let temp = tempfile::tempdir().unwrap();
        let v1 = temp.path().join(".cih/artifacts/v1");
        let v2 = temp.path().join(".cih/artifacts/v2");
        std::fs::create_dir_all(&v1).unwrap();
        std::fs::create_dir_all(&v2).unwrap();
        let catalog = Arc::new(TestCatalog::new([entry(
            "repo",
            temp.path(),
            "old-key",
            &v1,
        )]));
        let infra = infrastructure(0, Duration::ZERO);
        let provider = provider("primary", catalog.clone(), infra.clone());

        let snapshot = provider.catalog_snapshot();
        catalog.replace(entry("repo", temp.path(), "new-key", &v2));

        let old = snapshot
            .resolve(RepoSelector::NameOrPath("repo".into()))
            .unwrap();
        let new = provider
            .catalog_snapshot()
            .resolve(RepoSelector::NameOrPath("repo".into()))
            .unwrap();
        assert_eq!(old.graph_key(), "old-key");
        assert_eq!(new.graph_key(), "new-key");
        assert_eq!(infra.graph_calls.load(Ordering::SeqCst), 0);
        assert_eq!(infra.search_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_same_key_resolves_one_graph_and_one_search() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join(".cih/artifacts/v1");
        std::fs::create_dir_all(&artifacts).unwrap();
        let catalog = Arc::new(TestCatalog::new([entry(
            "repo",
            temp.path(),
            "graph",
            &artifacts,
        )]));
        let infra = infrastructure(0, Duration::from_millis(30));
        let provider = Arc::new(provider("primary", catalog, infra.clone()));
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let provider = provider.clone();
            tasks.push(tokio::spawn(async move {
                provider
                    .resolve(RepoSelector::NameOrPath("repo".into()))
                    .await
                    .unwrap()
            }));
        }
        let mut contexts = Vec::new();
        for task in tasks {
            contexts.push(task.await.unwrap());
        }
        assert_eq!(infra.graph_calls.load(Ordering::SeqCst), 1);
        assert_eq!(infra.search_calls.load(Ordering::SeqCst), 1);
        assert!(contexts
            .iter()
            .all(|context| Arc::ptr_eq(&context.store, &contexts[0].store)));
    }

    #[tokio::test]
    async fn failed_graph_initialization_is_retried() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join(".cih/artifacts/v1");
        std::fs::create_dir_all(&artifacts).unwrap();
        let catalog = Arc::new(TestCatalog::new([entry(
            "repo",
            temp.path(),
            "graph",
            &artifacts,
        )]));
        let infra = infrastructure(1, Duration::from_millis(1));
        let provider = provider("primary", catalog, infra.clone());
        let selector = RepoSelector::NameOrPath("repo".into());
        assert!(provider.resolve(selector.clone()).await.is_err());
        assert!(provider.resolve(selector).await.is_ok());
        assert_eq!(infra.graph_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_failed_resolves_share_error_then_later_resolve_retries() {
        const CALLERS: usize = 32;
        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join(".cih/artifacts/v1");
        std::fs::create_dir_all(&artifacts).unwrap();
        let catalog = Arc::new(TestCatalog::new([entry(
            "repo",
            temp.path(),
            "graph",
            &artifacts,
        )]));
        let infra = infrastructure(1, Duration::from_millis(100));
        let provider = Arc::new(provider("primary", catalog, infra.clone()));
        let start = Arc::new(tokio::sync::Barrier::new(CALLERS + 1));
        let mut tasks = Vec::new();
        for _ in 0..CALLERS {
            let provider = provider.clone();
            let start = start.clone();
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                provider
                    .resolve(RepoSelector::NameOrPath("repo".into()))
                    .await
            }));
        }
        start.wait().await;

        for task in tasks {
            let error = match task.await.unwrap() {
                Ok(_) => panic!("current waiter unexpectedly retried after the shared failure"),
                Err(error) => error,
            };
            assert_eq!(error.to_string(), "test graph unavailable: planned failure");
        }
        assert_eq!(infra.graph_calls.load(Ordering::SeqCst), 1);

        provider
            .resolve(RepoSelector::NameOrPath("repo".into()))
            .await
            .unwrap();
        assert_eq!(infra.graph_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_graph_keys_initialize_concurrently() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let av = a.join(".cih/artifacts/v1");
        let bv = b.join(".cih/artifacts/v1");
        std::fs::create_dir_all(&av).unwrap();
        std::fs::create_dir_all(&bv).unwrap();
        let catalog = Arc::new(TestCatalog::new([
            entry("a", &a, "a-key", &av),
            entry("b", &b, "b-key", &bv),
        ]));
        let infra = infrastructure(0, Duration::from_millis(50));
        let provider = Arc::new(provider("primary", catalog, infra.clone()));
        let (a_result, b_result) = tokio::join!(
            provider.resolve(RepoSelector::NameOrPath("a".into())),
            provider.resolve(RepoSelector::NameOrPath("b".into()))
        );
        a_result.unwrap();
        b_result.unwrap();
        assert_eq!(infra.max_active_graph_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn equivalent_artifact_paths_share_search_state() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared/artifacts");
        let version = shared.join("v1");
        std::fs::create_dir_all(&version).unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let catalog = Arc::new(TestCatalog::new([
            entry("a", &a, "a-key", &version),
            entry("b", &b, "b-key", &shared.join("./v1")),
        ]));
        let infra = infrastructure(0, Duration::ZERO);
        let provider = provider("primary", catalog, infra.clone());
        provider
            .resolve(RepoSelector::NameOrPath("a".into()))
            .await
            .unwrap();
        provider
            .resolve(RepoSelector::NameOrPath("b".into()))
            .await
            .unwrap();
        assert_eq!(infra.search_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn catalog_refresh_replaces_repository_identity() {
        let temp = tempfile::tempdir().unwrap();
        let v1 = temp.path().join(".cih/artifacts/v1");
        let v2 = temp.path().join(".cih/artifacts/v2");
        std::fs::create_dir_all(&v1).unwrap();
        std::fs::create_dir_all(&v2).unwrap();
        let catalog = Arc::new(TestCatalog::new([entry(
            "repo",
            temp.path(),
            "old-key",
            &v1,
        )]));
        let infra = infrastructure(0, Duration::ZERO);
        let provider = provider("primary", catalog.clone(), infra.clone());
        let old = provider
            .resolve(RepoSelector::NameOrPath("repo".into()))
            .await
            .unwrap();
        catalog.replace(entry("repo", temp.path(), "new-key", &v2));
        let new = provider
            .resolve(RepoSelector::NameOrPath("repo".into()))
            .await
            .unwrap();
        assert_eq!(old.repo.graph_key(), "old-key");
        assert_eq!(new.repo.graph_key(), "new-key");
        assert_ne!(
            old.repo.versioned_artifacts_dir,
            new.repo.versioned_artifacts_dir
        );
        assert_eq!(infra.graph_calls.load(Ordering::SeqCst), 2);
    }
}
