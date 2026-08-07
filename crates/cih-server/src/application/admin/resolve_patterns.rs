//! `add_resolve_pattern` / `list_resolve_patterns` — let a connected agent teach CIH a repo's own
//! framework conventions by writing rules to `<repo>/cih.patterns.toml`, then (optionally) kicking a
//! re-index so the deterministic engine re-applies them. The tool only persists rules + reindexes;
//! all rule *application* stays in the engine.

use cih_patterns::{load_patterns, patterns_path, to_toml, RouteRule};
use serde::Serialize;

use crate::application::app_services::RepoContextService;
use crate::application::indexing::{IndexRepositoryCommand, IndexingService};
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;

#[derive(Clone)]
pub(crate) struct ResolvePatternService {
    repos: RepoContextService,
    indexing: IndexingService,
}

impl ResolvePatternService {
    pub(crate) fn new(repos: RepoContextService, indexing: IndexingService) -> Self {
        Self { repos, indexing }
    }

    pub(crate) async fn add(
        &self,
        command: AddResolvePatternCommand,
    ) -> Result<AddResolvePatternOutput, AppError> {
        add_resolve_pattern(&self.repos, &self.indexing, command).await
    }

    pub(crate) fn list(
        &self,
        command: ListResolvePatternsCommand,
    ) -> Result<ListResolvePatternsOutput, AppError> {
        list_resolve_patterns(&self.repos, command)
    }
}

pub(crate) struct AddResolvePatternCommand {
    pub(crate) repo: String,
    pub(crate) kind: String,
    pub(crate) annotation: String,
    pub(crate) path_attr: String,
    pub(crate) method: String,
    pub(crate) method_attr: String,
    pub(crate) class_prefix_annotation: String,
    pub(crate) reindex: bool,
}

pub(crate) struct ListResolvePatternsCommand {
    pub(crate) repo: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AddResolvePatternOutput {
    pub(crate) added: bool,
    pub(crate) route_rules: usize,
    pub(crate) patterns_file: String,
    pub(crate) reindex_job_id: Option<String>,
    pub(crate) message: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListResolvePatternsOutput {
    pub(crate) patterns_file: String,
    pub(crate) routes: Vec<RouteRule>,
}

async fn add_resolve_pattern(
    repos: &RepoContextService,
    indexing: &IndexingService,
    command: AddResolvePatternCommand,
) -> Result<AddResolvePatternOutput, AppError> {
    if command.kind != "route" {
        return Err(AppError::InvalidInput {
            field: "kind",
            message: format!(
                "unsupported pattern kind '{}': only \"route\" is supported",
                command.kind
            ),
        });
    }
    if command.annotation.trim().is_empty() {
        return Err(AppError::InvalidInput {
            field: "annotation",
            message: "annotation name is required, without @".into(),
        });
    }
    // `method` is persisted verbatim into cih.patterns.toml and then used as the
    // route's HTTP verb, so a typo silently produces routes nobody can match.
    let method = validate_http_method(&command.method)?;

    let repo = repos.resolve_repo(RepoSelector::from_wire(&command.repo))?;
    let repo_path = repo.canonical_path.as_path();

    let rule = RouteRule {
        annotation: command.annotation.trim().to_string(),
        path_attr: nonempty(&command.path_attr).unwrap_or_else(|| "value".to_string()),
        method,
        method_attr: nonempty(&command.method_attr),
        class_prefix_annotation: nonempty(&command.class_prefix_annotation),
        class_prefix_attr: "value".to_string(),
    };

    let mut rules = load_patterns(repo_path);
    let added = rules.add_route(rule);
    let path = patterns_path(repo_path);
    if added {
        std::fs::write(&path, to_toml(&rules)).map_err(|error| AppError::Unavailable {
            dependency: "patterns file",
            message: format!("failed to write {}: {error}", path.display()),
            retryable: false,
        })?;
    }

    let mut job_id = None;
    if command.reindex {
        if let Ok(index_command) = IndexRepositoryCommand::try_new(
            repo.canonical_path.display().to_string(),
            String::new(),
            String::new(),
        ) {
            if let Ok(receipt) = indexing.start(index_command).await {
                job_id = Some(receipt.job_id().to_string());
            }
        }
    }

    Ok(AddResolvePatternOutput {
        added,
        route_rules: rules.routes.len(),
        patterns_file: path.display().to_string(),
        reindex_job_id: job_id,
        message: if added {
            "Pattern added. Poll index_status(job_id=...) if a reindex was started, then re-run route_map."
        } else {
            "Pattern already present (no change)."
        },
    })
}

fn list_resolve_patterns(
    repos: &RepoContextService,
    command: ListResolvePatternsCommand,
) -> Result<ListResolvePatternsOutput, AppError> {
    let repo = repos.resolve_repo(RepoSelector::from_wire(&command.repo))?;
    let rules = load_patterns(&repo.canonical_path);
    Ok(ListResolvePatternsOutput {
        patterns_file: patterns_path(&repo.canonical_path).display().to_string(),
        routes: rules.routes,
    })
}

/// HTTP verbs a route rule may pin. Empty means "no fixed verb" (the rule then
/// relies on `method_attr`, or the engine's GET default).
const ROUTE_METHODS: &[&str] = &[
    "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE",
];

fn validate_http_method(method: &str) -> Result<Option<String>, AppError> {
    let Some(method) = nonempty(method) else {
        return Ok(None);
    };
    let upper = method.to_ascii_uppercase();
    if !ROUTE_METHODS.contains(&upper.as_str()) {
        return Err(AppError::InvalidInput {
            field: "method",
            message: format!(
                "unknown HTTP method '{method}'; expected one of {}",
                ROUTE_METHODS.join(", ")
            ),
        });
    }
    Ok(Some(upper))
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `method` is persisted into `cih.patterns.toml` and becomes the route's
    /// verb, so an unvalidated typo produced routes nothing could match.
    #[test]
    fn http_method_is_validated_and_normalized() {
        assert_eq!(
            validate_http_method("post").unwrap().as_deref(),
            Some("POST")
        );
        assert_eq!(
            validate_http_method(" Get ").unwrap().as_deref(),
            Some("GET")
        );
        // Empty means "no fixed verb" — the rule uses method_attr instead.
        assert_eq!(validate_http_method("").unwrap(), None);
        assert_eq!(validate_http_method("   ").unwrap(), None);

        let error = validate_http_method("POSTT").unwrap_err();
        match error {
            AppError::InvalidInput { field, message } => {
                assert_eq!(field, "method");
                assert!(message.contains("POSTT"), "{message}");
                assert!(message.contains("GET, POST"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert!(validate_http_method("SELECT").is_err());
    }

    #[test]
    fn nonempty_trims_and_drops_blanks() {
        assert_eq!(nonempty("  value  ").as_deref(), Some("value"));
        assert_eq!(nonempty("   "), None);
        assert_eq!(nonempty(""), None);
    }
}
