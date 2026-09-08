//! Top-level wire envelopes for request, response, error, and event frames.

use serde::{Deserialize, Serialize};

use crate::constants::PROTOCOL_VERSION;
use crate::error::{ErrorPayload, ProtocolError};
use crate::events::EventPayload;
use crate::request::RequestPayload;
use crate::response::ResponsePayload;

/// Top-level request envelope transmitted from client to service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub version: u32,
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub request: RequestPayload,
}

impl RequestEnvelope {
    pub fn new(request: RequestPayload) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            envelope_type: "request".into(),
            request,
        }
    }
}

/// Top-level success response envelope transmitted from service to client.
///
/// NOTE: By architectural design, there is NO request_id field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub version: u32,
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub response: ResponsePayload,
}

impl ResponseEnvelope {
    pub fn new(response: ResponsePayload) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            envelope_type: "response".into(),
            response,
        }
    }
}

/// Top-level error envelope transmitted from service to client upon failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    pub version: u32,
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub error: ErrorPayload,
}

impl ErrorEnvelope {
    pub fn new(error: ErrorPayload) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            envelope_type: "error".into(),
            error,
        }
    }

    pub fn from_protocol_error(err: &ProtocolError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            envelope_type: "error".into(),
            error: err.to_payload(),
        }
    }
}

/// Top-level event envelope transmitted from service to subscribed client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub version: u32,
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub event: EventPayload,
}

impl EventEnvelope {
    pub fn new(event: EventPayload) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            envelope_type: "event".into(),
            event,
        }
    }
}

impl std::fmt::Display for RequestEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RequestEnvelope {{ version: {}, type: {:?}, request: {} }}",
            self.version, self.envelope_type, self.request
        )
    }
}

impl std::fmt::Display for ResponseEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ResponseEnvelope {{ version: {}, type: {:?} }}",
            self.version, self.envelope_type
        )
    }
}

impl std::fmt::Display for ErrorEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ErrorEnvelope {{ version: {}, type: {:?}, error: {:?} }}",
            self.version, self.envelope_type, self.error
        )
    }
}

impl std::fmt::Display for EventEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "EventEnvelope {{ version: {}, type: {:?} }}",
            self.version, self.envelope_type
        )
    }
}
