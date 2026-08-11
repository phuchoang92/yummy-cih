use anyhow::{anyhow, Context as _};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const REPOSITORY_IDENTITY_SCHEMA: u8 = 1;
const REPOSITORY_IDENTITY_FILE: &str = "repository-identity.json";
const GRAPH_REPORT_SCHEMA: u8 = 1;
pub const GRAPH_REPORT_HUB_LIMIT: usize = 256;
pub const GRAPH_REPORT_MAX_BYTES: usize = 256 * 1024;

/// Portable identity of a repository, independent of its mutable path, display
/// name, graph key, or host.
///
/// IDs are full, canonical BLAKE3 digests over operating-system entropy. They
/// are allocated once and then copied between the repository-owned identity
/// record and the global registry; they are never reconstructed from a path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RepositoryId(String);

impl RepositoryId {
    pub fn parse(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(anyhow!(
                "repository ID must be exactly 64 lowercase hexadecimal characters"
            ));
        }
        Ok(Self(value))
    }

    pub fn allocate() -> Self {
        Self(allocate_opaque_digest(b"cih-repository-id-v1\0"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RepositoryId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for RepositoryId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn allocate_opaque_digest(domain: &[u8]) -> String {
    let mut entropy = [0_u8; 32];
    rand::rng().fill_bytes(&mut entropy);
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&entropy);
    hasher.finalize().to_hex().to_string()
}

#[derive(Serialize, Deserialize)]
struct RepositoryIdentityDocument {
    schema_version: u8,
    repository_id: RepositoryId,
}

fn repository_identity_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".cih").join(REPOSITORY_IDENTITY_FILE)
}

/// Read the repository-owned identity record without creating one.
pub fn load_repository_id(repo_root: &Path) -> anyhow::Result<Option<RepositoryId>> {
    let path = repository_identity_path(repo_root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read repository identity {}", path.display()));
        }
    };
    let document = serde_json::from_slice::<RepositoryIdentityDocument>(&bytes)
        .with_context(|| format!("failed to parse repository identity {}", path.display()))?;
    if document.schema_version != REPOSITORY_IDENTITY_SCHEMA {
        return Err(anyhow!(
            "unsupported repository identity schema {} at {}",
            document.schema_version,
            path.display()
        ));
    }
    Ok(Some(document.repository_id))
}

/// Return the immutable repository identity, creating its repository-owned
/// record when absent. A preferred registry ID is adopted during migration;
/// an existing conflicting record is rejected instead of silently changing
/// identity.
pub fn ensure_repository_id(
    repo_root: &Path,
    preferred: Option<&RepositoryId>,
) -> anyhow::Result<RepositoryId> {
    let path = repository_identity_path(repo_root);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("repository identity path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create repository metadata directory {}",
            parent.display()
        )
    })?;
    let lock_path = sibling_with_suffix(&path, ".lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "failed to open repository identity lock {}",
                lock_path.display()
            )
        })?;
    lock.lock()
        .with_context(|| format!("failed to lock repository identity {}", path.display()))?;

    if let Some(existing) = load_repository_id(repo_root)? {
        if preferred.is_some_and(|preferred| preferred != &existing) {
            return Err(anyhow!(
                "repository identity conflict at {}: registry has {}, repository has {}",
                repo_root.display(),
                preferred.expect("preferred ID checked above"),
                existing
            ));
        }
        return Ok(existing);
    }

    let repository_id = preferred.cloned().unwrap_or_else(RepositoryId::allocate);
    let encoded = serde_json::to_vec_pretty(&RepositoryIdentityDocument {
        schema_version: REPOSITORY_IDENTITY_SCHEMA,
        repository_id: repository_id.clone(),
    })?;
    write_synced_then_rename(&path, &encoded)?;
    sync_parent_directory(&path)?;
    Ok(repository_id)
}

/// Compatibility graph-content identity for the current base/overlay loader.
/// Components are domain-tagged and order-sensitive; the result is always a
/// full digest even while the analyzer's legacy artifact version is shorter.
pub fn graph_content_version(base_artifact_version: &str, overlays: &[(&str, &str)]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cih-graph-content-compat-v1\0");
    hash_length_prefixed(&mut hasher, base_artifact_version.as_bytes());
    for (kind, version) in overlays {
        hash_length_prefixed(&mut hasher, kind.as_bytes());
        hash_length_prefixed(&mut hasher, version.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Fresh opaque mirror epoch for a successful legacy publication. This is not
/// a timestamp or content hash; identical content receives a new epoch.
pub fn new_publication_epoch() -> String {
    allocate_opaque_digest(b"cih-graph-publication-epoch-v1\0")
}

fn hash_length_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Exact publication-bound count for one logical node kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryKindCount {
    pub kind: String,
    pub count: u64,
}

/// One deterministic high-degree symbol retained for bounded overview reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryGraphHub {
    pub node: crate::Node,
    pub degree: u64,
}

/// Small reporting projection bound to an exact published graph composition.
///
/// The builder refuses duplicate node IDs, duplicate reduced edge keys, and
/// dangling endpoints. That makes the counts equal to the loader's logical
/// graph only when the artifact composition itself proves that equality. A
/// failed proof leaves this metadata absent and serving falls back to the
/// legacy graph query instead of presenting an input-line count as truth.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryGraphReport {
    pub schema_version: u8,
    pub graph_content_version: String,
    pub total_nodes: u64,
    pub total_edges: u64,
    pub kinds: Vec<RegistryKindCount>,
    pub symbol_hubs: Vec<RegistryGraphHub>,
}

impl RegistryGraphReport {
    pub fn try_build(
        graph_content_version: String,
        node_sets: &[&[crate::Node]],
        edge_sets: &[&[crate::Edge]],
    ) -> Result<Self, String> {
        let estimated_nodes = node_sets.iter().map(|nodes| nodes.len()).sum();
        let mut nodes_by_id = HashMap::with_capacity(estimated_nodes);
        let mut kinds = BTreeMap::<String, u64>::new();
        for nodes in node_sets {
            for node in *nodes {
                if nodes_by_id.insert(node.id.as_str(), node).is_some() {
                    return Err(format!("duplicate node id {}", node.id));
                }
                *kinds.entry(node.kind.label().to_string()).or_default() += 1;
            }
        }

        let estimated_edges = edge_sets.iter().map(|edges| edges.len()).sum();
        let mut edge_keys = HashSet::with_capacity(estimated_edges);
        let mut degrees = HashMap::<&str, u64>::with_capacity(nodes_by_id.len());
        for edges in edge_sets {
            for edge in *edges {
                if !nodes_by_id.contains_key(edge.src.as_str())
                    || !nodes_by_id.contains_key(edge.dst.as_str())
                {
                    return Err(format!(
                        "edge {}-[:{}]->{} has a missing endpoint",
                        edge.src,
                        edge.kind.cypher_label(),
                        edge.dst
                    ));
                }
                let key = (
                    edge.src.as_str(),
                    edge.dst.as_str(),
                    edge.kind.cypher_label(),
                );
                if !edge_keys.insert(key) {
                    return Err(format!(
                        "duplicate edge {}-[:{}]->{}",
                        edge.src,
                        edge.kind.cypher_label(),
                        edge.dst
                    ));
                }
                *degrees.entry(edge.src.as_str()).or_default() += 1;
                *degrees.entry(edge.dst.as_str()).or_default() += 1;
            }
        }

        let mut kinds = kinds
            .into_iter()
            .map(|(kind, count)| RegistryKindCount { kind, count })
            .collect::<Vec<_>>();
        kinds.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.kind.cmp(&b.kind)));

        let mut symbol_hubs = nodes_by_id
            .values()
            .filter(|node| {
                matches!(
                    node.kind,
                    crate::NodeKind::Class
                        | crate::NodeKind::Interface
                        | crate::NodeKind::Function
                        | crate::NodeKind::Method
                )
            })
            .map(|node| {
                let mut projected = (*node).clone();
                // Reporting needs identity/source location only. Analyzer and
                // framework properties can be arbitrarily large and must not
                // turn the bounded hub list into an unbounded registry value.
                projected.props = None;
                RegistryGraphHub {
                    node: projected,
                    degree: degrees.get(node.id.as_str()).copied().unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        symbol_hubs.sort_by(|a, b| {
            b.degree
                .cmp(&a.degree)
                .then_with(|| a.node.id.as_str().cmp(b.node.id.as_str()))
        });
        symbol_hubs.truncate(GRAPH_REPORT_HUB_LIMIT);

        let report = Self {
            schema_version: GRAPH_REPORT_SCHEMA,
            graph_content_version,
            total_nodes: nodes_by_id.len() as u64,
            total_edges: edge_keys.len() as u64,
            kinds,
            symbol_hubs,
        };
        let encoded_bytes = serde_json::to_vec(&report)
            .map_err(|error| format!("could not size graph report: {error}"))?
            .len();
        if encoded_bytes > GRAPH_REPORT_MAX_BYTES {
            return Err(format!(
                "graph report is {encoded_bytes} bytes, above the {GRAPH_REPORT_MAX_BYTES}-byte limit"
            ));
        }
        Ok(report)
    }

    pub fn matches_content(&self, graph_content_version: &str) -> bool {
        self.schema_version == GRAPH_REPORT_SCHEMA
            && self.graph_content_version == graph_content_version
    }

    /// Validate persisted metadata again at the serving boundary. The builder
    /// enforces these properties for new reports, while this check also protects
    /// legacy/manual registry JSON from bypassing the hub and byte limits.
    pub fn is_usable_for(&self, graph_content_version: &str) -> bool {
        self.matches_content(graph_content_version)
            && self.kinds.iter().map(|kind| kind.count).sum::<u64>() == self.total_nodes
            && self.symbol_hubs.len() <= GRAPH_REPORT_HUB_LIMIT
            && self.symbol_hubs.iter().all(|hub| hub.node.props.is_none())
            && serde_json::to_vec(self).is_ok_and(|encoded| encoded.len() <= GRAPH_REPORT_MAX_BYTES)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegistryStats {
    pub nodes: usize,
    pub edges: usize,
    pub files: usize,
    pub routes: usize,
    /// Whether `routes` was measured by a pipeline version that records Route
    /// nodes during analyze/discover. Legacy registry entries default to false
    /// so a historical zero is not presented as a current codebase fact.
    #[serde(default)]
    pub routes_current: bool,
    pub communities: usize,
    pub processes: usize,
    /// Index *quality*, not just size — persisted so `status` can answer "is this
    /// graph any good?". Previously computed by analyze and then dropped on the
    /// floor, which is part of why a near-zero-coverage index looked healthy.
    /// `#[serde(default)]`: entries written before these existed still load.
    #[serde(default)]
    pub resolved_edges: usize,
    #[serde(default)]
    pub unresolved_refs: u64,
    #[serde(default)]
    pub unresolved_internal_refs: u64,
    #[serde(default)]
    pub unresolved_external_refs: u64,
    #[serde(default)]
    pub unresolved_dynamic_refs: u64,
    #[serde(default)]
    pub reference_site_count: u64,
    #[serde(default)]
    pub resolved_reference_count: u64,
    #[serde(default)]
    pub measured_callable_node_count: usize,
    #[serde(default)]
    pub syntactic_callables: u32,
    /// Emitted callable nodes ÷ callables in the AST. `None` when unmeasured (no
    /// provider in scope opts in, or the run was a cached no-op).
    #[serde(default)]
    pub callable_coverage: Option<f64>,
    /// Exact counts and bounded hubs for the publication named by
    /// `RegistryEntry.published_graph_content_version`. Legacy entries and
    /// artifact compositions that cannot prove reducer uniqueness leave this
    /// absent, preserving the existing live-query fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_graph_report: Option<RegistryGraphReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// Immutable portable identity. Missing only while deserializing a legacy
    /// entry; `RegistryStore` allocates and persists it before returning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<RepositoryId>,
    pub name: String,
    pub path: String,
    pub graph_key: String,
    pub artifacts_dir: String,
    /// Most recent complete base artifact on disk, whether or not publication
    /// succeeded. Legacy entries are migrated from a canonical artifact path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_artifact_version: Option<String>,
    /// Base artifact currently mirrored as published. This is deliberately
    /// separate from `latest_artifact_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_artifact_version: Option<String>,
    /// Full digest of the exact published base-plus-overlay composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_graph_content_version: Option<String>,
    /// Fresh opaque value for the latest successful publication transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_epoch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_artifacts_dir: Option<String>,
    pub indexed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_git_head: Option<String>,
    pub stats: RegistryStats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub entries: Vec<RegistryEntry>,
}

/// Durable identity of one registry snapshot.
///
/// `sequence` advances for every content-changing transaction. The digest is a
/// full BLAKE3 hash of the canonical logical registry content and deliberately
/// excludes the sequence itself, so equal content has an equal digest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRevision {
    pub sequence: u64,
    pub content_digest: String,
}

/// Registry plus the revision loaded from its durable envelope.
#[derive(Clone, Debug)]
pub struct RegistrySnapshot {
    pub registry: Registry,
    pub revision: RegistryRevision,
    /// True when the primary was absent, malformed, or failed digest validation
    /// and the last-known-good backup was used instead.
    pub recovered_from_backup: bool,
}

/// Result of a transactional registry mutation.
#[derive(Clone, Debug)]
pub struct RegistryUpdate<T> {
    pub value: T,
    pub snapshot: RegistrySnapshot,
    /// False when the closure left the canonical registry content unchanged.
    pub changed: bool,
}

/// Path-scoped durable registry storage.
///
/// Writers take an inter-process lock, re-read the latest snapshot while that
/// lock is held, and only then apply the mutation. This is the important
/// distinction from a `load()` followed later by `save()`: two processes cannot
/// silently overwrite one another's updates.
#[derive(Clone, Debug)]
pub struct RegistryStore {
    path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RegistryDocument {
    /// Legacy files had no revision metadata, so both fields default cleanly.
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    content_digest: String,
    entries: Vec<RegistryEntry>,
}

#[derive(Deserialize)]
struct RawRegistryDocument {
    entries: Vec<Box<RawValue>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistrySource {
    Empty,
    Primary,
    Backup,
}

struct LoadedRegistry {
    document: RegistryDocument,
    source: RegistrySource,
    needs_canonical_repair: bool,
}

struct ReadRegistryDocument {
    document: RegistryDocument,
    needs_canonical_repair: bool,
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn registry_path() -> Option<std::path::PathBuf> {
    crate::cih_home().map(|dir| dir.join("registry.json"))
}

/// Current time as RFC-3339 UTC (no external dep required).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix_secs_to_rfc3339(secs)
}

#[doc(hidden)]
pub fn unix_secs_to_rfc3339(secs: u64) -> String {
    let tod = secs % 86400;
    let mut days = secs / 86400;
    let h = tod / 3600;
    let m = (tod / 60) % 60;
    let s = tod % 60;
    let mut y = 1970u64;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let mut mo = 1u64;
    loop {
        let dim = month_days(mo, y);
        if days < dim {
            break;
        }
        days -= dim;
        mo += 1;
    }
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z", d = days + 1)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn month_days(m: u64, y: u64) -> u64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Returns the current git HEAD SHA for the given repo path, or None.
pub fn git_head(repo_path: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Returns the list of files changed between `since_ref` and HEAD (`git diff --name-only <ref>`).
/// Returns an empty vec when git is unavailable or the ref is invalid.
pub fn git_changed_files(repo_path: &Path, since_ref: &str) -> Vec<String> {
    // Refuse refs that could be parsed as git options (e.g. `--output=…`) and
    // terminate the option list with `--` so the ref is always treated as a ref.
    if since_ref.starts_with('-') {
        return vec![];
    }
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", since_ref, "--"])
        .current_dir(repo_path)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

impl RegistryDocument {
    fn empty() -> Self {
        Self {
            revision: 0,
            content_digest: String::new(),
            entries: Vec::new(),
        }
    }

    fn canonical(revision: u64, entries: Vec<RegistryEntry>) -> anyhow::Result<Self> {
        let entries = normalize_registry_entries(&entries)?;
        Ok(Self {
            revision,
            content_digest: digest_normalized_entries(&entries)?,
            entries,
        })
    }

    fn validate(
        self,
        raw: &RawRegistryDocument,
        path: &Path,
    ) -> anyhow::Result<ReadRegistryDocument> {
        match (self.revision, self.content_digest.is_empty()) {
            // Legacy registry: no revision metadata existed yet.
            (0, true) => Ok(ReadRegistryDocument {
                document: Self {
                    entries: normalize_registry_entries(&self.entries)?,
                    ..self
                },
                needs_canonical_repair: false,
            }),
            (0, false) => Err(anyhow!(
                "registry {} has a content digest but revision is zero",
                path.display()
            )),
            (_, true) => Err(anyhow!(
                "registry {} revision {} is missing its content digest",
                path.display(),
                self.revision
            )),
            _ => {
                let entries = normalize_registry_entries(&self.entries)?;
                let canonical_digest = digest_normalized_entries(&entries)?;
                if canonical_digest == self.content_digest {
                    return Ok(ReadRegistryDocument {
                        document: Self { entries, ..self },
                        needs_canonical_repair: false,
                    });
                }

                let legacy_digest = legacy_raw_content_digest(&raw.entries)?;
                if legacy_digest == self.content_digest {
                    tracing::warn!(
                        registry = %path.display(),
                        revision = self.revision,
                        legacy_digest = %self.content_digest,
                        canonical_digest = %canonical_digest,
                        "registry uses a legacy float-sensitive digest; repairing canonical representation"
                    );
                    return Ok(ReadRegistryDocument {
                        document: Self {
                            content_digest: canonical_digest,
                            entries,
                            ..self
                        },
                        needs_canonical_repair: true,
                    });
                }

                Err(anyhow!(
                    "registry {} content digest mismatch: expected {}, computed {}",
                    path.display(),
                    self.content_digest,
                    canonical_digest
                ))
            }
        }
    }

    fn into_snapshot(self, recovered_from_backup: bool) -> RegistrySnapshot {
        RegistrySnapshot {
            registry: Registry {
                entries: self.entries,
            },
            revision: RegistryRevision {
                sequence: self.revision,
                content_digest: self.content_digest,
            },
            recovered_from_backup,
        }
    }
}

/// Hash the logical entry set, independent of insertion order and JSON layout.
fn canonical_content_digest(entries: &[RegistryEntry]) -> anyhow::Result<String> {
    let normalized = normalize_registry_entries(entries)?;
    digest_normalized_entries(&normalized)
}

/// Force every entry through serde's deserialize boundary before it is hashed
/// or persisted. In particular, this makes a floating-point value use the same
/// representation before and after the registry is read back from disk.
fn normalize_registry_entries(entries: &[RegistryEntry]) -> anyhow::Result<Vec<RegistryEntry>> {
    entries
        .iter()
        .map(|entry| {
            let encoded = serde_json::to_vec(entry)?;
            Ok(serde_json::from_slice(&encoded)?)
        })
        .collect()
}

fn digest_normalized_entries(entries: &[RegistryEntry]) -> anyhow::Result<String> {
    let mut encoded_entries = entries
        .iter()
        .map(serde_json::to_vec)
        .collect::<Result<Vec<_>, _>>()?;
    encoded_entries.sort_unstable();

    digest_encoded_entries(encoded_entries.iter().map(Vec::as_slice))
}

fn legacy_raw_content_digest(entries: &[Box<RawValue>]) -> anyhow::Result<String> {
    let mut encoded_entries = entries
        .iter()
        .map(|entry| compact_json_lexeme(entry.get()))
        .collect::<Vec<_>>();
    encoded_entries.sort_unstable();

    digest_encoded_entries(encoded_entries.iter().map(Vec::as_slice))
}

fn digest_encoded_entries<'a>(
    entries: impl IntoIterator<Item = &'a [u8]>,
) -> anyhow::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cih-registry-content-v1\0");
    for encoded in entries {
        let len = u64::try_from(encoded.len())
            .map_err(|_| anyhow!("registry entry is too large to hash"))?;
        hasher.update(&len.to_le_bytes());
        hasher.update(encoded);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Minify already-validated JSON without parsing numeric tokens. This is used
/// only to verify registries written by the legacy float-sensitive digest path:
/// string contents and numeric lexemes remain byte-for-byte intact.
fn compact_json_lexeme(raw: &str) -> Vec<u8> {
    let mut compact = Vec::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;

    for byte in raw.bytes() {
        if in_string {
            compact.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
            compact.push(byte);
        } else if !matches!(byte, b' ' | b'\n' | b'\r' | b'\t') {
            compact.push(byte);
        }
    }

    compact
}

fn artifact_version_from_path(path: &str) -> Option<String> {
    let path = Path::new(path);
    let parent = path.parent()?;
    (parent.file_name()?.to_str()? == "artifacts")
        .then(|| path.file_name()?.to_str().map(str::to_string))?
}

/// Upgrade legacy entry metadata while the registry lock is held. This is
/// deliberately idempotent: assigned IDs and publication fields are never
/// regenerated, and only absent latest-artifact metadata is inferred.
fn migrate_registry_entries(entries: &mut [RegistryEntry]) -> anyhow::Result<bool> {
    let mut changed = false;
    let mut assigned = HashMap::<RepositoryId, String>::new();

    for entry in entries.iter() {
        let Some(repository_id) = entry.repository_id.as_ref() else {
            continue;
        };
        if let Some(previous_path) = assigned.insert(repository_id.clone(), entry.path.clone()) {
            return Err(anyhow!(
                "repository ID {} is assigned to both {} and {}",
                repository_id,
                previous_path,
                entry.path
            ));
        }
    }

    for entry in entries {
        if entry.repository_id.is_none() {
            let recorded = load_repository_id(Path::new(&entry.path))?;
            let repository_id = match recorded {
                Some(repository_id) => repository_id,
                None => loop {
                    let candidate = RepositoryId::allocate();
                    if !assigned.contains_key(&candidate) {
                        break candidate;
                    }
                },
            };
            if let Some(previous_path) = assigned.get(&repository_id) {
                return Err(anyhow!(
                    "repository ID {} is assigned to both {} and {}",
                    repository_id,
                    previous_path,
                    entry.path
                ));
            }
            assigned.insert(repository_id.clone(), entry.path.clone());
            entry.repository_id = Some(repository_id);
            changed = true;
        }

        if entry.latest_artifact_version.is_none() {
            if let Some(version) = artifact_version_from_path(&entry.artifacts_dir) {
                entry.latest_artifact_version = Some(version);
                changed = true;
            }
        }
    }

    Ok(changed)
}

fn validate_immutable_repository_ids(
    previous: &[RegistryEntry],
    next: &[RegistryEntry],
) -> anyhow::Result<()> {
    for previous_entry in previous {
        let Some(previous_id) = previous_entry.repository_id.as_ref() else {
            continue;
        };
        let Some(next_entry) = next
            .iter()
            .find(|candidate| candidate.path == previous_entry.path)
        else {
            continue;
        };
        if next_entry.repository_id.as_ref() != Some(previous_id) {
            return Err(anyhow!(
                "repository ID for {} is immutable (existing {}, attempted {})",
                previous_entry.path,
                previous_id,
                next_entry
                    .repository_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "missing".to_string())
            ));
        }
    }
    Ok(())
}

impl RegistryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn global() -> anyhow::Result<Self> {
        registry_path()
            .map(Self::new)
            .ok_or_else(|| anyhow!("cannot determine HOME for registry path"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> anyhow::Result<RegistrySnapshot> {
        let _lock = self.lock_exclusive()?;
        let loaded = self.load_document_unlocked()?;
        let recovered_from_backup = loaded.source == RegistrySource::Backup;
        let needs_canonical_repair = loaded.needs_canonical_repair;
        let old_document = loaded.document;
        let mut entries = old_document.entries.clone();
        let migrated = migrate_registry_entries(&mut entries)?;

        if migrated {
            let new_document = RegistryDocument::canonical(
                old_document
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("registry revision overflow"))?,
                entries,
            )?;
            self.persist_identity_migration(&new_document)?;
            invalidate_registry_cache();
            return Ok(new_document.into_snapshot(recovered_from_backup));
        }

        if recovered_from_backup {
            // Repair the primary with the exact recovered revision. This is not
            // a logical mutation and therefore must not advance the sequence.
            self.persist_identity_migration(&old_document)?;
            invalidate_registry_cache();
        } else if needs_canonical_repair {
            // A legacy float-sensitive digest is a representation repair, not
            // a logical mutation. Rewrite only the primary and retain both the
            // revision and the existing last-known-good backup.
            self.persist_primary_repair(&old_document)?;
            invalidate_registry_cache();
        }
        Ok(old_document.into_snapshot(recovered_from_backup))
    }

    /// Apply `mutator` to the latest locked registry snapshot and durably
    /// publish the result. Equal logical content is a no-op and does not advance
    /// the sequence.
    pub fn update<T>(
        &self,
        mutator: impl FnOnce(&mut Registry) -> anyhow::Result<T>,
    ) -> anyhow::Result<RegistryUpdate<T>> {
        let _lock = self.lock_exclusive()?;
        let loaded = self.load_document_unlocked()?;
        let needs_canonical_repair = loaded.needs_canonical_repair;
        let before_digest = canonical_content_digest(&loaded.document.entries)?;
        let old_document = loaded.document;
        let mut registry = Registry {
            entries: old_document.entries.clone(),
        };

        let migrated_existing_entries = migrate_registry_entries(&mut registry.entries)?;

        let value = mutator(&mut registry)?;
        migrate_registry_entries(&mut registry.entries)?;
        validate_immutable_repository_ids(&old_document.entries, &registry.entries)?;
        let content_digest = canonical_content_digest(&registry.entries)?;
        let changed = content_digest != before_digest;
        let repairing_primary = loaded.source == RegistrySource::Backup;

        if !changed && !repairing_primary && !needs_canonical_repair {
            return Ok(RegistryUpdate {
                value,
                snapshot: old_document.into_snapshot(false),
                changed: false,
            });
        }

        let revision = if changed {
            old_document
                .revision
                .checked_add(1)
                .ok_or_else(|| anyhow!("registry revision overflow"))?
        } else {
            old_document.revision
        };
        let new_document = RegistryDocument::canonical(revision, registry.entries)?;
        debug_assert_eq!(new_document.content_digest, content_digest);

        if migrated_existing_entries {
            // Both copies must carry IDs assigned to entries that already
            // existed in the loaded snapshot. If the process dies between the
            // two renames, the higher-revision backup repairs the primary.
            self.persist_identity_migration(&new_document)?;
        } else if !changed && needs_canonical_repair && !repairing_primary {
            self.persist_primary_repair(&new_document)?;
        } else {
            self.persist_documents(&old_document, &new_document)?;
        }
        invalidate_registry_cache();

        Ok(RegistryUpdate {
            value,
            snapshot: new_document.into_snapshot(false),
            changed,
        })
    }

    fn backup_path(&self) -> PathBuf {
        sibling_with_suffix(&self.path, ".bak")
    }

    fn lock_path(&self) -> PathBuf {
        sibling_with_suffix(&self.path, ".lock")
    }

    fn lock_exclusive(&self) -> anyhow::Result<File> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("registry path {} has no parent", self.path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create registry directory {}", parent.display()))?;
        let lock_path = self.lock_path();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open registry lock {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("failed to lock registry {}", self.path.display()))?;
        Ok(lock)
    }

    fn load_document_unlocked(&self) -> anyhow::Result<LoadedRegistry> {
        let backup_path = self.backup_path();
        let primary = read_document(&self.path);
        let backup = read_document(&backup_path);

        match (primary, backup) {
            (Ok(primary), Ok(backup))
                if backup.document.revision > primary.document.revision =>
            {
                tracing::warn!(
                    registry = %self.path.display(),
                    backup = %backup_path.display(),
                    primary_revision = primary.document.revision,
                    backup_revision = backup.document.revision,
                    "registry backup is newer than primary; recovering interrupted identity migration"
                );
                Ok(LoadedRegistry {
                    document: backup.document,
                    source: RegistrySource::Backup,
                    needs_canonical_repair: backup.needs_canonical_repair,
                })
            }
            (Ok(primary), _) => Ok(LoadedRegistry {
                document: primary.document,
                source: RegistrySource::Primary,
                needs_canonical_repair: primary.needs_canonical_repair,
            }),
            (Err(primary_error), Ok(backup)) => {
                let primary_missing = is_not_found(&primary_error);
                tracing::warn!(
                    registry = %self.path.display(),
                    backup = %backup_path.display(),
                    error = %primary_error,
                    primary_missing,
                    "registry primary is unavailable; using last-known-good backup"
                );
                Ok(LoadedRegistry {
                    document: backup.document,
                    source: RegistrySource::Backup,
                    needs_canonical_repair: backup.needs_canonical_repair,
                })
            }
            (Err(primary_error), Err(backup_error))
                if is_not_found(&primary_error) && is_not_found(&backup_error) =>
            {
                Ok(LoadedRegistry {
                    document: RegistryDocument::empty(),
                    source: RegistrySource::Empty,
                    needs_canonical_repair: false,
                })
            }
            (Err(primary_error), Err(backup_error)) => Err(anyhow!(
                "registry primary {} is invalid ({primary_error:#}); backup {} is unusable ({backup_error:#})",
                self.path.display(),
                backup_path.display()
            )),
        }
    }

    fn persist_documents(
        &self,
        old_document: &RegistryDocument,
        new_document: &RegistryDocument,
    ) -> anyhow::Result<()> {
        let old_encoded = serde_json::to_vec_pretty(old_document)?;
        let new_encoded = serde_json::to_vec_pretty(new_document)?;

        // Keep one complete prior snapshot before replacing the primary. If the
        // process dies at any later point, startup sees either the old primary,
        // the new primary, or this last-known-good backup.
        write_synced_then_rename(&self.backup_path(), &old_encoded)?;
        write_synced_then_rename(&self.path, &new_encoded)?;
        sync_parent_directory(&self.path)?;
        Ok(())
    }

    fn persist_identity_migration(&self, document: &RegistryDocument) -> anyhow::Result<()> {
        let encoded = serde_json::to_vec_pretty(document)?;
        write_synced_then_rename(&self.backup_path(), &encoded)?;
        write_synced_then_rename(&self.path, &encoded)?;
        sync_parent_directory(&self.path)
    }

    fn persist_primary_repair(&self, document: &RegistryDocument) -> anyhow::Result<()> {
        let encoded = serde_json::to_vec_pretty(document)?;
        write_synced_then_rename(&self.path, &encoded)?;
        sync_parent_directory(&self.path)
    }
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

fn read_document(path: &Path) -> anyhow::Result<ReadRegistryDocument> {
    let bytes = std::fs::read(path)?;
    let raw = serde_json::from_slice::<RawRegistryDocument>(&bytes)
        .with_context(|| format!("failed to parse registry {}", path.display()))?;
    let document = serde_json::from_slice::<RegistryDocument>(&bytes)
        .with_context(|| format!("failed to parse registry {}", path.display()))?;
    document.validate(&raw, path)
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!("{name}{suffix}"))
}

struct PendingTempFile {
    path: Option<PathBuf>,
}

impl PendingTempFile {
    fn create_synced(target: &Path, bytes: &[u8]) -> anyhow::Result<Self> {
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("registry path {} has no parent", target.display()))?;
        let target_name = target
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();

        for _ in 0..128 {
            let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".{target_name}.tmp.{}.{}",
                std::process::id(),
                counter
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(bytes).with_context(|| {
                        format!("failed to write registry temporary file {}", path.display())
                    })?;
                    file.sync_all().with_context(|| {
                        format!("failed to sync registry temporary file {}", path.display())
                    })?;
                    return Ok(Self { path: Some(path) });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create registry temporary file in {}",
                            parent.display()
                        )
                    });
                }
            }
        }
        Err(anyhow!(
            "could not allocate a unique registry temporary file in {}",
            parent.display()
        ))
    }

    fn persist(mut self, target: &Path) -> anyhow::Result<()> {
        let temp = self.path.as_ref().expect("pending registry temporary path");
        std::fs::rename(temp, target).with_context(|| {
            format!(
                "failed to atomically replace registry {} from {}",
                target.display(),
                temp.display()
            )
        })?;
        self.path = None;
        Ok(())
    }
}

impl Drop for PendingTempFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_synced_then_rename(target: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    PendingTempFile::create_synced(target, bytes)?.persist(target)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("registry path {} has no parent", path.display()))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("failed to sync registry directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> anyhow::Result<()> {
    // Windows does not permit opening a directory through std::fs::File. The
    // primary and backup files themselves are still fully synced before rename.
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetadataStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl MetadataStamp {
    fn read(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RegistryFileStamp {
    primary: Option<MetadataStamp>,
    backup: Option<MetadataStamp>,
}

fn registry_file_stamp(path: &Path) -> RegistryFileStamp {
    RegistryFileStamp {
        primary: MetadataStamp::read(path),
        backup: MetadataStamp::read(&sibling_with_suffix(path, ".bak")),
    }
}

struct RegistryCache {
    stamp: RegistryFileStamp,
    registry: std::sync::Arc<Registry>,
}

static REGISTRY_CACHE: std::sync::OnceLock<std::sync::RwLock<Option<RegistryCache>>> =
    std::sync::OnceLock::new();

fn invalidate_registry_cache() {
    if let Some(cache) = REGISTRY_CACHE.get() {
        if let Ok(mut guard) = cache.write() {
            *guard = None;
        }
    }
}

impl Registry {
    pub fn load() -> Self {
        match Self::load_snapshot() {
            Ok(snapshot) => snapshot.registry,
            Err(error) => {
                // Preserve the historical infallible API for readers. Mutating
                // production paths use `update`, which returns this error and
                // never replaces corruption with an empty file.
                tracing::warn!(error = %error, "failed to load registry");
                Self::default()
            }
        }
    }

    pub fn load_snapshot() -> anyhow::Result<RegistrySnapshot> {
        RegistryStore::global()?.load()
    }

    pub fn update<T>(
        mutator: impl FnOnce(&mut Registry) -> anyhow::Result<T>,
    ) -> anyhow::Result<RegistryUpdate<T>> {
        RegistryStore::global()?.update(mutator)
    }

    /// Like [`load`](Self::load), but returns a shared snapshot cached on the
    /// registry file's mtime. The file is small yet read+parsed on every MCP tool
    /// call (via `resolve`); this skips the re-parse when it hasn't changed. Any
    /// [`save`](Self::save) changes the file identity, so cached readers pick up
    /// writes even on filesystems with coarse mtime resolution. Use this only on
    /// read-only paths; mutating callers should use [`update`](Self::update).
    pub fn load_cached() -> std::sync::Arc<Registry> {
        let cache = REGISTRY_CACHE.get_or_init(|| std::sync::RwLock::new(None));
        let Some(path) = registry_path() else {
            return std::sync::Arc::new(Self::default());
        };
        let current_stamp = registry_file_stamp(&path);
        if let Ok(guard) = cache.read() {
            if let Some(cached) = guard.as_ref() {
                if cached.stamp == current_stamp {
                    return cached.registry.clone();
                }
            }
        }
        let registry = std::sync::Arc::new(Self::load());
        let current_stamp = registry_file_stamp(&path);
        if let Ok(mut guard) = cache.write() {
            *guard = Some(RegistryCache {
                stamp: current_stamp,
                registry: registry.clone(),
            });
        }
        registry
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let replacement = self.clone();
        RegistryStore::global()?.update(move |registry| {
            *registry = replacement;
            Ok(())
        })?;
        Ok(())
    }

    /// Insert or replace an entry matched by `path`.
    ///
    /// An assigned repository ID and valid publication mirror are sticky when
    /// a compatibility caller omits the new fields. Matching by repository ID
    /// also lets a repository move without receiving a new identity.
    pub fn upsert(&mut self, mut entry: RegistryEntry) {
        let id_match = entry.repository_id.as_ref().and_then(|repository_id| {
            self.entries
                .iter()
                .position(|candidate| candidate.repository_id.as_ref() == Some(repository_id))
        });
        let path_match = self
            .entries
            .iter()
            .position(|candidate| candidate.path == entry.path);
        let position = path_match.or(id_match);

        if let Some(pos) = position {
            let previous = &self.entries[pos];
            match (&previous.repository_id, &entry.repository_id) {
                (Some(repository_id), None) => {
                    entry.repository_id = Some(repository_id.clone());
                }
                (Some(repository_id), Some(incoming)) if repository_id != incoming => {
                    tracing::warn!(
                        path = %entry.path,
                        existing_repository_id = %repository_id,
                        ignored_repository_id = %incoming,
                        "ignored attempt to replace immutable repository identity"
                    );
                    entry.repository_id = Some(repository_id.clone());
                }
                _ => {}
            }
            if entry.latest_artifact_version.is_none() {
                entry.latest_artifact_version = artifact_version_from_path(&entry.artifacts_dir)
                    .or_else(|| previous.latest_artifact_version.clone());
            }
            if entry.published_artifact_version.is_none() {
                entry.published_artifact_version = previous.published_artifact_version.clone();
            }
            if entry.published_graph_content_version.is_none() {
                entry.published_graph_content_version =
                    previous.published_graph_content_version.clone();
            }
            if entry.published_epoch.is_none() {
                entry.published_epoch = previous.published_epoch.clone();
            }
            self.entries[pos] = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn find(&self, name_or_path: &str) -> Option<&RegistryEntry> {
        self.entries
            .iter()
            .find(|e| e.name == name_or_path || e.path == name_or_path)
    }

    pub fn find_mut(&mut self, name_or_path: &str) -> Option<&mut RegistryEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.name == name_or_path || e.path == name_or_path)
    }

    /// Returns true if the repo's current git HEAD differs from the indexed HEAD.
    pub fn is_stale(&self, name_or_path: &str) -> bool {
        let Some(entry) = self.find(name_or_path) else {
            return true;
        };
        let current = git_head(Path::new(&entry.path));
        match (&entry.last_git_head, current) {
            (Some(saved), Some(cur)) => saved != &cur,
            _ => false,
        }
    }
}

#[cfg(test)]
mod git_arg_tests {
    use super::git_changed_files;
    use std::path::Path;

    #[test]
    fn git_changed_files_refuses_option_like_ref() {
        // Leading-dash refs (e.g. `--output=…`) are refused so git can't be driven
        // into writing a file; returns empty rather than shelling out.
        assert!(git_changed_files(Path::new("."), "--output=/tmp/pwn").is_empty());
    }

    #[test]
    fn registry_path_composes_from_cih_home() {
        // registry.json lives under ~/.cih; verify the composition without
        // depending on HOME being set (skips when it isn't).
        if let Some(home) = crate::cih_home() {
            assert_eq!(super::registry_path(), Some(home.join("registry.json")));
        }
    }
}
