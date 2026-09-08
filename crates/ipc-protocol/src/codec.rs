//! Binary frame codec, length prefixing, resource bounds validation, and strict JSON parsing.

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::HashSet;
use std::fmt;

use crate::constants::{MAX_EVENT_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION};
use crate::envelopes::{ErrorEnvelope, EventEnvelope, RequestEnvelope, ResponseEnvelope};
use crate::error::{ErrorCode, ProtocolError};

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
// Strict JSON tree parsing with duplicate-key rejection (IPC-86)
// -----------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug)]
enum StrictValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<StrictValue>),
    Object(Vec<(String, StrictValue)>),
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(StrictValue::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(StrictValue::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(StrictValue::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let n = serde_json::Number::from_f64(v)
            .ok_or_else(|| de::Error::custom("NaN or Infinity values are not permitted"))?;
        Ok(StrictValue::Number(n))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(StrictValue::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(StrictValue::String(v))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut vec = Vec::new();
        while let Some(elem) = seq.next_element()? {
            vec.push(elem);
        }
        Ok(StrictValue::Array(vec))
    }

    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        let mut keys_seen = HashSet::new();

        while let Some(key) = access.next_key::<String>()? {
            if !keys_seen.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "Duplicate key '{}' is strictly prohibited in JSON object",
                    key
                )));
            }
            let val: StrictValue = access.next_value()?;
            entries.push((key, val));
        }
        Ok(StrictValue::Object(entries))
    }
}

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

/// Inspects a UTF-8 JSON payload to strictly reject any duplicate object keys (IPC-86).
pub fn check_no_duplicate_keys(json_str: &str) -> Result<(), ProtocolError> {
    let mut de = serde_json::Deserializer::from_str(json_str);
    StrictValue::deserialize(&mut de).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("Duplicate key") {
            ProtocolError::protocol_error(msg)
        } else {
            ProtocolError::malformed_frame(format!("Malformed JSON: {}", msg))
        }
    })?;
    Ok(())
}

/// Validates the protocol version from a raw JSON map without narrowing conversions (IPC-03..05).
///
/// Authoritative rules:
/// - version == 1 => accepted
/// - integer version != 1 => UnsupportedProtocolVersion
/// - missing version => ProtocolError
/// - wrong JSON type => ProtocolError
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
///
/// If the field is omitted entirely, deserialization fails with a missing field error.
pub fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Validates that the payload received matches the declared frame length (IPC-15).
///
/// If fewer bytes than declared were received before stream end / EOF,
/// this is classified as a `TransportFailure`.
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
///
/// Given a stream buffer, checks for length prefix, validates pre-allocation length bounds,
/// and extracts complete payload or returns `TransportFailure` on premature EOF.
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
    let json_bytes = serde_json::to_vec(req).map_err(|e| {
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

    let raw_val: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| ProtocolError::malformed_frame(format!("Malformed request JSON: {}", e)))?;

    let raw_obj = raw_val
        .as_object()
        .ok_or_else(|| ProtocolError::protocol_error("Request envelope must be a JSON object"))?;

    for key in raw_obj.keys() {
        if key != "version" && key != "type" && key != "request" {
            return Err(ProtocolError::protocol_error(format!(
                "Unknown top-level field '{}' in request envelope",
                key
            )));
        }
    }

    validate_raw_version_field(raw_obj, "Request envelope")?;

    match raw_obj.get("type") {
        None => {
            return Err(ProtocolError::protocol_error(
                "Request envelope is missing required 'type' field",
            ));
        }
        Some(v) => {
            if v.as_str() != Some("request") {
                return Err(ProtocolError::protocol_error(format!(
                    "Invalid envelope type '{:?}', expected 'request'",
                    v
                )));
            }
        }
    }

    let req_val = raw_obj.get("request").ok_or_else(|| {
        ProtocolError::protocol_error("Request envelope is missing required 'request' field")
    })?;

    let req_obj = req_val
        .as_object()
        .ok_or_else(|| ProtocolError::protocol_error("Field 'request' must be a JSON object"))?;

    let kind = req_obj
        .get("kind")
        .and_then(|k| k.as_str())
        .ok_or_else(|| {
            ProtocolError::invalid_request("Missing or invalid 'kind' field in request")
        })?;

    let allowed_fields: &[&str] = match kind {
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
            return Err(ProtocolError::invalid_request(format!(
                "Unknown request kind: {}",
                kind
            )));
        }
    };

    for key in req_obj.keys() {
        if !allowed_fields.contains(&key.as_str()) {
            return Err(ProtocolError::protocol_error(format!(
                "Unknown field '{}' inside request of kind '{}'",
                key, kind
            )));
        }
    }

    let req: RequestEnvelope = serde_json::from_str(json_str).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("unknown field") {
            ProtocolError::protocol_error(format!("Unknown field in request envelope: {}", msg))
        } else if msg.contains("unknown variant") {
            ProtocolError::invalid_request(format!("Unknown request kind: {}", msg))
        } else if msg.contains("missing field") {
            ProtocolError::invalid_request(format!("Missing required field in request: {}", msg))
        } else {
            ProtocolError::malformed_frame(format!("Malformed request: {}", msg))
        }
    })?;

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
