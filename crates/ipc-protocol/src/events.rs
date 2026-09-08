//! Closed allowlist of V1 IPC event stream notifications.

use serde::{Deserialize, Serialize};

use crate::ids::WireTimerId;
use crate::snapshot::{
    ActionKindDto, ChatMessageDto, DesiredInternetStateDto, InternetStateDto, ScheduledActionDto,
    ServiceHealthDto, ShutdownStateDto, StateChangeReasonDto, WarningEventDto,
};

/// Closed allowlist of events eligible for IPC stream transmission in V1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum EventPayload {
    /// Desired or observed Internet filtering policy has changed.
    InternetPolicyChanged {
        desired: DesiredInternetStateDto,
        observed: InternetStateDto,
        reason: StateChangeReasonDto,
    },
    /// Operating system shutdown execution state has transitioned.
    ShutdownStateChanged {
        previous: ShutdownStateDto,
        current: ShutdownStateDto,
    },
    /// A new action timer has been scheduled.
    TimerScheduled { action: ScheduledActionDto },
    /// An action timer has been cancelled.
    TimerCancelled {
        id: WireTimerId,
        action_kind: ActionKindDto,
    },
    /// An action timer has expired.
    TimerExpired {
        id: WireTimerId,
        action_kind: ActionKindDto,
    },
    /// A pre-deadline warning threshold has been reached.
    WarningThresholdReached { event: WarningEventDto },
    /// An action deadline was missed (e.g. past-deadline shutdown skipped on recovery).
    MissedDeadlineOccurred {
        action: ScheduledActionDto,
        reason: String,
    },
    /// A new chat message was received.
    ChatMessageReceived { message: ChatMessageDto },
    /// Service health status or diagnostics updated.
    ServiceHealthUpdated { health: ServiceHealthDto },
}

/// Converts a domain event from `palka-core` into a V1 wire event, enforcing the strict allowlist.
///
/// Explicitly excludes `PinAuthenticationResult` and non-whitelisted lifecycle events.
pub fn try_from_core_event(event: &palka_core::Event) -> Option<EventPayload> {
    match event {
        palka_core::Event::InternetPolicyChanged {
            desired,
            observed,
            reason,
        } => Some(EventPayload::InternetPolicyChanged {
            desired: (*desired).into(),
            observed: (*observed).into(),
            reason: reason.into(),
        }),
        palka_core::Event::ShutdownStateChanged { previous, current } => {
            Some(EventPayload::ShutdownStateChanged {
                previous: (*previous).into(),
                current: (*current).into(),
            })
        }
        palka_core::Event::TimerScheduled { action } => Some(EventPayload::TimerScheduled {
            action: action.into(),
        }),
        palka_core::Event::TimerCancelled { id, action_kind } => {
            Some(EventPayload::TimerCancelled {
                id: WireTimerId::from_core(*id),
                action_kind: (*action_kind).into(),
            })
        }
        palka_core::Event::TimerExpired { id, action_kind } => Some(EventPayload::TimerExpired {
            id: WireTimerId::from_core(*id),
            action_kind: (*action_kind).into(),
        }),
        palka_core::Event::WarningThresholdReached { event } => {
            Some(EventPayload::WarningThresholdReached {
                event: event.into(),
            })
        }
        palka_core::Event::MissedDeadlineOccurred { action, reason } => {
            Some(EventPayload::MissedDeadlineOccurred {
                action: action.into(),
                reason: reason.clone(),
            })
        }
        palka_core::Event::ChatMessageReceived { message } => {
            Some(EventPayload::ChatMessageReceived {
                message: message.into(),
            })
        }
        palka_core::Event::ServiceHealthUpdated { health } => {
            Some(EventPayload::ServiceHealthUpdated {
                health: health.into(),
            })
        }
        // Explicitly excluded from wire stream (IPC-91)
        palka_core::Event::PinAuthenticationResult { .. } => None,
        // Excluded from client stream
        palka_core::Event::ServiceLifecycleEvent { .. } => None,
    }
}
