//! Phase 3 parse driver: selected Java files -> structure graph + unresolved IR.
//!
//! This crate intentionally stops before Phase 4 resolution. It emits stable
//! structure nodes/edges and persists `ParsedFile`s containing raw imports and
//! unresolved reference sites for the next phase.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{file_id, folder_id, Edge, EdgeKind, Node, NodeId, ParsedFile, Range};
use cih_lang::LanguageProvider;
use rayon::prelude::*;

pub mod sql;

pub use cih_core::ParsedUnit;

#[derive(Clone, Debug, Default)]
pub struct ParseOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub parsed_files: Vec<ParsedFile>,
    /// Files that could not be read/parsed. The rest of the run still succeeds —
    /// on a large repo one bad file must not abort indexing.
    pub skipped: Vec<SkippedFile>,
}

#[derive(Clone, Debug)]
pub struct SkippedFile {
    pub rel: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct ParseUnitsOutput {
    pub units: Vec<ParsedUnit>,
    pub skipped: Vec<SkippedFile>,
}

#[derive(Clone, Debug)]
pub struct ParseArtifacts {
    pub parsed_files_path: PathBuf,
}

pub struct LanguageRegistry {
    providers: Vec<Box<dyn LanguageProvider>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self { providers: vec![] }
    }

    pub fn register(&mut self, p: impl LanguageProvider + 'static) {
        self.providers.push(Box::new(p));
    }

    pub fn register_boxed(&mut self, p: Box<dyn LanguageProvider>) {
        self.providers.push(p);
    }

    pub fn provider_for(&self, path: &str) -> Option<&dyn LanguageProvider> {
        self.providers
            .iter()
            .find(|p| p.extensions().iter().any(|ext| path.ends_with(ext)))
            .map(|p| p.as_ref())
    }

    pub fn all_extensions(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .flat_map(|p| p.extensions().iter().copied())
            .collect()
    }
}

pub fn parse_files(
    repo_root: &Path,
    files: &[String],
    registry: &LanguageRegistry,
) -> Result<ParseOutput> {
    let output = parse_file_units(repo_root, files, registry)?;
    Ok(parse_output_from_units(output.units, output.skipped))
}

pub fn parse_file_units(
    repo_root: &Path,
    files: &[String],
    registry: &LanguageRegistry,
) -> Result<ParseUnitsOutput> {
    // Per-file failures are collected, not propagated: one unreadable/garbage file
    // must not abort indexing of a 12k-file repo.
    let results = files
        .par_iter()
        .map(|rel| (rel.clone(), parse_one(registry, repo_root, rel)))
        .collect::<Vec<_>>();

    let mut units = Vec::new();
    let mut skipped = Vec::new();
    for (rel, result) in results {
        match result {
            Ok(unit) => units.push(unit),
            Err(err) => skipped.push(SkippedFile {
                rel,
                reason: format!("{err:#}"),
            }),
        }
    }
    units.sort_by(|a, b| a.rel.cmp(&b.rel));
    skipped.sort_by(|a, b| a.rel.cmp(&b.rel));

    Ok(ParseUnitsOutput { units, skipped })
}

pub fn parse_output_from_units(
    mut units: Vec<ParsedUnit>,
    mut skipped: Vec<SkippedFile>,
) -> ParseOutput {
    units.sort_by(|a, b| a.rel.cmp(&b.rel));
    skipped.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut nodes = BTreeMap::new();
    let mut edges = BTreeMap::new();
    let mut parsed_files = Vec::new();

    for unit in units {
        add_structure_path(&unit.rel, &mut nodes, &mut edges);
        for node in unit.nodes {
            insert_node(&mut nodes, node);
        }
        for edge in unit.edges {
            insert_edge(&mut edges, edge);
        }
        parsed_files.push(unit.parsed_file);
    }

    ParseOutput {
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
        parsed_files,
        skipped,
    }
}

pub fn write_parsed_files(dir: &Path, parsed_files: &[ParsedFile]) -> Result<ParseArtifacts> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let parsed_files_path = dir.join("parsed-files.jsonl");
    let mut writer = BufWriter::new(
        File::create(&parsed_files_path)
            .with_context(|| format!("failed to create {}", parsed_files_path.display()))?,
    );
    for parsed in parsed_files {
        serde_json::to_writer(&mut writer, parsed)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;

    Ok(ParseArtifacts { parsed_files_path })
}

/// Read a `parsed-files.jsonl` produced by [`write_parsed_files`] back into memory.
pub fn load_parsed_files(dir: &Path) -> Result<Vec<ParsedFile>> {
    let path = dir.join("parsed-files.jsonl");
    let reader = BufReader::new(
        File::open(&path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("read error at line {} of {}", i + 1, path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let pf: ParsedFile = serde_json::from_str(&line)
            .with_context(|| format!("parse error at line {} of {}", i + 1, path.display()))?;
        out.push(pf);
    }
    Ok(out)
}

fn parse_one(registry: &LanguageRegistry, repo_root: &Path, rel: &str) -> Result<ParsedUnit> {
    let path = repo_root.join(rel);
    let src =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    // The File node + its Folder ancestry + CONTAINS edges are emitted centrally by
    // `add_structure_path` during merge, so the parse unit only carries declarations.
    let provider = registry
        .provider_for(rel)
        .ok_or_else(|| anyhow::anyhow!("no language provider for {rel}"))?;
    let mut unit = provider.parse_file(rel, &src)?;
    unit.parsed_file.language = provider.language_id().to_string();
    Ok(unit)
}

fn add_structure_path(
    rel: &str,
    nodes: &mut BTreeMap<String, Node>,
    edges: &mut BTreeMap<(String, String, &'static str), Edge>,
) {
    let parts = rel
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let mut current = String::new();
    let mut parent: Option<NodeId> = None;

    for (index, part) in parts.iter().enumerate() {
        if current.is_empty() {
            current.push_str(part);
        } else {
            current.push('/');
            current.push_str(part);
        }

        let is_file = index + 1 == parts.len();
        let id = if is_file {
            file_id(&current)
        } else {
            folder_id(&current)
        };
        let kind = if is_file {
            cih_core::NodeKind::File
        } else {
            cih_core::NodeKind::Folder
        };
        insert_node(
            nodes,
            Node {
                id: id.clone(),
                kind,
                name: (*part).to_string(),
                qualified_name: None,
                file: current.clone(),
                range: Range::default(),
                props: Some(serde_json::json!({ "filePath": current })),
            },
        );

        if let Some(parent_id) = parent {
            insert_edge(
                edges,
                Edge {
                    src: parent_id,
                    dst: id.clone(),
                    kind: EdgeKind::Contains,
                    confidence: 1.0,
                    reason: "structure".into(),
                    props: None,
                },
            );
        }
        parent = Some(id);
    }
}

fn insert_node(nodes: &mut BTreeMap<String, Node>, node: Node) {
    nodes.entry(node.id.as_str().to_string()).or_insert(node);
}

fn insert_edge(edges: &mut BTreeMap<(String, String, &'static str), Edge>, edge: Edge) {
    edges
        .entry((
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        ))
        .or_insert(edge);
}

#[cfg(test)]
mod tests;

