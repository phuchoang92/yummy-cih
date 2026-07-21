//! Persistence boundary for immutable artifact adjacency indexes.

use std::io;
use std::path::Path;

use crate::ports::artifact_repository::ArtifactIndexes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceFileIdentity {
    pub(crate) len: u64,
    pub(crate) modified_secs: u64,
    pub(crate) modified_nanos: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactSourceIdentity {
    pub(crate) nodes: SourceFileIdentity,
    pub(crate) edges: SourceFileIdentity,
}

pub(crate) trait ArtifactIndexStore: Send + Sync {
    fn load(
        &self,
        artifacts_dir: &Path,
        source: ArtifactSourceIdentity,
    ) -> io::Result<Option<ArtifactIndexes>>;

    fn persist(
        &self,
        artifacts_dir: &Path,
        source: ArtifactSourceIdentity,
        indexes: &ArtifactIndexes,
    ) -> io::Result<()>;
}
