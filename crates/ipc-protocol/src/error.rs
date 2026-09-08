//! Stable protocol error codes and error envelope payload for PALKA IPC V1.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable machine-readable protocol error codes defined by docs/021.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Packet protocol version is unsupported.
    UnsupportedProtocolVersion,
    /// Violations of protocol rules (zero-length frame, duplicate JSON keys, unexpected envelope fields).
    ProtocolError,
    /// Malformed UTF-8 or corrupt JSON body.
    MalformedFrame,
    /// Request length exceeds the 64 KiB limit.
    FrameTooLarge,
    /// Serialized response exceeds the 1 MiB limit.
    ResponseTooLarge,
    /// Client SID or role rejected by security policy.
    UnauthorizedClient,
    /// Operation requires preliminary PIN verification.
    PinRequired,
    /// Supplied PIN is incorrect.
    PinRejected,
    /// PIN verification locked due to excessive failed attempts.
    PinLocked,
    /// Invalid request parameters (empty chat text, invalid duration, unknown request kind).
    InvalidRequest,
    /// Service is shutting down and cannot accept requests.
    ServiceStopping,
    /// Runtime coordinator failure while executing request.
    RuntimeFailure,
    /// Transport failure or premature stream disconnection.
    TransportFailure,
    /// Unhandled internal error.
    InternalError,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Payload inside the top-level error envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorPayload {
    /// Authoritative stable error code.
    pub code: ErrorCode,
    /// Optional non-authoritative diagnostic message (never contains secrets, explicit null when absent).
    #[serde(deserialize_with = "crate::codec::deserialize_required_nullable")]
    pub message: Option<String>,
    /// Optional retry delay in seconds (returned for PinLocked, explicit null when absent).
    #[serde(deserialize_with = "crate::codec::deserialize_required_nullable")]
    pub retry_after_seconds: Option<u32>,
}

/// Internal protocol operational error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: Option<String>,
    pub retry_after_seconds: Option<u32>,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<Option<String>>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn with_retry(
        code: ErrorCode,
        message: impl Into<Option<String>>,
        retry_after_seconds: Option<u32>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_seconds,
        }
    }

    pub fn unsupported_version() -> Self {
        Self::new(
            ErrorCode::UnsupportedProtocolVersion,
            Some("Protocol version is not supported".into()),
        )
    }

    pub fn protocol_error(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::ProtocolError, Some(msg.into()))
    }

    pub fn malformed_frame(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::MalformedFrame, Some(msg.into()))
    }

    pub fn frame_too_large(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::FrameTooLarge, Some(msg.into()))
    }

    pub fn response_too_large(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::ResponseTooLarge, Some(msg.into()))
    }

    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, Some(msg.into()))
    }

    pub fn transport_failure(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::TransportFailure, Some(msg.into()))
    }

    pub fn to_payload(&self) -> ErrorPayload {
        ErrorPayload {
            code: self.code,
            message: self.message.clone(),
            retry_after_seconds: self.retry_after_seconds,
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.message {
            Some(msg) => write!(f, "{}: {}", self.code, msg),
            None => write!(f, "{}", self.code),
        }
    }
}

impl std::error::Error for ProtocolError {}
