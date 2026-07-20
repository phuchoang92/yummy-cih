//! Source-control boundary for resolving files changed in a repository.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeScope {
    Working,
    Staged,
    BaseRef,
}

pub(crate) trait ChangedFilesSource: Send + Sync {
    fn changed_files(
        &self,
        repo_path: &str,
        scope: ChangeScope,
        base_ref: Option<&str>,
    ) -> Result<Vec<String>, String>;
}
