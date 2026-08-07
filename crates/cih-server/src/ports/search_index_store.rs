use std::io;
use std::path::Path;

use cih_search::{
    SearchIndex, SearchIndexInspection, SearchIndexLoad, SearchIndexMetadata, SearchIndexSource,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchIndexPersistFailure {
    ReadOnly,
    Permission,
    Serialization,
    Durability,
    Io,
}

impl SearchIndexPersistFailure {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Permission => "permission",
            Self::Serialization => "serialization",
            Self::Durability => "durability",
            Self::Io => "io",
        }
    }
}

#[derive(Debug)]
pub(crate) struct SearchIndexPersistError {
    pub(crate) failure: SearchIndexPersistFailure,
}

impl std::fmt::Display for SearchIndexPersistError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "search sidecar persistence failed ({})",
            self.failure.label()
        )
    }
}

impl std::error::Error for SearchIndexPersistError {}

pub(crate) trait SearchIndexStore: Send + Sync {
    fn inspect(&self, path: &Path) -> io::Result<SearchIndexInspection>;

    fn load(&self, path: &Path, source: &SearchIndexSource) -> io::Result<SearchIndexLoad>;

    fn persist(
        &self,
        path: &Path,
        source: &SearchIndexSource,
        index: &SearchIndex,
    ) -> Result<SearchIndexMetadata, SearchIndexPersistError>;
}
