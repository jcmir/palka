//! State persistence and codec for PALKA service.

use palka_core::{
    ActionExecutionState, ActionKind, ChatMessage, Deadline, DeliveryStatus, DesiredInternetState,
    Initiator, MessageId, MessageSender, ScheduledAction, TimerId, UtcDateTime, WarningThreshold,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// Supported schema version of state.json.
pub const STATE_SCHEMA_VERSION_V1: u32 = 1;

/// Unique 128-bit identifier for a persistent Telegram outbox queue entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutboxEntryId(pub [u8; 16]);

impl OutboxEntryId {
    /// Formats the identifier as a 32-character lowercase hex string.
    pub fn to_hex(&self) -> String {
        format_128bit_id_hex(&self.0)
    }

    /// Parses an identifier from a 32-character lowercase hex string.
    pub fn from_hex(s: &str) -> Result<Self, PersistenceError> {
        let bytes = parse_128bit_id_hex("entry_id", s)?;
        Ok(Self(bytes))
    }
}

/// Metadata for background Internet policy reconciliation retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternetRetry {
    pub attempt_count: u32,
    pub last_error: Option<String>,
}

/// Payload variants stored in the persistent Telegram outbox queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelegramPayload {
    Chat { message: ChatMessage },
    ServiceNotification { text: String },
}

/// A durable transport entry in the Telegram outbox queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramOutboxEntry {
    pub entry_id: OutboxEntryId,
    pub payload: TelegramPayload,
    pub attempt_count: u32,
    pub last_error: Option<String>,
}

/// In-memory representation of the single authoritative mutable snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentState {
    pub desired_internet_state: DesiredInternetState,
    pub active_actions: Vec<ScheduledAction>,
    pub internet_retry: Option<InternetRetry>,
    pub telegram_outbox: Vec<TelegramOutboxEntry>,
}

/// Error type for persistence operations, validation and JSON decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceError {
    Json(String),
    UnsupportedSchemaVersion(u32),
    InvalidId {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    DuplicateTimerId(TimerId),
    DuplicateOutboxEntryId(OutboxEntryId),
    DuplicateWarningThreshold {
        timer_id: TimerId,
        threshold: WarningThreshold,
    },
    Validation(String),
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(msg) => write!(f, "JSON serialization/deserialization error: {msg}"),
            Self::UnsupportedSchemaVersion(v) => {
                write!(f, "Unsupported state schema version: {v} (expected 1)")
            }
            Self::InvalidId {
                field,
                value,
                reason,
            } => {
                write!(
                    f,
                    "Invalid identifier in field '{field}' ({value}): {reason}"
                )
            }
            Self::DuplicateTimerId(id) => {
                write!(
                    f,
                    "Duplicate TimerId in active_actions: {}",
                    format_128bit_id_hex(&id.0)
                )
            }
            Self::DuplicateOutboxEntryId(id) => {
                write!(
                    f,
                    "Duplicate entry_id in telegram_outbox: {}",
                    format_128bit_id_hex(&id.0)
                )
            }
            Self::DuplicateWarningThreshold {
                timer_id,
                threshold,
            } => {
                write!(
                    f,
                    "Duplicate warning threshold {:?} in action {}",
                    threshold,
                    format_128bit_id_hex(&timer_id.0)
                )
            }
            Self::Validation(msg) => write!(f, "State validation error: {msg}"),
        }
    }
}

impl std::error::Error for PersistenceError {}

// ---------------------------------------------------------------------------
// Strict DTO definitions for schema v1
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFileV1Dto {
    schema_version: u32,
    desired_internet_state: DesiredInternetStateDto,
    active_actions: Vec<ScheduledActionDto>,
    internet_retry: Option<InternetRetryDto>,
    telegram_outbox: Vec<TelegramOutboxEntryDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum DesiredInternetStateDto {
    Unrestricted,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduledActionDto {
    id: String,
    action_kind: ActionKindDto,
    deadline: i64,
    created_at: i64,
    created_by: InitiatorDto,
    emitted_thresholds: Vec<WarningThresholdDto>,
    execution_state: ActionExecutionStateDto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ActionKindDto {
    BlockInternet,
    ShutdownComputer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum InitiatorDto {
    ParentTelegram { user_id: u64 },
    ParentLocalPin {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum WarningThresholdDto {
    M60,
    M30,
    M20,
    M10,
    M3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum ActionExecutionStateDto {
    Pending {},
    Executing {},
    Completed {},
    Failed { reason: String },
    Missed {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InternetRetryDto {
    attempt_count: u32,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelegramOutboxEntryDto {
    entry_id: String,
    payload: TelegramPayloadDto,
    attempt_count: u32,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum TelegramPayloadDto {
    Chat { message: ChatMessageDto },
    ServiceNotification { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatMessageDto {
    id: String,
    sender: MessageSenderDto,
    text: String,
    timestamp: i64,
    delivery_status: DeliveryStatusDto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum MessageSenderDto {
    Child,
    Parent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum DeliveryStatusDto {
    Pending,
    AcceptedByService,
    AcceptedByTelegram,
    DeliveredToTray,
}

// ---------------------------------------------------------------------------
// Hex helper routines
// ---------------------------------------------------------------------------

/// Formats a 16-byte buffer as a 32-character lowercase hex string.
pub fn format_128bit_id_hex(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", byte);
    }
    s
}

/// Parses a 32-character lowercase hex string into a 16-byte array.
pub fn parse_128bit_id_hex(field: &'static str, s: &str) -> Result<[u8; 16], PersistenceError> {
    if s.len() != 32 {
        return Err(PersistenceError::InvalidId {
            field,
            value: s.to_string(),
            reason: "ID string must be exactly 32 characters",
        });
    }
    let mut bytes = [0u8; 16];
    let raw = s.as_bytes();
    for i in 0..16 {
        let hi = match raw[i * 2] {
            b'0'..=b'9' => raw[i * 2] - b'0',
            b'a'..=b'f' => raw[i * 2] - b'a' + 10,
            _ => {
                return Err(PersistenceError::InvalidId {
                    field,
                    value: s.to_string(),
                    reason: "ID contains invalid non-lowercase-hex character",
                });
            }
        };
        let lo = match raw[i * 2 + 1] {
            b'0'..=b'9' => raw[i * 2 + 1] - b'0',
            b'a'..=b'f' => raw[i * 2 + 1] - b'a' + 10,
            _ => {
                return Err(PersistenceError::InvalidId {
                    field,
                    value: s.to_string(),
                    reason: "ID contains invalid non-lowercase-hex character",
                });
            }
        };
        bytes[i] = (hi << 4) | lo;
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// DTO Conversions & Validation
// ---------------------------------------------------------------------------

impl TryFrom<StateFileV1Dto> for PersistentState {
    type Error = PersistenceError;

    fn try_from(dto: StateFileV1Dto) -> Result<Self, Self::Error> {
        if dto.schema_version != STATE_SCHEMA_VERSION_V1 {
            return Err(PersistenceError::UnsupportedSchemaVersion(
                dto.schema_version,
            ));
        }

        let desired_internet_state = match dto.desired_internet_state {
            DesiredInternetStateDto::Unrestricted => DesiredInternetState::Unrestricted,
            DesiredInternetStateDto::Blocked => DesiredInternetState::Blocked,
        };

        let mut seen_timer_ids = HashSet::new();
        let mut active_actions = Vec::with_capacity(dto.active_actions.len());

        for action_dto in dto.active_actions {
            let id_bytes = parse_128bit_id_hex("id", &action_dto.id)?;
            let timer_id = TimerId(id_bytes);

            if !seen_timer_ids.insert(timer_id) {
                return Err(PersistenceError::DuplicateTimerId(timer_id));
            }

            let action_kind = match action_dto.action_kind {
                ActionKindDto::BlockInternet => ActionKind::BlockInternet,
                ActionKindDto::ShutdownComputer => ActionKind::ShutdownComputer,
            };

            let created_by = match action_dto.created_by {
                InitiatorDto::ParentTelegram { user_id } => Initiator::ParentTelegram { user_id },
                InitiatorDto::ParentLocalPin {} => Initiator::ParentLocalPin,
            };

            let mut emitted_thresholds = HashSet::new();
            for th_dto in action_dto.emitted_thresholds {
                let th = match th_dto {
                    WarningThresholdDto::M60 => WarningThreshold::M60,
                    WarningThresholdDto::M30 => WarningThreshold::M30,
                    WarningThresholdDto::M20 => WarningThreshold::M20,
                    WarningThresholdDto::M10 => WarningThreshold::M10,
                    WarningThresholdDto::M3 => WarningThreshold::M3,
                };
                if !emitted_thresholds.insert(th) {
                    return Err(PersistenceError::DuplicateWarningThreshold {
                        timer_id,
                        threshold: th,
                    });
                }
            }

            let execution_state = match action_dto.execution_state {
                ActionExecutionStateDto::Pending {} => ActionExecutionState::Pending,
                ActionExecutionStateDto::Executing {} => ActionExecutionState::Executing,
                ActionExecutionStateDto::Completed {} => ActionExecutionState::Completed,
                ActionExecutionStateDto::Failed { reason } => {
                    ActionExecutionState::Failed { reason }
                }
                ActionExecutionStateDto::Missed {} => ActionExecutionState::Missed,
            };

            active_actions.push(ScheduledAction {
                id: timer_id,
                action_kind,
                deadline: Deadline(UtcDateTime(action_dto.deadline)),
                created_at: UtcDateTime(action_dto.created_at),
                created_by,
                emitted_thresholds,
                execution_state,
            });
        }

        let internet_retry = dto.internet_retry.map(|r| InternetRetry {
            attempt_count: r.attempt_count,
            last_error: r.last_error,
        });

        let mut seen_outbox_ids = HashSet::new();
        let mut telegram_outbox = Vec::with_capacity(dto.telegram_outbox.len());

        for entry_dto in dto.telegram_outbox {
            let entry_id_bytes = parse_128bit_id_hex("entry_id", &entry_dto.entry_id)?;
            let outbox_entry_id = OutboxEntryId(entry_id_bytes);

            if !seen_outbox_ids.insert(outbox_entry_id) {
                return Err(PersistenceError::DuplicateOutboxEntryId(outbox_entry_id));
            }

            let payload = match entry_dto.payload {
                TelegramPayloadDto::Chat { message: msg_dto } => {
                    let msg_id_bytes = parse_128bit_id_hex("message.id", &msg_dto.id)?;
                    let msg_id = MessageId(msg_id_bytes);
                    let sender = match msg_dto.sender {
                        MessageSenderDto::Child => MessageSender::Child,
                        MessageSenderDto::Parent => MessageSender::Parent,
                    };
                    let delivery_status = match msg_dto.delivery_status {
                        DeliveryStatusDto::Pending => DeliveryStatus::Pending,
                        DeliveryStatusDto::AcceptedByService => DeliveryStatus::AcceptedByService,
                        DeliveryStatusDto::AcceptedByTelegram => DeliveryStatus::AcceptedByTelegram,
                        DeliveryStatusDto::DeliveredToTray => DeliveryStatus::DeliveredToTray,
                    };
                    TelegramPayload::Chat {
                        message: ChatMessage {
                            id: msg_id,
                            sender,
                            text: msg_dto.text,
                            timestamp: UtcDateTime(msg_dto.timestamp),
                            delivery_status,
                        },
                    }
                }
                TelegramPayloadDto::ServiceNotification { text } => {
                    TelegramPayload::ServiceNotification { text }
                }
            };

            telegram_outbox.push(TelegramOutboxEntry {
                entry_id: outbox_entry_id,
                payload,
                attempt_count: entry_dto.attempt_count,
                last_error: entry_dto.last_error,
            });
        }

        Ok(PersistentState {
            desired_internet_state,
            active_actions,
            internet_retry,
            telegram_outbox,
        })
    }
}

impl From<&PersistentState> for StateFileV1Dto {
    fn from(state: &PersistentState) -> Self {
        let desired_internet_state = match state.desired_internet_state {
            DesiredInternetState::Unrestricted => DesiredInternetStateDto::Unrestricted,
            DesiredInternetState::Blocked => DesiredInternetStateDto::Blocked,
        };

        let mut active_actions = Vec::with_capacity(state.active_actions.len());
        for action in &state.active_actions {
            let action_kind = match action.action_kind {
                ActionKind::BlockInternet => ActionKindDto::BlockInternet,
                ActionKind::ShutdownComputer => ActionKindDto::ShutdownComputer,
            };

            let created_by = match action.created_by {
                Initiator::ParentTelegram { user_id } => InitiatorDto::ParentTelegram { user_id },
                Initiator::ParentLocalPin => InitiatorDto::ParentLocalPin {},
            };

            let mut emitted_thresholds = Vec::new();
            for th in &action.emitted_thresholds {
                emitted_thresholds.push(match th {
                    WarningThreshold::M60 => WarningThresholdDto::M60,
                    WarningThreshold::M30 => WarningThresholdDto::M30,
                    WarningThreshold::M20 => WarningThresholdDto::M20,
                    WarningThreshold::M10 => WarningThresholdDto::M10,
                    WarningThreshold::M3 => WarningThresholdDto::M3,
                });
            }

            let execution_state = match &action.execution_state {
                ActionExecutionState::Pending => ActionExecutionStateDto::Pending {},
                ActionExecutionState::Executing => ActionExecutionStateDto::Executing {},
                ActionExecutionState::Completed => ActionExecutionStateDto::Completed {},
                ActionExecutionState::Failed { reason } => ActionExecutionStateDto::Failed {
                    reason: reason.clone(),
                },
                ActionExecutionState::Missed => ActionExecutionStateDto::Missed {},
            };

            active_actions.push(ScheduledActionDto {
                id: format_128bit_id_hex(&action.id.0),
                action_kind,
                deadline: action.deadline.0.0,
                created_at: action.created_at.0,
                created_by,
                emitted_thresholds,
                execution_state,
            });
        }

        let internet_retry = state.internet_retry.as_ref().map(|r| InternetRetryDto {
            attempt_count: r.attempt_count,
            last_error: r.last_error.clone(),
        });

        let mut telegram_outbox = Vec::with_capacity(state.telegram_outbox.len());
        for entry in &state.telegram_outbox {
            let payload = match &entry.payload {
                TelegramPayload::Chat { message } => {
                    let sender = match message.sender {
                        MessageSender::Child => MessageSenderDto::Child,
                        MessageSender::Parent => MessageSenderDto::Parent,
                    };
                    let delivery_status = match message.delivery_status {
                        DeliveryStatus::Pending => DeliveryStatusDto::Pending,
                        DeliveryStatus::AcceptedByService => DeliveryStatusDto::AcceptedByService,
                        DeliveryStatus::AcceptedByTelegram => DeliveryStatusDto::AcceptedByTelegram,
                        DeliveryStatus::DeliveredToTray => DeliveryStatusDto::DeliveredToTray,
                        DeliveryStatus::Failed { .. } => DeliveryStatusDto::Pending,
                    };
                    TelegramPayloadDto::Chat {
                        message: ChatMessageDto {
                            id: format_128bit_id_hex(&message.id.0),
                            sender,
                            text: message.text.clone(),
                            timestamp: message.timestamp.0,
                            delivery_status,
                        },
                    }
                }
                TelegramPayload::ServiceNotification { text } => {
                    TelegramPayloadDto::ServiceNotification { text: text.clone() }
                }
            };

            telegram_outbox.push(TelegramOutboxEntryDto {
                entry_id: format_128bit_id_hex(&entry.entry_id.0),
                payload,
                attempt_count: entry.attempt_count,
                last_error: entry.last_error.clone(),
            });
        }

        StateFileV1Dto {
            schema_version: STATE_SCHEMA_VERSION_V1,
            desired_internet_state,
            active_actions,
            internet_retry,
            telegram_outbox,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Decodes and strictly validates a JSON string into `PersistentState`.
pub fn decode_state_json(json_str: &str) -> Result<PersistentState, PersistenceError> {
    let dto: StateFileV1Dto =
        serde_json::from_str(json_str).map_err(|e| PersistenceError::Json(e.to_string()))?;
    PersistentState::try_from(dto)
}

/// Decodes and strictly validates UTF-8 JSON bytes into `PersistentState`.
pub fn decode_state_json_bytes(bytes: &[u8]) -> Result<PersistentState, PersistenceError> {
    let dto: StateFileV1Dto =
        serde_json::from_slice(bytes).map_err(|e| PersistenceError::Json(e.to_string()))?;
    PersistentState::try_from(dto)
}

/// Encodes `PersistentState` into a compact UTF-8 JSON string.
pub fn encode_state_json(state: &PersistentState) -> Result<String, PersistenceError> {
    let dto = StateFileV1Dto::from(state);
    serde_json::to_string(&dto).map_err(|e| PersistenceError::Json(e.to_string()))
}

/// Encodes `PersistentState` into a formatted (pretty) UTF-8 JSON string.
pub fn encode_state_json_pretty(state: &PersistentState) -> Result<String, PersistenceError> {
    let dto = StateFileV1Dto::from(state);
    serde_json::to_string_pretty(&dto).map_err(|e| PersistenceError::Json(e.to_string()))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_timer_id(byte: u8) -> TimerId {
        TimerId([byte; 16])
    }

    fn sample_outbox_id(byte: u8) -> OutboxEntryId {
        OutboxEntryId([byte; 16])
    }

    fn sample_valid_state() -> PersistentState {
        let mut thresholds = HashSet::new();
        thresholds.insert(WarningThreshold::M60);
        thresholds.insert(WarningThreshold::M30);

        PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: vec![ScheduledAction {
                id: sample_timer_id(1),
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(1700000000000)),
                created_at: UtcDateTime(1699996400000),
                created_by: Initiator::ParentTelegram { user_id: 123456789 },
                emitted_thresholds: thresholds,
                execution_state: ActionExecutionState::Pending,
            }],
            internet_retry: Some(InternetRetry {
                attempt_count: 1,
                last_error: Some("WFP call failed".to_string()),
            }),
            telegram_outbox: vec![
                TelegramOutboxEntry {
                    entry_id: sample_outbox_id(2),
                    payload: TelegramPayload::Chat {
                        message: ChatMessage {
                            id: MessageId([3; 16]),
                            sender: MessageSender::Child,
                            text: "Я сделал уроки, можно поиграть?".to_string(),
                            timestamp: UtcDateTime(1700000000000),
                            delivery_status: DeliveryStatus::AcceptedByService,
                        },
                    },
                    attempt_count: 0,
                    last_error: None,
                },
                TelegramOutboxEntry {
                    entry_id: sample_outbox_id(4),
                    payload: TelegramPayload::ServiceNotification {
                        text: "Интернет заблокирован по дедлайну таймера.".to_string(),
                    },
                    attempt_count: 0,
                    last_error: None,
                },
            ],
        }
    }

    #[test]
    fn valid_state_v1_round_trip() {
        let original = sample_valid_state();
        let encoded = encode_state_json(&original).expect("encode should succeed");
        let decoded = decode_state_json(&encoded).expect("decode should succeed");
        assert_eq!(original, decoded);

        let pretty_encoded =
            encode_state_json_pretty(&original).expect("pretty encode should succeed");
        let pretty_decoded =
            decode_state_json(&pretty_encoded).expect("pretty decode should succeed");
        assert_eq!(original, pretty_decoded);
    }

    #[test]
    fn schema_version_not_one_rejected() {
        let json = r#"{
            "schema_version": 2,
            "desired_internet_state": "Unrestricted",
            "active_actions": [],
            "internet_retry": null,
            "telegram_outbox": []
        }"#;

        let err = decode_state_json(json).unwrap_err();
        assert_eq!(err, PersistenceError::UnsupportedSchemaVersion(2));
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let json = r#"{
            "schema_version": 1,
            "desired_internet_state": "Unrestricted",
            "active_actions": [],
            "internet_retry": null,
            "telegram_outbox": [],
            "unknown_extra_field": "disallowed"
        }"#;

        let err = decode_state_json(json).unwrap_err();
        match err {
            PersistenceError::Json(msg) => {
                assert!(msg.contains("unknown field `unknown_extra_field`"))
            }
            other => panic!("expected Json error with unknown field, got: {other:?}"),
        }
    }

    #[test]
    fn observed_internet_state_in_state_json_rejected_as_unknown_field() {
        let json = r#"{
            "schema_version": 1,
            "desired_internet_state": "Blocked",
            "observed_internet_state": "Blocked",
            "active_actions": [],
            "internet_retry": null,
            "telegram_outbox": []
        }"#;

        let err = decode_state_json(json).unwrap_err();
        match err {
            PersistenceError::Json(msg) => {
                assert!(msg.contains("unknown field `observed_internet_state`"))
            }
            other => panic!("expected unknown field error, got: {other:?}"),
        }
    }

    #[test]
    fn unknown_nested_field_rejected() {
        let json = r#"{
            "schema_version": 1,
            "desired_internet_state": "Blocked",
            "active_actions": [
                {
                    "id": "018f3a5b6c7d8e9f0a1b2c3d4e5f6071",
                    "action_kind": "BlockInternet",
                    "deadline": 1700000000000,
                    "created_at": 1699996400000,
                    "created_by": {
                        "kind": "ParentLocalPin",
                        "extra_nested_field": 123
                    },
                    "emitted_thresholds": [],
                    "execution_state": { "kind": "Pending" }
                }
            ],
            "internet_retry": null,
            "telegram_outbox": []
        }"#;

        let err = decode_state_json(json).unwrap_err();
        match err {
            PersistenceError::Json(msg) => {
                assert!(msg.contains("unknown field `extra_nested_field`"))
            }
            other => panic!("expected unknown field error, got: {other:?}"),
        }
    }

    #[test]
    fn invalid_enum_rejected() {
        let json = r#"{
            "schema_version": 1,
            "desired_internet_state": "InvalidStateName",
            "active_actions": [],
            "internet_retry": null,
            "telegram_outbox": []
        }"#;

        let err = decode_state_json(json).unwrap_err();
        match err {
            PersistenceError::Json(msg) => {
                assert!(msg.contains("unknown variant `InvalidStateName`"))
            }
            other => panic!("expected unknown variant error, got: {other:?}"),
        }
    }

    #[test]
    fn malformed_timer_id_rejected() {
        // Uppercase hex
        let json_uppercase = r#"{
            "schema_version": 1,
            "desired_internet_state": "Blocked",
            "active_actions": [
                {
                    "id": "018F3A5B6C7D8E9F0A1B2C3D4E5F6071",
                    "action_kind": "BlockInternet",
                    "deadline": 1700000000000,
                    "created_at": 1699996400000,
                    "created_by": { "kind": "ParentLocalPin" },
                    "emitted_thresholds": [],
                    "execution_state": { "kind": "Pending" }
                }
            ],
            "internet_retry": null,
            "telegram_outbox": []
        }"#;

        let err = decode_state_json(json_uppercase).unwrap_err();
        match err {
            PersistenceError::InvalidId { field, .. } => assert_eq!(field, "id"),
            other => panic!("expected InvalidId error, got: {other:?}"),
        }

        // Too short
        let json_short = r#"{
            "schema_version": 1,
            "desired_internet_state": "Blocked",
            "active_actions": [
                {
                    "id": "018f3a5b",
                    "action_kind": "BlockInternet",
                    "deadline": 1700000000000,
                    "created_at": 1699996400000,
                    "created_by": { "kind": "ParentLocalPin" },
                    "emitted_thresholds": [],
                    "execution_state": { "kind": "Pending" }
                }
            ],
            "internet_retry": null,
            "telegram_outbox": []
        }"#;

        let err2 = decode_state_json(json_short).unwrap_err();
        match err2 {
            PersistenceError::InvalidId { field, .. } => assert_eq!(field, "id"),
            other => panic!("expected InvalidId error, got: {other:?}"),
        }
    }

    #[test]
    fn duplicate_timer_id_rejected() {
        let json = r#"{
            "schema_version": 1,
            "desired_internet_state": "Blocked",
            "active_actions": [
                {
                    "id": "018f3a5b6c7d8e9f0a1b2c3d4e5f6071",
                    "action_kind": "BlockInternet",
                    "deadline": 1700000000000,
                    "created_at": 1699996400000,
                    "created_by": { "kind": "ParentLocalPin" },
                    "emitted_thresholds": [],
                    "execution_state": { "kind": "Pending" }
                },
                {
                    "id": "018f3a5b6c7d8e9f0a1b2c3d4e5f6071",
                    "action_kind": "ShutdownComputer",
                    "deadline": 1700000005000,
                    "created_at": 1699996400000,
                    "created_by": { "kind": "ParentLocalPin" },
                    "emitted_thresholds": [],
                    "execution_state": { "kind": "Pending" }
                }
            ],
            "internet_retry": null,
            "telegram_outbox": []
        }"#;

        let err = decode_state_json(json).unwrap_err();
        let expected_id =
            TimerId(parse_128bit_id_hex("id", "018f3a5b6c7d8e9f0a1b2c3d4e5f6071").unwrap());
        assert_eq!(err, PersistenceError::DuplicateTimerId(expected_id));
    }

    #[test]
    fn duplicate_telegram_outbox_entry_id_rejected() {
        let json = r#"{
            "schema_version": 1,
            "desired_internet_state": "Unrestricted",
            "active_actions": [],
            "internet_retry": null,
            "telegram_outbox": [
                {
                    "entry_id": "018f3a5b6c7d8e9f0a1b2c3d4e5f6081",
                    "payload": {
                        "kind": "ServiceNotification",
                        "text": "First notification"
                    },
                    "attempt_count": 0,
                    "last_error": null
                },
                {
                    "entry_id": "018f3a5b6c7d8e9f0a1b2c3d4e5f6081",
                    "payload": {
                        "kind": "ServiceNotification",
                        "text": "Second notification"
                    },
                    "attempt_count": 0,
                    "last_error": null
                }
            ]
        }"#;

        let err = decode_state_json(json).unwrap_err();
        let expected_id = OutboxEntryId(
            parse_128bit_id_hex("entry_id", "018f3a5b6c7d8e9f0a1b2c3d4e5f6081").unwrap(),
        );
        assert_eq!(err, PersistenceError::DuplicateOutboxEntryId(expected_id));
    }

    #[test]
    fn failed_execution_state_reason_round_trip() {
        let state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: vec![ScheduledAction {
                id: sample_timer_id(10),
                action_kind: ActionKind::ShutdownComputer,
                deadline: Deadline(UtcDateTime(1700000000000)),
                created_at: UtcDateTime(1699996400000),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: HashSet::new(),
                execution_state: ActionExecutionState::Failed {
                    reason: "Access denied from power management controller".to_string(),
                },
            }],
            internet_retry: None,
            telegram_outbox: vec![],
        };

        let encoded = encode_state_json(&state).unwrap();
        let decoded = decode_state_json(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn all_warning_threshold_names_round_trip() {
        let mut thresholds = HashSet::new();
        for th in WarningThreshold::ALL {
            thresholds.insert(th);
        }

        let state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![ScheduledAction {
                id: sample_timer_id(20),
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(1700000000000)),
                created_at: UtcDateTime(1699996400000),
                created_by: Initiator::ParentTelegram { user_id: 999888777 },
                emitted_thresholds: thresholds,
                execution_state: ActionExecutionState::Executing,
            }],
            internet_retry: None,
            telegram_outbox: vec![],
        };

        let encoded = encode_state_json(&state).unwrap();
        let decoded = decode_state_json(&encoded).unwrap();
        assert_eq!(state, decoded);
        assert_eq!(decoded.active_actions[0].emitted_thresholds.len(), 5);
    }

    #[test]
    fn parent_telegram_user_id_round_trip() {
        let state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![ScheduledAction {
                id: sample_timer_id(30),
                action_kind: ActionKind::ShutdownComputer,
                deadline: Deadline(UtcDateTime(1700000000000)),
                created_at: UtcDateTime(1699996400000),
                created_by: Initiator::ParentTelegram {
                    user_id: 18446744073709551615,
                },
                emitted_thresholds: HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            }],
            internet_retry: None,
            telegram_outbox: vec![],
        };

        let encoded = encode_state_json(&state).unwrap();
        let decoded = decode_state_json(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn chat_outbox_round_trip() {
        let state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![],
            internet_retry: None,
            telegram_outbox: vec![TelegramOutboxEntry {
                entry_id: sample_outbox_id(40),
                payload: TelegramPayload::Chat {
                    message: ChatMessage {
                        id: MessageId([50; 16]),
                        sender: MessageSender::Child,
                        text: "Папа, включи интернет пожалуйста!".to_string(),
                        timestamp: UtcDateTime(1700000000000),
                        delivery_status: DeliveryStatus::AcceptedByService,
                    },
                },
                attempt_count: 3,
                last_error: Some("HTTP 502 Bad Gateway".to_string()),
            }],
        };

        let encoded = encode_state_json(&state).unwrap();
        let decoded = decode_state_json(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn service_notification_outbox_round_trip() {
        let state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: vec![],
            internet_retry: Some(InternetRetry {
                attempt_count: 5,
                last_error: Some("WFP engine disconnected".to_string()),
            }),
            telegram_outbox: vec![TelegramOutboxEntry {
                entry_id: sample_outbox_id(60),
                payload: TelegramPayload::ServiceNotification {
                    text: "Компьютер выключен по расписанию.".to_string(),
                },
                attempt_count: 0,
                last_error: None,
            }],
        };

        let encoded = encode_state_json(&state).unwrap();
        let decoded = decode_state_json(&encoded).unwrap();
        assert_eq!(state, decoded);
    }
}
