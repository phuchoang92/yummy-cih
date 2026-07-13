//! Engine-side graph augmentors — the pieces of the augmentor set that can't
//! live in `cih-resolve` because they need engine-only deps (a repo FS walk).
//!
//! These implement the same `cih_resolve::GraphAugmentor` trait the core loop
//! iterates, so the orchestrator treats them identically to the resolve-side
//! augmentors. Language/framework gating lives here (in `applies`), never in the
//! core.

use cih_resolve::{AugmentCtx, AugmentOutput, GraphAugmentor};

use super::extract::extract_integration_xml_in_repo;

/// Spring/Camel integration-XML routes + message destinations, discovered by an
/// FS walk of the repo. JVM/Spring-only, so it self-gates on the `java` language
/// being in scope (and honors `--skip-xml-integration`).
pub struct IntegrationXmlAugmentor;

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
        let (nodes, edges) = extract_integration_xml_in_repo(root);
        AugmentOutput { nodes, edges }
    }
}
