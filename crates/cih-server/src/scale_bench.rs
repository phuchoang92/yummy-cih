//! Deterministic large-artifact fixture and measurement harness.
//!
//! This module is public only so the `scale_bench` example can exercise the
//! server's crate-private adapters. It is not part of the supported server API.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range, RegistryEntry, RegistryStats};
use cih_search::{SearchIndex, TextIndex};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::domain::repository::ResolvedRepo;
use crate::infrastructure::artifact_repository::ArtifactCache;
use crate::ports::artifact_repository::ArtifactRepository;

const FIXTURE_SCHEMA: u32 = 1;
const EVENT_LOOP_SAMPLE_MS: u64 = 5;

#[derive(Clone, Debug)]
pub struct ScaleConfig {
    pub fixture_dir: PathBuf,
    pub nodes: usize,
    pub edges_per_node: usize,
    pub iterations: usize,
    pub burst_callers: usize,
    pub regenerate: bool,
}

impl ScaleConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.nodes > 1, "nodes must be greater than one");
        anyhow::ensure!(
            self.edges_per_node > 0,
            "edges_per_node must be greater than zero"
        );
        anyhow::ensure!(self.iterations > 0, "iterations must be greater than zero");
        anyhow::ensure!(
            self.burst_callers > 0,
            "burst_callers must be greater than zero"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureMetadata {
    pub schema: u32,
    pub nodes: usize,
    pub edges: usize,
    pub edges_per_node: usize,
    pub communities: usize,
}

#[derive(Debug, Serialize)]
pub struct ScaleReport {
    pub generated_at_unix_secs: u64,
    pub machine: MachineInfo,
    pub fixture: FixtureReport,
    pub memory: MemoryReport,
    pub artifact_load: ArtifactLoadReport,
    pub search: SearchReport,
    pub wiki_search: WikiSearchReport,
    pub resource_scan: ResourceScanReport,
    pub same_key_cold_burst: ColdBurstReport,
    pub acceptance: Vec<AcceptanceResult>,
}

#[derive(Debug, Serialize)]
pub struct MachineInfo {
    pub os: &'static str,
    pub architecture: &'static str,
    pub logical_cpus: usize,
    pub build_profile: &'static str,
}

#[derive(Debug, Serialize)]
pub struct FixtureReport {
    pub directory: String,
    pub reused: bool,
    pub generation_ms: f64,
    pub nodes: usize,
    pub edges: usize,
    pub communities: usize,
    pub nodes_bytes: u64,
    pub edges_bytes: u64,
    pub communities_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct MemoryReport {
    pub baseline_rss_bytes: Option<u64>,
    pub after_artifact_load_rss_bytes: Option<u64>,
    pub after_artifact_indexes_rss_bytes: Option<u64>,
    pub after_search_index_rss_bytes: Option<u64>,
    pub observed_peak_rss_bytes: Option<u64>,
    pub artifact_estimated_bytes: usize,
    pub search_estimated_bytes: usize,
    pub wiki_search_estimated_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct ArtifactLoadReport {
    pub cold_parse_ms: f64,
    pub lazy_index_build_ms: f64,
    pub cache_hits: Distribution,
    pub event_loop_delay_during_cold_load: Distribution,
}

#[derive(Debug, Serialize)]
pub struct SearchReport {
    pub indexed_documents: usize,
    pub build_ms: f64,
    pub query: String,
    pub returned_hits: usize,
    pub warm_queries: Distribution,
}

#[derive(Debug, Serialize)]
pub struct WikiSearchReport {
    pub indexed_documents: usize,
    pub build_ms: f64,
    pub query: String,
    pub returned_hits: usize,
    pub warm_queries: Distribution,
}

#[derive(Debug, Serialize)]
pub struct ResourceScanReport {
    pub page_size: usize,
    pub first_page: Distribution,
    pub middle_page: Distribution,
    pub tail_page: Distribution,
}

#[derive(Debug, Serialize)]
pub struct ColdBurstReport {
    pub callers: usize,
    pub elapsed_ms: f64,
    pub loader_builds: u64,
    pub all_callers_shared_snapshot: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Distribution {
    pub samples: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl Distribution {
    fn from_durations(samples: Vec<Duration>) -> Self {
        let values = samples
            .into_iter()
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .collect();
        Self::from_millis(values)
    }

    fn from_millis(mut values: Vec<f64>) -> Self {
        values.sort_by(f64::total_cmp);
        Self {
            samples: values.len(),
            p50_ms: percentile(&values, 0.50),
            p95_ms: percentile(&values, 0.95),
            p99_ms: percentile(&values, 0.99),
            max_ms: values.last().copied().unwrap_or(0.0),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AcceptanceResult {
    pub name: &'static str,
    pub target: String,
    pub observed: String,
    pub passed: bool,
}

pub async fn run(config: ScaleConfig) -> Result<ScaleReport> {
    config.validate()?;
    let generation_started = Instant::now();
    let (metadata, reused) = ensure_fixture(&config)?;
    let generation_ms = generation_started.elapsed().as_secs_f64() * 1000.0;
    let paths = FixturePaths::new(&config.fixture_dir);
    let baseline_rss = current_rss_bytes();

    let repo = fixture_repo(&config.fixture_dir);
    let artifacts = ArtifactCache::default();
    let (stop_delay, delay_rx) = watch::channel(false);
    let delay_monitor = tokio::spawn(monitor_event_loop_delay(delay_rx));

    let cold_started = Instant::now();
    let snapshot = artifacts.snapshot(&repo).await?;
    let cold_parse_ms = cold_started.elapsed().as_secs_f64() * 1000.0;
    stop_delay.send_replace(true);
    let event_loop_delay = delay_monitor
        .await
        .context("event-loop delay monitor panicked")?;
    let after_artifact_load_rss = current_rss_bytes();

    let index_started = Instant::now();
    let indexed_snapshot = artifacts.indexed_snapshot(&repo).await?;
    let lazy_index_build_ms = index_started.elapsed().as_secs_f64() * 1000.0;
    let after_artifact_indexes_rss = current_rss_bytes();

    let cache_hits = measure_async(config.iterations, || artifacts.snapshot(&repo)).await?;

    let search_started = Instant::now();
    let search_index = SearchIndex::build(&snapshot.nodes);
    let search_build_ms = search_started.elapsed().as_secs_f64() * 1000.0;
    let after_search_index_rss = current_rss_bytes();
    let search_query = "process order service";
    let (search_queries, search_hits) =
        measure_sync(config.iterations, || search_index.search(search_query, 25));

    let wiki_documents = synthetic_wiki_documents(metadata.communities);
    let wiki_started = Instant::now();
    let wiki_index = TextIndex::build(wiki_documents.iter().map(String::as_str));
    let wiki_build_ms = wiki_started.elapsed().as_secs_f64() * 1000.0;
    let wiki_query = "order processing workflow";
    let (wiki_queries, wiki_hits) =
        measure_sync(config.iterations, || wiki_index.search(wiki_query, 25));

    let page_size = 100usize;
    let middle_offset = metadata.communities.saturating_div(2);
    let tail_offset = metadata.communities.saturating_sub(page_size);
    let first_page = measure_resource_scan(&paths.communities, 0, page_size, config.iterations)?;
    let middle_page = measure_resource_scan(
        &paths.communities,
        middle_offset,
        page_size,
        config.iterations,
    )?;
    let tail_page = measure_resource_scan(
        &paths.communities,
        tail_offset,
        page_size,
        config.iterations,
    )?;

    let artifact_estimated_bytes = indexed_snapshot.estimated_weight_bytes();
    let search_estimated_bytes = search_index.estimated_size_bytes();
    let wiki_search_estimated_bytes = wiki_index.estimated_size_bytes();
    let observed_peak_rss = [
        baseline_rss,
        after_artifact_load_rss,
        after_artifact_indexes_rss,
        after_search_index_rss,
        current_rss_bytes(),
    ]
    .into_iter()
    .flatten()
    .max();

    drop(wiki_index);
    drop(wiki_documents);
    drop(search_index);
    drop(indexed_snapshot);
    drop(snapshot);
    drop(artifacts);

    let burst = measure_same_key_cold_burst(&repo, config.burst_callers).await?;
    let artifact_cache_hits = Distribution::from_durations(cache_hits);
    let event_loop_delay = Distribution::from_durations(event_loop_delay);
    let search_queries = Distribution::from_durations(search_queries);
    let wiki_queries = Distribution::from_durations(wiki_queries);
    let tail_page_distribution = Distribution::from_durations(tail_page);

    let acceptance = vec![
        acceptance_below(
            "artifact_cache_hit_p95",
            5.0,
            artifact_cache_hits.p95_ms,
            "ms",
        ),
        acceptance_below("search_query_p95", 500.0, search_queries.p95_ms, "ms"),
        acceptance_below("event_loop_delay_p99", 50.0, event_loop_delay.p99_ms, "ms"),
        // Tight on purpose: paging is index-backed, so tail-page cost no longer
        // grows with offset (~0.04 ms at 500k/50k records). A regression to the
        // old full-scan behaviour measured ~15 ms here and would trip this.
        acceptance_below(
            "resource_tail_page_p95",
            5.0,
            tail_page_distribution.p95_ms,
            "ms",
        ),
        AcceptanceResult {
            name: "same_key_cold_load_single_flight",
            target: "exactly 1 loader build".into(),
            observed: format!("{} loader builds", burst.loader_builds),
            passed: burst.loader_builds == 1 && burst.all_callers_shared_snapshot,
        },
    ];

    Ok(ScaleReport {
        generated_at_unix_secs: unix_now_secs(),
        machine: MachineInfo {
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            logical_cpus: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
        },
        fixture: FixtureReport {
            directory: config.fixture_dir.display().to_string(),
            reused,
            generation_ms,
            nodes: metadata.nodes,
            edges: metadata.edges,
            communities: metadata.communities,
            nodes_bytes: file_len(&paths.nodes)?,
            edges_bytes: file_len(&paths.edges)?,
            communities_bytes: file_len(&paths.communities)?,
        },
        memory: MemoryReport {
            baseline_rss_bytes: baseline_rss,
            after_artifact_load_rss_bytes: after_artifact_load_rss,
            after_artifact_indexes_rss_bytes: after_artifact_indexes_rss,
            after_search_index_rss_bytes: after_search_index_rss,
            observed_peak_rss_bytes: observed_peak_rss,
            artifact_estimated_bytes,
            search_estimated_bytes,
            wiki_search_estimated_bytes,
        },
        artifact_load: ArtifactLoadReport {
            cold_parse_ms,
            lazy_index_build_ms,
            cache_hits: artifact_cache_hits,
            event_loop_delay_during_cold_load: event_loop_delay,
        },
        search: SearchReport {
            indexed_documents: search_index_len(metadata.nodes),
            build_ms: search_build_ms,
            query: search_query.into(),
            returned_hits: search_hits.len(),
            warm_queries: search_queries,
        },
        wiki_search: WikiSearchReport {
            indexed_documents: metadata.communities,
            build_ms: wiki_build_ms,
            query: wiki_query.into(),
            returned_hits: wiki_hits.len(),
            warm_queries: wiki_queries,
        },
        resource_scan: ResourceScanReport {
            page_size,
            first_page: Distribution::from_durations(first_page),
            middle_page: Distribution::from_durations(middle_page),
            tail_page: tail_page_distribution,
        },
        same_key_cold_burst: burst,
        acceptance,
    })
}

fn ensure_fixture(config: &ScaleConfig) -> Result<(FixtureMetadata, bool)> {
    let paths = FixturePaths::new(&config.fixture_dir);
    let expected = FixtureMetadata {
        schema: FIXTURE_SCHEMA,
        nodes: config.nodes,
        edges: config.nodes.saturating_mul(config.edges_per_node),
        edges_per_node: config.edges_per_node,
        communities: config.nodes.div_ceil(10),
    };
    if !config.regenerate
        && paths.nodes.is_file()
        && paths.edges.is_file()
        && paths.communities.is_file()
        && read_metadata(&paths.metadata).as_ref() == Some(&expected)
    {
        return Ok((expected, true));
    }

    fs::create_dir_all(&config.fixture_dir)
        .with_context(|| format!("create fixture directory {}", config.fixture_dir.display()))?;
    write_nodes(&paths.nodes, config.nodes)?;
    write_edges(&paths.edges, config.nodes, config.edges_per_node)?;
    write_communities(&paths.communities, expected.communities)?;
    let metadata = serde_json::to_vec_pretty(&expected)?;
    fs::write(&paths.metadata, metadata)
        .with_context(|| format!("write {}", paths.metadata.display()))?;
    Ok((expected, false))
}

fn write_nodes(path: &Path, count: usize) -> Result<()> {
    let mut writer =
        BufWriter::new(File::create(path).with_context(|| format!("create {}", path.display()))?);
    for index in 0..count {
        let service = index / 1_000;
        let node = Node {
            id: NodeId::new(format!(
                "Method:scale.Service{service}#processOrder{index}/1"
            )),
            kind: NodeKind::Method,
            name: format!("processOrder{index}"),
            qualified_name: Some(format!(
                "com.scale.service{service}.OrderService.processOrder{index}"
            )),
            file: format!("src/service_{service:05}/OrderService{}.java", index % 100),
            range: Range {
                start_line: (index % 2_000) as u32 + 1,
                start_col: 0,
                end_line: (index % 2_000) as u32 + 3,
                end_col: 1,
            },
            props: Some(serde_json::json!({
                "language": "java",
                "stereotype": "service",
                "module": format!("service-{service}")
            })),
        };
        serde_json::to_writer(&mut writer, &node)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

fn write_edges(path: &Path, nodes: usize, edges_per_node: usize) -> Result<()> {
    let mut writer =
        BufWriter::new(File::create(path).with_context(|| format!("create {}", path.display()))?);
    const OFFSETS: [usize; 8] = [1, 7, 31, 127, 509, 2_039, 8_191, 32_749];
    for source in 0..nodes {
        let source_service = source / 1_000;
        for edge_index in 0..edges_per_node {
            let offset =
                OFFSETS[edge_index % OFFSETS.len()].saturating_add(edge_index / OFFSETS.len());
            let target = (source + offset) % nodes;
            let target_service = target / 1_000;
            let edge = Edge::new(
                NodeId::new(format!(
                    "Method:scale.Service{source_service}#processOrder{source}/1"
                )),
                NodeId::new(format!(
                    "Method:scale.Service{target_service}#processOrder{target}/1"
                )),
                EdgeKind::Calls,
                1.0,
                "scale-fixture".into(),
            );
            serde_json::to_writer(&mut writer, &edge)?;
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn write_communities(path: &Path, count: usize) -> Result<()> {
    let mut writer =
        BufWriter::new(File::create(path).with_context(|| format!("create {}", path.display()))?);
    for index in 0..count {
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "kind": "Community",
                "id": format!("community-{index:06}"),
                "name": format!("Order Processing Feature {index}"),
                "confidence": 0.95,
                "member_count": 10
            }),
        )?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

async fn measure_same_key_cold_burst(
    repo: &ResolvedRepo,
    callers: usize,
) -> Result<ColdBurstReport> {
    let cache = ArtifactCache::default();
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..callers {
        let cache = cache.clone();
        let repo = repo.clone();
        tasks.spawn(async move { cache.snapshot(&repo).await });
    }
    let mut snapshots = Vec::with_capacity(callers);
    while let Some(result) = tasks.join_next().await {
        snapshots.push(result.context("cold-burst task panicked")??);
    }
    let all_callers_shared_snapshot = snapshots
        .first()
        .is_none_or(|first| snapshots.iter().all(|item| Arc::ptr_eq(first, item)));
    Ok(ColdBurstReport {
        callers,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        loader_builds: cache.metrics().builds,
        all_callers_shared_snapshot,
    })
}

async fn measure_async<T, Fut, F>(iterations: usize, mut operation: F) -> Result<Vec<Duration>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, crate::domain::error::AppError>>,
{
    let mut durations = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let _ = operation().await?;
        durations.push(started.elapsed());
    }
    Ok(durations)
}

fn measure_sync<T, F>(iterations: usize, mut operation: F) -> (Vec<Duration>, T)
where
    F: FnMut() -> T,
{
    let mut durations = Vec::with_capacity(iterations);
    let mut last = None;
    for _ in 0..iterations {
        let started = Instant::now();
        last = Some(operation());
        durations.push(started.elapsed());
    }
    (
        durations,
        last.expect("iterations are validated as non-zero"),
    )
}

fn measure_resource_scan(
    path: &Path,
    offset: usize,
    page_size: usize,
    iterations: usize,
) -> Result<Vec<Duration>> {
    let mut durations = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let count = crate::transport::mcp::resources::benchmark_scan_jsonl_candidates(
            path,
            "Community",
            offset,
            page_size + 1,
        )
        .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(count > 0, "resource scan returned no records");
        durations.push(started.elapsed());
    }
    Ok(durations)
}

async fn monitor_event_loop_delay(mut stop: watch::Receiver<bool>) -> Vec<Duration> {
    let period = Duration::from_millis(EVENT_LOOP_SAMPLE_MS);
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut expected = Instant::now() + period;
    let mut delays = Vec::new();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let now = Instant::now();
                delays.push(now.saturating_duration_since(expected));
                expected = now + period;
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
        }
    }
    if delays.is_empty() {
        delays.push(Duration::ZERO);
    }
    delays
}

fn synthetic_wiki_documents(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| {
            format!(
                "# Order Processing Feature {index}\n\
                 Handles order validation, payment coordination, fulfillment workflow, \
                 and service integration for module {}.",
                index / 100
            )
        })
        .collect()
}

fn fixture_repo(fixture_dir: &Path) -> ResolvedRepo {
    ResolvedRepo::from_entry(RegistryEntry {
        name: "scale-fixture".into(),
        path: fixture_dir.display().to_string(),
        graph_key: "scale-fixture".into(),
        artifacts_dir: fixture_dir.display().to_string(),
        community_artifacts_dir: Some(fixture_dir.display().to_string()),
        indexed_at: String::new(),
        last_git_head: None,
        stats: RegistryStats::default(),
    })
}

fn search_index_len(nodes: usize) -> usize {
    nodes
}

fn acceptance_below(
    name: &'static str,
    target: f64,
    observed: f64,
    unit: &'static str,
) -> AcceptanceResult {
    AcceptanceResult {
        name,
        target: format!("<= {target:.3} {unit}"),
        observed: format!("{observed:.3} {unit}"),
        passed: observed <= target,
    }
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let rank = (quantile * values.len() as f64).ceil() as usize;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn current_rss_bytes() -> Option<u64> {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        if let Some(kib) = status.lines().find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        }) {
            return Some(kib.saturating_mul(1024));
        }
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kib| kib.saturating_mul(1024))
}

fn read_metadata(path: &Path) -> Option<FixtureMetadata> {
    fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
}

fn file_len(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len())
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

struct FixturePaths {
    nodes: PathBuf,
    edges: PathBuf,
    communities: PathBuf,
    metadata: PathBuf,
}

impl FixturePaths {
    fn new(root: &Path) -> Self {
        Self {
            nodes: root.join("nodes.jsonl"),
            edges: root.join("edges.jsonl"),
            communities: root.join("communities.jsonl"),
            metadata: root.join("fixture.json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&values, 0.50), 3.0);
        assert_eq!(percentile(&values, 0.95), 5.0);
    }

    #[test]
    fn fixture_is_deterministic_and_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let config = ScaleConfig {
            fixture_dir: dir.path().to_path_buf(),
            nodes: 100,
            edges_per_node: 2,
            iterations: 1,
            burst_callers: 2,
            regenerate: false,
        };
        let (first, reused) = ensure_fixture(&config).unwrap();
        assert!(!reused);
        let paths = FixturePaths::new(dir.path());
        let nodes = fs::read(&paths.nodes).unwrap();
        let edges = fs::read(&paths.edges).unwrap();

        let (second, reused) = ensure_fixture(&config).unwrap();
        assert!(reused);
        assert_eq!(first, second);
        assert_eq!(nodes, fs::read(&paths.nodes).unwrap());
        assert_eq!(edges, fs::read(&paths.edges).unwrap());
    }

    #[tokio::test]
    async fn small_harness_exercises_production_paths() {
        let dir = tempfile::tempdir().unwrap();
        let report = run(ScaleConfig {
            fixture_dir: dir.path().to_path_buf(),
            nodes: 500,
            edges_per_node: 2,
            iterations: 2,
            burst_callers: 4,
            regenerate: false,
        })
        .await
        .unwrap();
        assert_eq!(report.fixture.nodes, 500);
        assert_eq!(report.fixture.edges, 1_000);
        assert_eq!(report.same_key_cold_burst.loader_builds, 1);
        assert!(report.same_key_cold_burst.all_callers_shared_snapshot);
        assert!(report.search.returned_hits > 0);
    }
}
