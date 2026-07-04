use std::path::Path;

use serde::{Deserialize, Serialize};

/// User-overridable configuration for the package-based feature strategy.
/// Loaded from `.cih/grouping.toml` when present; otherwise built-in defaults apply.
///
/// # `.cih/grouping.toml` example
/// ```toml
/// [package_grouping]
/// src_roots      = ["/src/main/java/", "/src/main/kotlin/"]
/// strip_prefixes = ["banking-", "payment-"]
/// strip_suffixes = ["-api", "-impl", "-service", "-core"]
/// catch_all      = ["core", "common", "shared", "custom", "impl"]
/// skip_segments  = ["service", "repository", "gateway", "controller"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageConfig {
    /// Source root markers (must include the leading and trailing `/`).
    pub src_roots: Vec<String>,
    /// Maven module name prefixes to strip (e.g. `"banking-"`).
    pub strip_prefixes: Vec<String>,
    /// Maven module name suffixes to strip (e.g. `"-api"`).
    pub strip_suffixes: Vec<String>,
    /// Normalised module names that are too generic to use as feature names.
    /// When matched, strategy falls through to the Java package path.
    pub catch_all: Vec<String>,
    /// Java package path segments to skip when searching for the feature name.
    pub skip_segments: Vec<String>,
}

#[derive(Deserialize)]
struct GroupingToml {
    package_grouping: Option<PackageConfig>,
}

impl PackageConfig {
    /// Load from `<repo>/.cih/grouping.toml`; fall back to `PackageConfig::default()`.
    pub fn load_or_default(repo: &Path) -> Self {
        let path = repo.join(".cih").join("grouping.toml");
        if path.is_file() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(parsed) = toml::from_str::<GroupingToml>(&text) {
                    if let Some(cfg) = parsed.package_grouping {
                        return cfg;
                    }
                }
            }
        }
        Self::default()
    }
}

impl Default for PackageConfig {
    fn default() -> Self {
        PackageConfig {
            src_roots: vec![
                "/src/main/java/".into(),
                "/src/main/kotlin/".into(),
                "/src/test/java/".into(),
                "/src/test/kotlin/".into(),
            ],
            strip_prefixes: vec![
                "banking-".into(),
                "payment-".into(),
                "finance-".into(),
                "base-".into(),
                "common-".into(),
                "core-".into(),
                "shared-".into(),
                "platform-".into(),
                "infra-".into(),
                "infrastructure-".into(),
                "app-".into(),
                "service-".into(),
            ],
            strip_suffixes: vec![
                "-api".into(),
                "-service".into(),
                "-impl".into(),
                "-core".into(),
                "-common".into(),
                "-module".into(),
                "-lib".into(),
                "-client".into(),
                "-server".into(),
                "-domain".into(),
                "-model".into(),
                "-dto".into(),
                "-web".into(),
                "-rest".into(),
                "-grpc".into(),
            ],
            catch_all: vec![
                "core".into(),
                "common".into(),
                "shared".into(),
                "base".into(),
                "impl".into(),
                "custom".into(),
                "default".into(),
                "generic".into(),
                "abstract".into(),
                "infra".into(),
                "infrastructure".into(),
                "platform".into(),
            ],
            skip_segments: vec![
                // language / build dirs
                "java".into(),
                "kotlin".into(),
                "scala".into(),
                "groovy".into(),
                "main".into(),
                "test".into(),
                // top-level TLDs and common org segments
                "com".into(),
                "org".into(),
                "net".into(),
                "io".into(),
                "co".into(),
                "dev".into(),
                // cross-cutting / generic package names
                "impl".into(),
                "internal".into(),
                "common".into(),
                "shared".into(),
                "core".into(),
                "base".into(),
                "custom".into(),
                "default".into(),
                "util".into(),
                "utils".into(),
                "helper".into(),
                "helpers".into(),
                "support".into(),
                // data model layers
                "model".into(),
                "models".into(),
                "dto".into(),
                "dtos".into(),
                "entity".into(),
                "entities".into(),
                "domain".into(),
                // config / properties
                "config".into(),
                "configuration".into(),
                "properties".into(),
                // service / use-case layers
                "service".into(),
                "services".into(),
                "usecase".into(),
                "usecases".into(),
                // persistence layers
                "repository".into(),
                "repositories".into(),
                "repo".into(),
                "repos".into(),
                "persistence".into(),
                // web / controller layers
                "controller".into(),
                "controllers".into(),
                "handler".into(),
                "handlers".into(),
                "resource".into(),
                "resources".into(),
                // error handling
                "exception".into(),
                "exceptions".into(),
                "error".into(),
                "errors".into(),
                // transport layers
                "web".into(),
                "rest".into(),
                "grpc".into(),
                "api".into(),
                "client".into(),
                "server".into(),
                // messaging
                "messaging".into(),
                "event".into(),
                "events".into(),
                "listener".into(),
                "listeners".into(),
                // infrastructure adapters
                "gateway".into(),
                "adapter".into(),
                "adapters".into(),
                "infrastructure".into(),
                "infra".into(),
                // security / filters
                "security".into(),
                "filter".into(),
                "filters".into(),
                "interceptor".into(),
                "interceptors".into(),
                // mapping / conversion
                "mapper".into(),
                "mappers".into(),
                "converter".into(),
                "converters".into(),
                // scheduling
                "scheduler".into(),
                "job".into(),
                "jobs".into(),
                "task".into(),
                "tasks".into(),
            ],
        }
    }
}
