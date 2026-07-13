//! Graph augmentors: post-parse phases that add language/framework-specific
//! nodes+edges to the assembled graph, behind a language-agnostic extension
//! point the core orchestrator iterates generically.
//!
//! The core builds the augmentor list, then for each one that `applies` to the
//! current scope, appends its output — it never names a language or framework.
//! Gating (which phase runs) is expressed per-augmentor via `applies`, so a
//! non-JVM analyze skips the Java/Spring phases automatically.
//!
//! `order()` is a FIXED priority that reproduces the historical node/edge
//! concatenation order (db → integration-xml → di), which `content_version`
//! hashes — augmentors must keep their order stable.

use std::collections::BTreeSet;
use std::path::Path;

use cih_core::{Edge, Node, NodeKind, ParsedFile};

use crate::lang::ResolverRegistry;

/// Read-only inputs an augmentor may consult. Assembled once by the core.
pub struct AugmentCtx<'a> {
    pub repo_root: Option<&'a Path>,
    pub parsed: &'a [ParsedFile],
    /// Parse-phase nodes (JPA entity detection reads these).
    pub nodes: &'a [Node],
    pub unresolved_external_fqcns: &'a [String],
    /// Language ids present in scope (`cih_lang::language_ids_for_paths`).
    pub languages_in_scope: &'a BTreeSet<&'static str>,
    /// `--skip-xml-integration` was requested.
    pub skip_xml_integration: bool,
    /// The resolver registry (DI augmentor dispatches through it).
    pub resolvers: &'a ResolverRegistry,
}

/// Nodes + edges an augmentor contributes to the graph.
#[derive(Default)]
pub struct AugmentOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// A post-parse graph augmentation phase. Implementations are language/framework
/// specific; the core treats them uniformly.
pub trait GraphAugmentor: Send + Sync {
    /// Stable id, used as the generic phase log label (e.g. `"db-access"`).
    fn id(&self) -> &'static str;
    /// Fixed priority — lower runs first. Preserves byte-identical concat order.
    fn order(&self) -> u16;
    /// Whether this phase should run for the current scope.
    fn applies(&self, ctx: &AugmentCtx) -> bool;
    /// Produce the phase's nodes+edges.
    fn augment(&self, ctx: &AugmentCtx) -> AugmentOutput;
}

/// The language/framework graph augmentors (DB/JPA, integration XML, Spring DI).
/// The JAR-API phase stays engine-side (it needs `cih-jar` and owns summary
/// metadata), so the core appends it separately.
pub fn language_augmentors() -> Vec<Box<dyn GraphAugmentor>> {
    vec![
        Box::new(DbAccessAugmentor),
        Box::new(IntegrationXmlAugmentor),
        Box::new(DiXmlAugmentor),
    ]
}

/// True when any parsed file carries SQL evidence (only some languages emit it).
fn has_sql(parsed: &[ParsedFile]) -> bool {
    parsed
        .iter()
        .any(|f| !f.sql_execution_sites.is_empty() || !f.sql_constants.is_empty())
}

/// True when any node is a JPA `@Entity` (mirrors `emit_jpa_tables`' own gate).
fn has_jpa(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| {
        matches!(
            n.kind,
            NodeKind::Class | NodeKind::Interface | NodeKind::Record
        ) && n
            .props
            .as_ref()
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str())
            == Some("entity")
    })
}

/// SQL access sites + JPA entity tables. Runs whenever the parse output actually
/// contains SQL or JPA evidence (language-agnostic).
struct DbAccessAugmentor;
impl GraphAugmentor for DbAccessAugmentor {
    fn id(&self) -> &'static str {
        "db-access"
    }
    fn order(&self) -> u16 {
        20
    }
    fn applies(&self, ctx: &AugmentCtx) -> bool {
        has_sql(ctx.parsed) || has_jpa(ctx.nodes)
    }
    fn augment(&self, ctx: &AugmentCtx) -> AugmentOutput {
        let (mut nodes, mut edges) = crate::emit_db_access(ctx.parsed);
        let (jpa_nodes, jpa_edges) = crate::emit_jpa_tables(ctx.nodes);
        nodes.extend(jpa_nodes);
        edges.extend(jpa_edges);
        AugmentOutput { nodes, edges }
    }
}

/// Spring/Camel integration-XML routes + message destinations, discovered by an
/// FS walk of the repo. JVM/Spring-only, so it gates on `java` being in scope
/// (matching the historical gate) and honors `--skip-xml-integration`.
struct IntegrationXmlAugmentor;
impl GraphAugmentor for IntegrationXmlAugmentor {
    fn id(&self) -> &'static str {
        "integration-xml"
    }
    fn order(&self) -> u16 {
        30
    }
    fn applies(&self, ctx: &AugmentCtx) -> bool {
        !ctx.skip_xml_integration
            && ctx.languages_in_scope.contains("java")
            && ctx.repo_root.is_some()
    }
    fn augment(&self, ctx: &AugmentCtx) -> AugmentOutput {
        let Some(root) = ctx.repo_root else {
            return AugmentOutput::default();
        };
        let (nodes, edges) = crate::extract_integration_xml_in_repo(root);
        AugmentOutput { nodes, edges }
    }
}

/// Spring/Blueprint DI beans + calls, dispatched through the resolver registry's
/// `extra_edges`. Only the Java resolver implements `extra_edges`, so gating on
/// `java` in scope both matches the historical `has_java` gate (byte-identical)
/// and keeps a non-JVM analyze from running/logging a guaranteed no-op phase.
struct DiXmlAugmentor;
impl GraphAugmentor for DiXmlAugmentor {
    fn id(&self) -> &'static str {
        "di-xml"
    }
    fn order(&self) -> u16 {
        40
    }
    fn applies(&self, ctx: &AugmentCtx) -> bool {
        !ctx.skip_xml_integration && ctx.languages_in_scope.contains("java")
    }
    fn augment(&self, ctx: &AugmentCtx) -> AugmentOutput {
        let (nodes, edges) = ctx.resolvers.extra_edges(ctx.repo_root, ctx.parsed);
        AugmentOutput { nodes, edges }
    }
}
