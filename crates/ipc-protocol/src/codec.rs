//! Binary frame codec, length prefixing, resource bounds validation, and strict JSON parsing.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::HashSet;
use std::fmt;

use crate::constants::{MAX_EVENT_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION};
use crate::envelopes::{ErrorEnvelope, EventEnvelope, RequestEnvelope, ResponseEnvelope};
use crate::error::{ErrorCode, ProtocolError};
use crate::ids::WireTimerId;
use crate::request::{RedactedPin, RequestPayload};

#[cfg(test)]
thread_local! {
    pub(crate) static SCALAR_STRING_DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Serializes a 32-bit payload length as a 4-byte Little-Endian prefix.
pub fn encode_length_prefix(len: u32) -> [u8; 4] {
    len.to_le_bytes()
}

/// Parses a 4-byte Little-Endian prefix into a 32-bit payload length.
pub fn decode_length_prefix(bytes: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*bytes)
}

/// Pre-allocation guard for request frame length (docs/021 Section 11.3 & IPC-07..09).
///
/// MUST be evaluated BEFORE allocating memory buffer for the request body.
pub fn validate_request_frame_length(len: u32) -> Result<(), ProtocolError> {
    if len == 0 {
        return Err(ProtocolError::protocol_error(
            "Zero-length frame is forbidden by protocol",
        ));
    }
    if len > MAX_REQUEST_BYTES as u32 {
        return Err(ProtocolError::frame_too_large(format!(
            "Declared request frame length {} exceeds maximum limit of {} bytes",
            len, MAX_REQUEST_BYTES
        )));
    }
    Ok(())
}

/// Validates response frame length, strictly forbidding truncation (IPC-10, IPC-11).
pub fn validate_response_frame_length(len: usize) -> Result<(), ProtocolError> {
    if len > MAX_RESPONSE_BYTES {
        return Err(ProtocolError::response_too_large(format!(
            "Serialized response length {} exceeds maximum limit of {} bytes",
            len, MAX_RESPONSE_BYTES
        )));
    }
    Ok(())
}

/// Validates event frame length, strictly forbidding truncation (IPC-12, IPC-13).
pub fn validate_event_frame_length(len: usize) -> Result<(), ProtocolError> {
    if len > MAX_EVENT_BYTES {
        return Err(ProtocolError::frame_too_large(format!(
            "Serialized event length {} exceeds maximum limit of {} bytes",
            len, MAX_EVENT_BYTES
        )));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Typed Request Parse Failure
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestParseFailure {
    Protocol(String),
    Invalid(String),
    Malformed(String),
}

impl fmt::Display for RequestParseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(msg) => write!(f, "Protocol error: {}", msg),
            Self::Invalid(msg) => write!(f, "Invalid request: {}", msg),
            Self::Malformed(msg) => write!(f, "Malformed frame: {}", msg),
        }
    }
}

impl std::error::Error for RequestParseFailure {}

impl From<RequestParseFailure> for ProtocolError {
    fn from(failure: RequestParseFailure) -> Self {
        match failure {
            RequestParseFailure::Protocol(msg) => ProtocolError::protocol_error(msg),
            RequestParseFailure::Invalid(msg) => ProtocolError::invalid_request(msg),
            RequestParseFailure::Malformed(msg) => ProtocolError::malformed_frame(msg),
        }
    }
}

// -----------------------------------------------------------------------------
// Recursive RawValue Structural Duplicate-Key Checker (IPC-86)
// -----------------------------------------------------------------------------

enum RawStructuralNode<'a> {
    Object(Vec<(String, &'a serde_json::value::RawValue)>),
    Array(Vec<&'a serde_json::value::RawValue>),
    Leaf,
}

struct RawStructuralVisitor<'a> {
    duplicate_detected: &'a std::cell::Cell<bool>,
}

impl<'de, 'a> Visitor<'de> for RawStructuralVisitor<'a> {
    type Value = RawStructuralNode<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E> {
        Ok(RawStructuralNode::Leaf)
    }

    fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E> {
        Ok(RawStructuralNode::Leaf)
    }

    fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E> {
        Ok(RawStructuralNode::Leaf)
    }

    fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E> {
        Ok(RawStructuralNode::Leaf)
    }

    fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E> {
        #[cfg(test)]
        SCALAR_STRING_DECODE_COUNT.with(|c| c.set(c.get() + 1));
        Ok(RawStructuralNode::Leaf)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(RawStructuralNode::Leaf)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(RawStructuralNode::Leaf)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut elements = Vec::new();
        while let Some(elem) = seq.next_element::<&'de serde_json::value::RawValue>()? {
            elements.push(elem);
        }
        Ok(RawStructuralNode::Array(elements))
    }

    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        let mut keys_seen = HashSet::new();

        while let Some(key) = access.next_key::<String>()? {
            if !keys_seen.insert(key.clone()) {
                self.duplicate_detected.set(true);
                return Err(de::Error::custom("duplicate key"));
            }
            let val: &'de serde_json::value::RawValue = access.next_value()?;
            entries.push((key, val));
        }
        Ok(RawStructuralNode::Object(entries))
    }
}

struct RawStructuralSeed<'a> {
    duplicate_detected: &'a std::cell::Cell<bool>,
}

impl<'de, 'a> de::DeserializeSeed<'de> for RawStructuralSeed<'a> {
    type Value = RawStructuralNode<'de>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(RawStructuralVisitor {
            duplicate_detected: self.duplicate_detected,
        })
    }
}

fn check_raw_structural(raw_json: &str) -> Result<(), RequestParseFailure> {
    let duplicate_detected = std::cell::Cell::new(false);
    let seed = RawStructuralSeed {
        duplicate_detected: &duplicate_detected,
    };

    let mut de = serde_json::Deserializer::from_str(raw_json);
    let node = seed.deserialize(&mut de).map_err(|e| {
        if duplicate_detected.get() {
            RequestParseFailure::Protocol("Duplicate key in JSON object".into())
        } else {
            RequestParseFailure::Malformed(format!("Malformed JSON: {}", e))
        }
    })?;

    de.end()
        .map_err(|e| RequestParseFailure::Malformed(format!("Malformed JSON: {}", e)))?;

    match node {
        RawStructuralNode::Object(entries) => {
            for (_key, val) in entries {
                let trimmed = val.get().trim();
                if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    check_raw_structural(val.get())?;
                }
            }
        }
        RawStructuralNode::Array(elements) => {
            for elem in elements {
                let trimmed = elem.get().trim();
                if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    check_raw_structural(elem.get())?;
                }
            }
        }
        RawStructuralNode::Leaf => {}
    }
    Ok(())
}

/// Inspects a UTF-8 JSON payload to strictly reject any duplicate object keys (IPC-86).
pub fn check_no_duplicate_keys(json_str: &str) -> Result<(), ProtocolError> {
    check_raw_structural(json_str).map_err(ProtocolError::from)
}

// -----------------------------------------------------------------------------
// Secret-Safe Custom PIN Unescaper
// -----------------------------------------------------------------------------

pub(crate) fn unescape_pin_json_string(raw: &str) -> Result<RedactedPin, RequestParseFailure> {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return Err(RequestParseFailure::Malformed(
            "PIN field must be a JSON string".into(),
        ));
    }

    let inner = &trimmed[1..trimmed.len() - 1];
    let mut pin = RedactedPin::new(String::with_capacity(inner.len()));
    let mut chars = inner.chars().peekable();

    while let Some(ch) = chars.next() {
        if (ch as u32) < 0x20 {
            return Err(RequestParseFailure::Malformed(
                "Unescaped control character in PIN string".into(),
            ));
        }
        if ch == '\\' {
            let esc = chars.next().ok_or_else(|| {
                RequestParseFailure::Malformed("Truncated escape sequence in PIN string".into())
            })?;
            match esc {
                '"' => pin.0.push('"'),
                '\\' => pin.0.push('\\'),
                '/' => pin.0.push('/'),
                'b' => pin.0.push('\x08'),
                'f' => pin.0.push('\x0C'),
                'n' => pin.0.push('\n'),
                'r' => pin.0.push('\r'),
                't' => pin.0.push('\t'),
                'u' => {
                    let u1 = parse_hex4(&mut chars)?;
                    if (0xD800..=0xDBFF).contains(&u1) {
                        // High surrogate, expect \uDC00..=\uDFFF next
                        if chars.next() != Some('\\') || chars.next() != Some('u') {
                            return Err(RequestParseFailure::Malformed(
                                "Expected low surrogate escape sequence after high surrogate"
                                    .into(),
                            ));
                        }
                        let u2 = parse_hex4(&mut chars)?;
                        if !(0xDC00..=0xDFFF).contains(&u2) {
                            return Err(RequestParseFailure::Malformed(
                                "Invalid low surrogate in unicode escape".into(),
                            ));
                        }
                        let code_point =
                            0x10000 + (((u1 as u32 - 0xD800) << 10) | (u2 as u32 - 0xDC00));
                        let decoded_char = char::from_u32(code_point).ok_or_else(|| {
                            RequestParseFailure::Malformed(
                                "Invalid unicode code point in surrogate pair".into(),
                            )
                        })?;
                        pin.0.push(decoded_char);
                    } else if (0xDC00..=0xDFFF).contains(&u1) {
                        return Err(RequestParseFailure::Malformed(
                            "Isolated low surrogate in unicode escape".into(),
                        ));
                    } else {
                        let decoded_char = char::from_u32(u1 as u32).ok_or_else(|| {
                            RequestParseFailure::Malformed(
                                "Invalid unicode code point in escape".into(),
                            )
                        })?;
                        pin.0.push(decoded_char);
                    }
                }
                _ => {
                    return Err(RequestParseFailure::Malformed(
                        "Invalid escape character in PIN string".into(),
                    ));
                }
            }
        } else {
            pin.0.push(ch);
        }
    }

    Ok(pin)
}

fn parse_hex4<I: Iterator<Item = char>>(chars: &mut I) -> Result<u16, RequestParseFailure> {
    let mut val: u16 = 0;
    for _ in 0..4 {
        let ch = chars.next().ok_or_else(|| {
            RequestParseFailure::Malformed("Truncated \\u unicode escape sequence".into())
        })?;
        let digit = ch.to_digit(16).ok_or_else(|| {
            RequestParseFailure::Malformed("Invalid hex digit in \\u unicode escape".into())
        })?;
        val = (val << 4) | (digit as u16);
    }
    Ok(val)
}

// -----------------------------------------------------------------------------
// Authoritative Raw Request Parser
// -----------------------------------------------------------------------------

struct RawObjectMapVisitor<'a> {
    duplicate_detected: &'a std::cell::Cell<bool>,
}

impl<'de, 'a> Visitor<'de> for RawObjectMapVisitor<'a> {
    type Value = Vec<(String, &'de serde_json::value::RawValue)>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        let mut keys_seen = HashSet::new();
        while let Some(key) = access.next_key::<String>()? {
            if !keys_seen.insert(key.clone()) {
                self.duplicate_detected.set(true);
                return Err(de::Error::custom("duplicate key"));
            }
            let val: &'de serde_json::value::RawValue = access.next_value()?;
            entries.push((key, val));
        }
        Ok(entries)
    }
}

struct RawObjectSeed<'a> {
    duplicate_detected: &'a std::cell::Cell<bool>,
}

impl<'de, 'a> de::DeserializeSeed<'de> for RawObjectSeed<'a> {
    type Value = Vec<(String, &'de serde_json::value::RawValue)>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RawObjectMapVisitor {
            duplicate_detected: self.duplicate_detected,
        })
    }
}

fn parse_raw_object_entries<'a>(
    json_str: &'a str,
) -> Result<Vec<(String, &'a serde_json::value::RawValue)>, RequestParseFailure> {
    let trimmed = json_str.trim();
    if !trimmed.starts_with('{') {
        return Err(RequestParseFailure::Protocol(
            "Expected a JSON object".into(),
        ));
    }

    let duplicate_detected = std::cell::Cell::new(false);
    let seed = RawObjectSeed {
        duplicate_detected: &duplicate_detected,
    };

    let mut de = serde_json::Deserializer::from_str(json_str);
    let entries = seed.deserialize(&mut de).map_err(|e| {
        if duplicate_detected.get() {
            RequestParseFailure::Protocol("Duplicate key in JSON object".into())
        } else {
            RequestParseFailure::Malformed(format!("Malformed JSON: {}", e))
        }
    })?;

    de.end()
        .map_err(|e| RequestParseFailure::Malformed(format!("Malformed JSON: {}", e)))?;

    Ok(entries)
}

pub(crate) fn parse_request_payload_raw(
    raw_request: &serde_json::value::RawValue,
) -> Result<RequestPayload, RequestParseFailure> {
    let entries = parse_raw_object_entries(raw_request.get())?;

    let kind_raw = entries
        .iter()
        .find(|(k, _)| k == "kind")
        .map(|(_, v)| *v)
        .ok_or_else(|| {
            RequestParseFailure::Invalid("Missing required 'kind' field in request".into())
        })?;

    let kind_str: String = match serde_json::from_str(kind_raw.get()) {
        Ok(s) => s,
        Err(_) => {
            return Err(RequestParseFailure::Invalid(
                "Field 'kind' must be a valid JSON string".into(),
            ));
        }
    };

    let allowed_fields: &[&str] = match kind_str.as_str() {
        "QueryStatus" => &["kind"],
        "VerifyPin" => &["kind", "pin"],
        "SubscribeEvents" => &["kind"],
        "SendChildMessage" => &["kind", "text"],
        "ScheduleInternetBlock" => &["kind", "duration_minutes"],
        "ImmediateInternetBlock" => &["kind"],
        "CancelInternetBlockTimer" => &["kind", "timer_id"],
        "RestoreInternet" => &["kind"],
        "ScheduleShutdown" => &["kind", "duration_minutes"],
        "CancelShutdownTimer" => &["kind", "timer_id"],
        _ => {
            return Err(RequestParseFailure::Invalid(format!(
                "Unknown request kind: {}",
                kind_str
            )));
        }
    };

    for (key, _) in &entries {
        if !allowed_fields.contains(&key.as_str()) {
            return Err(RequestParseFailure::Protocol(format!(
                "Unknown field '{}' inside request of kind '{}'",
                key, kind_str
            )));
        }
    }

    match kind_str.as_str() {
        "QueryStatus" => Ok(RequestPayload::QueryStatus),
        "VerifyPin" => {
            let pin_raw = entries
                .iter()
                .find(|(k, _)| k == "pin")
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    RequestParseFailure::Invalid(
                        "Missing required field 'pin' in VerifyPin request".into(),
                    )
                })?;
            let pin = unescape_pin_json_string(pin_raw.get())?;
            Ok(RequestPayload::VerifyPin { pin })
        }
        "SubscribeEvents" => Ok(RequestPayload::SubscribeEvents),
        "SendChildMessage" => {
            let text_raw = entries
                .iter()
                .find(|(k, _)| k == "text")
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    RequestParseFailure::Invalid(
                        "Missing required field 'text' in SendChildMessage request".into(),
                    )
                })?;
            let text: String = serde_json::from_str(text_raw.get()).map_err(|_| {
                RequestParseFailure::Invalid("Field 'text' must be a valid JSON string".into())
            })?;
            Ok(RequestPayload::SendChildMessage { text })
        }
        "ScheduleInternetBlock" => {
            let dur_raw = entries
                .iter()
                .find(|(k, _)| k == "duration_minutes")
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    RequestParseFailure::Invalid(
                        "Missing required field 'duration_minutes' in ScheduleInternetBlock request"
                            .into(),
                    )
                })?;
            let duration_minutes = parse_duration_minutes_raw(dur_raw.get())?;
            Ok(RequestPayload::ScheduleInternetBlock { duration_minutes })
        }
        "ImmediateInternetBlock" => Ok(RequestPayload::ImmediateInternetBlock),
        "CancelInternetBlockTimer" => {
            let tid_raw = entries
                .iter()
                .find(|(k, _)| k == "timer_id")
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    RequestParseFailure::Invalid(
                        "Missing required field 'timer_id' in CancelInternetBlockTimer request"
                            .into(),
                    )
                })?;
            let timer_id: WireTimerId = serde_json::from_str(tid_raw.get()).map_err(|_| {
                RequestParseFailure::Invalid("Invalid timer_id field in request".into())
            })?;
            Ok(RequestPayload::CancelInternetBlockTimer { timer_id })
        }
        "RestoreInternet" => Ok(RequestPayload::RestoreInternet),
        "ScheduleShutdown" => {
            let dur_raw = entries
                .iter()
                .find(|(k, _)| k == "duration_minutes")
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    RequestParseFailure::Invalid(
                        "Missing required field 'duration_minutes' in ScheduleShutdown request"
                            .into(),
                    )
                })?;
            let duration_minutes = parse_duration_minutes_raw(dur_raw.get())?;
            Ok(RequestPayload::ScheduleShutdown { duration_minutes })
        }
        "CancelShutdownTimer" => {
            let tid_raw = entries
                .iter()
                .find(|(k, _)| k == "timer_id")
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    RequestParseFailure::Invalid(
                        "Missing required field 'timer_id' in CancelShutdownTimer request".into(),
                    )
                })?;
            let timer_id: WireTimerId = serde_json::from_str(tid_raw.get()).map_err(|_| {
                RequestParseFailure::Invalid("Invalid timer_id field in request".into())
            })?;
            Ok(RequestPayload::CancelShutdownTimer { timer_id })
        }
        _ => unreachable!(),
    }
}

fn parse_duration_minutes_raw(s: &str) -> Result<u32, RequestParseFailure> {
    let trimmed = s.trim();
    if let Ok(u) = trimmed.parse::<u64>() {
        if u > u32::MAX as u64 {
            return Err(RequestParseFailure::Invalid(
                "Duration minutes exceeds u32 limit".into(),
            ));
        }
        Ok(u as u32)
    } else {
        Err(RequestParseFailure::Invalid(
            "Field 'duration_minutes' must be an integer u32".into(),
        ))
    }
}

// -----------------------------------------------------------------------------
// Version and Frame Validation Helpers
// -----------------------------------------------------------------------------

fn validate_raw_version_value(ver_str: &str, envelope_name: &str) -> Result<u32, ProtocolError> {
    let trimmed = ver_str.trim();
    if let Ok(u) = trimmed.parse::<u64>() {
        if u == (PROTOCOL_VERSION as u64) {
            Ok(PROTOCOL_VERSION)
        } else {
            Err(ProtocolError::new(
                ErrorCode::UnsupportedProtocolVersion,
                Some(format!(
                    "Protocol version {} is unsupported, expected {}",
                    u, PROTOCOL_VERSION
                )),
            ))
        }
    } else if let Ok(i) = trimmed.parse::<i64>() {
        Err(ProtocolError::new(
            ErrorCode::UnsupportedProtocolVersion,
            Some(format!(
                "Protocol version {} is unsupported, expected {}",
                i, PROTOCOL_VERSION
            )),
        ))
    } else if trimmed.parse::<f64>().is_ok() {
        Err(ProtocolError::protocol_error(format!(
            "{} 'version' field must be an integer, received floating point number",
            envelope_name
        )))
    } else {
        Err(ProtocolError::protocol_error(format!(
            "{} 'version' field must be an integer, received non-numeric type",
            envelope_name
        )))
    }
}

/// Validates the protocol version from a raw JSON map without narrowing conversions (IPC-03..05).
pub fn validate_raw_version_field(
    raw_obj: &serde_json::Map<String, serde_json::Value>,
    envelope_name: &str,
) -> Result<u32, ProtocolError> {
    let ver_val = raw_obj.get("version").ok_or_else(|| {
        ProtocolError::protocol_error(format!(
            "{} is missing required 'version' field",
            envelope_name
        ))
    })?;

    if let Some(num) = ver_val.as_number() {
        if let Some(u) = num.as_u64() {
            if u == (PROTOCOL_VERSION as u64) {
                Ok(PROTOCOL_VERSION)
            } else {
                Err(ProtocolError::new(
                    ErrorCode::UnsupportedProtocolVersion,
                    Some(format!(
                        "Protocol version {} is unsupported, expected {}",
                        u, PROTOCOL_VERSION
                    )),
                ))
            }
        } else if let Some(i) = num.as_i64() {
            Err(ProtocolError::new(
                ErrorCode::UnsupportedProtocolVersion,
                Some(format!(
                    "Protocol version {} is unsupported, expected {}",
                    i, PROTOCOL_VERSION
                )),
            ))
        } else {
            Err(ProtocolError::protocol_error(format!(
                "{} 'version' field must be an integer, received floating point number",
                envelope_name
            )))
        }
    } else {
        Err(ProtocolError::protocol_error(format!(
            "{} 'version' field must be an integer, received non-numeric type",
            envelope_name
        )))
    }
}

/// Checks the protocol version field in a deserialized envelope (IPC-03..05).
pub fn validate_version(version: u32) -> Result<(), ProtocolError> {
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::new(
            ErrorCode::UnsupportedProtocolVersion,
            Some(format!(
                "Protocol version {} is unsupported, expected {}",
                version, PROTOCOL_VERSION
            )),
        ));
    }
    Ok(())
}

/// Deserializes an Option<T> field that MUST be explicitly present in the JSON object (as null or value).
pub fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Validates that the payload received matches the declared frame length (IPC-15).
pub fn validate_frame_completeness(
    declared_len: u32,
    actual_len: usize,
) -> Result<(), ProtocolError> {
    if (actual_len as u64) < (declared_len as u64) {
        return Err(ProtocolError::transport_failure(format!(
            "Incomplete frame: declared payload length was {} bytes, but only {} bytes received before stream ended",
            declared_len, actual_len
        )));
    }
    Ok(())
}

/// Pure transport-independent frame extraction primitive (IPC-06, IPC-07..09, IPC-15).
pub fn extract_frame_payload<'a>(
    buffer: &'a [u8],
    is_eof: bool,
) -> Result<Option<(&'a [u8], usize)>, ProtocolError> {
    if buffer.len() < 4 {
        if is_eof && !buffer.is_empty() {
            return Err(ProtocolError::transport_failure(
                "Premature stream disconnection: incomplete 4-byte length prefix before EOF",
            ));
        }
        return Ok(None);
    }

    let mut prefix = [0u8; 4];
    prefix.copy_from_slice(&buffer[..4]);
    let declared_len = decode_length_prefix(&prefix);
    validate_request_frame_length(declared_len)?;

    let total_needed = 4 + declared_len as usize;
    if buffer.len() < total_needed {
        if is_eof {
            return Err(ProtocolError::transport_failure(format!(
                "Premature stream disconnection: declared {} payload bytes, but stream terminated after {} payload bytes",
                declared_len,
                buffer.len() - 4
            )));
        }
        return Ok(None);
    }

    Ok(Some((&buffer[4..total_needed], total_needed)))
}

// -----------------------------------------------------------------------------
// Envelope encoding and decoding
// -----------------------------------------------------------------------------

/// Encodes a request envelope into framed bytes: [4 bytes length prefix] [UTF-8 JSON].
pub fn encode_request_frame(req: &RequestEnvelope) -> Result<Vec<u8>, ProtocolError> {
    req.request.validate()?;
    let mut json_bytes = zeroize::Zeroizing::new(Vec::<u8>::new());
    serde_json::to_writer(&mut *json_bytes, req).map_err(|e| {
        ProtocolError::internal_error(format!("Failed to serialize request: {}", e))
    })?;
    validate_request_frame_length(json_bytes.len() as u32)?;
    let mut frame = Vec::with_capacity(4 + json_bytes.len());
    frame.extend_from_slice(&encode_length_prefix(json_bytes.len() as u32));
    frame.extend_from_slice(&json_bytes);
    Ok(frame)
}

/// Decodes an unframed request payload (raw UTF-8 JSON bytes).
pub fn decode_request_frame(payload: &[u8]) -> Result<RequestEnvelope, ProtocolError> {
    validate_request_frame_length(payload.len() as u32)?;
    let json_str = std::str::from_utf8(payload).map_err(|e| {
        ProtocolError::malformed_frame(format!("Request payload is not valid UTF-8: {}", e))
    })?;

    // Check for duplicate JSON keys (IPC-86)
    check_no_duplicate_keys(json_str)?;

    let top_entries = parse_raw_object_entries(json_str)?;

    // Check top-level allowed fields
    for (key, _) in &top_entries {
        if key != "version" && key != "type" && key != "request" {
            return Err(ProtocolError::protocol_error(format!(
                "Unknown top-level field '{}' in request envelope",
                key
            )));
        }
    }

    // Validate version
    let ver_raw = top_entries
        .iter()
        .find(|(k, _)| k == "version")
        .map(|(_, v)| *v)
        .ok_or_else(|| {
            ProtocolError::protocol_error("Request envelope is missing required 'version' field")
        })?;

    validate_raw_version_value(ver_raw.get(), "Request envelope")?;

    // Validate type (Escaped JSON type compatibility)
    let type_raw = top_entries
        .iter()
        .find(|(k, _)| k == "type")
        .map(|(_, v)| *v)
        .ok_or_else(|| {
            ProtocolError::protocol_error("Request envelope is missing required 'type' field")
        })?;

    let type_str: String = match serde_json::from_str(type_raw.get()) {
        Ok(s) => s,
        Err(_) => {
            return Err(ProtocolError::protocol_error(format!(
                "Invalid envelope type '{}', expected 'request'",
                type_raw.get().trim()
            )));
        }
    };

    if type_str != "request" {
        return Err(ProtocolError::protocol_error(format!(
            "Invalid envelope type '{:?}', expected 'request'",
            type_str
        )));
    }

    // Validate request field
    let req_raw = top_entries
        .iter()
        .find(|(k, _)| k == "request")
        .map(|(_, v)| *v)
        .ok_or_else(|| {
            ProtocolError::protocol_error("Request envelope is missing required 'request' field")
        })?;

    if !req_raw.get().trim().starts_with('{') {
        return Err(ProtocolError::protocol_error(
            "Field 'request' must be a JSON object",
        ));
    }

    let request_payload = parse_request_payload_raw(req_raw)?;
    let req = RequestEnvelope::new(request_payload);

    req.request.validate()?;
    Ok(req)
}

/// Encodes a response envelope into framed bytes: [4 bytes length prefix] [UTF-8 JSON].
pub fn encode_response_frame(resp: &ResponseEnvelope) -> Result<Vec<u8>, ProtocolError> {
    let json_bytes = serde_json::to_vec(resp).map_err(|e| {
        ProtocolError::internal_error(format!("Failed to serialize response: {}", e))
    })?;
    validate_response_frame_length(json_bytes.len())?;
    let mut frame = Vec::with_capacity(4 + json_bytes.len());
    frame.extend_from_slice(&encode_length_prefix(json_bytes.len() as u32));
    frame.extend_from_slice(&json_bytes);
    Ok(frame)
}

/// Decodes an unframed response payload (raw UTF-8 JSON bytes).
pub fn decode_response_frame(payload: &[u8]) -> Result<ResponseEnvelope, ProtocolError> {
    validate_response_frame_length(payload.len())?;
    let json_str = std::str::from_utf8(payload).map_err(|e| {
        ProtocolError::malformed_frame(format!("Response payload is not valid UTF-8: {}", e))
    })?;

    check_no_duplicate_keys(json_str)?;

    let raw_val: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| ProtocolError::malformed_frame(format!("Malformed response JSON: {}", e)))?;

    let raw_obj = raw_val
        .as_object()
        .ok_or_else(|| ProtocolError::protocol_error("Response envelope must be a JSON object"))?;

    for key in raw_obj.keys() {
        if key != "version" && key != "type" && key != "response" {
            return Err(ProtocolError::protocol_error(format!(
                "Unknown top-level field '{}' in response envelope",
                key
            )));
        }
    }

    validate_raw_version_field(raw_obj, "Response envelope")?;

    match raw_obj.get("type") {
        None => {
            return Err(ProtocolError::protocol_error(
                "Response envelope is missing required 'type' field",
            ));
        }
        Some(v) => {
            if v.as_str() != Some("response") {
                return Err(ProtocolError::protocol_error(format!(
                    "Invalid envelope type '{:?}', expected 'response'",
                    v
                )));
            }
        }
    }

    let resp: ResponseEnvelope = serde_json::from_str(json_str).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("unknown field") {
            ProtocolError::protocol_error(format!("Unknown field in response envelope: {}", msg))
        } else if msg.contains("missing field") {
            ProtocolError::protocol_error(format!("Missing required field in response: {}", msg))
        } else {
            ProtocolError::malformed_frame(format!("Malformed response: {}", msg))
        }
    })?;

    Ok(resp)
}

/// Encodes an error envelope into framed bytes: [4 bytes length prefix] [UTF-8 JSON].
pub fn encode_error_frame(err: &ErrorEnvelope) -> Result<Vec<u8>, ProtocolError> {
    let json_bytes = serde_json::to_vec(err)
        .map_err(|e| ProtocolError::internal_error(format!("Failed to serialize error: {}", e)))?;
    validate_response_frame_length(json_bytes.len())?;
    let mut frame = Vec::with_capacity(4 + json_bytes.len());
    frame.extend_from_slice(&encode_length_prefix(json_bytes.len() as u32));
    frame.extend_from_slice(&json_bytes);
    Ok(frame)
}

/// Decodes an unframed error payload (raw UTF-8 JSON bytes).
pub fn decode_error_frame(payload: &[u8]) -> Result<ErrorEnvelope, ProtocolError> {
    validate_response_frame_length(payload.len())?;
    let json_str = std::str::from_utf8(payload).map_err(|e| {
        ProtocolError::malformed_frame(format!("Error payload is not valid UTF-8: {}", e))
    })?;

    check_no_duplicate_keys(json_str)?;

    let raw_val: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| ProtocolError::malformed_frame(format!("Malformed error JSON: {}", e)))?;

    let raw_obj = raw_val
        .as_object()
        .ok_or_else(|| ProtocolError::protocol_error("Error envelope must be a JSON object"))?;

    for key in raw_obj.keys() {
        if key != "version" && key != "type" && key != "error" {
            return Err(ProtocolError::protocol_error(format!(
                "Unknown top-level field '{}' in error envelope",
                key
            )));
        }
    }

    validate_raw_version_field(raw_obj, "Error envelope")?;

    match raw_obj.get("type") {
        None => {
            return Err(ProtocolError::protocol_error(
                "Error envelope is missing required 'type' field",
            ));
        }
        Some(v) => {
            if v.as_str() != Some("error") {
                return Err(ProtocolError::protocol_error(format!(
                    "Invalid envelope type '{:?}', expected 'error'",
                    v
                )));
            }
        }
    }

    let err: ErrorEnvelope = serde_json::from_str(json_str).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("unknown field") {
            ProtocolError::protocol_error(format!("Unknown field in error envelope: {}", msg))
        } else if msg.contains("missing field") {
            ProtocolError::protocol_error(format!(
                "Missing required field in error envelope: {}",
                msg
            ))
        } else {
            ProtocolError::malformed_frame(format!("Malformed error: {}", msg))
        }
    })?;

    Ok(err)
}

/// Encodes an event envelope into framed bytes: [4 bytes length prefix] [UTF-8 JSON].
pub fn encode_event_frame(ev: &EventEnvelope) -> Result<Vec<u8>, ProtocolError> {
    let json_bytes = serde_json::to_vec(ev)
        .map_err(|e| ProtocolError::internal_error(format!("Failed to serialize event: {}", e)))?;
    validate_event_frame_length(json_bytes.len())?;
    let mut frame = Vec::with_capacity(4 + json_bytes.len());
    frame.extend_from_slice(&encode_length_prefix(json_bytes.len() as u32));
    frame.extend_from_slice(&json_bytes);
    Ok(frame)
}

/// Decodes an unframed event payload (raw UTF-8 JSON bytes).
pub fn decode_event_frame(payload: &[u8]) -> Result<EventEnvelope, ProtocolError> {
    validate_event_frame_length(payload.len())?;
    let json_str = std::str::from_utf8(payload).map_err(|e| {
        ProtocolError::malformed_frame(format!("Event payload is not valid UTF-8: {}", e))
    })?;

    check_no_duplicate_keys(json_str)?;

    let raw_val: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| ProtocolError::malformed_frame(format!("Malformed event JSON: {}", e)))?;

    let raw_obj = raw_val
        .as_object()
        .ok_or_else(|| ProtocolError::protocol_error("Event envelope must be a JSON object"))?;

    for key in raw_obj.keys() {
        if key != "version" && key != "type" && key != "event" {
            return Err(ProtocolError::protocol_error(format!(
                "Unknown top-level field '{}' in event envelope",
                key
            )));
        }
    }

    validate_raw_version_field(raw_obj, "Event envelope")?;

    match raw_obj.get("type") {
        None => {
            return Err(ProtocolError::protocol_error(
                "Event envelope is missing required 'type' field",
            ));
        }
        Some(v) => {
            if v.as_str() != Some("event") {
                return Err(ProtocolError::protocol_error(format!(
                    "Invalid envelope type '{:?}', expected 'event'",
                    v
                )));
            }
        }
    }

    let ev: EventEnvelope = serde_json::from_str(json_str).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("unknown field") {
            ProtocolError::protocol_error(format!("Unknown field in event envelope: {}", msg))
        } else if msg.contains("missing field") {
            ProtocolError::protocol_error(format!("Missing required field in event: {}", msg))
        } else {
            ProtocolError::malformed_frame(format!("Malformed event: {}", msg))
        }
    })?;

    Ok(ev)
}

impl ProtocolError {
    fn internal_error(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, Some(msg.into()))
    }
}

// =============================================================================
// Security Tests (Slice 1 PIN Secret Zeroization Security Correction)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{RedactedPin, RequestPayload};

    #[test]
    fn test_redacted_pin_drop_zeroization_structural() {
        crate::request::REDACTED_PIN_DROP_COUNT.with(|c| c.set(0));
        let pin = RedactedPin::new("secret_drop_test");
        assert_eq!(pin.as_str(), "secret_drop_test");
        drop(pin);
        assert_eq!(crate::request::REDACTED_PIN_DROP_COUNT.with(|c| c.get()), 1);
    }

    #[test]
    fn test_redacted_pin_into_sensitive_move_no_clone() {
        let pin = RedactedPin::new("moving_secret_no_clone");
        let ptr_before = pin.as_str().as_ptr();
        let sensitive = pin.into_sensitive();
        let ptr_after = sensitive.as_str().as_ptr();
        assert_eq!(ptr_before, ptr_after);
        assert_eq!(sensitive.as_str(), "moving_secret_no_clone");
    }

    #[test]
    fn test_redacted_pin_clone_retains_drop_zeroization() {
        crate::request::REDACTED_PIN_DROP_COUNT.with(|c| c.set(0));
        let pin1 = RedactedPin::new("cloned_secret_proof");
        let pin2 = pin1.clone();
        assert_ne!(pin1.as_str().as_ptr(), pin2.as_str().as_ptr());
        assert_eq!(pin1.as_str(), pin2.as_str());
        drop(pin1);
        assert_eq!(crate::request::REDACTED_PIN_DROP_COUNT.with(|c| c.get()), 1);
        drop(pin2);
        assert_eq!(crate::request::REDACTED_PIN_DROP_COUNT.with(|c| c.get()), 2);
    }

    #[test]
    fn test_redacted_pin_public_api_has_no_unprotected_owned_secret_extraction() {
        let req_src = include_str!("request.rs");
        assert!(!req_src.contains("pub fn into_inner"));
        assert!(!req_src.contains("pub fn to_sensitive"));
        assert!(req_src.contains("pub fn into_sensitive"));
        assert!(req_src.contains("pub fn as_str"));
    }

    #[test]
    fn test_duplicate_key_precheck_scalar_strings_not_decoded() {
        SCALAR_STRING_DECODE_COUNT.with(|c| c.set(0));
        let valid_json = r#"{"kind":"VerifyPin","pin":"secret"}"#;
        assert!(check_no_duplicate_keys(valid_json).is_ok());
        assert_eq!(SCALAR_STRING_DECODE_COUNT.with(|c| c.get()), 0);
    }

    #[test]
    fn test_duplicate_key_precheck_detects_nested_object_duplicates() {
        let nested_dup = r#"{"outer":{"dup":1,"dup":2}}"#;
        let err = check_no_duplicate_keys(nested_dup).expect_err("nested dup must fail");
        assert_eq!(err.code, ErrorCode::ProtocolError);
    }

    #[test]
    fn test_duplicate_key_precheck_detects_nested_array_object_duplicates() {
        let array_dup = r#"{"arr":[{"dup":1,"dup":2}]}"#;
        let err = check_no_duplicate_keys(array_dup).expect_err("array dup must fail");
        assert_eq!(err.code, ErrorCode::ProtocolError);
    }

    #[test]
    fn test_raw_pin_before_kind_order_independent_and_secret_safe() {
        let json = r#"{"pin":"secret99","kind":"VerifyPin"}"#;
        let raw = serde_json::value::RawValue::from_string(json.to_string()).unwrap();
        let parsed = parse_request_payload_raw(&raw).expect("order independent pin before kind");
        match parsed {
            RequestPayload::VerifyPin { pin } => assert_eq!(pin.as_str(), "secret99"),
            _ => panic!("Expected VerifyPin variant"),
        }
    }

    #[test]
    fn test_kind_before_raw_pin_order_independent_and_secret_safe() {
        let json = r#"{"kind":"VerifyPin","pin":"secret99"}"#;
        let raw = serde_json::value::RawValue::from_string(json.to_string()).unwrap();
        let parsed = parse_request_payload_raw(&raw).expect("order independent kind before pin");
        match parsed {
            RequestPayload::VerifyPin { pin } => assert_eq!(pin.as_str(), "secret99"),
            _ => panic!("Expected VerifyPin variant"),
        }
    }

    #[test]
    fn test_direct_request_payload_deserialize_safe() {
        let json = r#"{"kind":"VerifyPin","pin":"direct_pin"}"#;
        let payload: RequestPayload =
            serde_json::from_str(json).expect("Direct RequestPayload deserialize");
        match payload {
            RequestPayload::VerifyPin { pin } => assert_eq!(pin.as_str(), "direct_pin"),
            _ => panic!("Expected VerifyPin"),
        }
    }

    #[test]
    fn test_direct_request_envelope_deserialize_safe() {
        let json =
            r#"{"version":1,"type":"request","request":{"kind":"VerifyPin","pin":"env_pin"}}"#;
        let env: RequestEnvelope =
            serde_json::from_str(json).expect("Direct RequestEnvelope deserialize");
        assert_eq!(env.version, 1);
        match env.request {
            RequestPayload::VerifyPin { pin } => assert_eq!(pin.as_str(), "env_pin"),
            _ => panic!("Expected VerifyPin"),
        }
    }

    #[test]
    fn test_verify_pin_raw_unescaped_ascii_fidelity() {
        let pin_raw = r#""plainAscii123!@#$%^&*()""#;
        let pin = unescape_pin_json_string(pin_raw).expect("Valid ASCII PIN");
        assert_eq!(pin.as_str(), "plainAscii123!@#$%^&*()");
    }

    #[test]
    fn test_verify_pin_escaped_control_characters_fidelity() {
        let pin_raw = r#""quote\"slash\\forward\/back\bform\fline\nreturn\rtab\t""#;
        let pin = unescape_pin_json_string(pin_raw).expect("Valid escaped controls");
        assert_eq!(
            pin.as_str(),
            "quote\"slash\\forward/back\x08form\x0Cline\nreturn\rtab\t"
        );
    }

    #[test]
    fn test_verify_pin_unicode_4hex_escape_fidelity() {
        let pin_raw = r#""\u0048\u0065\u006c\u006c\u006f \u041f\u0440\u0438\u0432\u0435\u0442""#;
        let pin = unescape_pin_json_string(pin_raw).expect("Valid hex4 unicode");
        assert_eq!(pin.as_str(), "Hello Привет");
    }

    #[test]
    fn test_verify_pin_unicode_surrogate_pair_fidelity() {
        let pin_raw = r#""emoji:\uD83D\uDE00""#;
        let pin = unescape_pin_json_string(pin_raw).expect("Valid surrogate pair");
        assert_eq!(pin.as_str(), "emoji:😀");
    }

    #[test]
    fn test_verify_pin_malformed_escape_drops_partial_buffer_protected() {
        crate::request::REDACTED_PIN_DROP_COUNT.with(|c| c.set(0));
        let bad_escapes = [
            r#""valid_prefix\u12""#,
            r#""valid_prefix\q""#,
            r#""valid_prefix\u00GG""#,
            r#""valid_prefix\uD800!""#,
            r#""valid_prefix\uDC00!""#,
        ];
        for bad in &bad_escapes {
            let res = unescape_pin_json_string(bad);
            assert!(res.is_err(), "Bad escape {} must fail", bad);
            match res.unwrap_err() {
                RequestParseFailure::Malformed(_) => {}
                other => panic!("Expected Malformed, got {:?}", other),
            }
        }
        let after = crate::request::REDACTED_PIN_DROP_COUNT.with(|c| c.get());
        assert_eq!(after, bad_escapes.len());
    }

    #[test]
    fn test_duplicate_pin_field_rejected_before_pin_decode() {
        let json = r#"{"version":1,"type":"request","request":{"kind":"VerifyPin","pin":"1111","pin":"2222"}}"#;
        let err = decode_request_frame(json.as_bytes()).expect_err("Duplicate pin field must fail");
        assert_eq!(err.code, ErrorCode::ProtocolError);
    }

    #[test]
    fn test_typed_request_parser_timer_id_invalid_request_mapping() {
        let bad_timer_req = r#"{"version":1,"type":"request","request":{"kind":"CancelShutdownTimer","timer_id":"not_valid_hex"}}"#;
        let err =
            decode_request_frame(bad_timer_req.as_bytes()).expect_err("Invalid timer id must fail");
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn test_typed_request_parser_duration_invalid_request_mapping() {
        let bad_dur_req = r#"{"version":1,"type":"request","request":{"kind":"ScheduleShutdown","duration_minutes":"sixty"}}"#;
        let err =
            decode_request_frame(bad_dur_req.as_bytes()).expect_err("Invalid duration must fail");
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn test_verify_pin_wrong_type_malformed_frame_classification() {
        let wrong_type_req =
            r#"{"version":1,"type":"request","request":{"kind":"VerifyPin","pin":1234}}"#;
        let err = decode_request_frame(wrong_type_req.as_bytes())
            .expect_err("Non-string pin must fail as MalformedFrame");
        assert_eq!(err.code, ErrorCode::MalformedFrame);
    }

    #[test]
    fn test_authoritative_decode_has_no_serde_substring_classification() {
        // Static check of codec.rs request decode section
        let codec_src = include_str!("codec.rs");
        let req_section_end = codec_src
            .find("pub fn encode_response_frame")
            .expect("encode_response_frame boundary");
        let req_section = &codec_src[..req_section_end];
        assert!(!req_section.contains("msg.contains("));
        assert!(!req_section.contains(".contains(\"Duplicate key\")"));
        assert!(!req_section.contains(".contains(\"expected a JSON object\")"));
        assert!(!req_section.contains(".contains(\"unknown field\")"));
        assert!(!req_section.contains(".contains(\"unknown variant\")"));
        assert!(!req_section.contains(".contains(\"missing field\")"));

        // Behavioral test for escaped JSON type compatibility
        let escaped_type_req =
            r#"{"version":1,"type":"requ\u0065st","request":{"kind":"QueryStatus"}}"#;
        let decoded =
            decode_request_frame(escaped_type_req.as_bytes()).expect("escaped type must succeed");
        assert_eq!(decoded.request, RequestPayload::QueryStatus);
    }

    #[test]
    fn test_authoritative_decode_has_no_full_raw_serde_value_prepass() {
        let codec_src = include_str!("codec.rs");
        let req_section_end = codec_src
            .find("pub fn encode_response_frame")
            .expect("encode_response_frame boundary");
        let req_section = &codec_src[..req_section_end];
        assert!(!req_section.contains("serde_json::from_str::<serde_json::Value>"));

        let valid = r#"{"version":1,"type":"request","request":{"kind":"QueryStatus"}}"#;
        let decoded = decode_request_frame(valid.as_bytes()).expect("Valid QueryStatus");
        assert_eq!(decoded.request, RequestPayload::QueryStatus);

        // Trailing whitespace is accepted
        let valid_trailing_spaces = format!("   {}   \t\r\n  ", valid);
        let decoded_spaces = decode_request_frame(valid_trailing_spaces.as_bytes())
            .expect("Valid QueryStatus with trailing whitespace");
        assert_eq!(decoded_spaces.request, RequestPayload::QueryStatus);

        // Trailing garbage is rejected as MalformedFrame
        let trailing_garbage = format!("{} garbage", valid);
        let err_garbage = decode_request_frame(trailing_garbage.as_bytes())
            .expect_err("Trailing non-whitespace garbage must fail");
        assert_eq!(err_garbage.code, ErrorCode::MalformedFrame);

        // Concatenated JSON values rejected as MalformedFrame
        let concat_json = format!("{}{}", valid, valid);
        let err_concat = decode_request_frame(concat_json.as_bytes())
            .expect_err("Concatenated JSON values must fail");
        assert_eq!(err_concat.code, ErrorCode::MalformedFrame);

        // Direct check_no_duplicate_keys trailing validation
        assert!(check_no_duplicate_keys(&valid_trailing_spaces).is_ok());
        let err_dup_garbage = check_no_duplicate_keys(&trailing_garbage)
            .expect_err("check_no_duplicate_keys with trailing garbage must fail");
        assert_eq!(err_dup_garbage.code, ErrorCode::MalformedFrame);
        let err_dup_concat = check_no_duplicate_keys(&concat_json)
            .expect_err("check_no_duplicate_keys with concatenated JSON must fail");
        assert_eq!(err_dup_concat.code, ErrorCode::MalformedFrame);
    }

    #[test]
    fn test_internally_tagged_derived_deserialize_absent_structurally() {
        let req_src = include_str!("request.rs");
        assert!(req_src.contains("pub enum RequestPayload"));
        assert!(req_src.contains("derive(Debug, Clone, PartialEq, Eq, Serialize)"));
        assert!(!req_src.contains("derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)"));
        assert!(req_src.contains("impl<'de> Deserialize<'de> for RequestPayload"));

        let payload = RequestPayload::QueryStatus;
        let serialized = serde_json::to_string(&payload).expect("Serialize QueryStatus");
        assert_eq!(serialized, r#"{"kind":"QueryStatus"}"#);
    }

    #[test]
    fn test_encode_request_frame_temporary_zeroized() {
        let codec_src = include_str!("codec.rs");
        assert!(codec_src.contains("zeroize::Zeroizing::new(Vec::<u8>::new())"));
        assert!(codec_src.contains("serde_json::to_writer(&mut *json_bytes, req)"));

        let req = RequestEnvelope::new(RequestPayload::VerifyPin {
            pin: RedactedPin::new("encode_zeroize_test"),
        });
        let frame = encode_request_frame(&req).expect("Encoding succeeds");
        assert!(frame.len() > 4);
    }

    #[test]
    fn test_encode_request_frame_validation_error_drops_protected_temporary() {
        let large_pin = "a".repeat(70000);
        let req = RequestEnvelope::new(RequestPayload::VerifyPin {
            pin: RedactedPin::new(large_pin),
        });
        // RequestPayload::validate() passes for VerifyPin
        assert!(req.request.validate().is_ok());
        let err = encode_request_frame(&req).expect_err("Oversized serialized frame must fail");
        assert_eq!(err.code, ErrorCode::FrameTooLarge);
    }

    #[test]
    fn test_encoded_verify_pin_frame_retains_exact_wire_secret() {
        let req = RequestEnvelope::new(RequestPayload::VerifyPin {
            pin: RedactedPin::new("wire_secret_1234"),
        });
        let frame = encode_request_frame(&req).expect("Encode succeeds");
        let payload_str = std::str::from_utf8(&frame[4..]).unwrap();
        assert!(payload_str.contains(r#""pin":"wire_secret_1234""#));
    }

    #[test]
    fn test_request_parse_failure_diagnostics_never_contain_pin() {
        let bad_pin_req = r#"{"version":1,"type":"request","request":{"kind":"VerifyPin","pin":"bad_escape_\q_my_secret_pin_12345"}}"#;
        let err =
            decode_request_frame(bad_pin_req.as_bytes()).expect_err("Bad escape pin must fail");
        if let Some(msg) = err.message {
            assert!(!msg.contains("my_secret_pin_12345"));
        }
        let unknown_field_pin_req = r#"{"version":1,"type":"request","request":{"kind":"VerifyPin","pin":"my_secret_pin_12345","extra":"leak"}}"#;
        let err2 = decode_request_frame(unknown_field_pin_req.as_bytes())
            .expect_err("Unknown field must fail");
        if let Some(msg) = err2.message {
            assert!(!msg.contains("my_secret_pin_12345"));
        }
    }
}
