//! Language providers: tree-sitter parsing of each supported language into the IR.
//!
//! # Error philosophy
//!
//! Provider entrypoints return `anyhow::Result` **by design**: callers
//! (`cih-parse`) never branch on failure kind — a failed file becomes a
//! `SkippedFile` diagnostic and indexing continues. Rich context strings
//! matter here; a structured enum would add surface without a consumer.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

pub mod constant_resolver;
pub(crate) mod contracts_common;
pub mod fingerprint;
pub mod generic_parse;

pub use constant_resolver::{
    resolve_relative_module, strip_source_extension, ConstantResolver, NullConstantResolver,
    ResolutionContext,
};
pub use contracts_common::normalize_external_url;

/// Version of the on-disk parse cache (`.cih/parse-cache/v<N>/`).
///
/// **BUMP THIS whenever any parser/extractor changes the shape OR content of
/// `ParsedUnit` output** — new node/edge kinds, new extraction passes (routes,
/// contract sites, constants), changed folding or normalization. Cached units
/// from an older schema must never be served to a newer engine: that silently
/// suppresses the new extraction on every unchanged file. The
/// `parse_schema_guard` test in cih-engine fails when parser output changes
/// without a bump. Starts at 2: the flat pre-versioning cache layout is
/// implicitly v1 and is pruned on first contact.
pub const PARSE_CACHE_SCHEMA: u32 = 14;

/// Declares all language modules and generates `all_providers()`.
/// To add a new language: add one line here (plus the implementation files).
macro_rules! languages {
    ($($lang:ident : $provider:ident),* $(,)?) => {
        $(pub mod $lang;)*

        pub fn all_providers() -> Vec<Box<dyn LanguageProvider>> {
            vec![$(Box::new($lang::$provider::new())),*]
        }
    }
}

languages! {
    java: JavaProvider,
    typescript: TypescriptProvider,
    python: PythonProvider,
    kotlin: KotlinProvider,
    go: GoProvider,
    rust: RustProvider,
    csharp: CSharpProvider,
    ruby: RubyProvider,
    php: PhpProvider,
    scala: ScalaProvider,
    cpp: CppProvider,
    bash: BashProvider,
    elixir: ElixirProvider,
}

/// Maps a file path to its syntax-highlight language tag via the provider registry.
/// Returns `""` for unrecognized extensions.
pub fn lang_for_path(path: &str) -> &'static str {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for p in all_providers() {
            let tag = p.lang_tag();
            for &ext in p.extensions() {
                m.insert(ext, tag);
            }
        }
        m
    });
    let ext = path.rfind('.').map(|i| &path[i..]).unwrap_or("");
    map.get(ext).copied().unwrap_or("")
}

/// The set of language ids ([`LanguageProvider::language_id`]) present among
/// `paths`, derived from file extensions via the provider registry. The single
/// source of truth for "which languages are in scope" — drives
/// language-conditional analysis phases so the core never hardcodes a language
/// (e.g. `f.ends_with(".java")`).
pub fn language_ids_for_paths<S: AsRef<str>>(paths: &[S]) -> BTreeSet<&'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for p in all_providers() {
            let id = p.language_id();
            for &ext in p.extensions() {
                m.insert(ext, id);
            }
        }
        m
    });
    let mut out = BTreeSet::new();
    for path in paths {
        let path = path.as_ref();
        let ext = path.rfind('.').map(|i| &path[i..]).unwrap_or("");
        if let Some(&id) = map.get(ext) {
            out.insert(id);
        }
    }
    out
}

/// Returns the single-line comment prefix for a language id (e.g. `"#"` for python, `"//"` for java).
pub fn comment_prefix_for_lang(lang: &str) -> &'static str {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for p in all_providers() {
            m.insert(p.language_id(), p.comment_prefix());
        }
        m
    });
    map.get(lang).copied().unwrap_or("//")
}

/// Lightweight per-file scan metadata (no tree-sitter parse).
/// Returned by [`LanguageProvider::scan_file`] during the scan phase.
#[derive(Clone, Debug, Default)]
pub struct SourceScan {
    /// Lines of code (newline count).
    pub loc: u64,
    /// Best-effort namespace / package declaration, if any.
    pub package: Option<String>,
    /// Framework identifiers detected from cheap string matching.
    /// Normalized to a fixed set: `"spring"`, `"nestjs"`, `"flask"`, `"fastapi"`.
    pub frameworks: BTreeSet<String>,
}

pub trait LanguageProvider: Send + Sync {
    fn language(&self) -> tree_sitter::Language;
    /// Short lowercase identifier for this language, e.g. `"java"`, `"typescript"`, `"python"`.
    fn language_id(&self) -> &'static str;
    fn extensions(&self) -> &'static [&'static str];
    fn scope_query(&self) -> &tree_sitter::Query;
    fn package_of(&self, root: tree_sitter::Node<'_>, src: &str) -> Option<String>;
    fn stereotype(&self, def_text: &str) -> Option<Stereotype>;
    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit>;

    /// Cheap per-file scan: LOC, package/namespace, framework hints.
    /// Called during the scan phase — no tree-sitter, just string matching.
    fn scan_file(&self, rel: &str, src: &str) -> anyhow::Result<SourceScan>;

    /// Single-line comment prefix. Default: `"//"`. Override for `"#"` languages.
    fn comment_prefix(&self) -> &'static str {
        "//"
    }

    /// Language tag used for markdown syntax highlighting. Default: same as `language_id()`.
    fn lang_tag(&self) -> &'static str {
        self.language_id()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stereotype {
    Spring,
    JaxRs,
    NestJs,
    Flask,
    FastApi,
}

#[cfg(test)]
mod scope_lang_tests {
    use super::language_ids_for_paths;

    #[test]
    fn language_ids_derived_from_extensions() {
        let ids = language_ids_for_paths(&[
            "src/A.java".to_string(),
            "src/b.ts".to_string(),
            "src/c.js".to_string(), // JS is handled by the typescript provider
            "svc/d.py".to_string(),
            "README.md".to_string(), // unknown ext → ignored
            "Makefile".to_string(),  // no ext → ignored
        ]);
        assert!(ids.contains("java"));
        assert!(ids.contains("typescript")); // both .ts and .js
        assert!(ids.contains("python"));
        assert!(!ids.contains("go"));
        // Empty / unknown-only inputs yield an empty set.
        assert!(language_ids_for_paths(&["x.md".to_string(), "y".to_string()]).is_empty());
    }
}
