//! Pure markdown-skeleton rendering for `doc_pack`. No I/O, no store access —
//! every byte is derived from the delivered evidence, so identical evidence
//! renders identical markdown (the regeneration contract depends on it).

use cih_core::NodeId;

use crate::application::files::SourceSpan;
use crate::viz;

use super::{
    ContractsBody, DocSection, EvidenceProfileV1, FlowBody, IdentityBody, SectionState, TestScope,
    TestsBody, UpstreamBody,
};

pub(crate) struct RenderInput<'a> {
    pub(crate) node_id: &'a str,
    pub(crate) evidence_hash: &'a str,
    /// Diagnostics-only provenance; the frontmatter line is omitted entirely
    /// when absent (never an empty scalar).
    pub(crate) graph_version: Option<&'a str>,
    pub(crate) profile: &'a EvidenceProfileV1,
    pub(crate) requested_profile: &'a EvidenceProfileV1,
    pub(crate) identity: &'a IdentityBody,
    pub(crate) flow: &'a SectionState<FlowBody>,
    pub(crate) upstream: &'a SectionState<UpstreamBody>,
    pub(crate) tests: &'a SectionState<TestsBody>,
    pub(crate) source: &'a SectionState<SourceSpan>,
    pub(crate) contracts: &'a SectionState<ContractsBody>,
}

/// `METHOD /path` for routes, otherwise the qualified (or simple) name.
fn page_title(identity: &IdentityBody) -> String {
    if identity.kind == "Route" {
        if let (Some(method), Some(path)) = (&identity.http_method, &identity.path) {
            return format!("{method} {path}");
        }
    }
    identity
        .qualified_name
        .clone()
        .unwrap_or_else(|| identity.name.clone())
}

/// JSON string escaping doubles as YAML-safe double-quoted scalar escaping.
fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings always serialize")
}

fn profile_json(profile: &EvidenceProfileV1) -> String {
    serde_json::to_string(profile).expect("profiles always serialize")
}

/// A fence delimiter strictly longer than any backtick run in the content, so
/// embedded fences cannot terminate ours.
fn fence_for(content: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for c in content.chars() {
        if c == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn prose_markers(name: &str, out: &mut String) {
    out.push_str(&format!(
        "<!-- cih:prose:{name}:start -->\n<!-- cih:prose:{name}:end -->\n"
    ));
}

fn unavailable_note(reason: &str, remedy: &Option<String>, out: &mut String) {
    out.push_str(&format!("*Not available: {reason}.*\n"));
    if let Some(remedy) = remedy {
        out.push_str(&format!("*Remedy: {remedy}*\n"));
    }
}

fn bounded_note(out: &mut String) {
    out.push_str("\n> This list is bounded — more entries exist beyond the cap.\n");
}

pub(crate) fn render_doc_page(input: &RenderInput<'_>) -> String {
    let title = page_title(input.identity);
    let mut out = String::with_capacity(4 * 1024);

    // ---- frontmatter -------------------------------------------------------
    out.push_str("---\n");
    out.push_str(&format!("title: {}\n", yaml_string(&title)));
    out.push_str(&format!("cih_node: {}\n", yaml_string(input.node_id)));
    out.push_str(&format!("cih_evidence_hash: {}\n", input.evidence_hash));
    if let Some(graph_version) = input.graph_version {
        out.push_str(&format!(
            "cih_graph_version: {}\n",
            yaml_string(graph_version)
        ));
    }
    out.push_str(&format!("cih_generator: {}\n", super::DOC_GENERATOR));
    out.push_str(&format!("cih_profile: {}\n", profile_json(input.profile)));
    out.push_str(&format!(
        "cih_requested_profile: {}\n",
        profile_json(input.requested_profile)
    ));
    out.push_str("---\n\n");

    // ---- body --------------------------------------------------------------
    out.push_str(&format!("# {title}\n"));
    prose_markers("overview", &mut out);
    out.push('\n');

    render_facts(input.identity, &mut out);

    let wants = |section: DocSection| input.profile.sections.contains(&section);
    if wants(DocSection::Flow) {
        render_flow(input.node_id, input.flow, &mut out);
    }
    if wants(DocSection::Upstream) {
        render_upstream(input.upstream, &mut out);
    }
    if wants(DocSection::Tests) {
        render_tests(input.tests, &mut out);
    }
    if wants(DocSection::Source) {
        render_source(input.source, &mut out);
    }
    if wants(DocSection::Contracts) {
        render_contracts(input.contracts, &mut out);
    }

    out.push('\n');
    prose_markers("notes", &mut out);
    out
}

fn render_facts(identity: &IdentityBody, out: &mut String) {
    out.push_str("\n## Facts\n\n");
    out.push_str(&format!("- **Node**: `{}`\n", identity.id));
    out.push_str(&format!("- **Kind**: {}\n", identity.kind));
    if let Some(qualified) = &identity.qualified_name {
        out.push_str(&format!("- **Qualified name**: `{qualified}`\n"));
    }
    if !identity.file.is_empty() {
        if identity.start_line > 0 {
            out.push_str(&format!(
                "- **File**: `{}` (lines {}–{})\n",
                identity.file, identity.start_line, identity.end_line
            ));
        } else {
            out.push_str(&format!("- **File**: `{}`\n", identity.file));
        }
    }
    if let (Some(method), Some(path)) = (&identity.http_method, &identity.path) {
        out.push_str(&format!("- **Endpoint**: {method} `{path}`\n"));
    }
    if let Some(stereotype) = &identity.stereotype {
        out.push_str(&format!("- **Stereotype**: {stereotype}\n"));
    }
    let mut complexity = Vec::new();
    if let Some(value) = identity.cyclomatic {
        complexity.push(format!("cyclomatic {value}"));
    }
    if let Some(value) = identity.cognitive {
        complexity.push(format!("cognitive {value}"));
    }
    if let Some(value) = identity.transitive_loop_depth {
        complexity.push(format!("transitive loop depth {value}"));
    }
    if let Some(true) = identity.is_recursive {
        complexity.push("recursive".to_string());
    }
    if !complexity.is_empty() {
        out.push_str(&format!("- **Complexity**: {}\n", complexity.join(", ")));
    }
}

fn render_flow(node_id: &str, flow: &SectionState<FlowBody>, out: &mut String) {
    match flow {
        SectionState::Off => {}
        SectionState::Unavailable { reason, remedy, .. } => {
            out.push_str("\n## Execution flow\n\n");
            prose_markers("flow", out);
            out.push('\n');
            unavailable_note(reason, remedy, out);
        }
        SectionState::Ok { body } => {
            out.push_str("\n## Execution flow\n\n");
            prose_markers("flow", out);
            out.push('\n');
            let entry = NodeId::new(node_id.to_string());
            let mermaid = viz::render_mermaid_flow(&entry, &body.steps);
            let fence = fence_for(&mermaid);
            out.push_str(&format!("{fence}mermaid\n{mermaid}{fence}\n"));
            out.push_str(&format!(
                "\n{} step{} within depth {} (business-logic view).\n",
                body.steps.len(),
                if body.steps.len() == 1 { "" } else { "s" },
                super::FLOW_MAX_DEPTH
            ));
            if !body.completeness.complete {
                out.push_str(
                    "\n> The flow walk was bounded — steps beyond the caps are not shown.\n",
                );
            }
            // Data access is a flow-owned unit: rendered only with available
            // flow evidence, so a type page never shows an orphaned empty
            // data-access section under an unavailable flow.
            out.push_str("\n## Data access\n\n");
            if body.db_effects.is_empty() {
                out.push_str("No table reads or writes were traced from this flow.\n");
            } else {
                out.push_str("| Access | Table | Operation | Method |\n");
                out.push_str("|--------|-------|-----------|--------|\n");
                for effect in &body.db_effects {
                    out.push_str(&format!(
                        "| {} | {} | {} | `{}` |\n",
                        effect.access,
                        effect.table,
                        effect.operation,
                        effect.method.as_str()
                    ));
                }
            }
        }
    }
}

fn render_upstream(upstream: &SectionState<UpstreamBody>, out: &mut String) {
    match upstream {
        SectionState::Off => {}
        SectionState::Unavailable { reason, remedy, .. } => {
            out.push_str("\n## Callers & processes\n\n");
            unavailable_note(reason, remedy, out);
        }
        SectionState::Ok { body } => {
            out.push_str("\n## Callers & processes\n\n");
            if body.callers.is_empty() {
                out.push_str("No indexed callers.\n");
            } else {
                for caller in &body.callers {
                    out.push_str(&format!("- `{}` ({})\n", caller.id, caller.file));
                }
            }
            if !body.processes.is_empty() {
                out.push_str("\nProcesses:\n");
                for process in &body.processes {
                    out.push_str(&format!("- `{process}`\n"));
                }
            }
            if !body.completeness.complete {
                bounded_note(out);
            }
        }
    }
}

fn render_tests(tests: &SectionState<TestsBody>, out: &mut String) {
    match tests {
        SectionState::Off => {}
        SectionState::Unavailable { reason, remedy, .. } => {
            out.push_str("\n## Tests\n\n");
            unavailable_note(reason, remedy, out);
        }
        SectionState::Ok { body } => {
            out.push_str("\n## Tests\n\n");
            if body.tests.is_empty() {
                // Scope-aware honest wording: an empty bounded-but-complete
                // result is a real "none"; an incomplete one proves nothing.
                if body.completeness.complete {
                    out.push_str(match body.scope {
                        TestScope::Direct => "No tests target this symbol.\n",
                        TestScope::DirectAndOwner => {
                            "No tests target this callable or its owning type.\n"
                        }
                        TestScope::DirectAndMembers => {
                            "No tests target this type or its indexed members.\n"
                        }
                    });
                } else {
                    out.push_str(
                        "Test evidence is inconclusive — the bounded query returned no rows \
                         but did not complete.\n",
                    );
                }
            } else {
                for test in &body.tests {
                    out.push_str(&format!("- `{}` ({})\n", test.id, test.file));
                }
                if !body.completeness.complete {
                    bounded_note(out);
                }
            }
        }
    }
}

fn render_source(source: &SectionState<SourceSpan>, out: &mut String) {
    match source {
        SectionState::Off => {}
        SectionState::Unavailable { reason, remedy, .. } => {
            out.push_str("\n## Source\n\n");
            unavailable_note(reason, remedy, out);
        }
        SectionState::Ok { body } => {
            out.push_str("\n## Source\n\n");
            out.push_str(&format!(
                "`{}` lines {}–{}{}\n\n",
                body.path,
                body.start_line,
                body.end_line,
                if body.truncated { " (truncated)" } else { "" }
            ));
            let fence = fence_for(&body.content);
            let language = std::path::Path::new(&body.path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            out.push_str(&format!("{fence}{language}\n{}\n{fence}\n", body.content));
        }
    }
}

fn render_contracts(contracts: &SectionState<ContractsBody>, out: &mut String) {
    match contracts {
        SectionState::Off => {}
        SectionState::Unavailable { reason, remedy, .. } => {
            out.push_str("\n## Cross-repo consumers\n\n");
            unavailable_note(reason, remedy, out);
        }
        SectionState::Ok { body } => {
            out.push_str("\n## Cross-repo consumers\n\n");
            if body.consumers.is_empty() {
                if body.completeness.complete {
                    out.push_str("No cross-repo consumers of this route in the group.\n");
                } else {
                    out.push_str(
                        "Consumer evidence is inconclusive — the bounded contract scan did \
                         not complete.\n",
                    );
                }
            } else {
                for consumer in &body.consumers {
                    out.push_str(&format!(
                        "- {}: `{}`\n",
                        consumer.consumer_repo, consumer.consumer_endpoint
                    ));
                }
                if !body.completeness.complete {
                    bounded_note(out);
                }
            }
            if body.contracts_stale {
                out.push_str(
                    "\n> Group contracts are stale — re-run `cih-engine group sync` and \
                     regenerate.\n",
                );
            }
        }
    }
}
