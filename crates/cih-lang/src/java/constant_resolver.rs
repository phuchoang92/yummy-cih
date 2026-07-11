//! JavaConstantResolver — resolves Java `static final String` constant names to
//! their folded literal values. Handles simple identifiers, qualified names
//! (`Cls.NAME`), static imports, and one-level inherited constants.

use std::collections::HashMap;

use cih_core::StringConstant;

use crate::constant_resolver::{ConstantResolver, ResolutionContext, ResolvedConstant};

/// Index key: `(owner_fqcn, const_name)` → folded value + provenance.
type ConstantIndex = HashMap<(String, String), ResolvedConstant>;

/// Index of static imports: `(file_path, imported_name)` → owner_fqcn.
type StaticImportIndex = HashMap<(String, String), String>;

/// Index of superclass chains: `owner_fqcn` → parent_fqcn (one level).
type SuperIndex = HashMap<String, String>;

pub struct JavaConstantResolver {
    /// (owner_fqcn, const_name) → value
    index: ConstantIndex,
    /// For static imports: map from (owner_fqcn, member_name) → const_name → value.
    /// We reuse `index` for this; static imports just resolve owner_fqcn from imports.
    /// simple_name → owner_fqcn (for single-class imports `import static pkg.Cls.NAME`)
    #[allow(dead_code)]
    static_import_owners: StaticImportIndex,
    /// owner_fqcn → parent_fqcn (one level, for inheritance)
    super_index: SuperIndex,
    /// type simple_name → fqcn (from imports)
    type_index: HashMap<String, String>,
    /// const_name → the ONE non-dynamic constant with that name repo-wide, or
    /// `None` when 2+ exist (ambiguous — never guess). Last-resort fallback for
    /// script-language sites only (`ResolutionContext::allow_unique_fallback`).
    unique_by_name: HashMap<String, Option<ResolvedConstant>>,
}

impl JavaConstantResolver {
    pub fn build(constants: &[StringConstant], all_defs: &[(String, Option<String>)]) -> Self {
        let mut index = ConstantIndex::new();
        let mut type_index: HashMap<String, String> = HashMap::new();
        let mut super_index: SuperIndex = HashMap::new();

        let mut unique_by_name: HashMap<String, Option<ResolvedConstant>> = HashMap::new();
        for c in constants {
            if !c.dynamic {
                let resolved = ResolvedConstant {
                    value: c.value.clone(),
                    env_default: c.env_default,
                };
                unique_by_name
                    .entry(c.const_name.clone())
                    .and_modify(|slot| *slot = None)
                    .or_insert_with(|| Some(resolved.clone()));
                index.insert((c.owner_fqcn.clone(), c.const_name.clone()), resolved);
            }
        }

        // Build type index from all type defs (fqcn, simple_name pairs)
        for (fqcn, _super_fqcn) in all_defs {
            let simple = fqcn.rsplit('.').next().unwrap_or(fqcn.as_str()).to_string();
            type_index.entry(simple).or_insert_with(|| fqcn.clone());
        }
        for (fqcn, super_fqcn) in all_defs {
            if let Some(sup) = super_fqcn {
                super_index.insert(fqcn.clone(), sup.clone());
            }
        }

        Self {
            index,
            static_import_owners: StaticImportIndex::new(),
            super_index,
            type_index,
            unique_by_name,
        }
    }
}

/// Normalize a script-language relative import (`./apiClient`, `../lib/x`)
/// against the importing file's directory into a repo-relative, extensionless
/// module path — the `owner_fqcn` scheme TS module constants use. Non-relative
/// specifiers (bare packages, absolute) return `None`.
fn resolve_relative_module(importer: &std::path::Path, spec: &str) -> Option<String> {
    if !spec.starts_with("./") && !spec.starts_with("../") && spec != "." {
        return None;
    }
    let dir = importer.parent().unwrap_or_else(|| std::path::Path::new(""));
    let mut parts: Vec<&str> = dir
        .to_str()?
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Strip a script-language source extension from a `File:`-derived owner so it
/// matches the extensionless module-path owner scheme.
fn strip_source_extension(owner: &str) -> Option<&str> {
    for ext in [".tsx", ".ts", ".jsx", ".js", ".py"] {
        if let Some(stripped) = owner.strip_suffix(ext) {
            return Some(stripped);
        }
    }
    None
}

impl ConstantResolver for JavaConstantResolver {
    fn resolve(&self, name: &str, ctx: &ResolutionContext<'_>) -> Option<ResolvedConstant> {
        // 1. Simple identifier in same class
        if !name.contains('.') {
            // Try owner class first
            if let Some(v) = self.index.get(&(ctx.owner_fqcn.to_string(), name.to_string())) {
                return Some(v.clone());
            }
            // Try static imports
            for imp in ctx.imports {
                if !imp.is_static {
                    continue;
                }
                if imp.is_wildcard {
                    // `import static com.example.Config.*`
                    let owner = imp.raw.trim_end_matches(".*");
                    if let Some(v) = self.index.get(&(owner.to_string(), name.to_string())) {
                        return Some(v.clone());
                    }
                } else if let Some((owner, imported_name)) = imp.raw.rsplit_once('.') {
                    if imported_name == name {
                        if let Some(v) = self.index.get(&(owner.to_string(), name.to_string())) {
                            return Some(v.clone());
                        }
                    }
                }
            }
            // Try inherited (one level)
            if let Some(parent_fqcn) = self.super_index.get(ctx.owner_fqcn) {
                if let Some(v) = self.index.get(&(parent_fqcn.clone(), name.to_string())) {
                    return Some(v.clone());
                }
            }
            // Script-language cross-file resolution — gated so Java/Kotlin
            // bare-name behavior stays byte-identical.
            if ctx.allow_unique_fallback {
                let convention_const = crate::contracts_common::is_screaming_snake(name);
                // (a) Module-scope sites carry `File:src/x.ts`-derived owners;
                // constants own the extensionless module path.
                if let Some(stripped) = strip_source_extension(ctx.owner_fqcn) {
                    if let Some(v) = self.index.get(&(stripped.to_string(), name.to_string())) {
                        return Some(v.clone());
                    }
                }
                // (b) Import-scoped: the site's file imports the constant's
                // module (TS relative specifiers; Python dotted modules).
                // Cross-file steps require the SCREAMING_SNAKE convention —
                // lowercase names from concat chains stay same-file only.
                if !convention_const {
                    return None;
                }
                for imp in ctx.imports {
                    if imp.is_static {
                        continue;
                    }
                    if let Some(owner) = resolve_relative_module(ctx.file, &imp.raw) {
                        if let Some(v) = self.index.get(&(owner, name.to_string())) {
                            return Some(v.clone());
                        }
                    }
                    // Python `from services.api import X` records the dotted
                    // module, which IS the owner scheme.
                    if let Some(v) = self.index.get(&(imp.raw.clone(), name.to_string())) {
                        return Some(v.clone());
                    }
                }
                // (c) Last resort: the repo-wide unique name (2+ candidates
                // → None — degrade to a wildcard, never guess).
                if let Some(v) = self.unique_by_name.get(name).and_then(Clone::clone) {
                    return Some(v);
                }
            }
            return None;
        }

        // 2. Qualified `Cls.NAME`
        if let Some((cls_part, const_name)) = name.rsplit_once('.') {
            // Try to resolve cls_part to an fqcn via imports
            let resolved_fqcn = self.resolve_type(cls_part, ctx);
            if let Some(fqcn) = resolved_fqcn {
                if let Some(v) = self.index.get(&(fqcn, const_name.to_string())) {
                    return Some(v.clone());
                }
            }
            // Also try cls_part directly as fqcn
            if let Some(v) = self.index.get(&(cls_part.to_string(), const_name.to_string())) {
                return Some(v.clone());
            }
        }

        None
    }
}

impl JavaConstantResolver {
    fn resolve_type(&self, simple_or_qualified: &str, ctx: &ResolutionContext<'_>) -> Option<String> {
        // Already qualified?
        if simple_or_qualified.contains('.') {
            return Some(simple_or_qualified.to_string());
        }
        // Check explicit imports
        for imp in ctx.imports {
            if imp.is_static || imp.is_wildcard {
                continue;
            }
            if let Some((_, imported_name)) = imp.raw.rsplit_once('.') {
                if imported_name == simple_or_qualified {
                    return Some(imp.raw.clone());
                }
            }
        }
        // Check type index
        self.type_index.get(simple_or_qualified).cloned()
    }
}
