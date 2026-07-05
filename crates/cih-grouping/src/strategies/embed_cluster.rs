use std::collections::HashMap;

use cih_core::NodeKind;

use crate::entry::{fnv64_node, FeatureGroupEntry};
use crate::strategy::{FeatureStrategy, StrategyInput};

/// Configuration for the primary embedding-based feature clusterer.
///
/// Distinct from [`crate::strategies::embed::EmbedConfig`], which drives the *residual filler*
/// used inside `hybrid`. This one clusters from scratch via k-NN + Leiden.
#[derive(Clone, Debug)]
pub struct EmbedClusterConfig {
    /// Cosine similarity threshold for a k-NN edge to be kept. Default: 0.65.
    pub similarity_threshold: f32,
    /// Neighbors per node in the k-NN graph. Default: 15.
    pub knn: usize,
    /// Leiden resolution — higher = more, smaller clusters. Default: 0.8.
    pub leiden_resolution: f64,
    /// Leiden RNG seed for near-deterministic membership. Default: 0xc0de.
    pub leiden_seed: u32,
}

impl Default for EmbedClusterConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.65,
            knn: 15,
            leiden_resolution: 0.8,
            leiden_seed: 0xc0de,
        }
    }
}

/// Per-node metadata (kind/name/file), sourced from `cih_node_vectors` by the engine.
#[derive(Clone, Debug)]
pub struct NodeMeta {
    pub kind: String,
    pub name: String,
    pub file: String,
}

/// Primary embedding clusterer: turns precomputed Leiden cluster assignments (over a semantic
/// k-NN graph) into named `FeatureGroupEntry` records. It performs **no** DB or graph work —
/// the engine hands it cluster assignments, per-node vectors, and metadata, keeping
/// `cih-grouping` free of Postgres and heavy compute (see the plan's architecture note).
pub struct EmbedClusterStrategy {
    /// node_id → cluster id.
    clusters: HashMap<String, usize>,
    /// node_id → averaged embedding.
    vectors: HashMap<String, Vec<f32>>,
    /// node_id → metadata.
    meta: HashMap<String, NodeMeta>,
    config: EmbedClusterConfig,
}

impl EmbedClusterStrategy {
    pub fn new(
        clusters: Vec<(String, usize)>,
        vectors: HashMap<String, Vec<f32>>,
        meta: HashMap<String, NodeMeta>,
        config: EmbedClusterConfig,
    ) -> Self {
        Self {
            clusters: clusters.into_iter().collect(),
            vectors,
            meta,
            config,
        }
    }

    /// Compute, for each cluster: its slug, its label node, and each member's similarity to the
    /// cluster centroid. Returns `node_id → (slug, confidence)`.
    fn label_clusters(&self) -> HashMap<String, (String, f32)> {
        // Group member node_ids by cluster id (sorted for determinism).
        let mut members_by_cluster: HashMap<usize, Vec<&String>> = HashMap::new();
        for (node_id, cluster) in &self.clusters {
            members_by_cluster.entry(*cluster).or_default().push(node_id);
        }

        let mut result: HashMap<String, (String, f32)> = HashMap::new();
        let mut used_slugs: HashMap<String, usize> = HashMap::new();

        // Deterministic cluster order.
        let mut cluster_ids: Vec<usize> = members_by_cluster.keys().copied().collect();
        cluster_ids.sort_unstable();

        for cluster_id in cluster_ids {
            let mut members = members_by_cluster.remove(&cluster_id).unwrap_or_default();
            members.sort();

            // Centroid = mean of member vectors that we actually have.
            let centroid = self.centroid(&members);

            // Label node = member closest to the centroid (max cosine similarity), but **prefer a
            // type-level member** (Class/Interface/Enum/Record/Annotation) so the slug comes from a
            // type name rather than an arbitrary method/field. Fall back to any member only if the
            // cluster has no type-level member. node_id tiebreaks for stability (members is sorted).
            let mut best_type: Option<(&String, f32)> = None;
            let mut best_any: Option<(&String, f32)> = None;
            let mut sims: HashMap<&String, f32> = HashMap::new();
            for node_id in &members {
                let sim = self
                    .vectors
                    .get(*node_id)
                    .map(|v| cosine_similarity(v, &centroid))
                    .unwrap_or(0.0);
                sims.insert(*node_id, sim);
                if !matches!(best_any, Some((_, s)) if sim <= s) {
                    best_any = Some((node_id, sim));
                }
                let is_type = self
                    .meta
                    .get(*node_id)
                    .map(|m| is_type_level(&m.kind))
                    .unwrap_or(false);
                if is_type && !matches!(best_type, Some((_, s)) if sim <= s) {
                    best_type = Some((node_id, sim));
                }
            }

            let slug = match best_type.or(best_any) {
                Some((label_id, _)) => {
                    let base = self
                        .meta
                        .get(label_id)
                        .map(|m| derive_slug(label_id, &m.file, &m.name, cluster_id))
                        .unwrap_or_else(|| format!("cluster-{cluster_id}"));
                    // Disambiguate slug collisions across clusters.
                    let count = used_slugs.entry(base.clone()).or_insert(0);
                    let slug = if *count == 0 {
                        base.clone()
                    } else {
                        format!("{base}-{count}")
                    };
                    *count += 1;
                    slug
                }
                None => format!("cluster-{cluster_id}"),
            };

            for node_id in &members {
                let conf = sims.get(node_id).copied().unwrap_or(0.0).clamp(0.0, 1.0);
                result.insert((*node_id).clone(), (slug.clone(), conf));
            }
        }

        result
    }

    fn centroid(&self, members: &[&String]) -> Vec<f32> {
        let mut sum: Vec<f32> = Vec::new();
        let mut n = 0usize;
        for node_id in members {
            if let Some(v) = self.vectors.get(*node_id) {
                if sum.is_empty() {
                    sum = vec![0.0; v.len()];
                }
                if sum.len() == v.len() {
                    for (acc, x) in sum.iter_mut().zip(v) {
                        *acc += x;
                    }
                    n += 1;
                }
            }
        }
        if n > 0 {
            let scale = 1.0 / n as f32;
            for x in &mut sum {
                *x *= scale;
            }
        }
        sum
    }
}

impl FeatureStrategy for EmbedClusterStrategy {
    fn name(&self) -> &str {
        "embed"
    }

    fn feature_of(&self, _file: &str) -> String {
        // Single-file classification isn't meaningful for a cluster-based strategy.
        "shared".to_string()
    }

    fn assign(&self, input: &StrategyInput<'_>) -> Vec<FeatureGroupEntry> {
        let labels = self.label_clusters();
        let ev = format!(
            "knn-leiden k={} thr={:.2} res={:.2}",
            self.config.knn, self.config.similarity_threshold, self.config.leiden_resolution
        );

        input
            .nodes
            .iter()
            .map(|node| {
                let node_id = node.id.as_str();
                match labels.get(node_id) {
                    Some((slug, confidence)) => FeatureGroupEntry {
                        id: format!("feature:{slug}"),
                        name: slug.clone(),
                        node_id: node_id.to_string(),
                        strategy: "embed".to_string(),
                        confidence: *confidence,
                        pinned: false,
                        evidence: format!("{ev} sim={:.3}", confidence),
                        node_content_hash: fnv64_node(node),
                    },
                    None => FeatureGroupEntry {
                        id: "feature:shared".to_string(),
                        name: "shared".to_string(),
                        node_id: node_id.to_string(),
                        strategy: "embed".to_string(),
                        confidence: 0.0,
                        pinned: false,
                        evidence: "unclustered (no embedding or no k-NN edge)".to_string(),
                        node_content_hash: fnv64_node(node),
                    },
                }
            })
            .collect()
    }
}

/// Type-level node kinds — preferred as cluster labels so slugs come from a type name.
fn is_type_level(kind: &str) -> bool {
    matches!(
        kind,
        "Class" | "Interface" | "Enum" | "Record" | "Annotation"
    )
}

/// Directory names that commonly *contain* per-feature packages: the segment immediately after
/// one of these is the feature name (`.../modules/product/...` → `product`).
fn is_feature_container(seg: &str) -> bool {
    matches!(
        seg,
        "modules" | "module" | "feature" | "features" | "domain" | "domains"
    )
}

/// Generic class-name suffixes stripped when deriving a feature slug from a label node.
const STRIP_SUFFIXES: [&str; 6] = [
    "Controller",
    "Service",
    "Repository",
    "Handler",
    "Impl",
    "Manager",
];

/// Path/package segments and stripped class names that carry no feature meaning — skipped so slug
/// derivation falls through to a more informative source (feature-container/package segment).
fn is_generic_segment(seg: &str) -> bool {
    matches!(
        seg,
        // path/package scaffolding
        "src" | "main" | "test" | "java" | "kotlin" | "scala" | "resources"
            | "com" | "org" | "net" | "io" | "target" | "build"
            // generic method/field/DTO tokens that leaked in as slugs
            | "list" | "get" | "set" | "is" | "name" | "id" | "value" | "values"
            | "data" | "type" | "types" | "request" | "response" | "req" | "res"
            | "dto" | "dtos" | "entity" | "entities" | "model" | "models"
            | "mapper" | "mappers" | "util" | "utils" | "helper" | "helpers"
            | "base" | "abstract" | "application" | "app" | "tests" | "action" | "actions"
            | "config" | "common" | "core" | "shared" | "api" | "impl"
            | ""
    ) || seg.len() <= 1
}

/// Derive a feature slug from a cluster's label node.
///
/// Order (first that yields something usable wins):
/// 1. Feature-container segment: the path element after `modules`/`feature`/`domain`
///    (`.../modules/product/...` → `product`). Highest signal for module-per-feature layouts.
/// 2. A Maven-style module directory (a path segment containing `-`, e.g. `banking-overdraft`).
/// 3. The label class name with generic suffixes stripped (`PaymentService` → `payment`), when not
///    itself generic.
/// 4. Owner-class simple name when the label is a member (`Method:...Foo#bar` → `foo`), when not
///    generic.
/// 5. The immediate package directory (last non-generic path segment).
/// 6. `cluster-{id}` fallback.
fn derive_slug(node_id: &str, file: &str, name: &str, cluster_id: usize) -> String {
    let dirs: Vec<&str> = {
        let mut d: Vec<&str> = file.split('/').collect();
        d.pop(); // drop filename
        d
    };

    // 1. feature-container segment (modules/<feature>/...)
    for (i, seg) in dirs.iter().enumerate() {
        if is_feature_container(seg) {
            if let Some(feat) = dirs.get(i + 1) {
                let slug = slugify(feat);
                if !slug.is_empty() && !is_generic_segment(&slug) {
                    return slug;
                }
            }
        }
    }

    // 2. module dir (hyphenated)
    if let Some(module) = dirs.iter().rev().find(|s| s.contains('-') && s.len() > 1) {
        let slug = slugify(module);
        if !slug.is_empty() {
            return slug;
        }
    }

    // 3. stripped class simple name — only for type-level labels; for a member label `name` is the
    //    member's own name (e.g. "getName"), which is not a class name, so skip to step 4.
    if !node_id.contains('#') {
        let stripped = strip_suffixes(simple_name(name));
        if !stripped.is_empty() && !is_generic_segment(&stripped.to_lowercase()) {
            let slug = slugify(&stripped);
            if !slug.is_empty() {
                return slug;
            }
        }
    }

    // 4. owner-class simple name (for member labels: `Kind:owner.fqn#member/arity`)
    if let Some(owner) = owner_simple_name(node_id) {
        let stripped = strip_suffixes(&owner);
        if !stripped.is_empty() && !is_generic_segment(&stripped.to_lowercase()) {
            let slug = slugify(&stripped);
            if !slug.is_empty() {
                return slug;
            }
        }
    }

    // 5. immediate (last non-generic) package dir
    if let Some(seg) = dirs.iter().rev().find(|s| !is_generic_segment(s)) {
        let slug = slugify(seg);
        if !slug.is_empty() {
            return slug;
        }
    }

    // 6. fallback
    format!("cluster-{cluster_id}")
}

/// Last dotted segment of a (possibly qualified) name: `a.b.Foo` → `Foo`, `Foo` → `Foo`.
fn simple_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// For a member NodeId `Kind:owner.fully.Qualified#member/arity`, return the owner's simple name.
/// Returns `None` when the id has no `#` (i.e. it is already a type-level node).
fn owner_simple_name(node_id: &str) -> Option<String> {
    let after_colon = node_id.split_once(':').map(|(_, r)| r).unwrap_or(node_id);
    let owner_fqn = after_colon.split('#').next().unwrap_or(after_colon);
    if owner_fqn == after_colon {
        return None; // no '#' → not a member
    }
    Some(simple_name(owner_fqn).to_string())
}

/// Repeatedly strip a trailing generic suffix (handles `PaymentServiceImpl` → `Payment`).
fn strip_suffixes(name: &str) -> String {
    let mut current = name.to_string();
    loop {
        let mut stripped = false;
        for suffix in STRIP_SUFFIXES {
            // `>=` so a class named exactly after a suffix (e.g. "Impl", "Service") empties out
            // and the slug falls through to the package/module directory instead.
            if current.len() >= suffix.len() && current.ends_with(suffix) {
                current.truncate(current.len() - suffix.len());
                stripped = true;
            }
        }
        if !stripped {
            break;
        }
    }
    current
}

/// Lowercase; non-alphanumeric runs become single `-`; trim leading/trailing `-`.
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Kinds this strategy expects to cluster (mirrors `cih-embed::is_embeddable_kind` intent).
/// Kept for callers that want to pre-filter; the strategy itself emits for whatever nodes it is
/// given.
pub fn is_clusterable_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Record
            | NodeKind::Annotation
            | NodeKind::Method
            | NodeKind::Constructor
            | NodeKind::Field
            | NodeKind::Route
            | NodeKind::IntegrationRoute
    )
}

#[cfg(test)]
#[path = "embed_cluster_tests.rs"]
mod tests;
