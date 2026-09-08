//! Wire identifier representations and conversions for TimerId and MessageId.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

use crate::error::{ErrorCode, ProtocolError};

fn encode_hex_16(bytes: &[u8; 16]) -> String {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for &b in bytes {
        out.push(HEX_CHARS[(b >> 4) as usize] as char);
        out.push(HEX_CHARS[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex_16(s: &str) -> Result<[u8; 16], ProtocolError> {
    if s.len() != 32 {
        return Err(ProtocolError::new(
            ErrorCode::InvalidRequest,
            Some(format!(
                "Identifier must be exactly 32 hex characters, got {}",
                s.len()
            )),
        ));
    }
    let mut bytes = [0u8; 16];
    let src = s.as_bytes();
    for i in 0..16 {
        let hi = match src[2 * i] {
            b'0'..=b'9' => src[2 * i] - b'0',
            b'a'..=b'f' => src[2 * i] - b'a' + 10,
            _ => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    Some("Identifier contains invalid characters or uppercase hex".into()),
                ));
            }
        };
        let lo = match src[2 * i + 1] {
            b'0'..=b'9' => src[2 * i + 1] - b'0',
            b'a'..=b'f' => src[2 * i + 1] - b'a' + 10,
            _ => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    Some("Identifier contains invalid characters or uppercase hex".into()),
                ));
            }
        };
        bytes[i] = (hi << 4) | lo;
    }
    Ok(bytes)
}

/// Canonical wire representation of a 128-bit TimerId (32 lowercase hex characters).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WireTimerId(pub [u8; 16]);

impl WireTimerId {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn from_core(id: palka_core::TimerId) -> Self {
        Self(id.0)
    }

    pub fn to_core(self) -> palka_core::TimerId {
        palka_core::TimerId(self.0)
    }

    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        decode_hex_16(s).map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for WireTimerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", encode_hex_16(&self.0))
    }
}

impl fmt::Debug for WireTimerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WireTimerId(\"{}\")", self)
    }
}

impl Serialize for WireTimerId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_hex_16(&self.0))
    }
}

struct WireTimerIdVisitor;

impl<'de> Visitor<'de> for WireTimerIdVisitor {
    type Value = WireTimerId;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a 32-character lowercase hex string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        WireTimerId::parse(v).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for WireTimerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(WireTimerIdVisitor)
    }
}

/// Canonical wire representation of a 128-bit MessageId (32 lowercase hex characters).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WireMessageId(pub [u8; 16]);

impl WireMessageId {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn from_core(id: palka_core::MessageId) -> Self {
        Self(id.0)
    }

    pub fn to_core(self) -> palka_core::MessageId {
        palka_core::MessageId(self.0)
    }

    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        decode_hex_16(s).map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for WireMessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", encode_hex_16(&self.0))
    }
}

impl fmt::Debug for WireMessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WireMessageId(\"{}\")", self)
    }
}

impl Serialize for WireMessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_hex_16(&self.0))
    }
}

struct WireMessageIdVisitor;

impl<'de> Visitor<'de> for WireMessageIdVisitor {
    type Value = WireMessageId;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a 32-character lowercase hex string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        WireMessageId::parse(v).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for WireMessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(WireMessageIdVisitor)
    }
}
