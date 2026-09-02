//! Persistence codec for service credentials (`credentials.json`).
//!
//! Stores already-protected credential materials: Argon2id PIN hash (PHC string)
//! and Windows DPAPI protected Telegram bot token blob.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Supported schema version for `credentials.json`.
pub const CREDENTIALS_SCHEMA_VERSION_V1: u32 = 1;

/// In-memory representation of persisted credentials.
///
/// Contains already-protected credential materials only.
/// Does not contain plaintext PIN or plaintext Telegram Bot token.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PersistentCredentials {
    /// Argon2id PHC string representing the hashed PIN.
    pub pin_hash: String,
    /// Raw binary blob of the Telegram bot token protected by Windows DPAPI.
    pub telegram_bot_token_dpapi: Vec<u8>,
}

impl PersistentCredentials {
    /// Validates domain invariants of in-memory credentials before serialization.
    pub fn validate(&self) -> Result<(), CredentialsPersistenceError> {
        validate_pin_hash(&self.pin_hash)?;
        if self.telegram_bot_token_dpapi.is_empty() {
            return Err(CredentialsPersistenceError::Validation(
                "telegram_bot_token_dpapi cannot be empty".to_string(),
            ));
        }
        Ok(())
    }
}

/// Errors occurring during credentials encoding, decoding, or schema validation.
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialsPersistenceError {
    /// JSON serialization or deserialization error.
    Json(String),
    /// Schema version is not supported.
    UnsupportedSchemaVersion(u32),
    /// Content or schema validation failed.
    Validation(String),
}

impl fmt::Display for CredentialsPersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(msg) => write!(f, "JSON error in credentials codec: {msg}"),
            Self::UnsupportedSchemaVersion(v) => {
                write!(
                    f,
                    "Unsupported credentials schema version: {v} (expected 1)"
                )
            }
            Self::Validation(msg) => write!(f, "Credentials validation error: {msg}"),
        }
    }
}

impl std::error::Error for CredentialsPersistenceError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsWireV1 {
    schema_version: u32,
    pin_hash: String,
    telegram_bot_token_dpapi: String,
}

fn validate_pin_hash(pin_hash: &str) -> Result<(), CredentialsPersistenceError> {
    if pin_hash.is_empty() {
        return Err(CredentialsPersistenceError::Validation(
            "pin_hash cannot be empty".to_string(),
        ));
    }

    // Must be in Argon2id PHC format starting with $argon2id$
    if !pin_hash.starts_with("$argon2id$") {
        return Err(CredentialsPersistenceError::Validation(
            "pin_hash must be a valid Argon2id PHC string starting with '$argon2id$'".to_string(),
        ));
    }

    // Split by '$': ["", "argon2id", ... params ..., salt, hash]
    let parts: Vec<&str> = pin_hash.split('$').collect();
    if parts.len() < 5 {
        return Err(CredentialsPersistenceError::Validation(
            "pin_hash is missing required PHC components (parameters, salt, or hash)".to_string(),
        ));
    }

    // Leading part before the first '$' must be empty
    if !parts[0].is_empty() || parts[1] != "argon2id" {
        return Err(CredentialsPersistenceError::Validation(
            "pin_hash has invalid PHC format".to_string(),
        ));
    }

    // All intermediate and trailing components must be non-empty
    for (idx, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            return Err(CredentialsPersistenceError::Validation(format!(
                "pin_hash PHC component at index {idx} is empty"
            )));
        }
    }

    Ok(())
}

fn decode_dpapi_base64(raw_b64: &str) -> Result<Vec<u8>, CredentialsPersistenceError> {
    if raw_b64.is_empty() {
        return Err(CredentialsPersistenceError::Validation(
            "telegram_bot_token_dpapi base64 string cannot be empty".to_string(),
        ));
    }

    // Reject any whitespace, newlines, carriage returns
    if raw_b64
        .chars()
        .any(|c| c.is_whitespace() || c == '\r' || c == '\n')
    {
        return Err(CredentialsPersistenceError::Validation(
            "telegram_bot_token_dpapi contains whitespace or newline characters".to_string(),
        ));
    }

    let decoded = BASE64_STANDARD.decode(raw_b64).map_err(|e| {
        CredentialsPersistenceError::Validation(format!(
            "invalid Base64 in telegram_bot_token_dpapi: {e}"
        ))
    })?;

    if decoded.is_empty() {
        return Err(CredentialsPersistenceError::Validation(
            "decoded telegram_bot_token_dpapi blob is empty".to_string(),
        ));
    }

    // Ensure canonical Base64 encoding by re-encoding and comparing
    let canonical_b64 = BASE64_STANDARD.encode(&decoded);
    if canonical_b64 != raw_b64 {
        return Err(CredentialsPersistenceError::Validation(
            "telegram_bot_token_dpapi is not canonically Base64-encoded".to_string(),
        ));
    }

    Ok(decoded)
}

/// Decodes credentials from a JSON string.
pub fn decode_credentials_json(
    json_str: &str,
) -> Result<PersistentCredentials, CredentialsPersistenceError> {
    let wire: CredentialsWireV1 = serde_json::from_str(json_str)
        .map_err(|e| CredentialsPersistenceError::Json(e.to_string()))?;

    if wire.schema_version != CREDENTIALS_SCHEMA_VERSION_V1 {
        return Err(CredentialsPersistenceError::UnsupportedSchemaVersion(
            wire.schema_version,
        ));
    }

    validate_pin_hash(&wire.pin_hash)?;
    let telegram_bot_token_dpapi = decode_dpapi_base64(&wire.telegram_bot_token_dpapi)?;

    let creds = PersistentCredentials {
        pin_hash: wire.pin_hash,
        telegram_bot_token_dpapi,
    };

    Ok(creds)
}

/// Decodes credentials from a UTF-8 JSON byte slice.
pub fn decode_credentials_json_bytes(
    bytes: &[u8],
) -> Result<PersistentCredentials, CredentialsPersistenceError> {
    let json_str = std::str::from_utf8(bytes)
        .map_err(|e| CredentialsPersistenceError::Json(format!("invalid UTF-8 sequence: {e}")))?;
    decode_credentials_json(json_str)
}

/// Encodes credentials to a compact JSON string.
pub fn encode_credentials_json(
    creds: &PersistentCredentials,
) -> Result<String, CredentialsPersistenceError> {
    creds.validate()?;
    let wire = CredentialsWireV1 {
        schema_version: CREDENTIALS_SCHEMA_VERSION_V1,
        pin_hash: creds.pin_hash.clone(),
        telegram_bot_token_dpapi: BASE64_STANDARD.encode(&creds.telegram_bot_token_dpapi),
    };
    serde_json::to_string(&wire).map_err(|e| CredentialsPersistenceError::Json(e.to_string()))
}

/// Encodes credentials to a pretty-printed JSON string.
pub fn encode_credentials_json_pretty(
    creds: &PersistentCredentials,
) -> Result<String, CredentialsPersistenceError> {
    creds.validate()?;
    let wire = CredentialsWireV1 {
        schema_version: CREDENTIALS_SCHEMA_VERSION_V1,
        pin_hash: creds.pin_hash.clone(),
        telegram_bot_token_dpapi: BASE64_STANDARD.encode(&creds.telegram_bot_token_dpapi),
    };
    serde_json::to_string_pretty(&wire)
        .map_err(|e| CredentialsPersistenceError::Json(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_VALID_PIN_HASH: &str =
        "$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHR2YWx1ZQ$c29tZWhhc2h2YWx1ZXNhbXBsZQ";
    const SAMPLE_DPAPI_BLOB: &[u8] = b"synthetic-dpapi-protected-token-blob-bytes-12345";

    fn sample_valid_credentials() -> PersistentCredentials {
        PersistentCredentials {
            pin_hash: SAMPLE_VALID_PIN_HASH.to_string(),
            telegram_bot_token_dpapi: SAMPLE_DPAPI_BLOB.to_vec(),
        }
    }

    #[test]
    fn valid_v1_round_trip() {
        let creds = sample_valid_credentials();
        let json = encode_credentials_json(&creds).expect("encode should succeed");
        let decoded = decode_credentials_json(&json).expect("decode should succeed");
        assert_eq!(creds, decoded);

        let pretty = encode_credentials_json_pretty(&creds).expect("pretty encode should succeed");
        let decoded_pretty =
            decode_credentials_json(&pretty).expect("decode pretty should succeed");
        assert_eq!(creds, decoded_pretty);
    }

    #[test]
    fn binary_non_utf8_dpapi_blob_round_trip() {
        let mut binary_blob = Vec::new();
        for b in 0..=255u8 {
            binary_blob.push(b);
        }
        binary_blob.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x01, 0x80, 0xC0]);

        let creds = PersistentCredentials {
            pin_hash: SAMPLE_VALID_PIN_HASH.to_string(),
            telegram_bot_token_dpapi: binary_blob.clone(),
        };

        let json = encode_credentials_json(&creds).expect("encode binary should succeed");
        let decoded = decode_credentials_json(&json).expect("decode binary should succeed");
        assert_eq!(decoded.telegram_bot_token_dpapi, binary_blob);
    }

    #[test]
    fn canonical_base64_output_contains_no_cr_lf() {
        let creds = sample_valid_credentials();
        let json = encode_credentials_json(&creds).expect("encode should succeed");
        let wire: serde_json::Value = serde_json::from_str(&json).unwrap();
        let b64_str = wire["telegram_bot_token_dpapi"].as_str().unwrap();

        assert!(!b64_str.contains('\r'), "Base64 must not contain CR");
        assert!(!b64_str.contains('\n'), "Base64 must not contain LF");
        assert!(!b64_str.contains(' '), "Base64 must not contain spaces");
    }

    #[test]
    fn malformed_base64_rejected() {
        let json = format!(
            r#"{{"schema_version":1,"pin_hash":"{}","telegram_bot_token_dpapi":"not-valid-base64!@#$"}}"#,
            SAMPLE_VALID_PIN_HASH
        );
        let err = decode_credentials_json(&json).unwrap_err();
        match err {
            CredentialsPersistenceError::Validation(msg) => {
                assert!(msg.contains("Base64") || msg.contains("invalid"));
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn base64_containing_whitespace_or_newline_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);
        let b64_with_space = format!("{} ", b64);
        let json_space = format!(
            r#"{{"schema_version":1,"pin_hash":"{}","telegram_bot_token_dpapi":"{}"}}"#,
            SAMPLE_VALID_PIN_HASH, b64_with_space
        );
        assert!(decode_credentials_json(&json_space).is_err());

        let b64_with_newline = format!("{}\n", b64);
        let json_newline = format!(
            r#"{{"schema_version":1,"pin_hash":"{}","telegram_bot_token_dpapi":"{}"}}"#,
            SAMPLE_VALID_PIN_HASH, b64_with_newline
        );
        assert!(decode_credentials_json(&json_newline).is_err());

        let b64_with_cr = format!("{}\r", b64);
        let json_cr = format!(
            r#"{{"schema_version":1,"pin_hash":"{}","telegram_bot_token_dpapi":"{}"}}"#,
            SAMPLE_VALID_PIN_HASH, b64_with_cr
        );
        assert!(decode_credentials_json(&json_cr).is_err());
    }

    #[test]
    fn empty_decoded_protected_blob_rejected() {
        let json = format!(
            r#"{{"schema_version":1,"pin_hash":"{}","telegram_bot_token_dpapi":""}}"#,
            SAMPLE_VALID_PIN_HASH
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Validation(_)));
    }

    #[test]
    fn missing_schema_version_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);
        let json = format!(
            r#"{{"pin_hash":"{}","telegram_bot_token_dpapi":"{}"}}"#,
            SAMPLE_VALID_PIN_HASH, b64
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Json(_)));
    }

    #[test]
    fn schema_version_not_one_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);
        let json = format!(
            r#"{{"schema_version":2,"pin_hash":"{}","telegram_bot_token_dpapi":"{}"}}"#,
            SAMPLE_VALID_PIN_HASH, b64
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert_eq!(
            err,
            CredentialsPersistenceError::UnsupportedSchemaVersion(2)
        );
    }

    #[test]
    fn missing_pin_hash_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);
        let json = format!(
            r#"{{"schema_version":1,"telegram_bot_token_dpapi":"{}"}}"#,
            b64
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Json(_)));
    }

    #[test]
    fn missing_telegram_bot_token_dpapi_rejected() {
        let json = format!(
            r#"{{"schema_version":1,"pin_hash":"{}"}}"#,
            SAMPLE_VALID_PIN_HASH
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Json(_)));
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);
        let json = format!(
            r#"{{"schema_version":1,"pin_hash":"{}","telegram_bot_token_dpapi":"{}","unknown_field":true}}"#,
            SAMPLE_VALID_PIN_HASH, b64
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Json(_)));
    }

    #[test]
    fn wrong_field_type_rejected() {
        let json = format!(
            r#"{{"schema_version":1,"pin_hash":12345,"telegram_bot_token_dpapi":"{}"}}"#,
            BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB)
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Json(_)));
    }

    #[test]
    fn malformed_json_rejected() {
        let json = r#"{"schema_version":1, "pin_hash": "#;
        let err = decode_credentials_json(json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Json(_)));
    }

    #[test]
    fn invalid_utf8_rejected() {
        let invalid_bytes = &[0xFF, 0xFE, 0xFD];
        let err = decode_credentials_json_bytes(invalid_bytes).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Json(_)));
    }

    #[test]
    fn empty_pin_hash_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);
        let json = format!(
            r#"{{"schema_version":1,"pin_hash":"","telegram_bot_token_dpapi":"{}"}}"#,
            b64
        );
        let err = decode_credentials_json(&json).unwrap_err();
        assert!(matches!(err, CredentialsPersistenceError::Validation(_)));
    }

    #[test]
    fn obvious_non_argon2id_plaintext_looking_pin_hash_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);

        let plaintext_pins = &["1234", "password", "my_secret_pin", "abcd1234efgh"];
        for pin in plaintext_pins {
            let json = format!(
                r#"{{"schema_version":1,"pin_hash":"{pin}","telegram_bot_token_dpapi":"{b64}"}}"#
            );
            let err = decode_credentials_json(&json).unwrap_err();
            assert!(
                matches!(err, CredentialsPersistenceError::Validation(_)),
                "Plaintext pin '{pin}' must be rejected"
            );
        }
    }

    #[test]
    fn malformed_incomplete_argon2id_phc_shape_rejected() {
        let b64 = BASE64_STANDARD.encode(SAMPLE_DPAPI_BLOB);

        let malformed_phcs = &[
            "$argon2id$",
            "$argon2i$v=19$m=65536,t=3,p=4$salt$hash", // wrong algorithm (argon2i)
            "$argon2d$v=19$m=65536,t=3,p=4$salt$hash", // wrong algorithm (argon2d)
            "$argon2id$v=19$m=65536",                  // missing salt and hash
            "$argon2id$v=19$m=65536$$hash",            // empty salt component
            "$argon2id$v=19$m=65536$salt$",            // empty hash component
            "argon2id$v=19$m=65536$salt$hash",         // missing leading $
        ];

        for phc in malformed_phcs {
            let json = format!(
                r#"{{"schema_version":1,"pin_hash":"{phc}","telegram_bot_token_dpapi":"{b64}"}}"#
            );
            let err = decode_credentials_json(&json).unwrap_err();
            assert!(
                matches!(err, CredentialsPersistenceError::Validation(_)),
                "Malformed PHC '{phc}' must be rejected"
            );
        }
    }

    #[test]
    fn encode_rejects_invalid_credentials() {
        let invalid_creds_empty_blob = PersistentCredentials {
            pin_hash: SAMPLE_VALID_PIN_HASH.to_string(),
            telegram_bot_token_dpapi: Vec::new(),
        };
        assert!(encode_credentials_json(&invalid_creds_empty_blob).is_err());

        let invalid_creds_bad_pin = PersistentCredentials {
            pin_hash: "plaintext-pin".to_string(),
            telegram_bot_token_dpapi: SAMPLE_DPAPI_BLOB.to_vec(),
        };
        assert!(encode_credentials_json(&invalid_creds_bad_pin).is_err());
    }
}
