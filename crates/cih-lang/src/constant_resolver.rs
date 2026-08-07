//! ConstantResolver trait — language-provider service for resolving `static final String`
//! constants (and script-language module constants) to their folded literal values at
//! extraction time (Gap 4).

use std::path::Path;

use cih_core::RawImport;

/// Context supplied to a resolver call.
pub struct ResolutionContext<'a> {
    pub file: &'a Path,
    pub owner_fqcn: &'a str,
    pub imports: &'a [RawImport],
    /// Allow cross-file resolution beyond Java scoping rules (import-scoped
    /// module lookup, then repo-wide unique-name fallback). Set ONLY for
    /// script-language sites (TypeScript/Python) — Java/Kotlin bare names
    /// resolve by class scoping alone, exactly as before.
    pub allow_unique_fallback: bool,
}

/// A resolved constant value plus its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedConstant {
    pub value: String,
    /// The value is the literal default of an env override — the effective
    /// runtime value may differ.
    pub env_default: bool,
}

/// Resolves a name (simple identifier or `Qualified.IDENT`) to its folded
/// string literal value, using the constant index built from ParsedFiles.
pub trait ConstantResolver: Send + Sync {
    fn resolve(&self, name: &str, ctx: &ResolutionContext<'_>) -> Option<ResolvedConstant>;
}

/// A no-op resolver that always returns `None`. Used as a default when no
/// constant index has been built.
pub struct NullConstantResolver;

impl ConstantResolver for NullConstantResolver {
    fn resolve(&self, _name: &str, _ctx: &ResolutionContext<'_>) -> Option<ResolvedConstant> {
        None
    }
}

/// Normalize a script-language relative import (`./apiClient`, `../lib/x`)
/// against the importing file's directory into a repo-relative, extensionless
/// module path — the `owner_fqcn` scheme TS module constants use. Non-relative
/// specifiers (bare packages, absolute) return `None`.
pub fn resolve_relative_module(importer: &std::path::Path, spec: &str) -> Option<String> {
    if !spec.starts_with("./") && !spec.starts_with("../") && spec != "." {
        return None;
    }
    let dir = importer
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let mut parts: Vec<&str> = dir.to_str()?.split('/').filter(|p| !p.is_empty()).collect();
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
pub fn strip_source_extension(owner: &str) -> Option<&str> {
    for ext in [".tsx", ".ts", ".jsx", ".js", ".py"] {
        if let Some(stripped) = owner.strip_suffix(ext) {
            return Some(stripped);
        }
    }
    None
}
