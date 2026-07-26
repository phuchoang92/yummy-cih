//! Authenticated, operation-bound application cursors.
//!
//! Cursors are opaque replay state, never authority. Every token carries a
//! schema, key id, operation, and expiry inside a keyed-MAC envelope. Callers
//! add their own repository/filter/generation bindings to the typed payload.

use std::sync::OnceLock;

use rand::RngCore as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::domain::error::AppError;

pub(crate) const CURSOR_SIGNING_KEY_ENV: &str = "CIH_CURSOR_SIGNING_KEY";
pub(crate) const DEFAULT_CURSOR_TTL_SECS: u64 = 15 * 60;

const ENVELOPE_SCHEMA: u8 = 1;
const ENVELOPE_PREFIX: &str = "c1";
const MAX_CURSOR_BYTES: usize = 8 * 1024;

#[derive(Clone)]
pub(crate) struct CursorCodec {
    key: [u8; 32],
    key_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CursorEnvelope<T> {
    schema_version: u8,
    key_id: String,
    operation: String,
    expires_at: u64,
    payload: T,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepositoryCursorBinding {
    pub(crate) identity: String,
    pub(crate) published_epoch: Option<String>,
    pub(crate) published_graph_content_version: Option<String>,
}

impl RepositoryCursorBinding {
    pub(crate) fn from_entry(entry: &cih_core::RegistryEntry) -> Self {
        Self {
            identity: repository_cursor_identity(entry),
            published_epoch: entry.published_epoch.clone(),
            published_graph_content_version: entry.published_graph_content_version.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CursorError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl CursorError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn into_app_error(self, field: &'static str) -> AppError {
        AppError::InvalidInput {
            field,
            message: format!("{}: {}", self.code, self.message),
        }
    }
}

impl CursorCodec {
    pub(crate) fn global() -> Result<&'static Self, AppError> {
        static CODEC: OnceLock<Result<CursorCodec, String>> = OnceLock::new();
        CODEC
            .get_or_init(Self::from_environment)
            .as_ref()
            .map_err(|message| AppError::Unavailable {
                dependency: "cursor signing key",
                message: message.clone(),
                retryable: false,
            })
    }

    fn from_environment() -> Result<Self, String> {
        match std::env::var(CURSOR_SIGNING_KEY_ENV) {
            Ok(value) => {
                let key = parse_signing_key(&value)?;
                Ok(Self::new(key, key_id("env-v1", &key)))
            }
            Err(std::env::VarError::NotPresent) => {
                let mut key = [0_u8; 32];
                rand::rng().fill_bytes(&mut key);
                let key_id = key_id("process-v1", &key);
                tracing::warn!(
                    setting = CURSOR_SIGNING_KEY_ENV,
                    %key_id,
                    "cursor signing uses a secure process-local key; cursors expire on restart"
                );
                Ok(Self::new(key, key_id))
            }
            Err(std::env::VarError::NotUnicode(_)) => Err(format!(
                "{CURSOR_SIGNING_KEY_ENV} must be 64 hexadecimal UTF-8 characters"
            )),
        }
    }

    fn new(key: [u8; 32], key_id: String) -> Self {
        Self { key, key_id }
    }

    #[cfg(test)]
    pub(crate) fn for_test(key: [u8; 32], key_id: &str) -> Self {
        Self::new(key, key_id.to_string())
    }

    pub(crate) fn encode_at<T: Serialize>(
        &self,
        operation: &str,
        ttl_secs: u64,
        payload: &T,
        now: u64,
    ) -> Result<String, CursorError> {
        let envelope = CursorEnvelope {
            schema_version: ENVELOPE_SCHEMA,
            key_id: self.key_id.clone(),
            operation: operation.to_string(),
            expires_at: now.saturating_add(ttl_secs),
            payload,
        };
        let encoded = serde_json::to_vec(&envelope).map_err(|error| {
            CursorError::new(
                "cursor_encode_failed",
                format!("cannot encode cursor: {error}"),
            )
        })?;
        let signature = blake3::keyed_hash(&self.key, &encoded);
        let token = format!(
            "{ENVELOPE_PREFIX}.{}.{}",
            encode_hex(&encoded),
            signature.to_hex()
        );
        if token.len() > MAX_CURSOR_BYTES {
            return Err(CursorError::new(
                "cursor_too_large",
                "encoded cursor exceeds the application limit",
            ));
        }
        Ok(token)
    }

    pub(crate) fn decode_at<T: DeserializeOwned>(
        &self,
        raw: &str,
        expected_operation: &str,
        now: u64,
    ) -> Result<T, CursorError> {
        if raw.len() > MAX_CURSOR_BYTES {
            return Err(CursorError::new("malformed_cursor", "cursor is too large"));
        }
        let mut parts = raw.split('.');
        let prefix = parts.next();
        let payload_hex = parts.next();
        let signature_hex = parts.next();
        if prefix != Some(ENVELOPE_PREFIX)
            || payload_hex.is_none()
            || signature_hex.is_none()
            || parts.next().is_some()
        {
            return Err(CursorError::new(
                "malformed_cursor",
                "malformed cursor; reuse next_cursor exactly as returned",
            ));
        }
        let payload = decode_hex(payload_hex.unwrap_or_default()).ok_or_else(|| {
            CursorError::new("malformed_cursor", "cursor payload is not valid hex")
        })?;
        let signature = decode_hex(signature_hex.unwrap_or_default())
            .filter(|bytes| bytes.len() == 32)
            .ok_or_else(|| CursorError::new("malformed_cursor", "cursor signature is not valid"))?;
        let expected_signature = blake3::keyed_hash(&self.key, &payload);
        if !constant_time_eq::constant_time_eq(signature.as_slice(), expected_signature.as_bytes())
        {
            return Err(CursorError::new(
                "cursor_tampered",
                "cursor authentication failed; restart from the first page",
            ));
        }

        let envelope: CursorEnvelope<T> = serde_json::from_slice(&payload).map_err(|_| {
            CursorError::new(
                "malformed_cursor",
                "cursor payload is invalid; restart from the first page",
            )
        })?;
        if envelope.schema_version != ENVELOPE_SCHEMA {
            return Err(CursorError::new(
                "wrong_schema",
                "cursor schema is unsupported; restart from the first page",
            ));
        }
        if envelope.key_id != self.key_id {
            return Err(CursorError::new(
                "wrong_key_id",
                "cursor was issued by a different signing-key generation",
            ));
        }
        if envelope.operation != expected_operation {
            return Err(CursorError::new(
                "wrong_operation",
                format!("cursor is not valid for {expected_operation}"),
            ));
        }
        if envelope.expires_at < now {
            return Err(CursorError::new(
                "cursor_expired",
                "cursor expired; restart from the first page",
            ));
        }
        Ok(envelope.payload)
    }
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn canonical_filter_hash(value: &[u8]) -> String {
    blake3::hash(value).to_hex().to_string()
}

/// Stable repository binding for cursors and ordered catalogs. New entries use
/// immutable `RepositoryId`; legacy entries explicitly fall back to graph key.
/// Paths remain mutable data and are never used as cursor identity.
pub(crate) fn repository_cursor_identity(entry: &cih_core::RegistryEntry) -> String {
    entry.repository_id.as_ref().map_or_else(
        || format!("legacy-graph-key:{}", entry.graph_key),
        |repository_id| format!("repository-id:{repository_id}"),
    )
}

fn parse_signing_key(value: &str) -> Result<[u8; 32], String> {
    let bytes = decode_hex(value).ok_or_else(|| {
        format!("{CURSOR_SIGNING_KEY_ENV} must contain exactly 64 hexadecimal characters")
    })?;
    bytes.try_into().map_err(|_| {
        format!("{CURSOR_SIGNING_KEY_ENV} must contain exactly 64 hexadecimal characters")
    })
}

fn key_id(prefix: &str, key: &[u8; 32]) -> String {
    let digest = blake3::hash(key).to_hex().to_string();
    format!("{prefix}-{}", &digest[..16])
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    raw.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TestPayload {
        value: String,
    }

    #[test]
    fn authenticated_cursor_round_trips_and_rejects_tampering() {
        let codec = CursorCodec::for_test([0x42; 32], "test-v1");
        let token = codec
            .encode_at(
                "test_operation",
                60,
                &TestPayload {
                    value: "page-key".to_string(),
                },
                100,
            )
            .unwrap();
        assert_eq!(
            codec
                .decode_at::<TestPayload>(&token, "test_operation", 101)
                .unwrap(),
            TestPayload {
                value: "page-key".to_string()
            }
        );

        let mut tampered = token.into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let error = codec
            .decode_at::<TestPayload>(&String::from_utf8(tampered).unwrap(), "test_operation", 101)
            .unwrap_err();
        assert_eq!(error.code, "cursor_tampered");
    }

    #[test]
    fn cursor_validates_operation_key_id_and_expiry() {
        let key = [0x5a; 32];
        let codec = CursorCodec::for_test(key, "test-v1");
        let token = codec
            .encode_at("operation-a", 10, &TestPayload { value: "x".into() }, 100)
            .unwrap();
        assert_eq!(
            codec
                .decode_at::<TestPayload>(&token, "operation-b", 101)
                .unwrap_err()
                .code,
            "wrong_operation"
        );
        assert_eq!(
            codec
                .decode_at::<TestPayload>(&token, "operation-a", 111)
                .unwrap_err()
                .code,
            "cursor_expired"
        );
        assert_eq!(
            CursorCodec::for_test(key, "rotated-v2")
                .decode_at::<TestPayload>(&token, "operation-a", 101)
                .unwrap_err()
                .code,
            "wrong_key_id"
        );

        let wrong_schema = CursorEnvelope {
            schema_version: ENVELOPE_SCHEMA + 1,
            key_id: codec.key_id.clone(),
            operation: "operation-a".to_string(),
            expires_at: 110,
            payload: TestPayload { value: "x".into() },
        };
        let payload = serde_json::to_vec(&wrong_schema).unwrap();
        let signature = blake3::keyed_hash(&codec.key, &payload);
        let token = format!(
            "{ENVELOPE_PREFIX}.{}.{}",
            encode_hex(&payload),
            signature.to_hex()
        );
        assert_eq!(
            codec
                .decode_at::<TestPayload>(&token, "operation-a", 101)
                .unwrap_err()
                .code,
            "wrong_schema"
        );
    }

    #[test]
    fn configured_key_is_exactly_thirty_two_hex_bytes() {
        let key = parse_signing_key(&"ab".repeat(32)).unwrap();
        assert_eq!(key, [0xab; 32]);
        assert!(parse_signing_key("too-short").is_err());
        assert!(parse_signing_key(&"gg".repeat(32)).is_err());

        let issuer = CursorCodec::new(key, key_id("env-v1", &key));
        let verifier = CursorCodec::new(key, key_id("env-v1", &key));
        let token = issuer
            .encode_at("stable", 60, &TestPayload { value: "x".into() }, 100)
            .unwrap();
        assert_eq!(
            verifier
                .decode_at::<TestPayload>(&token, "stable", 101)
                .unwrap(),
            TestPayload { value: "x".into() }
        );
    }
}
