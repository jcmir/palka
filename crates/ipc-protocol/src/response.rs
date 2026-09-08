//! Closed set of V1 IPC success responses and payloads.

use serde::{Deserialize, Serialize};

use crate::ids::{WireMessageId, WireTimerId};
use crate::snapshot::StatusSnapshotDto;

/// Canonical outcome vocabulary for timer cancellation requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CancellationResult {
    /// Timer existed, matched expected kind, and was successfully cancelled.
    Cancelled,
    /// Timer with specified ID does not exist or has already elapsed/been cancelled.
    AlreadyAbsent,
    /// Timer exists but belongs to a different action kind.
    TimerKindMismatch,
}

/// Closed set of success responses supported by PALKA IPC V1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ResponsePayload {
    /// System status snapshot returned in response to QueryStatus.
    Status { snapshot: StatusSnapshotDto },
    /// PIN verification success indicator with authorized TTL.
    PinVerified { expires_in_seconds: u32 },
    /// Initial subscription snapshot returned in response to SubscribeEvents.
    Subscribed { snapshot: StatusSnapshotDto },
    /// Acknowledgment of child message acceptance into service outbox.
    AcceptedByService { message_id: WireMessageId },
    /// Notification of scheduled timer ID for delayed actions.
    TimerScheduled { timer_id: WireTimerId },
    /// Generic positive acknowledgment for immediate actions.
    Acknowledged,
    /// Detailed cancellation outcome for timer cancellation requests.
    TimerCancellation { result: CancellationResult },
}
