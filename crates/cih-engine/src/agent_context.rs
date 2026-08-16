//! Repository-local coding-agent instructions and skills generated from CIH artifacts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::{EdgeKind, GraphArtifacts, Node, NodeKind};
use serde::Serialize;

const START: &str = "<!-- cih:start -->";
const END: &str = "<!-- cih:end -->";
const LEGACY_START: &str = "<!-- cih-wiki:start -->";
const LEGACY_END: &str = "<!-- cih-wiki:end -->";
const AREA_PREFIX: &str = "cih-area-";
const MAX_AREA_SKILLS: usize = 20;
const MIN_AREA_SYMBOLS: usize = 3;

const SKILL_ROOTS: &[&str] = &[".claude/skills", ".agents/skills", ".kiro/skills"];

#[derive(Debug, Clone, Copy)]
pub struct AgentContextOptions {
    pub enabled: bool,
    pub area_skills: bool,
}

impl Default for AgentContextOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            area_skills: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentContextReport {
    pub enabled: bool,
    pub instruction_files: Vec<String>,
    pub standard_skills: Vec<String>,
    pub area_skills: Vec<String>,
}

struct SkillSpec {
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

const STANDARD_SKILLS: &[SkillSpec] = &[
    SkillSpec {
        name: "cih-exploring",
        description: "Explore an unfamiliar codebase, architecture, and execution flows with CIH.",
        body: EXPLORING_WORKFLOW,
    },
    SkillSpec {
        name: "cih-impact-analysis",
        description:
            "Assess the upstream blast radius and affected processes before changing code.",
        body: IMPACT_WORKFLOW,
    },
    SkillSpec {
        name: "cih-debugging",
        description: "Trace failures through callers, callees, and execution flows with CIH.",
        body: DEBUGGING_WORKFLOW,
    },
    SkillSpec {
        name: "cih-product-owner",
        description:
            "Understand APIs, business processes, and functional areas without reading all code.",
        body: PRODUCT_OWNER_WORKFLOW,
    },
    SkillSpec {
        name: "cih-testing",
        description: "Scope regression, integration, and end-to-end tests from a code change.",
        body: TESTING_WORKFLOW,
    },
    SkillSpec {
        name: "cih-security",
        description: "Review source-to-sink taint paths and scope security fixes with CIH.",
        body: SECURITY_WORKFLOW,
    },
    SkillSpec {
        name: "cih-documenting",
        description: "Generate and verify architecture documentation grounded in CIH evidence.",
        body: DOCUMENTING_WORKFLOW,
    },
    SkillSpec {
        name: "cih-cli",
        description: "Run CIH indexing, status, wiki, setup, and maintenance commands safely.",
        body: r#"# CIH CLI

Use the unified `cih` executable for the portable workflow.

- `cih index [REPO]` runs analyze, discover, wiki, and agent-context generation.
- `cih index [REPO] --no-agent-context` opts out for that run.
- `cih engine status <name>` checks an indexed repository.
- `cih engine config show --repo <path>` explains effective configuration.
- `cih setup --coding-agent <agents>` configures global MCP and skills.
- `cih uninstall --coding-agent <agents>` previews removal; add `--force` to apply it.

Do not put raw access tokens in configuration. Use an environment-variable name with
`--token-env`, and keep remote MCP endpoints on HTTPS.
"#,
    },
    SkillSpec {
        name: "cih-guide",
        description:
            "Use CIH MCP tools, graph resources, and code-intelligence workflows correctly.",
        body: r#"# CIH Guide

Start with repository status and freshness. Re-index stale repositories before drawing
conclusions from the graph.

1. Use `query` to find relevant execution flows and definitions.
2. Use `context` for a symbol's callers, callees, and process membership.
3. Use `impact` in the upstream direction before editing an existing symbol.
4. Use `detect_changes` before committing to map the diff to affected flows.
5. Use route, API-impact, test-coverage, and taint tools for their specialized questions.

Prefer exact symbol IDs when a short name is ambiguous. An absent graph relationship is
evidence-limited, not proof that runtime coupling cannot exist.
"#,
    },
];

const EXPLORING_WORKFLOW: &str = r#"# Exploring a codebase with CIH

Use this workflow when asked how a feature works, where logic lives, or how the
architecture is connected.

1. Check the repository status and index freshness. Re-run `cih index .` if stale.
2. Use `query` with the user's concept to find ranked execution flows and definitions.
3. Use `context` on the most relevant exact symbol for callers, callees, and process membership.
4. Read the matching process resource for the ordered execution trace.
5. Read only the source ranges needed to confirm behavior.

Prefer graph results over broad text search for relationships. If a short symbol name is
ambiguous, present the candidates or retry with its exact ID and file path. Explain where
graph evidence ends; reflection and runtime wiring may not be fully represented.
"#;

const IMPACT_WORKFLOW: &str = r#"# Impact analysis with CIH

Use this workflow before changing an existing function, method, class, or public contract.

1. Run `impact` in the upstream direction on the exact symbol.
2. Review depth 1 first: these are direct callers or importers most likely to break.
3. Review affected execution processes and functional areas, not only the symbol count.
4. Warn before proceeding when the result is HIGH or CRITICAL.
5. After editing, run `detect_changes` and compare the observed scope with the intended scope.

Risk guide: fewer than five dependents is usually LOW; 5–15 or several processes is MEDIUM;
more than 15 or broad cross-module fan-out is HIGH. Authentication, payments, security, and
public API paths may be CRITICAL even with a smaller raw count. Include test symbols when
scoping regression coverage.
"#;

const DEBUGGING_WORKFLOW: &str = r#"# Debugging with CIH

Use this workflow to trace an error, unexpected behavior, or failing operation.

1. Capture the concrete symptom, entry point, error text, and reproduction boundary.
2. Use `query` with the error and domain concept to find likely flows and definitions.
3. Use `context` on the failing or suspected symbol to inspect incoming and outgoing edges.
4. Use `trace` when both the source and destination symbols are known.
5. Read the process resource and source around the relevant steps; verify guards and data shape.
6. Once the root cause is identified, run upstream `impact` before proposing a code change.

Separate observed facts from hypotheses. A missing graph edge is not proof that reflection,
framework dispatch, generated code, or configuration cannot connect two components.
"#;

const PRODUCT_OWNER_WORKFLOW: &str = r#"# Product and business analysis with CIH

Use this workflow to explain what a service does without requiring the reader to inspect code.

1. Check repository freshness and whether route/community/process data is current.
2. Use the route map to catalogue the HTTP surface and group endpoints by business prefix.
3. Read communities to identify functional areas, size, and cohesion.
4. Read processes to identify named business flows and their entry points.
5. Use `context` on a handler to explain the domain services and downstream effects.
6. Use API impact or change detection to scope proposed sprint work.

Translate internal node IDs into HTTP method + path, functional-area names, and business-flow
language. Call out stale or absent discovery data rather than treating zero results as proof
that a feature does not exist.
"#;

const TESTING_WORKFLOW: &str = r#"# Regression testing with CIH

Use this workflow to decide which unit, integration, and end-to-end tests a change requires.

1. Run `detect_changes` for the staged diff or comparison base.
2. Treat changed symbols as unit-test targets and depth-1/2 dependents as integration scope.
3. Use upstream `impact` with tests included to find existing test callers.
4. Map affected processes to their handlers and routes for end-to-end scenarios.
5. Use test-coverage tools when available and identify explicit coverage gaps.

Require at least one end-to-end pass for each affected authentication or payment process.
When impact crosses functional areas or is HIGH/CRITICAL, recommend broader regression and
cross-team review. Do not infer coverage solely from test-file naming conventions.
"#;

const SECURITY_WORKFLOW: &str = r#"# Security review with CIH

Use this workflow to investigate user-controlled data reaching SQL, command execution, file,
template, or other sensitive sinks.

1. Enumerate persisted taint findings for the repository or target area.
2. Refine by category and inspect the complete ordered source-to-sink path.
3. Read the source and guards around both endpoints and every important hop.
4. Use `context` on the sink and upstream `impact` to scope every reachable entry point.
5. Check regression coverage before recommending a fix.

Prioritize command/code injection, SQL injection, path traversal, and XSS findings by evidence
and reachability. CIH analysis has known blind spots around callbacks, reflection, properties,
and context-sensitive same-name callees; absence of findings is not proof of safety.
"#;

const DOCUMENTING_WORKFLOW: &str = r#"# Architecture documentation with CIH

Use this workflow to create documentation grounded in the current graph rather than guesses.

1. Confirm the index and wiki are current for the repository HEAD.
2. Use repository context, communities, routes, and process resources to outline the system.
3. Use `context` for key entry points and shared services, then verify important source ranges.
4. Link claims to symbol IDs, file paths, routes, or process names as appropriate for the reader.
5. Generate or refresh the CIH wiki when a durable documentation bundle is requested.
6. Verify links and `.cih/wiki/agent-index.json` after generation.

Keep business descriptions separate from implementation detail, state evidence limitations,
and never invent an execution edge merely to make a narrative complete.
"#;

/// Generate repository-local instructions and skills. Existing user-authored content
/// outside CIH's reserved regions is never modified.
pub fn generate_repo_agent_context(
    repo: &Path,
    options: AgentContextOptions,
) -> Result<AgentContextReport> {
    if !options.enabled {
        return Ok(AgentContextReport {
            enabled: false,
            instruction_files: Vec::new(),
            standard_skills: Vec::new(),
            area_skills: Vec::new(),
        });
    }

    let repo = repo
        .canonicalize()
        .with_context(|| format!("repository does not exist: {}", repo.display()))?;
    let area_skills = if options.area_skills {
        build_area_skills(&repo)?
    } else {
        Vec::new()
    };

    for root in SKILL_ROOTS {
        let root = repo.join(root);
        install_standard_skills(&root, true)?;
        sync_area_skills(&root, &area_skills)?;
    }

    let block = instruction_block(&area_skills);
    let mut instruction_files = Vec::new();
    for filename in ["AGENTS.md", "CLAUDE.md"] {
        let path = repo.join(filename);
        upsert_managed_block(&path, &block)?;
        instruction_files.push(filename.to_string());
    }

    Ok(AgentContextReport {
        enabled: true,
        instruction_files,
        standard_skills: STANDARD_SKILLS
            .iter()
            .map(|skill| skill.name.to_string())
            .collect(),
        area_skills: area_skills.iter().map(|skill| skill.name.clone()).collect(),
    })
}

/// Install CIH's standard skill catalog into one global or repository-local root.
pub fn install_standard_skills(root: &Path, overwrite: bool) -> Result<Vec<PathBuf>> {
    let mut installed = Vec::new();
    for skill in STANDARD_SKILLS {
        let path = root.join(skill.name).join("SKILL.md");
        if path.exists() && !overwrite {
            continue;
        }
        let content = format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            skill.name,
            skill.description,
            skill.body.trim()
        );
        atomic_write(&path, &content)?;
        installed.push(path);
    }
    Ok(installed)
}

#[derive(Debug)]
struct AreaSkill {
    name: String,
    content: String,
}

fn build_area_skills(repo: &Path) -> Result<Vec<AreaSkill>> {
    let Some(community_artifacts) =
        GraphArtifacts::latest_in_dir(&repo.join(".cih").join("artifacts-community")).ok()
    else {
        return Ok(Vec::new());
    };
    let communities = community_artifacts.read_nodes()?;
    let memberships = community_artifacts.read_edges()?;
    let graph_nodes = crate::versioning::latest_graph_artifacts(repo)
        .ok()
        .and_then(|artifacts| artifacts.read_nodes().ok())
        .unwrap_or_default();
    let nodes_by_id: BTreeMap<&str, &Node> = graph_nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();

    let mut areas = Vec::new();
    for community in communities
        .iter()
        .filter(|node| node.kind == NodeKind::Community)
    {
        let mut members: Vec<&Node> = memberships
            .iter()
            .filter(|edge| {
                edge.kind == EdgeKind::MemberOf && edge.dst.as_str() == community.id.as_str()
            })
            .filter_map(|edge| nodes_by_id.get(edge.src.as_str()).copied())
            .collect();
        members.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        members.dedup_by(|a, b| a.id == b.id);
        if members.len() < MIN_AREA_SYMBOLS {
            continue;
        }
        areas.push((community, members));
    }
    areas.sort_by(|(a, am), (b, bm)| {
        bm.len()
            .cmp(&am.len())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    areas.truncate(MAX_AREA_SKILLS);

    let mut base_counts = BTreeMap::new();
    for (community, _) in &areas {
        let area = community.name.replace(['\r', '\n'], " ");
        *base_counts
            .entry(format!("{AREA_PREFIX}{}", slug(&area)))
            .or_insert(0usize) += 1;
    }
    let mut used = BTreeSet::new();
    let mut result = Vec::new();
    for (community, members) in areas {
        let area = community.name.replace(['\r', '\n'], " ");
        let base = format!("{AREA_PREFIX}{}", slug(&area));
        let digest = &blake3::hash(community.id.as_str().as_bytes()).to_hex()[..8];
        let mut name = if base_counts.get(&base).copied().unwrap_or_default() > 1 {
            name_with_suffix(&base, digest)
        } else {
            truncate_name(&base)
        };
        if !used.insert(name.clone()) {
            let digest = &blake3::hash(community.id.as_str().as_bytes()).to_hex()[..12];
            name = name_with_suffix(&base, digest);
            used.insert(name.clone());
        }
        let symbols = members
            .iter()
            .take(60)
            .map(|node| {
                if node.file.is_empty() {
                    format!("- `{}`", node.id.as_str())
                } else {
                    format!(
                        "- `{}` — `{}:{}`",
                        node.id.as_str(),
                        node.file,
                        node.range.start_line
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let description = serde_json::to_string(&format!(
            "Work safely in the CIH-detected {area} functional area."
        ))?;
        let content = format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n\
             # {area}\n\n\
             This skill is generated from the latest CIH community artifacts. Use `query` with \
             `{area}` to find current execution flows, then `context` on the exact symbol. Run \
             upstream `impact` before editing and `detect_changes` before committing.\n\n\
             ## Representative symbols\n\n{symbols}\n",
        );
        result.push(AreaSkill { name, content });
    }
    Ok(result)
}

fn sync_area_skills(root: &Path, skills: &[AreaSkill]) -> Result<()> {
    fs::create_dir_all(root)?;
    let wanted: BTreeSet<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir()
            && name.starts_with(AREA_PREFIX)
            && !wanted.contains(name.as_str())
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    for skill in skills {
        atomic_write(&root.join(&skill.name).join("SKILL.md"), &skill.content)?;
    }
    Ok(())
}

fn instruction_block(area_skills: &[AreaSkill]) -> String {
    let areas = if area_skills.is_empty() {
        "- Area skills are unavailable until community discovery has produced at least one area with three symbols.".to_string()
    } else {
        area_skills
            .iter()
            .map(|skill| format!("- `{}`", skill.name))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{START}\n# CIH — Code Intelligence\n\n\
         This repository is indexed by CIH. Use its MCP graph before broad text search when \
         understanding execution flows or dependencies.\n\n\
         - Run upstream impact analysis before editing an existing symbol.\n\
         - Review direct callers and affected processes first; warn before HIGH or CRITICAL changes.\n\
         - Run change detection before committing.\n\
         - Re-run `cih index .` when the index is stale.\n\n\
         Generated documentation is under `.cih/wiki/`; use `.cih/wiki/agent-index.json` for \
         symbol-to-page navigation.\n\n\
         Standard workflows are installed as `cih-exploring`, `cih-impact-analysis`, \
         `cih-debugging`, `cih-product-owner`, `cih-testing`, `cih-security`, \
         `cih-documenting`, `cih-cli`, and `cih-guide`.\n\n\
         ## Repository areas\n\n{areas}\n{END}"
    )
}

fn upsert_managed_block(path: &Path, block: &str) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut normalized = existing.replace("\r\n", "\n");
    let mut kept = marker_block(&normalized, LEGACY_START, LEGACY_END, path)?
        .map(keep_lines)
        .unwrap_or_default();
    normalized = remove_marker_block(&normalized, LEGACY_START, LEGACY_END, path)?;
    kept.extend(
        marker_block(&normalized, START, END, path)?
            .map(keep_lines)
            .unwrap_or_default(),
    );
    kept.sort_unstable();
    kept.dedup();
    let mut replacement = block.to_string();
    if !kept.is_empty() {
        replacement = replacement.replace(
            END,
            &format!("## Preserved notes\n\n{}\n{END}", kept.join("\n")),
        );
    }
    let updated = if let Some((start, end)) = marker_range(&normalized, START, END, path)? {
        format!(
            "{}{}{}",
            &normalized[..start],
            replacement,
            &normalized[end..]
        )
    } else if normalized.trim().is_empty() {
        format!("{replacement}\n")
    } else {
        format!("{}\n\n{replacement}\n", normalized.trim_end_matches('\n'))
    };
    let updated = if newline == "\r\n" {
        updated.replace('\n', "\r\n")
    } else {
        updated
    };
    if updated != existing {
        atomic_write(path, &updated)?;
    }
    Ok(())
}

fn keep_lines(block: &str) -> Vec<String> {
    block
        .lines()
        .filter(|line| line.contains("cih:keep"))
        .map(|line| line.trim_end().to_string())
        .collect()
}

fn marker_block<'a>(text: &'a str, start: &str, end: &str, path: &Path) -> Result<Option<&'a str>> {
    Ok(marker_range(text, start, end, path)?.map(|(a, b)| &text[a..b]))
}

fn marker_range(text: &str, start: &str, end: &str, path: &Path) -> Result<Option<(usize, usize)>> {
    let raw_starts: Vec<_> = text.match_indices(start).map(|(index, _)| index).collect();
    let raw_ends: Vec<_> = text.match_indices(end).map(|(index, _)| index).collect();
    let is_line_marker = |index: usize, marker: &str| {
        (index == 0 || text.as_bytes().get(index.wrapping_sub(1)) == Some(&b'\n'))
            && (index + marker.len() == text.len()
                || text.as_bytes().get(index + marker.len()) == Some(&b'\n'))
    };
    let starts: Vec<_> = raw_starts
        .iter()
        .copied()
        .filter(|index| is_line_marker(*index, start))
        .collect();
    let ends: Vec<_> = raw_ends
        .iter()
        .copied()
        .filter(|index| is_line_marker(*index, end))
        .collect();
    if raw_starts.len() != starts.len() || raw_ends.len() != ends.len() {
        bail!(
            "invalid CIH marker layout in {}: markers must be on their own lines",
            path.display()
        );
    }
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([a], [b]) if a < b => Ok(Some((*a, *b + end.len()))),
        _ => bail!(
            "invalid CIH marker layout in {}: expected one ordered start/end pair",
            path.display()
        ),
    }
}

fn remove_marker_block(text: &str, start: &str, end: &str, path: &Path) -> Result<String> {
    let Some((a, b)) = marker_range(text, start, end, path)? else {
        return Ok(text.to_string());
    };
    let before = text[..a].trim_end_matches('\n');
    let after = text[b..].trim_start_matches('\n');
    Ok(match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{after}\n"),
        (false, true) => format!("{before}\n"),
        (false, false) => format!("{before}\n\n{after}"),
    })
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("managed file has no parent")?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cih");
    let tmp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out.to_string()
    }
}

fn truncate_name(name: &str) -> String {
    name.chars()
        .take(64)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

fn name_with_suffix(base: &str, suffix: &str) -> String {
    let max_base = 64usize.saturating_sub(suffix.len() + 1);
    let base = base
        .chars()
        .take(max_base)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string();
    format!("{base}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_upsert_preserves_content_keep_lines_and_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        fs::write(
            &path,
            "user\r\n\r\n<!-- cih:start -->\r\nold\r\ncustom cih:keep\r\n<!-- cih:end -->\r\n",
        )
        .unwrap();
        upsert_managed_block(&path, "<!-- cih:start -->\nnew\n<!-- cih:end -->").unwrap();
        let output = fs::read_to_string(path).unwrap();
        assert!(output.starts_with("user\r\n"));
        assert!(output.contains("new\r\n"));
        assert!(output.contains("custom cih:keep\r\n"));
        assert!(!output.contains("\nold\n"));
        assert!(!output.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn malformed_markers_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CLAUDE.md");
        fs::write(&path, "<!-- cih:start -->\nmissing end\n").unwrap();
        assert!(upsert_managed_block(&path, "unused").is_err());
        fs::write(&path, "prefix <!-- cih:start -->\n<!-- cih:end -->\n").unwrap();
        assert!(upsert_managed_block(&path, "unused").is_err());
    }

    #[test]
    fn legacy_marker_is_migrated_and_keep_line_survives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        fs::write(
            &path,
            "user\n\n<!-- cih-wiki:start -->\nlegacy\nowner note cih:keep\n<!-- cih-wiki:end -->\n",
        )
        .unwrap();
        upsert_managed_block(&path, "<!-- cih:start -->\nnew\n<!-- cih:end -->").unwrap();
        let output = fs::read_to_string(path).unwrap();
        assert!(output.starts_with("user\n"));
        assert!(output.contains("<!-- cih:start -->"));
        assert!(!output.contains("cih-wiki:start"));
        assert!(output.contains("owner note cih:keep"));
    }

    #[test]
    fn slug_and_name_are_agent_compatible() {
        assert_eq!(slug("Payments / API"), "payments-api");
        assert!(truncate_name(&format!("{AREA_PREFIX}{}", "x".repeat(100))).len() <= 64);
    }
}
