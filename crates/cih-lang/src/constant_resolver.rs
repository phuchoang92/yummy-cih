//! ConstantResolver trait — language-provider service for resolving `static final String`
//! constants to their folded literal values at extraction time (Gap 4).

use std::path::Path;

use cih_core::RawImport;

/// Context supplied to a resolver call.
pub struct ResolutionContext<'a> {
    pub file: &'a Path,
    pub owner_fqcn: &'a str,
    pub imports: &'a [RawImport],
}

/// Resolves a name (simple identifier or `Qualified.IDENT`) to its folded
/// string literal value, using the constant index built from ParsedFiles.
pub trait ConstantResolver: Send + Sync {
    fn resolve(&self, name: &str, ctx: &ResolutionContext<'_>) -> Option<String>;
}

/// A no-op resolver that always returns `None`. Used as a default when no
/// constant index has been built.
pub struct NullConstantResolver;

impl ConstantResolver for NullConstantResolver {
    fn resolve(&self, _name: &str, _ctx: &ResolutionContext<'_>) -> Option<String> {
        None
    }
}
