//! Configuration persistence and JSON codec for PALKA service.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Supported schema version of config.json.
pub const CONFIG_SCHEMA_VERSION_V1: u32 = 1;

/// In-memory representation of static service configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentConfig {
    pub child_sid: String,
    pub telegram_allowed_user_ids: Vec<u64>,
    pub telegram_allowed_chat_ids: Vec<i64>,
    pub heartbeat_interval_seconds: u64,
}

/// Error type for configuration persistence operations and JSON codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigPersistenceError {
    Json(String),
    UnsupportedSchemaVersion(u32),
    Validation(String),
}

impl fmt::Display for ConfigPersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(msg) => write!(f, "JSON error in configuration codec: {msg}"),
            Self::UnsupportedSchemaVersion(v) => {
                write!(f, "Unsupported configuration schema version: {v} (expected 1)")
            }
            Self::Validation(msg) => write!(f, "Configuration validation error: {msg}"),
        }
    }
}

impl std::error::Error for ConfigPersistenceError {}

impl From<serde_json::Error> for ConfigPersistenceError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err.to_string())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigWireV1 {
    schema_version: u32,
    child_sid: String,
    telegram_allowed_user_ids: Vec<u64>,
    telegram_allowed_chat_ids: Vec<i64>,
    heartbeat_interval_seconds: u64,
}

fn validate_config(config: &PersistentConfig) -> Result<(), ConfigPersistenceError> {
    if config.child_sid.is_empty() {
        return Err(ConfigPersistenceError::Validation(
            "child_sid cannot be empty".to_string(),
        ));
    }

    if config.heartbeat_interval_seconds == 0 {
        return Err(ConfigPersistenceError::Validation(
            "heartbeat_interval_seconds must be greater than 0".to_string(),
        ));
    }

    Ok(())
}

/// Decodes and strictly validates `PersistentConfig` from a JSON string.
pub fn decode_config_json(s: &str) -> Result<PersistentConfig, ConfigPersistenceError> {
    decode_config_json_bytes(s.as_bytes())
}

/// Decodes and strictly validates `PersistentConfig` from JSON bytes.
pub fn decode_config_json_bytes(bytes: &[u8]) -> Result<PersistentConfig, ConfigPersistenceError> {
    let wire: ConfigWireV1 = serde_json::from_slice(bytes)?;

    if wire.schema_version != CONFIG_SCHEMA_VERSION_V1 {
        return Err(ConfigPersistenceError::UnsupportedSchemaVersion(
            wire.schema_version,
        ));
    }

    let config = PersistentConfig {
        child_sid: wire.child_sid,
        telegram_allowed_user_ids: wire.telegram_allowed_user_ids,
        telegram_allowed_chat_ids: wire.telegram_allowed_chat_ids,
        heartbeat_interval_seconds: wire.heartbeat_interval_seconds,
    };

    validate_config(&config)?;
    Ok(config)
}

/// Validates and encodes `PersistentConfig` to a compact JSON string.
pub fn encode_config_json(config: &PersistentConfig) -> Result<String, ConfigPersistenceError> {
    validate_config(config)?;

    let wire = ConfigWireV1 {
        schema_version: CONFIG_SCHEMA_VERSION_V1,
        child_sid: config.child_sid.clone(),
        telegram_allowed_user_ids: config.telegram_allowed_user_ids.clone(),
        telegram_allowed_chat_ids: config.telegram_allowed_chat_ids.clone(),
        heartbeat_interval_seconds: config.heartbeat_interval_seconds,
    };

    serde_json::to_string(&wire).map_err(Into::into)
}

/// Validates and encodes `PersistentConfig` to a formatted pretty JSON string.
pub fn encode_config_json_pretty(
    config: &PersistentConfig,
) -> Result<String, ConfigPersistenceError> {
    validate_config(config)?;

    let wire = ConfigWireV1 {
        schema_version: CONFIG_SCHEMA_VERSION_V1,
        child_sid: config.child_sid.clone(),
        telegram_allowed_user_ids: config.telegram_allowed_user_ids.clone(),
        telegram_allowed_chat_ids: config.telegram_allowed_chat_ids.clone(),
        heartbeat_interval_seconds: config.heartbeat_interval_seconds,
    };

    serde_json::to_string_pretty(&wire).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_valid_config() -> PersistentConfig {
        PersistentConfig {
            child_sid: "S-1-5-21-1004336340-2796464520-3498877520-1001".to_string(),
            telegram_allowed_user_ids: vec![123456789, 987654321],
            telegram_allowed_chat_ids: vec![-1001234567890, 123456789],
            heartbeat_interval_seconds: 60,
        }
    }

    #[test]
    fn valid_config_round_trip() {
        let config = sample_valid_config();
        let encoded = encode_config_json_pretty(&config).expect("encoding should succeed");
        let decoded = decode_config_json(&encoded).expect("decoding should succeed");
        assert_eq!(config, decoded);
    }

    #[test]
    fn negative_telegram_chat_id_round_trip() {
        let mut config = sample_valid_config();
        config.telegram_allowed_chat_ids = vec![-1001987654321, -1, i64::MIN, i64::MAX];
        let encoded = encode_config_json(&config).unwrap();
        let decoded = decode_config_json(&encoded).unwrap();
        assert_eq!(config, decoded);
        assert_eq!(
            decoded.telegram_allowed_chat_ids,
            vec![-1001987654321, -1, i64::MIN, i64::MAX]
        );
    }

    #[test]
    fn empty_telegram_allowlists_are_valid() {
        let config = PersistentConfig {
            child_sid: "S-1-5-21-500".to_string(),
            telegram_allowed_user_ids: vec![],
            telegram_allowed_chat_ids: vec![],
            heartbeat_interval_seconds: 10,
        };
        let encoded = encode_config_json(&config).unwrap();
        let decoded = decode_config_json(&encoded).unwrap();
        assert_eq!(config, decoded);
    }

    #[test]
    fn unsupported_schema_version_rejected() {
        let json = r#"{
            "schema_version": 2,
            "child_sid": "S-1-5-21-500",
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 60
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::UnsupportedSchemaVersion(v) => assert_eq!(v, 2),
            other => panic!("expected UnsupportedSchemaVersion, got: {other:?}"),
        }
    }

    #[test]
    fn missing_schema_version_rejected() {
        let json = r#"{
            "child_sid": "S-1-5-21-500",
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 60
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(msg) => {
                assert!(msg.contains("missing field `schema_version`"))
            }
            other => panic!("expected Json missing field, got: {other:?}"),
        }
    }

    #[test]
    fn missing_child_sid_rejected() {
        let json = r#"{
            "schema_version": 1,
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 60
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(msg) => {
                assert!(msg.contains("missing field `child_sid`"))
            }
            other => panic!("expected Json missing field, got: {other:?}"),
        }
    }

    #[test]
    fn missing_user_allowlist_rejected() {
        let json = r#"{
            "schema_version": 1,
            "child_sid": "S-1-5-21-500",
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 60
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(msg) => {
                assert!(msg.contains("missing field `telegram_allowed_user_ids`"))
            }
            other => panic!("expected Json missing field, got: {other:?}"),
        }
    }

    #[test]
    fn missing_chat_allowlist_rejected() {
        let json = r#"{
            "schema_version": 1,
            "child_sid": "S-1-5-21-500",
            "telegram_allowed_user_ids": [123],
            "heartbeat_interval_seconds": 60
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(msg) => {
                assert!(msg.contains("missing field `telegram_allowed_chat_ids`"))
            }
            other => panic!("expected Json missing field, got: {other:?}"),
        }
    }

    #[test]
    fn missing_heartbeat_rejected() {
        let json = r#"{
            "schema_version": 1,
            "child_sid": "S-1-5-21-500",
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456]
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(msg) => {
                assert!(msg.contains("missing field `heartbeat_interval_seconds`"))
            }
            other => panic!("expected Json missing field, got: {other:?}"),
        }
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let json = r#"{
            "schema_version": 1,
            "child_sid": "S-1-5-21-500",
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 60,
            "unknown_extra": "rejected"
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(msg) => {
                assert!(msg.contains("unknown field `unknown_extra`"))
            }
            other => panic!("expected Json unknown field, got: {other:?}"),
        }
    }

    #[test]
    fn wrong_field_type_rejected() {
        let json = r#"{
            "schema_version": 1,
            "child_sid": 12345,
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 60
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(msg) => {
                assert!(msg.contains("invalid type: integer `12345`"))
            }
            other => panic!("expected Json invalid type, got: {other:?}"),
        }
    }

    #[test]
    fn malformed_json_rejected() {
        let json = "{ this is not json }";
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Json(_) => {}
            other => panic!("expected Json error, got: {other:?}"),
        }
    }

    #[test]
    fn invalid_utf8_rejected() {
        let bytes = [0xFF, 0xFE, 0x00, 0x01];
        let err = decode_config_json_bytes(&bytes).unwrap_err();
        match err {
            ConfigPersistenceError::Json(_) => {}
            other => panic!("expected Json error for invalid UTF-8 bytes, got: {other:?}"),
        }
    }

    #[test]
    fn empty_child_sid_rejected() {
        let json = r#"{
            "schema_version": 1,
            "child_sid": "",
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 60
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Validation(msg) => {
                assert!(msg.contains("child_sid cannot be empty"))
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn zero_heartbeat_rejected() {
        let json = r#"{
            "schema_version": 1,
            "child_sid": "S-1-5-21-500",
            "telegram_allowed_user_ids": [123],
            "telegram_allowed_chat_ids": [456],
            "heartbeat_interval_seconds": 0
        }"#;
        let err = decode_config_json(json).unwrap_err();
        match err {
            ConfigPersistenceError::Validation(msg) => {
                assert!(msg.contains("heartbeat_interval_seconds must be greater than 0"))
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_invalid_config() {
        let invalid_sid = PersistentConfig {
            child_sid: "".to_string(),
            telegram_allowed_user_ids: vec![],
            telegram_allowed_chat_ids: vec![],
            heartbeat_interval_seconds: 60,
        };
        assert!(encode_config_json(&invalid_sid).is_err());
        assert!(encode_config_json_pretty(&invalid_sid).is_err());

        let invalid_heartbeat = PersistentConfig {
            child_sid: "S-1-5-21-500".to_string(),
            telegram_allowed_user_ids: vec![],
            telegram_allowed_chat_ids: vec![],
            heartbeat_interval_seconds: 0,
        };
        assert!(encode_config_json(&invalid_heartbeat).is_err());
        assert!(encode_config_json_pretty(&invalid_heartbeat).is_err());
    }
}
