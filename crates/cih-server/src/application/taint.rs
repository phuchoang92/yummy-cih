//! Typed `taint_paths` application use case over graph artifacts.
//!
//! Runs cih-taint Phase 0 (inter-procedural BFS on the call graph) on every
//! call, plus phases 1–3 (liveness, CFG, PDG flow-sensitive) when `refine` is
//! set. Reads the same `.cih` artifacts `cih-engine analyze` wrote, so no prior
//! `cih-engine taint` run is required and results always match the live index.

use std::collections::HashMap;
use std::sync::Arc;

use cih_taint::{run_taint_analysis, SinkCategory, TaintAnalysisInput, TaintPhaseConfig};
use serde::Serialize;

use crate::domain::error::AppError;
use crate::domain::repository::ResolvedRepo;
use crate::infrastructure::artifact_repository::{ArtifactRepository, ArtifactSnapshot};
use crate::infrastructure::blocking_runtime::{
    blocking_timeout, run_blocking_heavy, BlockingError,
};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

#[derive(Clone)]
pub(crate) struct TaintService {
    artifacts: Arc<dyn ArtifactRepository>,
}

impl TaintService {
    pub(crate) fn new(artifacts: Arc<dyn ArtifactRepository>) -> Self {
        Self { artifacts }
    }
}

#[derive(Debug)]
pub(crate) struct TaintPathsCommand {
    category: Option<SinkCategory>,
    min_confidence: f32,
    refine: bool,
    limit: usize,
}

impl TaintPathsCommand {
    pub(crate) fn try_new(
        category: String,
        min_confidence: f32,
        refine: bool,
        limit: usize,
    ) -> Result<Self, AppError> {
        let category = parse_category(&category).map_err(|message| AppError::InvalidInput {
            field: "category",
            message,
        })?;
        if !min_confidence.is_finite() {
            return Err(AppError::InvalidInput {
                field: "min_confidence",
                message: "must be a finite number".into(),
            });
        }
        Ok(Self {
            category,
            min_confidence: min_confidence.clamp(0.0, 1.0),
            refine,
            limit: if limit == 0 {
                DEFAULT_LIMIT
            } else {
                limit.min(MAX_LIMIT)
            },
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct TaintPathOutput {
    /// Entry-point method the tainted data enters through (HTTP handler, listener).
    source: String,
    /// Method performing the dangerous operation.
    sink_method: String,
    category: &'static str,
    severity: &'static str,
    confidence: f32,
    hop_count: usize,
    /// Full method chain from source to sink (NodeIds).
    hops: Vec<String>,
    file: Option<String>,
    line: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TaintPathsOutput {
    total_found: usize,
    returned: usize,
    refined: bool,
    min_confidence: f32,
    paths: Vec<TaintPathOutput>,
    stats: TaintStats,
}

#[derive(Debug, Serialize)]
struct TaintStats {
    phase0_paths: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    cfg: Option<TaintCfgStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pdg: Option<TaintPdgStats>,
}

#[derive(Debug, Serialize)]
struct TaintCfgStats {
    methods_analyzed: usize,
    ir_unavailable: usize,
    max_cyclomatic: usize,
}

#[derive(Debug, Serialize)]
struct TaintPdgStats {
    methods_analyzed: usize,
    confirmed_sinks: usize,
    conditional_sinks: usize,
    ir_unavailable: usize,
}

fn parse_category(s: &str) -> Result<Option<SinkCategory>, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "all" => Ok(None),
        "sql" => Ok(Some(SinkCategory::Sql)),
        "exec" => Ok(Some(SinkCategory::Exec)),
        "file" => Ok(Some(SinkCategory::File)),
        "html" => Ok(Some(SinkCategory::Html)),
        other => Err(format!(
            "unknown category '{other}'; expected all, sql, exec, file, or html"
        )),
    }
}

impl TaintService {
    pub(crate) async fn taint_paths(
        &self,
        repo: ResolvedRepo,
        command: TaintPathsCommand,
    ) -> Result<TaintPathsOutput, AppError> {
        let snapshot = self.artifacts.snapshot(&repo).await?;
        let repo_path = repo.canonical_path;

        // The analysis is synchronous and CPU-bound (and reads source files when
        // refining) — keep it off the async runtime threads. Snapshot loading has
        // already passed through the asynchronous repository boundary.
        run_blocking_heavy(blocking_timeout(), "taint analysis", move || {
            run_and_shape(
                &repo_path.to_string_lossy(),
                &snapshot,
                command.category,
                command.min_confidence,
                command.refine,
                command.limit,
            )
        })
        .await
        .map_err(blocking_error)?
    }
}

fn run_and_shape(
    repo_path: &str,
    snapshot: &ArtifactSnapshot,
    category: Option<SinkCategory>,
    min_confidence: f32,
    refine: bool,
    limit: usize,
) -> Result<TaintPathsOutput, AppError> {
    let nodes = snapshot.nodes.as_ref();
    let edges = snapshot.edges.as_ref();

    let repo = std::path::Path::new(repo_path);
    let rules = cih_taint::load_taint_rules(repo);

    let node_meta: HashMap<&str, (&str, u32)> = nodes
        .iter()
        .map(|n| (n.id.as_str(), (n.file.as_str(), n.range.start_line)))
        .collect();

    let result = run_taint_analysis(TaintAnalysisInput {
        nodes,
        edges,
        rules: &rules,
        resolve_source: Box::new(move |file| std::fs::read_to_string(repo.join(file)).ok()),
        node_file: Box::new(|id| {
            node_meta
                .get(id.as_str())
                .map(|(file, _)| (*file).to_string())
        }),
        phases: TaintPhaseConfig {
            run_intra_proc: refine,
            run_cfg: refine,
            run_pdg: refine,
        },
    })
    .map_err(|error| AppError::Unavailable {
        dependency: "taint analysis",
        message: error.to_string(),
        retryable: false,
    })?;

    let total_found = result.paths.len();
    let mut kept: Vec<_> = result
        .paths
        .into_iter()
        .filter(|p| category.is_none_or(|c| p.category == c))
        .filter(|p| p.confidence >= min_confidence)
        .collect();
    kept.sort_by(|a, b| {
        b.confidence
            .total_cmp(&a.confidence)
            .then_with(|| a.hops.len().cmp(&b.hops.len()))
            .then_with(|| a.source.as_str().cmp(b.source.as_str()))
    });
    kept.truncate(limit);

    let paths: Vec<TaintPathOutput> = kept
        .iter()
        .map(|p| {
            let meta = node_meta.get(p.source.as_str());
            TaintPathOutput {
                source: p.source.to_string(),
                sink_method: p.sink_method.to_string(),
                category: p.category.label(),
                severity: p.category.severity(),
                confidence: p.confidence,
                hop_count: p.edge_count(),
                hops: p.hops.iter().map(|h| h.to_string()).collect(),
                file: meta.map(|(f, _)| (*f).to_string()),
                line: meta.map(|(_, l)| *l),
            }
        })
        .collect();

    let stats = if refine {
        TaintStats {
            phase0_paths: total_found,
            cfg: Some(TaintCfgStats {
                methods_analyzed: result.cfg_stats.methods_analyzed,
                ir_unavailable: result.cfg_stats.ir_unavailable,
                max_cyclomatic: result.cfg_stats.max_cyclomatic,
            }),
            pdg: Some(TaintPdgStats {
                methods_analyzed: result.pdg_stats.methods_analyzed,
                confirmed_sinks: result.pdg_stats.confirmed_sinks,
                conditional_sinks: result.pdg_stats.conditional_sinks,
                ir_unavailable: result.pdg_stats.ir_unavailable,
            }),
        }
    } else {
        TaintStats {
            phase0_paths: total_found,
            cfg: None,
            pdg: None,
        }
    };

    Ok(TaintPathsOutput {
        total_found,
        returned: paths.len(),
        refined: refine,
        min_confidence,
        paths,
        stats,
    })
}

fn blocking_error(error: BlockingError) -> AppError {
    AppError::Unavailable {
        dependency: "blocking runtime",
        message: error.to_string(),
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use cih_core::{Edge, EdgeKind, GraphArtifacts, Node, NodeId, NodeKind, Range, VersionId};

    use super::*;
    use crate::infrastructure::artifact_repository::ArtifactCache;

    const CONTROLLER: &str = "Method:com.acme.OrderController#create/1";
    const SERVICE: &str = "Method:com.acme.OrderService#save/1";
    const DAO: &str = "Method:com.acme.OrderDao#persist/1";
    const EXEC_HELPER: &str = "Method:com.acme.ShellRunner#run/1";

    fn method(id: &str, file: &str, line: u32) -> Node {
        Node {
            id: NodeId::new(id),
            kind: NodeKind::Method,
            name: id.rsplit('#').next().unwrap_or(id).to_string(),
            qualified_name: None,
            file: file.to_string(),
            range: Range {
                start_line: line,
                start_col: 0,
                end_line: line + 10,
                end_col: 0,
            },
            props: None,
        }
    }

    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge {
            src: NodeId::new(src),
            dst: NodeId::new(dst),
            kind,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        }
    }

    /// Fixture graph with two taint paths from the same HTTP entry point:
    /// controller → service → dao → Statement#executeQuery (sql, 2 hops) and
    /// controller → helper → Runtime#exec (exec, 1 hop).
    fn write_fixture(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let nodes = [
            method(
                CONTROLLER,
                "src/main/java/com/acme/OrderController.java",
                42,
            ),
            method(SERVICE, "src/main/java/com/acme/OrderService.java", 10),
            method(DAO, "src/main/java/com/acme/OrderDao.java", 21),
            method(EXEC_HELPER, "src/main/java/com/acme/ShellRunner.java", 7),
        ];
        let edges = [
            edge(CONTROLLER, "Route:POST /api/orders", EdgeKind::HandlesRoute),
            edge(CONTROLLER, SERVICE, EdgeKind::Calls),
            edge(SERVICE, DAO, EdgeKind::Calls),
            edge(
                DAO,
                "Method:java.sql.Statement#executeQuery/1",
                EdgeKind::Calls,
            ),
            edge(CONTROLLER, EXEC_HELPER, EdgeKind::Calls),
            edge(
                EXEC_HELPER,
                "Method:java.lang.Runtime#exec/1",
                EdgeKind::Calls,
            ),
        ];
        let nodes_jsonl: String = nodes
            .iter()
            .map(|n| serde_json::to_string(n).unwrap() + "\n")
            .collect();
        let edges_jsonl: String = edges
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect();
        std::fs::write(dir.join("nodes.jsonl"), nodes_jsonl).unwrap();
        std::fs::write(dir.join("edges.jsonl"), edges_jsonl).unwrap();
    }

    fn fixture_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cih-server-taint-test-{name}"));
        write_fixture(&dir);
        dir
    }

    fn run(
        dir: &std::path::Path,
        category: Option<SinkCategory>,
        min_confidence: f32,
        limit: usize,
    ) -> TaintPathsOutput {
        let dir = dir.to_str().unwrap();
        let artifacts = GraphArtifacts {
            nodes_path: std::path::Path::new(dir).join("nodes.jsonl"),
            edges_path: std::path::Path::new(dir).join("edges.jsonl"),
            version: VersionId::new("fixture"),
        };
        let snapshot = ArtifactSnapshot::from_memory(
            artifacts.read_nodes().unwrap(),
            artifacts.read_edges().unwrap(),
        );
        run_and_shape(dir, &snapshot, category, min_confidence, false, limit).unwrap()
    }

    #[test]
    fn finds_both_paths_from_entry_point() {
        let dir = fixture_dir("both");
        let out = run(&dir, None, 0.0, 50);
        assert_eq!(out.total_found, 2);
        assert_eq!(out.returned, 2);

        let sql = out
            .paths
            .iter()
            .find(|p| p.category == "sql")
            .expect("sql path");
        assert_eq!(sql.source, CONTROLLER);
        assert_eq!(sql.sink_method, DAO);
        assert_eq!(sql.hops, vec![CONTROLLER, SERVICE, DAO]);
        assert_eq!(sql.hop_count, 2);
        assert_eq!(sql.severity, "high");
        assert_eq!(
            sql.file.as_deref(),
            Some("src/main/java/com/acme/OrderController.java")
        );
        assert_eq!(sql.line, Some(42));

        let exec = out
            .paths
            .iter()
            .find(|p| p.category == "exec")
            .expect("exec path");
        assert_eq!(exec.sink_method, EXEC_HELPER);
        assert_eq!(exec.hop_count, 1);
    }

    #[test]
    fn category_filter_narrows_results() {
        let dir = fixture_dir("category");
        let out = run(&dir, Some(SinkCategory::Sql), 0.0, 50);
        assert_eq!(out.total_found, 2);
        assert_eq!(out.returned, 1);
        assert_eq!(out.paths[0].category, "sql");
    }

    #[test]
    fn limit_truncates_after_sorting_by_confidence() {
        let dir = fixture_dir("limit");
        let all = run(&dir, None, 0.0, 50);
        let top = run(&dir, None, 0.0, 1);
        assert_eq!(top.total_found, 2);
        assert_eq!(top.returned, 1);
        assert_eq!(top.paths[0].confidence, all.paths[0].confidence);
        assert!(all.paths[0].confidence >= all.paths[1].confidence);
    }

    #[test]
    fn min_confidence_above_all_paths_returns_none() {
        let dir = fixture_dir("confidence");
        let all = run(&dir, None, 0.0, 50);
        let max_conf = all
            .paths
            .iter()
            .map(|p| p.confidence)
            .fold(0.0_f32, f32::max);
        assert!(max_conf > 0.0);
        let none = run(&dir, None, (max_conf + 0.01).min(1.0), 50);
        assert_eq!(none.returned, 0);
        assert_eq!(none.total_found, 2);
    }

    #[tokio::test]
    async fn missing_artifacts_is_an_error() {
        let dir = std::env::temp_dir().join("cih-server-taint-test-missing-nothing-here");
        let cache = ArtifactCache::new();
        let repo = ResolvedRepo::from_entry(cih_core::RegistryEntry {
            name: "missing".into(),
            path: dir.display().to_string(),
            graph_key: "missing".into(),
            artifacts_dir: dir.display().to_string(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        });
        let err = match cache.snapshot(&repo).await {
            Ok(_) => panic!("missing artifacts unexpectedly loaded"),
            Err(error) => error,
        };
        assert!(
            err.to_string().contains("graph artifacts"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_category_accepts_known_kinds() {
        assert_eq!(parse_category("").unwrap(), None);
        assert_eq!(parse_category("all").unwrap(), None);
        assert_eq!(parse_category("SQL").unwrap(), Some(SinkCategory::Sql));
        assert_eq!(parse_category("exec").unwrap(), Some(SinkCategory::Exec));
        assert_eq!(parse_category("file").unwrap(), Some(SinkCategory::File));
        assert_eq!(parse_category("html").unwrap(), Some(SinkCategory::Html));
        assert!(parse_category("bogus").is_err());
    }

    #[test]
    fn command_validates_category_and_applies_limits() {
        let command = TaintPathsCommand::try_new(" SQL ".into(), 2.0, true, 0).unwrap();
        assert_eq!(command.category, Some(SinkCategory::Sql));
        assert_eq!(command.min_confidence, 1.0);
        assert_eq!(command.limit, DEFAULT_LIMIT);

        let error = TaintPathsCommand::try_new("network".into(), 0.5, false, 10).unwrap_err();
        assert!(matches!(
            error,
            AppError::InvalidInput {
                field: "category",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn service_returns_typed_output() {
        let dir = fixture_dir("service");
        let repo = ResolvedRepo::from_entry(cih_core::RegistryEntry {
            name: "fixture".into(),
            path: dir.display().to_string(),
            graph_key: "fixture".into(),
            artifacts_dir: dir.display().to_string(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        });
        let service = TaintService::new(Arc::new(ArtifactCache::new()));
        let command = TaintPathsCommand::try_new("sql".into(), 0.0, false, 50).unwrap();

        let output = service.taint_paths(repo, command).await.unwrap();

        assert_eq!(output.total_found, 2);
        assert_eq!(output.returned, 1);
        assert_eq!(output.paths[0].category, "sql");
        assert_eq!(
            serde_json::to_value(output.stats).unwrap(),
            serde_json::json!({ "phase0_paths": 2 })
        );
    }
}
