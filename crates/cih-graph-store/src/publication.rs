//! Authoritative graph-publication lifecycle port.
//!
//! Graph reads and graph publication deliberately use separate ports. A
//! publication record pins one immutable physical graph and all identities
//! needed to prove that a request is not mixing generations. Backends own the
//! atomic compare-and-swap implementation; the registry is only a recoverable
//! mirror of the committed record.

use async_trait::async_trait;
use cih_core::{new_publication_epoch, RepositoryId};
use serde::{Deserialize, Serialize};

use crate::{GraphStoreError, Result};

fn parse_digest(label: &str, value: impl Into<String>) -> Result<String> {
    let value = value.into();
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(GraphStoreError::InvalidInput(format!(
            "{label} must be exactly 64 lowercase hexadecimal characters"
        )));
    }
    Ok(value)
}

macro_rules! digest_identity {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self> {
                parse_digest($label, value).map(Self)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

digest_identity!(GraphPublicationEpoch, "graph publication epoch");
digest_identity!(ArtifactVersion, "artifact version");
digest_identity!(GraphContentVersion, "graph content version");
digest_identity!(ManifestDigest, "graph content manifest digest");
digest_identity!(ValidationDigest, "publication validation digest");

impl GraphPublicationEpoch {
    /// Allocate a fresh opaque publication identity. It is deliberately not a
    /// timestamp or content hash, so republishing identical content rotates it.
    pub fn allocate() -> Self {
        Self::parse(new_publication_epoch()).expect("core always allocates a valid digest")
    }
}

/// Monotonically increasing publisher lease token. Zero is reserved for the
/// absence of a committed publisher and is never accepted for a CAS attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PublisherFencingToken(u64);

impl PublisherFencingToken {
    pub fn new(value: u64) -> Result<Self> {
        if value == 0 {
            return Err(GraphStoreError::InvalidInput(
                "publisher fencing token must be greater than zero".into(),
            ));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for PublisherFencingToken {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// The authoritative, immutable description of one published graph epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentPublication {
    pub repository_id: RepositoryId,
    pub epoch: GraphPublicationEpoch,
    pub graph_content_version: GraphContentVersion,
    pub physical_graph_key: String,
    pub artifact_version: ArtifactVersion,
    pub graph_content_manifest_digest: ManifestDigest,
    pub validation_digest: ValidationDigest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_epoch: Option<GraphPublicationEpoch>,
}

impl CurrentPublication {
    pub fn validate(&self) -> Result<()> {
        if self.physical_graph_key.trim().is_empty() {
            return Err(GraphStoreError::InvalidInput(
                "physical graph key must not be empty".into(),
            ));
        }
        if self
            .previous_epoch
            .as_ref()
            .is_some_and(|previous| previous == &self.epoch)
        {
            return Err(GraphStoreError::InvalidInput(
                "publication previous_epoch must differ from epoch".into(),
            ));
        }
        Ok(())
    }
}

/// An expected-epoch CAS is a normal race, not an infrastructure error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublicationCasResult {
    Published,
    Conflict {
        current_epoch: Option<GraphPublicationEpoch>,
    },
    StaleFencingToken {
        current_token: PublisherFencingToken,
    },
}

#[async_trait]
pub trait GraphPublicationStore: Send + Sync {
    /// Durably allocate a monotonically increasing token for one publication
    /// attempt. Allocation and CAS are deliberately separate: a process that
    /// stalls after allocation can never overwrite a later publisher.
    async fn allocate_fencing_token(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<PublisherFencingToken>;

    async fn current(&self, repository_id: &RepositoryId) -> Result<Option<CurrentPublication>>;

    async fn by_epoch(
        &self,
        repository_id: &RepositoryId,
        epoch: &GraphPublicationEpoch,
    ) -> Result<Option<CurrentPublication>>;

    /// Atomically commit `next` only when both the expected epoch and fencing
    /// token are current. A successful call durably retains the epoch record
    /// and changes the authoritative pointer as one backend transaction.
    async fn compare_and_swap(
        &self,
        repository_id: &RepositoryId,
        expected_epoch: Option<&GraphPublicationEpoch>,
        next: &CurrentPublication,
        fencing_token: PublisherFencingToken,
    ) -> Result<PublicationCasResult>;
}

#[cfg(feature = "test-support")]
pub mod contract {
    use std::sync::Arc;

    use super::*;

    fn digest(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn publication(
        repository_id: &RepositoryId,
        epoch: char,
        previous_epoch: Option<GraphPublicationEpoch>,
    ) -> CurrentPublication {
        CurrentPublication {
            repository_id: repository_id.clone(),
            epoch: GraphPublicationEpoch::parse(digest(epoch)).unwrap(),
            graph_content_version: GraphContentVersion::parse(digest('c')).unwrap(),
            physical_graph_key: format!("repo-{}-epoch-{epoch}", repository_id.as_str()),
            artifact_version: ArtifactVersion::parse(digest('a')).unwrap(),
            graph_content_manifest_digest: ManifestDigest::parse(digest('d')).unwrap(),
            validation_digest: ValidationDigest::parse(digest('e')).unwrap(),
            previous_epoch,
        }
    }

    /// Backend-neutral publication contracts. The caller must provide an empty
    /// namespace/root so the first expected epoch is reliably absent.
    pub async fn run_publication_contract_suite(
        store: Arc<dyn GraphPublicationStore>,
    ) -> Result<()> {
        let repository_id = RepositoryId::parse(digest('1')).map_err(GraphStoreError::Other)?;
        assert!(store.current(&repository_id).await?.is_none());

        let first_allocated = store.allocate_fencing_token(&repository_id).await?;
        let second_allocated = store.allocate_fencing_token(&repository_id).await?;
        assert!(second_allocated > first_allocated);

        let first = publication(&repository_id, '2', None);
        assert_eq!(
            store
                .compare_and_swap(&repository_id, None, &first, second_allocated)
                .await?,
            PublicationCasResult::Published
        );
        assert_eq!(store.current(&repository_id).await?, Some(first.clone()));
        assert_eq!(
            store.by_epoch(&repository_id, &first.epoch).await?,
            Some(first.clone())
        );

        let second = publication(&repository_id, '3', Some(first.epoch.clone()));
        assert_eq!(
            store
                .compare_and_swap(
                    &repository_id,
                    Some(&first.epoch),
                    &second,
                    second_allocated,
                )
                .await?,
            PublicationCasResult::StaleFencingToken {
                current_token: second_allocated,
            }
        );
        assert_eq!(store.current(&repository_id).await?, Some(first.clone()));

        let third_allocated = store.allocate_fencing_token(&repository_id).await?;
        let conflicting = publication(&repository_id, '4', None);
        assert_eq!(
            store
                .compare_and_swap(&repository_id, None, &conflicting, third_allocated,)
                .await?,
            PublicationCasResult::Conflict {
                current_epoch: Some(first.epoch.clone()),
            }
        );
        assert_eq!(store.current(&repository_id).await?, Some(first.clone()));

        assert_eq!(
            store
                .compare_and_swap(&repository_id, Some(&first.epoch), &second, third_allocated,)
                .await?,
            PublicationCasResult::Published
        );
        assert_eq!(store.current(&repository_id).await?, Some(second.clone()));
        assert_eq!(
            store.by_epoch(&repository_id, &first.epoch).await?,
            Some(first)
        );

        let wrong_repository = RepositoryId::parse(digest('4')).map_err(GraphStoreError::Other)?;
        let error = store
            .compare_and_swap(
                &wrong_repository,
                Some(&second.epoch),
                &second,
                PublisherFencingToken::new(3)?,
            )
            .await
            .expect_err("repository mismatch must be rejected");
        assert!(matches!(error, GraphStoreError::InvalidInput(_)));

        let concurrent_repository =
            RepositoryId::parse(digest('5')).map_err(GraphStoreError::Other)?;
        let left = publication(&concurrent_repository, '6', None);
        let right = publication(&concurrent_repository, '7', None);
        let left_store = store.clone();
        let right_store = store.clone();
        let concurrent_repository_left = concurrent_repository.clone();
        let concurrent_repository_right = concurrent_repository.clone();
        let left_record = left.clone();
        let right_record = right.clone();
        let (left_result, right_result) = tokio::join!(
            async move {
                left_store
                    .compare_and_swap(
                        &concurrent_repository_left,
                        None,
                        &left_record,
                        PublisherFencingToken::new(10)?,
                    )
                    .await
            },
            async move {
                right_store
                    .compare_and_swap(
                        &concurrent_repository_right,
                        None,
                        &right_record,
                        PublisherFencingToken::new(11)?,
                    )
                    .await
            }
        );
        let results = [left_result?, right_result?];
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == PublicationCasResult::Published)
                .count(),
            1,
            "exactly one concurrent publisher must commit: {results:?}"
        );
        let committed = store
            .current(&concurrent_repository)
            .await?
            .expect("one concurrent publication committed");
        assert!(committed == left || committed == right);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_and_records_reject_ambiguous_values() {
        assert!(GraphPublicationEpoch::parse("short").is_err());
        assert!(GraphContentVersion::parse("A".repeat(64)).is_err());
        assert!(PublisherFencingToken::new(0).is_err());
        assert!(serde_json::from_str::<GraphPublicationEpoch>(r#""short""#).is_err());
        assert!(serde_json::from_str::<PublisherFencingToken>("0").is_err());

        let repository_id = RepositoryId::parse("1".repeat(64)).unwrap();
        let epoch = GraphPublicationEpoch::parse("2".repeat(64)).unwrap();
        let record = CurrentPublication {
            repository_id,
            epoch: epoch.clone(),
            graph_content_version: GraphContentVersion::parse("3".repeat(64)).unwrap(),
            physical_graph_key: " ".into(),
            artifact_version: ArtifactVersion::parse("4".repeat(64)).unwrap(),
            graph_content_manifest_digest: ManifestDigest::parse("5".repeat(64)).unwrap(),
            validation_digest: ValidationDigest::parse("6".repeat(64)).unwrap(),
            previous_epoch: Some(epoch),
        };
        assert!(record.validate().is_err());
    }
}
