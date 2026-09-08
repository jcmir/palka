//! PALKA IPC Protocol Crate (palka-ipc-protocol).
//!
//! Provides the canonical wire framing codec, envelopes, DTOs, and resource bounds
//! for PALKA IPC V1 over Windows Named Pipes.

pub mod codec;
pub mod constants;
pub mod envelopes;
pub mod error;
pub mod events;
pub mod ids;
pub mod request;
pub mod response;
pub mod snapshot;

pub use codec::{
    check_no_duplicate_keys, decode_error_frame, decode_event_frame, decode_length_prefix,
    decode_request_frame, decode_response_frame, deserialize_required_nullable, encode_error_frame,
    encode_event_frame, encode_length_prefix, encode_request_frame, encode_response_frame,
    extract_frame_payload, validate_event_frame_length, validate_frame_completeness,
    validate_raw_version_field, validate_request_frame_length, validate_response_frame_length,
    validate_version,
};
pub use constants::{
    MAX_CHAT_TEXT_UTF8_BYTES, MAX_DURATION_MINUTES, MAX_EVENT_BYTES, MAX_REQUEST_BYTES,
    MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
};
pub use envelopes::{ErrorEnvelope, EventEnvelope, RequestEnvelope, ResponseEnvelope};
pub use error::{ErrorCode, ErrorPayload, ProtocolError};
pub use events::{EventPayload, try_from_core_event};
pub use ids::{WireMessageId, WireTimerId};
pub use request::{RedactedPin, RequestPayload};
pub use response::{CancellationResult, ResponsePayload};
pub use snapshot::{
    ActionExecutionStateDto, ActionKindDto, ChatMessageDto, DeliveryStatusDto,
    DesiredInternetStateDto, HealthStatusDto, InitiatorDto, InternetStateDto, MessageSenderDto,
    ScheduledActionDto, ServiceHealthDto, ShutdownStateDto, StateChangeReasonDto,
    StatusSnapshotDto, WarningEventDto,
};
