use std::path::Path;

use cih_core::{Edge, Node, ParsedFile, SymbolDef};

use crate::index::ResolveIndex;
use crate::lang::{DiRedirect, DiSite, InheritanceModel, LanguageResolver, PostProcessOptions};
use crate::types::class_of;

mod aop;
mod cxf;
pub mod di;

pub struct JavaResolver;

impl LanguageResolver for JavaResolver {
    fn language_id(&self) -> &'static str {
        "java"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("<init>")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        matches!(name, "this" | "super")
    }

    fn resolve_self_receiver(
        &self,
        keyword: &str,
        in_fqcn: &str,
        index: &ResolveIndex,
    ) -> Option<String> {
        match keyword {
            "this" => Some(class_of(in_fqcn).to_string()),
            "super" => index.supertypes(class_of(in_fqcn)).first().cloned(),
            _ => None,
        }
    }

    fn di_redirect(
        &self,
        type_qname: &str,
        site: &DiSite<'_>,
        index: &ResolveIndex,
    ) -> Option<DiRedirect> {
        // 1. Explicit @Qualifier/@Resource(name) naming an XML bean id — the wiring
        //    the container actually performs; strongest evidence.
        if let Some(qualifier) = site.qualifier {
            if let Some(target) = di::qualifier_bean_impl(type_qname, qualifier, index) {
                if target != site.enclosing_class {
                    return Some(DiRedirect {
                        target,
                        confidence: 0.95,
                        reason: "di-qualifier",
                    });
                }
            }
        }
        // 2. Unique Spring-annotated bean implementor (stereotype-driven wiring).
        if let Some(bean) = di::single_bean_impl(type_qname, index) {
            if bean != site.enclosing_class {
                return Some(DiRedirect {
                    target: bean,
                    confidence: 0.9,
                    reason: "di-resolved",
                });
            }
        }
        // 3. Sole concrete implementor in scope, regardless of stereotype. A guess —
        //    the real impl may live outside the indexed scope — so demoted confidence
        //    and its own reason for provenance.
        if let Some(sole) = index.single_programmatic_impl(type_qname, "java") {
            if sole != site.enclosing_class {
                return Some(DiRedirect {
                    target: sole.to_string(),
                    confidence: 0.75,
                    reason: "di-single-impl",
                });
            }
        }
        None
    }

    fn type_metadata(&self, def: &SymbolDef) -> Option<String> {
        def.framework_role.clone()
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::JavaNominal
    }

    fn extra_edges(
        &self,
        repo_root: Option<&Path>,
        parsed: &[ParsedFile],
    ) -> (Vec<Node>, Vec<Edge>) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        if let Some(root) = repo_root {
            let result = crate::di_xml::extract_di_xml(root, parsed);
            nodes.extend(result.nodes);
            edges.extend(result.edges);
        }
        (nodes, edges)
    }

    fn post_process(
        &self,
        repo_root: Option<&Path>,
        nodes: &mut Vec<Node>,
        edges: &mut Vec<Edge>,
        options: &PostProcessOptions,
    ) {
        // Prepend CXF <jaxrs:server> base paths (+ servlet prefix) onto Java Route nodes.
        if let Some(root) = repo_root {
            cxf::stitch_route_prefixes(root, nodes, edges, options.route_base_path.as_deref());
        }
        // Spring AOP: pointcut expressions on @Aspect advice → ADVISES edges.
        let aop = aop::emit_advises_edges(nodes, edges);
        if aop.aspects > 0 {
            tracing::info!(
                aspects = aop.aspects,
                advice_methods = aop.advice_methods,
                advises_edges = aop.edges,
                "Spring AOP pointcuts resolved"
            );
        }
    }
}
