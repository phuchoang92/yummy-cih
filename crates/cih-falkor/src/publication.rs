//! Redis-backed authoritative publication records for FalkorDB.

use async_trait::async_trait;
use cih_core::RepositoryId;
use cih_graph_store::publication::{
    CurrentPublication, GraphPublicationEpoch, GraphPublicationStore, PublicationCasResult,
    PublisherFencingToken,
};
use cih_graph_store::{GraphStoreError, Result};

const DEFAULT_NAMESPACE: &str = "cih:publication";

/// The script validates expected epoch and fencing token before any write.
/// Redis runs it atomically, so the immutable epoch record and current pointer
/// become visible together or neither is changed.
const PUBLICATION_CAS_SCRIPT: &str = r#"
local current_epoch = redis.call('HGET', KEYS[1], 'epoch')
local current_fence = tonumber(redis.call('HGET', KEYS[1], 'fence') or '0')
local allocated_fence = tonumber(redis.call('GET', KEYS[3]) or '0')
local requested_fence = tonumber(ARGV[4])

if requested_fence < allocated_fence then
    return {2, current_epoch or '', allocated_fence}
end
if requested_fence <= current_fence then
    return {2, current_epoch or '', current_fence}
end

local expected = ARGV[1]
local actual = current_epoch or ''
if expected ~= actual then
    return {1, actual, current_fence}
end

local existing = redis.call('GET', KEYS[2])
if existing and existing ~= ARGV[3] then
    return redis.error_reply('publication epoch collision')
end
if not existing then
    redis.call('SET', KEYS[2], ARGV[3])
end

redis.call('HSET', KEYS[1],
    'epoch', ARGV[2],
    'fence', ARGV[4],
    'record', ARGV[3])
return {0, ARGV[2], requested_fence}
"#;

pub struct FalkorPublicationStore {
    client: redis::Client,
    connection: tokio::sync::OnceCell<redis::aio::ConnectionManager>,
    namespace: String,
}

impl FalkorPublicationStore {
    pub fn connect(url: &str) -> Result<Self> {
        Self::connect_with_namespace(url, DEFAULT_NAMESPACE)
    }

    pub fn connect_with_namespace(url: &str, namespace: impl Into<String>) -> Result<Self> {
        let namespace = namespace.into();
        if namespace.trim().is_empty() {
            return Err(GraphStoreError::InvalidInput(
                "Falkor publication namespace must not be empty".into(),
            ));
        }
        let client = redis::Client::open(url).map_err(map_redis_error)?;
        Ok(Self {
            client,
            connection: tokio::sync::OnceCell::new(),
            namespace,
        })
    }

    async fn connection(&self) -> Result<redis::aio::ConnectionManager> {
        self.connection
            .get_or_try_init(|| async {
                redis::aio::ConnectionManager::new(self.client.clone()).await
            })
            .await
            .cloned()
            .map_err(map_redis_error)
    }

    fn current_key(&self, repository_id: &RepositoryId) -> String {
        format!("{}:{{{}}}:current", self.namespace, repository_id.as_str())
    }

    fn epoch_key(&self, repository_id: &RepositoryId, epoch: &GraphPublicationEpoch) -> String {
        format!(
            "{}:{{{}}}:epoch:{}",
            self.namespace,
            repository_id.as_str(),
            epoch.as_str()
        )
    }

    fn fence_key(&self, repository_id: &RepositoryId) -> String {
        format!(
            "{}:{{{}}}:fence-sequence",
            self.namespace,
            repository_id.as_str()
        )
    }

    fn parse_record(
        repository_id: &RepositoryId,
        expected_epoch: Option<&GraphPublicationEpoch>,
        raw: &str,
    ) -> Result<CurrentPublication> {
        let record: CurrentPublication = serde_json::from_str(raw).map_err(|error| {
            GraphStoreError::Backend(format!("parse Falkor publication record: {error}"))
        })?;
        record.validate()?;
        if &record.repository_id != repository_id
            || expected_epoch.is_some_and(|epoch| epoch != &record.epoch)
        {
            return Err(GraphStoreError::Backend(
                "Falkor publication record has mismatched identity".into(),
            ));
        }
        Ok(record)
    }
}

#[async_trait]
impl GraphPublicationStore for FalkorPublicationStore {
    async fn allocate_fencing_token(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<PublisherFencingToken> {
        let mut connection = self.connection().await?;
        let token: u64 = redis::cmd("INCR")
            .arg(self.fence_key(repository_id))
            .query_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        PublisherFencingToken::new(token)
    }

    async fn current(&self, repository_id: &RepositoryId) -> Result<Option<CurrentPublication>> {
        let mut connection = self.connection().await?;
        let raw: Option<String> = redis::cmd("HGET")
            .arg(self.current_key(repository_id))
            .arg("record")
            .query_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        raw.as_deref()
            .map(|raw| Self::parse_record(repository_id, None, raw))
            .transpose()
    }

    async fn by_epoch(
        &self,
        repository_id: &RepositoryId,
        epoch: &GraphPublicationEpoch,
    ) -> Result<Option<CurrentPublication>> {
        let mut connection = self.connection().await?;
        let raw: Option<String> = redis::cmd("GET")
            .arg(self.epoch_key(repository_id, epoch))
            .query_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        raw.as_deref()
            .map(|raw| Self::parse_record(repository_id, Some(epoch), raw))
            .transpose()
    }

    async fn compare_and_swap(
        &self,
        repository_id: &RepositoryId,
        expected_epoch: Option<&GraphPublicationEpoch>,
        next: &CurrentPublication,
        fencing_token: PublisherFencingToken,
    ) -> Result<PublicationCasResult> {
        next.validate()?;
        if &next.repository_id != repository_id {
            return Err(GraphStoreError::InvalidInput(
                "publication repository_id does not match the CAS repository".into(),
            ));
        }
        if next.previous_epoch.as_ref() != expected_epoch {
            return Err(GraphStoreError::InvalidInput(
                "publication previous_epoch must equal the CAS expected_epoch".into(),
            ));
        }

        let record = serde_json::to_string(next).map_err(|error| {
            GraphStoreError::Backend(format!("serialize Falkor publication record: {error}"))
        })?;
        let mut connection = self.connection().await?;
        let response: (i64, String, u64) = redis::Script::new(PUBLICATION_CAS_SCRIPT)
            .key(self.current_key(repository_id))
            .key(self.epoch_key(repository_id, &next.epoch))
            .key(self.fence_key(repository_id))
            .arg(expected_epoch.map_or("", GraphPublicationEpoch::as_str))
            .arg(next.epoch.as_str())
            .arg(record)
            .arg(fencing_token.get())
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;

        match response.0 {
            0 => Ok(PublicationCasResult::Published),
            1 => Ok(PublicationCasResult::Conflict {
                current_epoch: if response.1.is_empty() {
                    None
                } else {
                    Some(GraphPublicationEpoch::parse(response.1)?)
                },
            }),
            2 => Ok(PublicationCasResult::StaleFencingToken {
                current_token: PublisherFencingToken::new(response.2)?,
            }),
            code => Err(GraphStoreError::Backend(format!(
                "Falkor publication CAS returned unknown status {code}"
            ))),
        }
    }
}

fn map_redis_error(error: redis::RedisError) -> GraphStoreError {
    GraphStoreError::Backend(format!("Falkor publication metadata: {error}"))
}
