use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_grouping::{FeatureGroupEntry, FeatureOverrides};
use serde::Serialize;

// ── features show ─────────────────────────────────────────────────────────────

pub fn run_features_show(repo: PathBuf, json: bool) -> Result<()> {
    let (dir, entries) = load_feature_artifact(&repo)?;
    let version = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let summary = build_summary(&version, &entries);

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_summary(&repo, &summary);
    }
    Ok(())
}

fn load_feature_artifact(repo: &Path) -> Result<(PathBuf, Vec<FeatureGroupEntry>)> {
    let parent = repo.join(".cih").join("artifacts-features");
    let dir = latest_version_dir(&parent).with_context(|| {
        format!(
            "no feature artifacts at {} — run `discover` first",
            parent.display()
        )
    })?;
    let entries = cih_grouping::read_feature_artifact(&dir)
        .with_context(|| format!("failed to read groups.jsonl from {}", dir.display()))?;
    Ok((dir, entries))
}

fn latest_version_dir(parent: &Path) -> Result<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", parent.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.join("groups.jsonl").is_file())
        .collect();
    dirs.sort();
    dirs.pop()
        .ok_or_else(|| anyhow::anyhow!("no groups.jsonl found under {}", parent.display()))
}

#[derive(Serialize)]
struct FeatureRow {
    name: String,
    node_count: usize,
    /// Most common strategy in this feature's assignments.
    strategy: String,
    pinned_count: usize,
}

#[derive(Serialize)]
struct FeatureSummary {
    graph_version: String,
    features: Vec<FeatureRow>,
    totals: Totals,
}

#[derive(Serialize)]
struct Totals {
    features: usize,
    nodes: usize,
    pinned: usize,
}

fn build_summary(version: &str, entries: &[FeatureGroupEntry]) -> FeatureSummary {
    // Group by feature name.
    let mut by_feature: HashMap<&str, Vec<&FeatureGroupEntry>> = HashMap::new();
    for e in entries {
        by_feature.entry(e.name.as_str()).or_default().push(e);
    }

    let mut rows: Vec<FeatureRow> = by_feature
        .iter()
        .map(|(name, group)| {
            let pinned_count = group.iter().filter(|e| e.pinned).count();
            // Pick the most common non-override strategy as display strategy.
            let strategy = dominant_strategy(group);
            FeatureRow {
                name: name.to_string(),
                node_count: group.len(),
                strategy,
                pinned_count,
            }
        })
        .collect();

    rows.sort_by(|a, b| b.node_count.cmp(&a.node_count).then(a.name.cmp(&b.name)));

    let total_nodes = entries.len();
    let total_pinned = entries.iter().filter(|e| e.pinned).count();
    let feature_count = rows.len();

    FeatureSummary {
        graph_version: version.to_string(),
        features: rows,
        totals: Totals {
            features: feature_count,
            nodes: total_nodes,
            pinned: total_pinned,
        },
    }
}

fn dominant_strategy(group: &[&FeatureGroupEntry]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in group {
        if e.strategy != "override" {
            *counts.entry(e.strategy.as_str()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| "override".to_string())
}

fn print_summary(repo: &Path, summary: &FeatureSummary) {
    let repo_name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let ver = &summary.graph_version[..summary.graph_version.len().min(8)];
    crate::ui::print_header("Features", repo_name, Some(ver));

    // Column widths.
    let name_w = summary
        .features
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let strategy_w = 10usize;

    eprintln!(
        "     {:<name_w$}  {:>6}  {:<strategy_w$}  Pinned",
        "Feature",
        "Nodes",
        "Strategy",
        name_w = name_w,
        strategy_w = strategy_w
    );
    eprintln!(
        "     {}  ──────  {}  ──────",
        "─".repeat(name_w),
        "─".repeat(strategy_w)
    );

    for row in &summary.features {
        let pin = if row.pinned_count > 0 {
            format!("  \x1b[33m● {}\x1b[0m", row.pinned_count)
        } else {
            String::new()
        };
        eprintln!(
            "     {:<name_w$}  {:>6}  {:<strategy_w$}{}",
            row.name,
            row.node_count,
            row.strategy,
            pin,
            name_w = name_w,
            strategy_w = strategy_w
        );
    }

    eprintln!();
    eprintln!(
        "     \x1b[2m{} features  ·  {} nodes  ·  {} pinned\x1b[0m",
        summary.totals.features, summary.totals.nodes, summary.totals.pinned
    );
    eprintln!();
}

// ── features override ─────────────────────────────────────────────────────────

pub fn run_features_override(
    repo: PathBuf,
    node_id: String,
    feature: String,
    reason: String,
) -> Result<()> {
    let path = FeatureOverrides::path(&repo);

    let mut overrides = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str::<FeatureOverrides>(&text)
            .with_context(|| format!("malformed {}", path.display()))?
    } else {
        FeatureOverrides::default()
    };

    let is_update = overrides.upsert(node_id.clone(), feature.clone(), reason.clone());
    overrides.save(&repo)?;

    let action = if is_update { "Updated" } else { "Added" };
    eprintln!("{action} override: {node_id} → \x1b[1m{feature}\x1b[0m");
    eprintln!("Written to {}", path.display());
    eprintln!("Re-run `discover` to apply.");
    Ok(())
}

// ── features review (LLM auto-pin) ─────────────────────────────────────────────

pub struct ReviewFlags {
    pub repo: PathBuf,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    pub dry_run: bool,
    pub limit: Option<usize>,
    pub include_weak_members: bool,
    pub min_confidence: f32,
}

/// In-cluster members below this centroid confidence are eligible with `--include-weak-members`.
const WEAK_MEMBER_THRESHOLD: f32 = 0.75;
const REVIEW_BATCH: usize = 20;

struct Candidate {
    node_id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file: String,
    current: String,
    hash: u64,
}

struct CatalogEntry {
    package: String,
    classes: Vec<String>,
}

struct ReviewDecision {
    node_id: String,
    feature: String,
    confidence: f32,
    reason: String,
}

/// `.cih/feature-review-cache.json` — content-hash → decision, so re-runs only review new/changed
/// nodes. Keyed by node_content_hash (as string, since JSON object keys must be strings).
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ReviewCache {
    #[serde(default)]
    reviewed: HashMap<String, String>,
}

impl ReviewCache {
    fn path(repo: &Path) -> PathBuf {
        repo.join(".cih").join("feature-review-cache.json")
    }
    fn load(repo: &Path) -> Self {
        std::fs::read_to_string(Self::path(repo))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    fn save(&self, repo: &Path) -> Result<()> {
        let path = Self::path(repo);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}

pub fn run_features_review(flags: ReviewFlags) -> Result<()> {
    let (feat_dir, entries) = load_feature_artifact(&flags.repo)?;
    let version = feat_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let nodes = load_node_meta(&flags.repo)?;

    let catalog = build_catalog(&entries, &nodes);
    let valid: HashSet<&str> = catalog.keys().map(|s| s.as_str()).collect();

    let mut cache = ReviewCache::load(&flags.repo);
    let candidates = select_candidates(&entries, &nodes, &flags, &cache);

    let repo_name = flags
        .repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    crate::ui::print_header(
        "Features · review",
        repo_name,
        Some(&version[..version.len().min(8)]),
    );

    if candidates.is_empty() {
        eprintln!("     No candidates to review (all clear, or already cached from a prior run).");
        return Ok(());
    }
    eprintln!(
        "     Reviewing {} candidate node(s) against {} cluster(s) via {}…",
        candidates.len(),
        catalog.len(),
        flags.provider
    );

    let caller = build_review_caller(&flags)?;
    let by_id: HashMap<&str, &Candidate> =
        candidates.iter().map(|c| (c.node_id.as_str(), c)).collect();
    let mut overrides = FeatureOverrides::load(&flags.repo).unwrap_or_default();
    let system = review_system_prompt();

    let (mut added, mut kept_shared, mut skipped_human) = (0usize, 0usize, 0usize);
    let mut shown: Vec<(String, String, String)> = Vec::new();

    for batch in candidates.chunks(REVIEW_BATCH) {
        let user = review_user_prompt(&catalog, batch);
        let raw = match caller.classify_batch(&system, &user) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, "features review: LLM batch failed — skipping");
                continue;
            }
        };
        for d in parse_review(&raw) {
            let Some(cand) = by_id.get(d.node_id.as_str()) else {
                continue;
            };
            cache
                .reviewed
                .insert(cand.hash.to_string(), d.feature.clone());
            if d.feature == cand.current {
                continue;
            }
            if d.feature == "shared" {
                kept_shared += 1;
                continue;
            }
            if !valid.contains(d.feature.as_str()) {
                continue; // LLM named a cluster that doesn't exist
            }
            if d.confidence < flags.min_confidence {
                continue;
            }
            // Preserve human-authored overrides — only (over)write our own `llm-review:` entries.
            if let Some(existing) = overrides.entries.iter().find(|e| e.node_id == cand.node_id) {
                if !existing.reason.starts_with("llm-review:") {
                    skipped_human += 1;
                    continue;
                }
            }
            shown.push((cand.node_id.clone(), d.feature.clone(), d.reason.clone()));
            if !flags.dry_run {
                overrides.upsert(
                    cand.node_id.clone(),
                    d.feature.clone(),
                    format!("llm-review: {}", d.reason),
                );
            }
            added += 1;
        }
    }

    for (node_id, feature, reason) in &shown {
        eprintln!("     \x1b[32m{feature}\x1b[0m ← {}", short_node(node_id));
        if !reason.is_empty() {
            eprintln!("        \x1b[2m{reason}\x1b[0m");
        }
    }

    if !flags.dry_run {
        if added > 0 {
            overrides.save(&flags.repo)?;
        }
        cache.save(&flags.repo)?;
    }

    let flagged = low_cohesion_clusters(&entries);
    eprintln!();
    let verb = if flags.dry_run { "would pin" } else { "pinned" };
    eprintln!(
        "     \x1b[2m{added} {verb}  ·  {kept_shared} kept shared  ·  {skipped_human} human-kept\x1b[0m"
    );
    if !flagged.is_empty() {
        eprintln!(
            "     \x1b[2mlow-cohesion clusters (consider `discover --feature-llm-provider` to relabel): {}\x1b[0m",
            flagged.join(", ")
        );
    }
    if flags.dry_run {
        eprintln!("     \x1b[2m(dry run — nothing written; drop --dry-run to apply)\x1b[0m");
    } else if added > 0 {
        eprintln!(
            "     Written to {}",
            FeatureOverrides::path(&flags.repo).display()
        );
        eprintln!("     Re-run `discover` to apply.");
    }
    Ok(())
}

/// Load node metadata (kind/name/qualified_name/file/props) from the latest source graph artifacts.
fn load_node_meta(repo: &Path) -> Result<HashMap<String, cih_core::Node>> {
    let dir = repo.join(".cih").join("artifacts");
    let artifacts = cih_core::GraphArtifacts::latest_in_dir(&dir)
        .with_context(|| format!("no graph artifacts under {}", dir.display()))?;
    let nodes = artifacts
        .read_nodes()
        .with_context(|| "failed to read nodes.jsonl")?;
    Ok(nodes
        .into_iter()
        .map(|n| (n.id.as_str().to_string(), n))
        .collect())
}

/// Per-cluster catalog: dominant package + a few representative type names, for the LLM prompt.
fn build_catalog(
    entries: &[FeatureGroupEntry],
    nodes: &HashMap<String, cih_core::Node>,
) -> BTreeMap<String, CatalogEntry> {
    let mut members: BTreeMap<&str, Vec<&FeatureGroupEntry>> = BTreeMap::new();
    for e in entries {
        if e.name == "shared" {
            continue;
        }
        members.entry(e.name.as_str()).or_default().push(e);
    }

    let mut out = BTreeMap::new();
    for (name, mut grp) in members {
        grp.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut pkg_counts: HashMap<String, usize> = HashMap::new();
        let mut classes: Vec<String> = Vec::new();
        for e in &grp {
            let Some(n) = nodes.get(&e.node_id) else {
                continue;
            };
            if let Some(p) = package_of(n) {
                *pkg_counts.entry(p).or_default() += 1;
            }
            if classes.len() < 6
                && matches!(
                    n.kind,
                    cih_core::NodeKind::Class
                        | cih_core::NodeKind::Interface
                        | cih_core::NodeKind::Enum
                        | cih_core::NodeKind::Record
                )
                && !classes.contains(&n.name)
            {
                classes.push(n.name.clone());
            }
        }
        let package = pkg_counts
            .into_iter()
            .max_by_key(|(_, c)| *c)
            .map(|(p, _)| p)
            .unwrap_or_default();
        out.insert(name.to_string(), CatalogEntry { package, classes });
    }
    out
}

/// Boundary/weak first-party nodes worth an LLM second opinion, minus already-cached ones.
fn select_candidates(
    entries: &[FeatureGroupEntry],
    nodes: &HashMap<String, cih_core::Node>,
    flags: &ReviewFlags,
    cache: &ReviewCache,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for e in entries {
        let Some(n) = nodes.get(&e.node_id) else {
            continue;
        };
        if !is_project_node(n) || !cih_grouping::is_clusterable_kind(n.kind) {
            continue;
        }
        let is_boundary = e.name == "shared" && e.evidence.contains("below-confidence-threshold");
        let is_weak = flags.include_weak_members
            && e.name != "shared"
            && e.confidence < WEAK_MEMBER_THRESHOLD;
        if !(is_boundary || is_weak) {
            continue;
        }
        let hash = if e.node_content_hash != 0 {
            e.node_content_hash
        } else {
            cih_grouping::fnv64_node(n)
        };
        if cache.reviewed.contains_key(&hash.to_string()) {
            continue;
        }
        out.push(Candidate {
            node_id: e.node_id.clone(),
            kind: n.kind.label().to_string(),
            name: n.name.clone(),
            qualified_name: n.qualified_name.clone().unwrap_or_default(),
            file: n.file.clone(),
            current: e.name.clone(),
            hash,
        });
        if let Some(lim) = flags.limit {
            if out.len() >= lim {
                break;
            }
        }
    }
    out
}

/// First-party (not a jar/external stub) and not test source. Mirrors discover's filter.
fn is_project_node(n: &cih_core::Node) -> bool {
    let external = n
        .props
        .as_ref()
        .map(|p| {
            p.get("external").and_then(|v| v.as_bool()).unwrap_or(false)
                || p.get("fromJar").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .unwrap_or(false);
    let f = n.file.as_str();
    let test = f.ends_with(".jar")
        || f.contains("src/test/")
        || f.contains("/test/java/")
        || f.contains("/test/kotlin/");
    !external && !test
}

/// Package of a node from its qualified name (strip `#member`, drop the type's simple name).
fn package_of(n: &cih_core::Node) -> Option<String> {
    let q = n.qualified_name.as_deref()?;
    let base = q.split('#').next().unwrap_or(q);
    base.rsplit_once('.').map(|(pkg, _)| pkg.to_string())
}

fn build_review_caller(
    flags: &ReviewFlags,
) -> Result<std::sync::Arc<dyn cih_grouping::FeatureLlmCaller>> {
    let provider: crate::llm::LlmProvider = flags.provider.parse()?;
    let model = if flags.model.is_empty() {
        default_model(provider)
    } else {
        flags.model.clone()
    };
    let base_url = flags
        .base_url
        .clone()
        .unwrap_or_else(|| crate::settings::DEFAULT_FEATURE_LLM_BASE_URL.to_string());
    let adapter = crate::llm::make_adapter(&provider, &base_url, None)?;
    let api_key = crate::llm::resolve_api_key(flags.api_key_env.as_deref())?;
    Ok(crate::feature_strategy::make_feature_llm_caller(
        adapter,
        api_key,
        model,
        flags.max_tokens,
        flags.timeout_secs,
    ))
}

fn default_model(p: crate::llm::LlmProvider) -> String {
    use crate::llm::LlmProvider::*;
    match p {
        DeepSeek => "deepseek-chat",
        Gemini => "gemini-2.5-flash",
        Anthropic => "claude-haiku-4-5-20251001",
        Bedrock => "us.anthropic.claude-haiku-4-5-20251001",
        _ => "gpt-4o-mini",
    }
    .to_string()
}

fn review_system_prompt() -> String {
    "You are an expert software architect assigning code symbols to the correct feature cluster \
     in a Java/Spring codebase.\n\
     You are given a CATALOG of existing feature clusters (name, typical package, representative \
     classes) and a list of SYMBOLS that are currently unclustered or weakly assigned.\n\
     For each symbol pick the single best-fitting feature NAME from the catalog, judging mainly by \
     the symbol's package and owning class, then its name and role. If no catalog feature is a good \
     fit, answer \"shared\".\n\
     Strongly prefer the feature whose typical package matches the symbol's package.\n\
     Output ONLY a JSON array, one object per input symbol, no prose:\n\
     [{\"node_id\":\"<verbatim>\",\"feature\":\"<catalog name or shared>\",\"confidence\":0.0-1.0,\"reason\":\"<short>\"}]"
        .to_string()
}

fn review_user_prompt(catalog: &BTreeMap<String, CatalogEntry>, batch: &[Candidate]) -> String {
    let mut s = String::from("CATALOG:\n");
    for (name, c) in catalog {
        s.push_str(&format!(
            "- {name}: pkg={}; classes: {}\n",
            c.package,
            c.classes.join(", ")
        ));
    }
    s.push_str("\nSYMBOLS:\n");
    for cand in batch {
        s.push_str(&format!(
            "- node_id={} | kind={} | name={} | fqn={} | file={}\n",
            cand.node_id, cand.kind, cand.name, cand.qualified_name, cand.file
        ));
    }
    s
}

/// Tolerant parse of the LLM JSON array (accepts confidence as number or string, missing fields).
fn parse_review(raw: &str) -> Vec<ReviewDecision> {
    let (s, e) = match (raw.find('['), raw.rfind(']')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => {
            tracing::warn!("features review: no JSON array in LLM response");
            return Vec::new();
        }
    };
    let arr: Vec<serde_json::Value> = match serde_json::from_str(&raw[s..=e]) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "features review: JSON parse failed");
            return Vec::new();
        }
    };
    arr.into_iter()
        .filter_map(|v| {
            Some(ReviewDecision {
                node_id: v.get("node_id")?.as_str()?.to_string(),
                feature: v.get("feature")?.as_str()?.to_string(),
                confidence: v.get("confidence").and_then(num_or_str_f32).unwrap_or(0.6),
                reason: v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

fn num_or_str_f32(v: &serde_json::Value) -> Option<f32> {
    v.as_f64()
        .map(|f| f as f32)
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Named clusters (size ≥ 10) whose average member confidence is low — likely mislabeled.
fn low_cohesion_clusters(entries: &[FeatureGroupEntry]) -> Vec<String> {
    let mut by: BTreeMap<&str, (f32, usize)> = BTreeMap::new();
    for e in entries {
        if e.name == "shared" {
            continue;
        }
        let ent = by.entry(e.name.as_str()).or_insert((0.0, 0));
        ent.0 += e.confidence;
        ent.1 += 1;
    }
    let mut flagged: Vec<(String, f32)> = by
        .into_iter()
        .filter(|(_, (_, n))| *n >= 10)
        .map(|(name, (sum, n))| (name.to_string(), sum / n as f32))
        .filter(|(_, avg)| *avg < 0.72)
        .collect();
    flagged.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    flagged
        .into_iter()
        .take(5)
        .map(|(n, avg)| format!("{n} ({:.0}%)", avg * 100.0))
        .collect()
}

fn short_node(id: &str) -> String {
    id.split_once(':').map(|(_, r)| r).unwrap_or(id).to_string()
}

// ── feature info for status command ───────────────────────────────────────────

pub struct FeatureStatus {
    pub graph_version: String,
    pub feature_count: usize,
    pub node_count: usize,
    pub pinned_count: usize,
    pub strategy: String,
}

pub fn load_feature_status(repo: &Path) -> Option<FeatureStatus> {
    let parent = repo.join(".cih").join("artifacts-features");
    let dir = latest_version_dir(&parent).ok()?;
    let version = dir.file_name()?.to_str()?.to_string();
    let entries = cih_grouping::read_feature_artifact(&dir).ok()?;

    let mut features = std::collections::HashSet::new();
    for e in &entries {
        features.insert(e.name.as_str());
    }
    let pinned = entries.iter().filter(|e| e.pinned).count();
    let strategy = dominant_strategy(&entries.iter().collect::<Vec<_>>());

    Some(FeatureStatus {
        graph_version: version,
        feature_count: features.len(),
        node_count: entries.len(),
        pinned_count: pinned,
        strategy,
    })
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use cih_core::{Node, NodeId, NodeKind, Range};

    fn node(id: &str, kind: NodeKind, fqn: &str, file: &str, props: serde_json::Value) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: fqn.rsplit(['.', '#']).next().unwrap_or(fqn).to_string(),
            qualified_name: Some(fqn.to_string()),
            file: file.to_string(),
            range: Range::default(),
            props: if props.is_null() { None } else { Some(props) },
        }
    }

    #[test]
    fn parse_review_is_tolerant() {
        // Prose around the array, a string confidence, and a missing reason are all handled.
        let raw = "Here you go:\n[\n\
            {\"node_id\":\"Method:a.B#f/0\",\"feature\":\"payment\",\"confidence\":0.91,\"reason\":\"pkg match\"},\n\
            {\"node_id\":\"Class:a.C\",\"feature\":\"shared\",\"confidence\":\"0.4\"}\n\
        ]\nhope that helps";
        let out = parse_review(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].feature, "payment");
        assert!((out[0].confidence - 0.91).abs() < 1e-5);
        assert_eq!(out[1].feature, "shared");
        assert!((out[1].confidence - 0.4).abs() < 1e-5);
        assert_eq!(out[1].reason, "");
    }

    #[test]
    fn parse_review_handles_garbage() {
        assert!(parse_review("no json here").is_empty());
        assert!(parse_review("[not valid json}").is_empty());
    }

    #[test]
    fn package_of_strips_member_and_type() {
        let m = node(
            "Method:org.phuc.inv.StockService#numericToString/1",
            NodeKind::Method,
            "org.phuc.inv.StockService#numericToString/1",
            "src/main/java/org/phuc/inv/StockService.java",
            serde_json::Value::Null,
        );
        assert_eq!(package_of(&m).as_deref(), Some("org.phuc.inv"));
        let c = node(
            "Class:org.phuc.pay.PaymentService",
            NodeKind::Class,
            "org.phuc.pay.PaymentService",
            "x",
            serde_json::Value::Null,
        );
        assert_eq!(package_of(&c).as_deref(), Some("org.phuc.pay"));
    }

    #[test]
    fn is_project_node_excludes_jar_and_test() {
        let jar = node(
            "Class:com.x.Y",
            NodeKind::Class,
            "com.x.Y",
            "some.jar",
            serde_json::json!({"fromJar": true, "external": true}),
        );
        assert!(!is_project_node(&jar));
        let test = node(
            "Class:org.phuc.FooTest",
            NodeKind::Class,
            "org.phuc.FooTest",
            "src/test/java/org/phuc/FooTest.java",
            serde_json::Value::Null,
        );
        assert!(!is_project_node(&test));
        let prod = node(
            "Class:org.phuc.Foo",
            NodeKind::Class,
            "org.phuc.Foo",
            "src/main/java/org/phuc/Foo.java",
            serde_json::Value::Null,
        );
        assert!(is_project_node(&prod));
    }

    #[test]
    fn num_or_str_f32_accepts_both() {
        assert_eq!(num_or_str_f32(&serde_json::json!(0.5)), Some(0.5));
        assert_eq!(num_or_str_f32(&serde_json::json!("0.7")), Some(0.7));
        assert_eq!(num_or_str_f32(&serde_json::json!("nope")), None);
    }
}
