use std::io;
use std::path::Path;

use cih_search::{
    SearchIndex, SearchIndexInspection, SearchIndexLoad, SearchIndexMetadata, SearchIndexSource,
};

use crate::ports::search_index_store::{
    SearchIndexPersistError, SearchIndexPersistFailure, SearchIndexStore,
};

#[derive(Clone, Default)]
pub(crate) struct FileSearchIndexStore;

impl SearchIndexStore for FileSearchIndexStore {
    fn inspect(&self, path: &Path) -> io::Result<SearchIndexInspection> {
        cih_search::inspect_search_index(path)
    }

    fn load(&self, path: &Path, source: &SearchIndexSource) -> io::Result<SearchIndexLoad> {
        cih_search::load_search_index(path, source)
    }

    fn persist(
        &self,
        path: &Path,
        source: &SearchIndexSource,
        index: &SearchIndex,
    ) -> Result<SearchIndexMetadata, SearchIndexPersistError> {
        cih_search::persist_search_index(path, source, index).map_err(|error| {
            let failure = match error.kind() {
                io::ErrorKind::ReadOnlyFilesystem => SearchIndexPersistFailure::ReadOnly,
                io::ErrorKind::PermissionDenied => SearchIndexPersistFailure::Permission,
                io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => {
                    SearchIndexPersistFailure::Serialization
                }
                io::ErrorKind::WriteZero
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::BrokenPipe => SearchIndexPersistFailure::Durability,
                _ => SearchIndexPersistFailure::Io,
            };
            SearchIndexPersistError { failure }
        })
    }
}
