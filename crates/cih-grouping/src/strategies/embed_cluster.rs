use std::collections::HashMap;
use std::sync::Arc;

use cih_core::NodeKind;

use crate::entry::{fnv64_node, FeatureGroupEntry};
use crate::strategies::llm::FeatureLlmCaller;
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
    /// Optional LLM caller for the opt-in cluster-labeling pass (`--feature-llm-provider`).
    llm: Option<Arc<dyn FeatureLlmCaller>>,
    /// Prior run's embed entries, used to reuse LLM names for unchanged clusters (stability).
    prior_entries: Vec<FeatureGroupEntry>,
}

/// Pass-1 working data for one cluster (borrows member ids from `self.clusters`).
struct ClusterInfo<'a> {
    members: Vec<&'a String>,
    sims: HashMap<&'a String, f32>,
    base: String,
    distinguishers: Vec<String>,
}

impl EmbedClusterStrategy {
    pub fn new(
        clusters: Vec<(String, usize)>,
        vectors: HashMap<String, Vec<f32>>,
        meta: HashMap<String, NodeMeta>,
        config: EmbedClusterConfig,
        llm: Option<Arc<dyn FeatureLlmCaller>>,
        prior_entries: Vec<FeatureGroupEntry>,
    ) -> Self {
        Self {
            clusters: clusters.into_iter().collect(),
            vectors,
            meta,
            config,
            llm,
            prior_entries,
        }
    }

    /// Compute, for each cluster: its slug, its label node, and each member's similarity to the
    /// cluster centroid. Returns `node_id → (slug, confidence)`.
    ///
    /// Two passes: (1) per cluster compute the label node, `base` slug, ranked distinguishers, and
    /// member sims; (2) assign globally-unique names — clusters with a unique base keep it, colliding
    /// ones get a **meaningful** suffix (dominant sub-package, else label class name). Never a
    /// numeric counter (see [`assign_unique_names`]).
    /// Returns `node_id → (slug, confidence, from_llm)` where `from_llm` marks names produced (or
    /// reused) by the LLM labeling pass — used to tag entries so the cache only reuses LLM names.
    fn label_clusters(&self) -> HashMap<String, (String, f32, bool)> {
        let infos = self.cluster_infos();

        let specs: Vec<(String, Vec<String>)> = infos
            .iter()
            .map(|i| (i.base.clone(), i.distinguishers.clone()))
            .collect();
        let det_names = assign_unique_names(&specs);

        // Opt-in: an LLM renames clusters to concise domain slugs (cached for stability).
        let (names, from_llm) = match &self.llm {
            Some(caller) => self.llm_relabel(&infos, det_names, caller.as_ref()),
            None => {
                let n = det_names.len();
                (det_names, vec![false; n])
            }
        };

        let mut result: HashMap<String, (String, f32, bool)> = HashMap::new();
        for ((info, name), llm) in infos.iter().zip(names).zip(from_llm) {
            for node_id in &info.members {
                let conf = info.sims.get(node_id).copied().unwrap_or(0.0).clamp(0.0, 1.0);
                result.insert((*node_id).clone(), (name.clone(), conf, llm));
            }
        }
        result
    }

    /// Rename clusters via the LLM, reusing prior LLM names for unchanged clusters (member-set
    /// hash), then re-apply deterministic uniqueness so LLM-name collisions still avoid a counter.
    /// Returns `(names, from_llm)` — `from_llm[i]` is true when cluster `i`'s name is an LLM name
    /// (freshly generated or reused from a prior LLM-labeled run).
    fn llm_relabel(
        &self,
        infos: &[ClusterInfo<'_>],
        det_names: Vec<String>,
        caller: &dyn FeatureLlmCaller,
    ) -> (Vec<String>, Vec<bool>) {
        let prior = self.prior_cluster_names();

        // Reuse cached LLM names for clusters whose exact member set is unchanged; collect the rest.
        let mut candidate: Vec<Option<String>> = vec![None; infos.len()];
        let mut from_llm: Vec<bool> = vec![false; infos.len()];
        let mut to_ask: Vec<usize> = Vec::new();
        for (i, info) in infos.iter().enumerate() {
            let h = member_set_hash(&info.members);
            match prior.get(&h) {
                Some(name) => {
                    candidate[i] = Some(name.clone());
                    from_llm[i] = true;
                }
                None => to_ask.push(i),
            }
        }

        if !to_ask.is_empty() {
            let system = llm_system_prompt();
            let user = self.llm_user_prompt(infos, &det_names, &to_ask);
            match caller.classify_batch(&system, &user) {
                Ok(raw) => {
                    let map = parse_llm_labels(&raw);
                    for &i in &to_ask {
                        if let Some(slug) = map.get(&det_names[i]).map(|s| slugify(s)) {
                            if !slug.is_empty() {
                                candidate[i] = Some(slug);
                                from_llm[i] = true;
                            }
                        }
                    }
                    tracing::info!(
                        clusters = to_ask.len(),
                        cached = infos.len() - to_ask.len(),
                        "embed LLM labeling complete"
                    );
                }
                Err(err) => {
                    tracing::warn!(error = %err, "embed LLM labeling failed — keeping deterministic names");
                }
            }
        }

        // Fall back to deterministic name where the LLM gave nothing; then re-dedup meaningfully.
        let specs: Vec<(String, Vec<String>)> = infos
            .iter()
            .enumerate()
            .map(|(i, info)| {
                let base = candidate[i].clone().unwrap_or_else(|| det_names[i].clone());
                (base, info.distinguishers.clone())
            })
            .collect();
        (assign_unique_names(&specs), from_llm)
    }

    /// Map member-set hash → feature name from the previous run's **LLM-labeled** embed entries
    /// (tagged `labeler=llm` in `evidence`). Deterministic prior names are ignored so enabling the
    /// LLM for the first time relabels every cluster instead of reusing path-derived names.
    fn prior_cluster_names(&self) -> HashMap<u64, String> {
        let mut by_name: HashMap<&str, Vec<&String>> = HashMap::new();
        for e in &self.prior_entries {
            if e.strategy == "embed" && e.name != "shared" && e.evidence.contains("labeler=llm") {
                by_name.entry(e.name.as_str()).or_default().push(&e.node_id);
            }
        }
        by_name
            .into_iter()
            .map(|(name, mut ids)| {
                ids.sort();
                (hash_ids(&ids), name.to_string())
            })
            .collect()
    }

    /// Build the user prompt: for each cluster to label, its deterministic anchor name plus the top
    /// representative (type-level, most-central) class names.
    fn llm_user_prompt(
        &self,
        infos: &[ClusterInfo<'_>],
        det_names: &[String],
        to_ask: &[usize],
    ) -> String {
        let mut lines = vec!["Name these code clusters:".to_string(), String::new()];
        for &i in to_ask {
            let info = &infos[i];
            let classes = self.top_class_names(info, 8).join(", ");
            lines.push(format!("cluster: {}", det_names[i]));
            lines.push(format!("  classes: {classes}"));
            lines.push(String::new());
        }
        lines.join("\n")
    }

    /// Up to `n` representative class names: type-level members, highest cosine-sim to centroid first.
    fn top_class_names(&self, info: &ClusterInfo<'_>, n: usize) -> Vec<String> {
        let mut typed: Vec<(&String, f32)> = info
            .members
            .iter()
            .filter(|id| {
                self.meta
                    .get(**id)
                    .map(|m| is_type_level(&m.kind))
                    .unwrap_or(false)
            })
            .map(|id| (*id, info.sims.get(id).copied().unwrap_or(0.0)))
            .collect();
        typed.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
        typed
            .into_iter()
            .take(n)
            .filter_map(|(id, _)| self.meta.get(id).map(|m| simple_name(&m.name).to_string()))
            .collect()
    }

    /// Pass 1: per-cluster label node, base slug, ranked distinguishers, and member sims — in
    /// deterministic cluster-id order.
    fn cluster_infos(&self) -> Vec<ClusterInfo<'_>> {
        let mut members_by_cluster: HashMap<usize, Vec<&String>> = HashMap::new();
        for (node_id, cluster) in &self.clusters {
            members_by_cluster.entry(*cluster).or_default().push(node_id);
        }
        let mut cluster_ids: Vec<usize> = members_by_cluster.keys().copied().collect();
        cluster_ids.sort_unstable();

        let mut infos = Vec::with_capacity(cluster_ids.len());
        for cluster_id in cluster_ids {
            let mut members = members_by_cluster.remove(&cluster_id).unwrap_or_default();
            members.sort();
            let centroid = self.centroid(&members);

            // Label node = member closest to the centroid (max cosine sim), preferring a type-level
            // member (Class/Interface/...) so the slug comes from a type name. node_id tiebreaks.
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
            let label = best_type.or(best_any).map(|(id, _)| id);
            let base = label
                .and_then(|id| {
                    self.meta
                        .get(id)
                        .map(|m| derive_slug(id, &m.file, &m.name, cluster_id))
                })
                .unwrap_or_else(|| format!("cluster-{cluster_id}"));
            let distinguishers = self.distinguishers(&members, label);

            infos.push(ClusterInfo { members, sims, base, distinguishers });
        }
        infos
    }

    /// Ranked, meaningful tokens used to disambiguate clusters that share a `base` slug:
    /// dominant sub-package segment(s) after the feature-container (`dto`, `services`, `search`),
    /// interleaved with the label class's stripped simple name. No generic blocklist here — layer
    /// names *are* the useful signal for disambiguation.
    fn distinguishers(&self, members: &[&String], label: Option<&String>) -> Vec<String> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for m in members {
            if let Some(meta) = self.meta.get(*m) {
                if let Some(sub) = subsegment_after_feature(&meta.file) {
                    *counts.entry(sub).or_default() += 1;
                }
            }
        }
        let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        let label_tok = label
            .and_then(|id| self.meta.get(id))
            .map(|m| slugify(&strip_suffixes(simple_name(&m.name))))
            // Reject long/path-like tokens (e.g. a slugified route path) — they make ugly suffixes.
            .filter(|s| !s.is_empty() && !is_generic_segment(s) && s.matches('-').count() <= 1);

        // Prefer sub-package segments (by frequency) — `dto`, `services`, `search`, `controller` —
        // and only fall back to the label class name when all sub-packages are taken.
        let mut out: Vec<String> = Vec::new();
        for (s, _) in &ranked {
            push_unique(&mut out, slugify(s));
        }
        if let Some(l) = label_tok {
            push_unique(&mut out, l);
        }
        out.retain(|s| !s.is_empty());
        out
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
                    Some((slug, confidence, from_llm)) => FeatureGroupEntry {
                        id: format!("feature:{slug}"),
                        name: slug.clone(),
                        node_id: node_id.to_string(),
                        strategy: "embed".to_string(),
                        confidence: *confidence,
                        pinned: false,
                        evidence: format!(
                            "labeler={} {ev} sim={:.3}",
                            if *from_llm { "llm" } else { "path" },
                            confidence
                        ),
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

/// The path segment two levels below a feature-container dir: `.../modules/product/dto/Foo.java`
/// → `dto`. This is the layer/sub-area token used to disambiguate same-`base` clusters.
fn subsegment_after_feature(file: &str) -> Option<String> {
    let mut dirs: Vec<&str> = file.split('/').collect();
    dirs.pop(); // drop filename
    for (i, seg) in dirs.iter().enumerate() {
        if is_feature_container(seg) {
            return dirs.get(i + 2).map(|s| s.to_string());
        }
    }
    None
}

/// Push `s` onto `out` if non-empty and not already present.
fn push_unique(out: &mut Vec<String>, s: String) {
    if !s.is_empty() && !out.contains(&s) {
        out.push(s);
    }
}

/// Assign each cluster a globally-unique, human-meaningful name — **never a numeric counter**.
///
/// - A `base` slug used by exactly one cluster is kept as-is (`auth`, `payment`).
/// - Clusters that share a `base` each get a suffix from their ranked `distinguishers`
///   (`product-dto`, `product-services`, `product-search`): the first `"{base}-{token}"` not yet
///   used. If a cluster exhausts its tokens, it falls back to the bare `base` — so in the pathological
///   case two clusters *merge* under one name rather than receive a counter.
///
/// Deterministic: bases processed in sorted order, clusters in input (cluster-id) order.
fn assign_unique_names(clusters: &[(String, Vec<String>)]) -> Vec<String> {
    use std::collections::{BTreeMap, HashSet};

    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, (base, _)) in clusters.iter().enumerate() {
        groups.entry(base.as_str()).or_default().push(i);
    }

    let mut names = vec![String::new(); clusters.len()];
    let mut used: HashSet<String> = HashSet::new();

    // Round 1: unique bases claim the bare base name.
    for (base, idxs) in &groups {
        if idxs.len() == 1 {
            let name = (*base).to_string();
            used.insert(name.clone());
            names[idxs[0]] = name;
        }
    }
    // Round 2: collision groups — every member gets a meaningful suffix (or merges under base).
    for (base, idxs) in &groups {
        if idxs.len() <= 1 {
            continue;
        }
        for &i in idxs {
            let dist = &clusters[i].1;
            let chosen = dist
                .iter()
                .map(|t| format!("{base}-{t}"))
                .find(|cand| !used.contains(cand))
                .unwrap_or_else(|| (*base).to_string());
            used.insert(chosen.clone());
            names[i] = chosen;
        }
    }
    names
}

/// FNV-1a hash of a cluster's member ids (order-independent: caller sorts first).
fn hash_ids(ids: &[&String]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for id in ids {
        for b in id.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff; // member separator
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stable hash of a cluster's member set (sorts a copy first).
fn member_set_hash(members: &[&String]) -> u64 {
    let mut ids: Vec<&String> = members.to_vec();
    ids.sort();
    hash_ids(&ids)
}

/// System prompt for the opt-in cluster-labeling pass — asks for one concise kebab-case slug/cluster.
fn llm_system_prompt() -> String {
    r#"You label business-feature clusters in a Java/Spring codebase. For each cluster you are given its current name and representative class names. Output ONE concise kebab-case feature slug per cluster that best captures its business capability.

Output format — one JSON object per line, no extra text:
{"cluster":"<current-name>","name":"<slug>"}

Rules:
- slug: lowercase, hyphen-separated, 1-3 words (e.g. "product-catalog", "checkout-payments")
- business-oriented; avoid layer words like "dto"/"service"/"controller" unless nothing else fits
- echo the exact "cluster" value you were given so lines can be matched
- exactly one line per cluster, no markdown fences, no commentary"#
        .to_string()
}

/// Parse the LLM's JSONL response into `current-name → new-slug`.
fn parse_llm_labels(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let (Some(c), Some(n)) = (
            val.get("cluster").and_then(|v| v.as_str()),
            val.get("name").and_then(|v| v.as_str()),
        ) {
            map.insert(c.to_string(), n.to_string());
        }
    }
    map
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
