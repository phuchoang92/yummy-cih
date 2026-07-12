use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use crate::confidence::{CONTRACT_HTTP_CLIENT, CONTRACT_HTTP_CLIENT_DYNAMIC};

use cih_core::{
    external_endpoint_id, kafka_topic_id, ContractKind, ContractSite, Edge, EdgeKind, Node,
    NodeKind, ParsedFile, UrlPart,
};
use cih_lang::{ConstantResolver, ResolutionContext};

/// Internal marker for an unresolved URL part; any path segment containing it
/// becomes `{*}` wholesale before emission (never a partial `v{*}`).
const UNRESOLVED: char = '\u{0}';

/// Convert parser-discovered inter-service contract sites into graph nodes and edges.
/// `resolver` folds `ConstRef` URL parts through the cross-file constant index;
/// unresolved refs and `Dynamic` parts degrade to `{*}` wildcards.
pub fn resolve_contract_edges(
    parsed: &[ParsedFile],
    resolver: &dyn ConstantResolver,
) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let wrappers = WrapperIndex::build(parsed);

    for pf in parsed {
        for site in &pf.contract_sites {
            match &site.kind {
                ContractKind::HttpCall | ContractKind::HttpClientProxy => {
                    // PROVISIONAL wrapper calls join against detected wrapper
                    // defs; no match ⇒ the site silently vanishes.
                    let mut wrapper_provenance = None;
                    let mut wrapper_fixed_method = None;
                    let (url_template, dynamic, env_default) = if let Some(callee) =
                        site.via_wrapper.as_deref()
                    {
                        let Some((def, wrapper_pf)) = wrappers.lookup(callee, pf) else {
                            continue;
                        };
                        // Two-context fold: the wrapper's prefix constants
                        // (API_BASE_URL) live in the WRAPPER's module — the
                        // caller may not import them at all.
                        let wrapper_ctx = ResolutionContext {
                            file: Path::new(&wrapper_pf.file),
                            owner_fqcn: &def.module,
                            imports: &wrapper_pf.imports,
                            allow_unique_fallback: true,
                        };
                        let prefix = fold_url_parts(&def.prefix_parts, &wrapper_ctx, resolver);
                        let suffix = fold_url_parts(
                            site.url_parts.as_deref().unwrap_or(&[]),
                            &site_ctx(site, pf),
                            resolver,
                        );
                        let raw = format!("{}{}", prefix.raw, suffix.raw);
                        let env = prefix.env_default || suffix.env_default;
                        let Some((folded, env_default)) = wildcard_segments(&raw, env) else {
                            continue;
                        };
                        wrapper_provenance = Some(format!("{}#{}", def.module, def.name));
                        // Python wrappers hard-code their verb; the site's
                        // method is a placeholder in that case.
                        wrapper_fixed_method = def.fixed_method.clone();
                        (folded, true, env_default)
                    } else {
                        match site.url_template.as_deref() {
                            Some(url) => (url.to_string(), false, false),
                            None => {
                                let Some((folded, env_default)) = fold_http_url(site, pf, resolver)
                                else {
                                    continue;
                                };
                                (folded, true, env_default)
                            }
                        }
                    };
                    let Some(http_method) = wrapper_fixed_method
                        .as_deref()
                        .or(site.http_method.as_deref())
                    else {
                        continue;
                    };
                    let method = http_method.to_ascii_uppercase();
                    let id = external_endpoint_id(&method, &url_template);
                    let name = format!("{method} {url_template}");
                    let source = match &site.kind {
                        ContractKind::HttpClientProxy => "http-client-proxy",
                        _ => "http-client",
                    };
                    let mut props = serde_json::json!({
                        "httpMethod": method,
                        "path": url_template,
                        "urlTemplate": url_template,
                        "source": source,
                    });
                    if dynamic {
                        props["dynamic"] = serde_json::Value::Bool(true);
                    }
                    if env_default {
                        // The URL base came from an env-override's literal
                        // default — the runtime value may differ.
                        props["base_source"] = serde_json::Value::String("env_default".into());
                    }
                    if let Some(wrapper) = &wrapper_provenance {
                        props["via_wrapper"] = serde_json::Value::String(wrapper.clone());
                    }
                    nodes.push(Node {
                        id: id.clone(),
                        kind: NodeKind::ExternalEndpoint,
                        name: name.clone(),
                        qualified_name: Some(name),
                        file: pf.file.clone(),
                        range: site.range,
                        props: Some(props),
                    });
                    edges.push(Edge {
                        src: site.in_callable.clone(),
                        dst: id,
                        kind: EdgeKind::ExternalCall,
                        confidence: if dynamic {
                            CONTRACT_HTTP_CLIENT_DYNAMIC
                        } else {
                            CONTRACT_HTTP_CLIENT
                        },
                        reason: match &site.kind {
                            ContractKind::HttpClientProxy => "http-client-proxy",
                            _ => "http-client",
                        }
                        .to_string(),
                        props: None,
                    });
                }
                ContractKind::EventPublish | ContractKind::EventListen => {
                    let topic = match site.topic.as_deref() {
                        Some(topic) => topic.to_string(),
                        // A dynamic topic must fold to a full literal — topics
                        // match by exact string, so a `{*}` topic is useless.
                        None => match fold_literal_topic(site, pf, resolver) {
                            Some(topic) => topic,
                            None => continue,
                        },
                    };
                    let topic = topic.as_str();
                    let id = kafka_topic_id(topic);
                    nodes.push(Node {
                        id: id.clone(),
                        kind: NodeKind::KafkaTopic,
                        name: topic.to_string(),
                        qualified_name: Some(topic.to_string()),
                        file: pf.file.clone(),
                        range: site.range,
                        props: Some(serde_json::json!({
                            "topic": topic,
                        })),
                    });
                    let (kind, reason) = match &site.kind {
                        ContractKind::EventPublish => (EdgeKind::PublishesEvent, "event-publish"),
                        ContractKind::EventListen => (EdgeKind::ListensTo, "event-listen"),
                        _ => unreachable!("HTTP contract kind handled above"),
                    };
                    edges.push(Edge {
                        src: site.in_callable.clone(),
                        dst: id,
                        kind,
                        confidence: 0.8,
                        reason: reason.to_string(),
                        // Carry the messaging framework as structured data so cross-repo
                        // consumers classify Kafka vs Spring without guessing from `reason`.
                        props: site
                            .messaging_framework
                            .map(|fw| serde_json::json!({ "messaging_framework": fw })),
                    });
                }
                ContractKind::Custom(_) => continue,
            }
        }
    }

    let mut deduped_nodes = BTreeMap::new();
    for node in nodes {
        deduped_nodes
            .entry(node.id.as_str().to_string())
            .or_insert(node);
    }
    let mut deduped_edges = BTreeMap::new();
    for edge in edges {
        let key = (
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        );
        deduped_edges.entry(key).or_insert(edge);
    }

    (
        deduped_nodes.into_values().collect(),
        deduped_edges.into_values().collect(),
    )
}

/// Repo-wide index of detected HTTP wrapper functions, keyed by
/// (extensionless module path, name), plus a repo-unique-name fallback for
/// alias/barrel imports (2+ same-named wrappers → None, never guess).
struct WrapperIndex<'a> {
    by_key: HashMap<(String, String), (&'a cih_core::HttpWrapperDef, &'a ParsedFile)>,
    unique_by_name: HashMap<String, Option<(&'a cih_core::HttpWrapperDef, &'a ParsedFile)>>,
}

impl<'a> WrapperIndex<'a> {
    fn build(parsed: &'a [ParsedFile]) -> Self {
        let mut by_key = HashMap::new();
        let mut unique_by_name: HashMap<String, Option<_>> = HashMap::new();
        for pf in parsed {
            for def in &pf.http_wrappers {
                by_key.insert((def.module.clone(), def.name.clone()), (def, pf));
                unique_by_name
                    .entry(def.name.clone())
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some((def, pf)));
            }
        }
        Self {
            by_key,
            unique_by_name,
        }
    }

    /// Resolve a provisional site's callee: same module → import-scoped →
    /// repo-wide unique name.
    fn lookup(
        &self,
        callee: &str,
        caller_pf: &'a ParsedFile,
    ) -> Option<(&'a cih_core::HttpWrapperDef, &'a ParsedFile)> {
        // Dotted callee = module-attribute call (`api.api_get`): the receiver
        // names an import binding that PINS the module.
        if let Some((obj, attr)) = callee.rsplit_once('.') {
            return self.lookup_module_attr(obj, attr, caller_pf);
        }
        let caller_module =
            cih_lang::strip_source_extension(&caller_pf.file).unwrap_or(caller_pf.file.as_str());
        if let Some(hit) = self
            .by_key
            .get(&(caller_module.to_string(), callee.to_string()))
        {
            return Some(*hit);
        }
        // Python modules are DOTTED (`src.app.client`); language-gated so a
        // TS caller `src/api.ts` can never cross-match a python `src.api`.
        if caller_pf.language == "python" {
            if let Some(hit) = self
                .by_key
                .get(&(caller_module.replace('/', "."), callee.to_string()))
            {
                return Some(*hit);
            }
        }
        for imp in &caller_pf.imports {
            if imp.is_static {
                continue;
            }
            if let Some(module) =
                cih_lang::resolve_relative_module(Path::new(&caller_pf.file), &imp.raw)
            {
                if let Some(hit) = self.by_key.get(&(module, callee.to_string())) {
                    return Some(*hit);
                }
            }
            // Python raws ARE dotted module keys; TS `./x` raws never are.
            if let Some(hit) = self.by_key.get(&(imp.raw.clone(), callee.to_string())) {
                return Some(*hit);
            }
        }
        self.unique_by_name.get(callee).copied().flatten()
    }

    /// Dotted callee `obj.attr`: resolve `obj` through the caller's imports
    /// only — an aliased import (`import a.b as obj` / `import * as obj`),
    /// the full dotted receiver (python: raw == obj), or a plain import's
    /// last segment (python). No same-module steps (a same-module call is a
    /// bare name) and NO unique-name fallback: the receiver pins the module,
    /// so a miss drops the site — never guess.
    fn lookup_module_attr(
        &self,
        obj: &str,
        attr: &str,
        caller_pf: &'a ParsedFile,
    ) -> Option<(&'a cih_core::HttpWrapperDef, &'a ParsedFile)> {
        let python = caller_pf.language == "python";
        for imp in &caller_pf.imports {
            if imp.is_static {
                continue;
            }
            let alias_hit = imp.alias.as_deref() == Some(obj);
            let full_raw_hit = python && imp.alias.is_none() && imp.raw == obj;
            let last_segment_hit =
                python && imp.alias.is_none() && imp.raw.rsplit('.').next() == Some(obj);
            if !(alias_hit || full_raw_hit || last_segment_hit) {
                continue;
            }
            // TS namespace alias: relative spec → repo-relative module path.
            if let Some(module) =
                cih_lang::resolve_relative_module(Path::new(&caller_pf.file), &imp.raw)
            {
                if let Some(hit) = self.by_key.get(&(module, attr.to_string())) {
                    return Some(*hit);
                }
            }
            // Python raws ARE the dotted module keys.
            if let Some(hit) = self.by_key.get(&(imp.raw.clone(), attr.to_string())) {
                return Some(*hit);
            }
        }
        None
    }
}

/// Fold a site's `url_parts` into a normalized path with `{*}` wildcards for
/// unresolved segments. `None` when there are no parts or the result carries
/// no information (`/` or all-wildcard).
fn fold_http_url(
    site: &ContractSite,
    pf: &ParsedFile,
    resolver: &dyn ConstantResolver,
) -> Option<(String, bool)> {
    let folded = fold_parts_raw(site, pf, resolver)?;
    wildcard_segments(&folded.raw, folded.env_default)
}

/// Normalize a folded raw URL and wildcard unresolved segments. `None` when
/// the result carries no information (`/` or all-wildcard).
fn wildcard_segments(raw: &str, env_default: bool) -> Option<(String, bool)> {
    let normalized = cih_lang::normalize_external_url(raw);
    let segments: Vec<String> = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if segment.contains(UNRESOLVED) {
                "{*}".to_string()
            } else {
                segment.to_string()
            }
        })
        .collect();
    if segments.is_empty() || segments.iter().all(|segment| segment == "{*}") {
        return None;
    }
    Some((format!("/{}", segments.join("/")), env_default))
}

/// Fold a dynamic topic; only a fully-resolved literal is usable.
fn fold_literal_topic(
    site: &ContractSite,
    pf: &ParsedFile,
    resolver: &dyn ConstantResolver,
) -> Option<String> {
    let folded = fold_parts_raw(site, pf, resolver)?;
    let raw = folded.raw;
    (!raw.is_empty() && !raw.contains(UNRESOLVED)).then_some(raw)
}

/// Concatenate the parts, resolving `ConstRef`s via the constant index in the
/// site's own scope (owner class from `in_callable`, the file's imports).
/// Unresolved refs and `Dynamic` parts become the `UNRESOLVED` marker.
fn fold_parts_raw(
    site: &ContractSite,
    pf: &ParsedFile,
    resolver: &dyn ConstantResolver,
) -> Option<FoldedParts> {
    let parts = site.url_parts.as_ref()?;
    if parts.is_empty() {
        return None;
    }
    Some(fold_url_parts(parts, &site_ctx(site, pf), resolver))
}

/// Resolution context for a site: owner from `in_callable`, the caller file's
/// imports; script-language sites may resolve constants cross-file while
/// Java/Kotlin keep strict class scoping.
fn site_ctx<'a>(site: &'a ContractSite, pf: &'a ParsedFile) -> ResolutionContext<'a> {
    ResolutionContext {
        file: Path::new(&pf.file),
        owner_fqcn: owner_fqcn_of(site.in_callable.as_str()),
        imports: &pf.imports,
        allow_unique_fallback: matches!(pf.language.as_str(), "typescript" | "python"),
    }
}

/// Concatenate parts in the given context. Unresolved refs and `Dynamic`
/// parts become the `UNRESOLVED` marker.
fn fold_url_parts(
    parts: &[UrlPart],
    ctx: &ResolutionContext<'_>,
    resolver: &dyn ConstantResolver,
) -> FoldedParts {
    let mut out = String::new();
    let mut env_default = false;
    for part in parts {
        match part {
            UrlPart::Lit(lit) => out.push_str(lit),
            UrlPart::ConstRef(name) => match resolver.resolve(name, ctx) {
                Some(resolved) => {
                    env_default |= resolved.env_default;
                    out.push_str(&resolved.value);
                }
                None => out.push(UNRESOLVED),
            },
            UrlPart::Dynamic => out.push(UNRESOLVED),
        }
    }
    FoldedParts {
        raw: out,
        env_default,
    }
}

/// A concatenated `url_parts` string plus whether any resolved constant was an
/// env-override default (provenance surfaced on the emitted endpoint).
struct FoldedParts {
    raw: String,
    env_default: bool,
}

/// `Method:pkg.Cls#m/2` → `pkg.Cls`; `Function:module#f/1` → `module`.
fn owner_fqcn_of(in_callable: &str) -> &str {
    let qualified = in_callable
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(in_callable);
    qualified.split('#').next().unwrap_or(qualified)
}
