//! Closed set of V1 IPC requests and request payload validation.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use zeroize::Zeroize;

use crate::constants::{MAX_CHAT_TEXT_UTF8_BYTES, MAX_DURATION_MINUTES};
use crate::error::{ErrorCode, ProtocolError};
use crate::ids::WireTimerId;

#[cfg(test)]
thread_local! {
    pub(crate) static REDACTED_PIN_DROP_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Secure PIN container that always redacts the actual secret in Debug and Display.
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedPin(pub(crate) String);

impl Drop for RedactedPin {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        REDACTED_PIN_DROP_COUNT.with(|c| c.set(c.get() + 1));
    }
}

impl RedactedPin {
    pub fn new(pin: impl Into<String>) -> Self {
        Self(pin.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_sensitive(mut self) -> palka_core::SensitivePinString {
        let pin = std::mem::take(&mut self.0);
        palka_core::SensitivePinString::new(pin)
    }
}

impl fmt::Debug for RedactedPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"[REDACTED]\"")
    }
}

impl fmt::Display for RedactedPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl Serialize for RedactedPin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RedactedPin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = <&serde_json::value::RawValue>::deserialize(deserializer)?;
        crate::codec::unescape_pin_json_string(raw.get()).map_err(serde::de::Error::custom)
    }
}

/// Closed set of requests supported by PALKA IPC V1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum RequestPayload {
    /// Retrieve the aggregated system status snapshot.
    QueryStatus,
    /// Verify a parental PIN attempt to authorize mutating actions on this connection.
    VerifyPin { pin: RedactedPin },
    /// Subscribe this connection to the real-time event stream.
    SubscribeEvents,
    /// Send a chat message from child to parent outbox.
    SendChildMessage { text: String },
    /// Schedule a delayed Internet block timer.
    ScheduleInternetBlock { duration_minutes: u32 },
    /// Immediately block Internet access.
    ImmediateInternetBlock,
    /// Cancel an active Internet block timer by its exact 32-character hex ID.
    CancelInternetBlockTimer { timer_id: WireTimerId },
    /// Immediately restore Internet access.
    RestoreInternet,
    /// Schedule a delayed computer shutdown timer.
    ScheduleShutdown { duration_minutes: u32 },
    /// Cancel an active shutdown timer by its exact 32-character hex ID.
    CancelShutdownTimer { timer_id: WireTimerId },
}

impl<'de> Deserialize<'de> for RequestPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = <&serde_json::value::RawValue>::deserialize(deserializer)?;
        crate::codec::parse_request_payload_raw(raw).map_err(serde::de::Error::custom)
    }
}

impl RequestPayload {
    /// Performs normative protocol validation on request parameters.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::SendChildMessage { text } => {
                if text.is_empty() {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidRequest,
                        Some("Chat message text cannot be empty".into()),
                    ));
                }
                if text.len() > MAX_CHAT_TEXT_UTF8_BYTES {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidRequest,
                        Some(format!(
                            "Chat message text length ({} bytes) exceeds maximum of {} bytes",
                            text.len(),
                            MAX_CHAT_TEXT_UTF8_BYTES
                        )),
                    ));
                }
                if text.trim().is_empty() {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidRequest,
                        Some("Chat message text cannot consist solely of whitespace".into()),
                    ));
                }
                Ok(())
            }
            Self::ScheduleInternetBlock { duration_minutes }
            | Self::ScheduleShutdown { duration_minutes } => {
                validate_duration_minutes(*duration_minutes)
            }
            _ => Ok(()),
        }
    }
}

fn validate_duration_minutes(duration_minutes: u32) -> Result<(), ProtocolError> {
    if duration_minutes == 0 {
        return Err(ProtocolError::new(
            ErrorCode::InvalidRequest,
            Some("Duration minutes must be strictly positive (>= 1)".into()),
        ));
    }
    if duration_minutes > MAX_DURATION_MINUTES {
        return Err(ProtocolError::new(
            ErrorCode::InvalidRequest,
            Some(format!(
                "Duration minutes ({}) exceeds maximum limit ({})",
                duration_minutes, MAX_DURATION_MINUTES
            )),
        ));
    }
    if duration_minutes.checked_mul(60).is_none() {
        return Err(ProtocolError::new(
            ErrorCode::InvalidRequest,
            Some("Duration conversion to seconds would overflow u32".into()),
        ));
    }
    Ok(())
}

impl fmt::Display for RequestPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueryStatus => write!(f, "QueryStatus"),
            Self::VerifyPin { pin } => write!(f, "VerifyPin {{ pin: {} }}", pin),
            Self::SubscribeEvents => write!(f, "SubscribeEvents"),
            Self::SendChildMessage { text } => {
                write!(f, "SendChildMessage {{ len: {} }}", text.len())
            }
            Self::ScheduleInternetBlock { duration_minutes } => {
                write!(
                    f,
                    "ScheduleInternetBlock {{ duration_minutes: {} }}",
                    duration_minutes
                )
            }
            Self::ImmediateInternetBlock => write!(f, "ImmediateInternetBlock"),
            Self::CancelInternetBlockTimer { timer_id } => {
                write!(f, "CancelInternetBlockTimer {{ timer_id: {} }}", timer_id)
            }
            Self::RestoreInternet => write!(f, "RestoreInternet"),
            Self::ScheduleShutdown { duration_minutes } => {
                write!(
                    f,
                    "ScheduleShutdown {{ duration_minutes: {} }}",
                    duration_minutes
                )
            }
            Self::CancelShutdownTimer { timer_id } => {
                write!(f, "CancelShutdownTimer {{ timer_id: {} }}", timer_id)
            }
        }
    }
}
