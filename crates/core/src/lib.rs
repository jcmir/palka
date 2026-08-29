//! Pure domain crate for PALKA.

use std::collections::HashSet;
use std::fmt;
use zeroize::Zeroizing;

/// Absolute moment in time represented as a UTC Unix timestamp in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDateTime(pub i64);

/// Target execution deadline in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Deadline(pub UtcDateTime);

/// Opaque 128-bit identifier for scheduled actions and timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerId(pub [u8; 16]);

/// Opaque 128-bit identifier for chat messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(pub [u8; 16]);

/// Supported actions available for scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    BlockInternet,
    ShutdownComputer,
}

/// Fixed discrete warning intervals prior to a deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum WarningThreshold {
    M60 = 3600,
    M30 = 1800,
    M20 = 1200,
    M10 = 600,
    M3 = 180,
}

impl WarningThreshold {
    /// Returns the threshold duration in seconds.
    pub const fn seconds(self) -> u32 {
        self as u32
    }
}

/// Subject that initiated an action or policy change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Initiator {
    ParentTelegram { user_id: u64 },
    ParentLocalPin,
}

/// Execution lifecycle state for a scheduled action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionExecutionState {
    Pending,
    Executing,
    Completed,
    Failed { reason: String },
    Missed,
}

/// Active or processing scheduled action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledAction {
    pub id: TimerId,
    pub action_kind: ActionKind,
    pub deadline: Deadline,
    pub created_at: UtcDateTime,
    pub created_by: Initiator,
    pub emitted_thresholds: HashSet<WarningThreshold>,
    pub execution_state: ActionExecutionState,
}

/// Warning event emitted when a threshold is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WarningEvent {
    pub timer_id: TimerId,
    pub action_kind: ActionKind,
    pub threshold: WarningThreshold,
    pub deadline: Deadline,
    pub emitted_at: UtcDateTime,
}

/// Authoritative desired policy for Internet access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesiredInternetState {
    Unrestricted,
    Blocked,
}

/// Physically observed state of the Internet filtering gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InternetState {
    Unrestricted,
    Blocked,
    Unknown,
}

/// Volatile OS shutdown execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShutdownState {
    Idle,
    Scheduled,
    InProgress,
}

/// Reason triggering an Internet policy transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateChangeReason {
    TimerExpired { timer_id: TimerId },
    ImmediateCommand { initiator: Initiator },
    ManualRestore { initiator: Initiator },
    StartupRestoration,
    PlatformSync,
}

/// Owning wrapper for PIN data that zeroes memory on drop and prevents secret leakage.
pub struct SensitivePinString {
    secret: Zeroizing<String>,
}

impl SensitivePinString {
    /// Creates a new protected PIN container.
    pub fn new(value: String) -> Self {
        Self {
            secret: Zeroizing::new(value),
        }
    }

    /// Borrows the contained PIN string.
    pub fn as_str(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for SensitivePinString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SensitivePinString([REDACTED])")
    }
}

/// Sender of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageSender {
    Parent,
    Child,
}

/// Delivery status of a chat message across transport hops.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeliveryStatus {
    Pending,
    AcceptedByService,
    AcceptedByTelegram,
    DeliveredToTray,
    Failed { reason: String },
}

/// Text chat message exchanged between parent and child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: MessageId,
    pub sender: MessageSender,
    pub text: String,
    pub timestamp: UtcDateTime,
    pub delivery_status: DeliveryStatus,
}

/// Health status category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Critical,
}

/// Comprehensive health diagnostics of the service daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceHealth {
    pub status: HealthStatus,
    pub uptime_seconds: u64,
    pub internet_gate_healthy: bool,
    pub persistence_healthy: bool,
    pub telegram_connected: bool,
    pub active_tray_sessions: u32,
    pub last_error: Option<String>,
}

/// Lifecycle stage of the service daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceLifecycleStage {
    ServiceStarted,
    ServiceReady,
}

/// Aggregated system status snapshot provided to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub desired_internet_state: DesiredInternetState,
    pub observed_internet_state: InternetState,
    pub shutdown_state: ShutdownState,
    pub active_actions: Vec<ScheduledAction>,
    pub health: ServiceHealth,
    pub target_child_sid: String,
    pub timestamp: UtcDateTime,
}

/// Domain management commands.
#[derive(Debug)]
pub enum Command {
    ScheduleInternetBlock {
        duration_minutes: u32,
        initiator: Initiator,
    },
    CancelInternetBlockTimer {
        initiator: Initiator,
    },
    RestoreInternet {
        initiator: Initiator,
    },
    ImmediateInternetBlock {
        initiator: Initiator,
    },
    ScheduleShutdown {
        duration_minutes: u32,
        initiator: Initiator,
    },
    CancelShutdownTimer {
        initiator: Initiator,
    },
    SendChildMessage {
        text: String,
    },
    SendParentMessage {
        text: String,
    },
    VerifyPin {
        pin_attempt: SensitivePinString,
    },
    QueryStatus,
}

/// Domain events emitted by the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    InternetPolicyChanged {
        desired: DesiredInternetState,
        observed: InternetState,
        reason: StateChangeReason,
    },
    ShutdownStateChanged {
        previous: ShutdownState,
        current: ShutdownState,
    },
    TimerScheduled {
        action: ScheduledAction,
    },
    TimerCancelled {
        id: TimerId,
        action_kind: ActionKind,
    },
    TimerExpired {
        id: TimerId,
        action_kind: ActionKind,
    },
    WarningThresholdReached {
        event: WarningEvent,
    },
    MissedDeadlineOccurred {
        action: ScheduledAction,
        reason: String,
    },
    ChatMessageReceived {
        message: ChatMessage,
    },
    PinAuthenticationResult {
        success: bool,
        lock_timeout_seconds: Option<u32>,
    },
    ServiceHealthUpdated {
        health: ServiceHealth,
    },
    ServiceLifecycleEvent {
        stage: ServiceLifecycleStage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_threshold_values_are_contract_exact() {
        assert_eq!(WarningThreshold::M60.seconds(), 3600);
        assert_eq!(WarningThreshold::M30.seconds(), 1800);
        assert_eq!(WarningThreshold::M20.seconds(), 1200);
        assert_eq!(WarningThreshold::M10.seconds(), 600);
        assert_eq!(WarningThreshold::M3.seconds(), 180);

        assert_eq!(WarningThreshold::M60 as u32, 3600);
        assert_eq!(WarningThreshold::M30 as u32, 1800);
        assert_eq!(WarningThreshold::M20 as u32, 1200);
        assert_eq!(WarningThreshold::M10 as u32, 600);
        assert_eq!(WarningThreshold::M3 as u32, 180);
    }

    #[test]
    fn identifiers_are_exactly_128_bit() {
        assert_eq!(std::mem::size_of::<TimerId>(), 16);
        assert_eq!(std::mem::size_of::<MessageId>(), 16);
    }

    #[test]
    fn status_snapshot_can_represent_desired_blocked_observed_unrestricted() {
        let snapshot = StatusSnapshot {
            desired_internet_state: DesiredInternetState::Blocked,
            observed_internet_state: InternetState::Unrestricted,
            shutdown_state: ShutdownState::Idle,
            active_actions: Vec::new(),
            health: ServiceHealth {
                status: HealthStatus::Healthy,
                uptime_seconds: 100,
                internet_gate_healthy: true,
                persistence_healthy: true,
                telegram_connected: true,
                active_tray_sessions: 1,
                last_error: None,
            },
            target_child_sid: "S-1-5-21-test".to_string(),
            timestamp: UtcDateTime(1700000000000),
        };

        assert_eq!(
            snapshot.desired_internet_state,
            DesiredInternetState::Blocked
        );
        assert_eq!(
            snapshot.observed_internet_state,
            InternetState::Unrestricted
        );
        assert_ne!(
            snapshot.desired_internet_state as u8,
            snapshot.observed_internet_state as u8
        );
    }

    #[test]
    fn status_snapshot_can_represent_desired_unrestricted_observed_blocked() {
        let snapshot = StatusSnapshot {
            desired_internet_state: DesiredInternetState::Unrestricted,
            observed_internet_state: InternetState::Blocked,
            shutdown_state: ShutdownState::Idle,
            active_actions: Vec::new(),
            health: ServiceHealth {
                status: HealthStatus::Degraded,
                uptime_seconds: 200,
                internet_gate_healthy: false,
                persistence_healthy: true,
                telegram_connected: false,
                active_tray_sessions: 0,
                last_error: Some("Gate sync pending".to_string()),
            },
            target_child_sid: "S-1-5-21-test".to_string(),
            timestamp: UtcDateTime(1700000000000),
        };

        assert_eq!(
            snapshot.desired_internet_state,
            DesiredInternetState::Unrestricted
        );
        assert_eq!(snapshot.observed_internet_state, InternetState::Blocked);
        assert_ne!(
            snapshot.desired_internet_state as u8,
            snapshot.observed_internet_state as u8
        );
    }

    #[test]
    fn sensitive_pin_string_exposes_borrowed_value() {
        let pin = SensitivePinString::new("1234".to_string());
        assert_eq!(pin.as_str(), "1234");
    }
}
