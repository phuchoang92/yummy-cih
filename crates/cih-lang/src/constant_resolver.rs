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
