//! File-based route detection — Next.js (`pages/api/**`, App Router `app/**/route.ts`)
//! and Remix (`app/routes/**` loader/action) routes, derived from file paths.

use cih_core::RouteSource;
use tree_sitter::Node as TsNode;


use super::builder::Builder;
use super::helpers::*;

// ── File-based routes (Next.js / Remix) ───────────────────────────────────────

/// Top-level exported names (functions + `export const`), used to detect
/// App-Router verb handlers (`export function GET`) and Remix `loader`/`action`.
pub(super) fn exported_top_level_names(root: TsNode<'_>, src: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "export_statement" {
            continue;
        }
        let mut c2 = child.walk();
        for inner in child.named_children(&mut c2) {
            match inner.kind() {
                "function_declaration" | "generator_function_declaration" => {
                    if let Some(n) = inner.child_by_field_name("name") {
                        out.insert(text(n, src));
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    let mut c3 = inner.walk();
                    for d in inner.named_children(&mut c3) {
                        if d.kind() == "variable_declarator" {
                            if let Some(n) = d.child_by_field_name("name") {
                                out.insert(text(n, src));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// `[id]` → `:id`, `[...slug]`/`[[...slug]]` → `:slug` (Next.js dynamic segments).
pub(super) fn next_dynamic_segment(seg: &str) -> String {
    let inner = seg.trim_start_matches('[').trim_end_matches(']');
    if inner.len() != seg.len() {
        format!(":{}", inner.trim_start_matches("..."))
    } else {
        seg.to_string()
    }
}

/// Substring after a `pages/api/` path boundary, if `norm` is a Next.js pages API file.
pub(super) fn pages_api_subpath(norm: &str) -> Option<&str> {
    let idx = norm.find("pages/api/")?;
    if idx != 0 && norm.as_bytes()[idx - 1] != b'/' {
        return None;
    }
    Some(&norm[idx + "pages/api/".len()..])
}

/// Next.js pages API file path → HTTP path (e.g. `users/[id].ts` → `/api/users/:id`).
pub(super) fn next_pages_api_path(rest: &str) -> String {
    let stem = module_path(rest);
    let stem = stem.strip_suffix("/index").unwrap_or(&stem);
    let stem = if stem == "index" { "" } else { stem };
    let mut p = String::from("/api");
    for seg in stem.split('/').filter(|s| !s.is_empty()) {
        p.push('/');
        p.push_str(&next_dynamic_segment(seg));
    }
    p
}

/// App-Router directory (between `app/` and `/route.<ext>`), if `norm` is one.
pub(super) fn app_router_dir(norm: &str) -> Option<String> {
    let stem = module_path(norm);
    let base = stem.strip_suffix("/route")?;
    if base == "app" || base.ends_with("/app") {
        return Some(String::new());
    }
    let after = if let Some(i) = base.find("/app/") {
        &base[i + "/app/".len()..]
    } else {
        base.strip_prefix("app/")?
    };
    Some(after.to_string())
}

/// App-Router directory → HTTP path (drops `(groups)` and `@slots`; `[id]` → `:id`).
pub(super) fn next_app_router_path(dir: &str) -> String {
    let mut segs = Vec::new();
    for seg in dir.split('/').filter(|s| !s.is_empty()) {
        if (seg.starts_with('(') && seg.ends_with(')')) || seg.starts_with('@') {
            continue;
        }
        segs.push(next_dynamic_segment(seg));
    }
    if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    }
}

/// Remix route file (after `app/routes/`), if `norm` is one.
pub(super) fn remix_route_file(norm: &str) -> Option<&str> {
    let idx = norm.find("app/routes/")?;
    if idx != 0 && norm.as_bytes()[idx - 1] != b'/' {
        return None;
    }
    Some(&norm[idx + "app/routes/".len()..])
}

/// Remix route filename → HTTP path (`users.$id.tsx` → `/users/:id`; `$` splat → `*`).
pub(super) fn remix_route_path(routefile: &str) -> String {
    let stem = module_path(routefile);
    let stem = stem.strip_suffix("/route").unwrap_or(&stem);
    let mut segs = Vec::new();
    for seg in stem.split(['/', '.']) {
        if seg.is_empty() || seg == "_index" || seg.starts_with('_') {
            continue;
        }
        segs.push(match seg.strip_prefix('$') {
            Some("") => "*".to_string(),          // bare `$` splat
            Some(name) => format!(":{name}"),      // `$id` → `:id`
            None => seg.to_string(),
        });
    }
    if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    }
}

/// Detect file-based routes from the path convention + exported handler names:
/// Next.js pages API (all-methods handler), App Router (`export GET/POST/…`),
/// and Remix (`loader` → GET, `action` → POST).
pub(super) fn detect_file_based_routes(rel: &str, root: TsNode<'_>, src: &str, builder: &mut Builder) {
    let norm = rel.strip_prefix("src/").unwrap_or(rel);

    if let Some(rest) = pages_api_subpath(norm) {
        let path = next_pages_api_path(rest);
        builder.emit_backend_route(root, RouteSource::NextJs, "ALL", &path);
        return;
    }
    if let Some(dir) = app_router_dir(norm) {
        let path = next_app_router_path(&dir);
        let exports = exported_top_level_names(root, src);
        for verb in [
            "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS",
        ] {
            if exports.contains(verb) {
                builder.emit_backend_route(root, RouteSource::NextJs, verb, &path);
            }
        }
        return;
    }
    if let Some(routefile) = remix_route_file(norm) {
        let exports = exported_top_level_names(root, src);
        if exports.contains("loader") || exports.contains("action") {
            let path = remix_route_path(routefile);
            if exports.contains("loader") {
                builder.emit_backend_route(root, RouteSource::Remix, "GET", &path);
            }
            if exports.contains("action") {
                builder.emit_backend_route(root, RouteSource::Remix, "POST", &path);
            }
        }
    }
}

