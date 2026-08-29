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
    /// All fixed warning thresholds in deterministic descending order.
    pub const ALL: [Self; 5] = [Self::M60, Self::M30, Self::M20, Self::M10, Self::M3];

    /// Returns the threshold duration in seconds.
    pub const fn seconds(self) -> u32 {
        self as u32
    }
}

/// Returns the set of thresholds that are already in the past when creating a timer of `duration_seconds`.
///
/// A threshold is passed if `duration_seconds < threshold.seconds()`.
/// Exact equality (`duration_seconds == threshold.seconds()`) is NOT considered passed.
pub fn creation_passed_thresholds(duration_seconds: u64) -> HashSet<WarningThreshold> {
    let mut passed = HashSet::new();
    for threshold in WarningThreshold::ALL {
        if duration_seconds < threshold.seconds() as u64 {
            passed.insert(threshold);
        }
    }
    passed
}

/// Returns the threshold that is due immediately upon timer creation because `duration_seconds == threshold.seconds()`.
///
/// Under the fixed contract, at most one threshold can match.
/// The returned list follows descending order: M60, M30, M20, M10, M3.
pub fn creation_due_thresholds(duration_seconds: u64) -> Vec<WarningThreshold> {
    let mut due = Vec::new();
    for threshold in WarningThreshold::ALL {
        if duration_seconds == threshold.seconds() as u64 {
            due.push(threshold);
        }
    }
    due
}

/// Determines which warning thresholds were crossed between `previous_remaining_seconds` and `current_remaining_seconds`.
///
/// If `current_remaining_seconds <= 0`, deadline execution has priority and no warning thresholds are returned.
/// Thresholds already in `emitted_thresholds` are not returned.
/// Returned in deterministic descending order (M60 .. M3).
pub fn crossed_warning_thresholds(
    previous_remaining_seconds: i64,
    current_remaining_seconds: i64,
    emitted_thresholds: &HashSet<WarningThreshold>,
) -> Vec<WarningThreshold> {
    if current_remaining_seconds <= 0 {
        return Vec::new();
    }

    let mut crossed = Vec::new();
    for threshold in WarningThreshold::ALL {
        let threshold_seconds = threshold.seconds() as i64;
        if previous_remaining_seconds > threshold_seconds
            && current_remaining_seconds <= threshold_seconds
            && !emitted_thresholds.contains(&threshold)
        {
            crossed.push(threshold);
        }
    }
    crossed
}

/// For a timer recovered before its deadline (`current_remaining_seconds > 0`), returns all not-yet-emitted
/// thresholds satisfying `threshold.seconds() >= current_remaining_seconds`.
///
/// These thresholds were crossed while the service was unavailable and must be marked as passed without retroactive warning emission.
/// If `current_remaining_seconds <= 0`, deadline handling has priority and an empty vector is returned.
/// Returned in deterministic descending order (M60 .. M3).
pub fn recovery_passed_thresholds(
    current_remaining_seconds: i64,
    emitted_thresholds: &HashSet<WarningThreshold>,
) -> Vec<WarningThreshold> {
    if current_remaining_seconds <= 0 {
        return Vec::new();
    }

    let mut passed = Vec::new();
    for threshold in WarningThreshold::ALL {
        let threshold_seconds = threshold.seconds() as i64;
        if threshold_seconds >= current_remaining_seconds
            && !emitted_thresholds.contains(&threshold)
        {
            passed.push(threshold);
        }
    }
    passed
}

/// Predicate checking if the deadline condition is reached (`current_remaining_seconds <= 0`).
pub const fn deadline_is_due(current_remaining_seconds: i64) -> bool {
    current_remaining_seconds <= 0
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

    #[test]
    fn creation_above_all_thresholds_marks_none_passed() {
        let duration = 61 * 60;
        let passed = creation_passed_thresholds(duration);
        assert!(passed.is_empty());
    }

    #[test]
    fn creation_equal_m30_marks_m60_passed_and_m30_due() {
        let duration = 30 * 60;
        let passed = creation_passed_thresholds(duration);
        let due = creation_due_thresholds(duration);

        assert_eq!(passed.len(), 1);
        assert!(passed.contains(&WarningThreshold::M60));
        assert!(!passed.contains(&WarningThreshold::M30));

        assert_eq!(due, vec![WarningThreshold::M30]);
    }

    #[test]
    fn creation_shorter_than_thresholds_marks_only_past_thresholds() {
        let duration = 18 * 60;
        let passed = creation_passed_thresholds(duration);
        let due = creation_due_thresholds(duration);

        assert_eq!(passed.len(), 3);
        assert!(passed.contains(&WarningThreshold::M60));
        assert!(passed.contains(&WarningThreshold::M30));
        assert!(passed.contains(&WarningThreshold::M20));
        assert!(!passed.contains(&WarningThreshold::M10));
        assert!(!passed.contains(&WarningThreshold::M3));

        assert!(due.is_empty());
    }

    #[test]
    fn crossing_jump_over_m10_returns_m10() {
        let previous = 605;
        let current = 598;
        let emitted = HashSet::new();

        let crossed = crossed_warning_thresholds(previous, current, &emitted);
        assert_eq!(crossed, vec![WarningThreshold::M10]);
    }

    #[test]
    fn crossing_exactly_to_threshold_returns_threshold() {
        let previous = 601;
        let current = 600;
        let emitted = HashSet::new();

        let crossed = crossed_warning_thresholds(previous, current, &emitted);
        assert_eq!(crossed, vec![WarningThreshold::M10]);
    }

    #[test]
    fn crossing_does_not_repeat_emitted_threshold() {
        let previous = 605;
        let current = 598;
        let mut emitted = HashSet::new();
        emitted.insert(WarningThreshold::M10);

        let crossed = crossed_warning_thresholds(previous, current, &emitted);
        assert!(crossed.is_empty());
    }

    #[test]
    fn crossing_multiple_thresholds_is_deterministic() {
        let previous = 3700;
        let current = 100;
        let emitted = HashSet::new();

        let crossed = crossed_warning_thresholds(previous, current, &emitted);
        assert_eq!(
            crossed,
            vec![
                WarningThreshold::M60,
                WarningThreshold::M30,
                WarningThreshold::M20,
                WarningThreshold::M10,
                WarningThreshold::M3,
            ]
        );
    }

    #[test]
    fn deadline_priority_at_zero_suppresses_all_warnings() {
        let previous = 3700;
        let current = 0;
        let emitted = HashSet::new();

        let crossed = crossed_warning_thresholds(previous, current, &emitted);
        assert!(crossed.is_empty());
    }

    #[test]
    fn deadline_priority_after_deadline_suppresses_all_warnings() {
        let previous = 3700;
        let current = -1;
        let emitted = HashSet::new();

        let crossed = crossed_warning_thresholds(previous, current, &emitted);
        assert!(crossed.is_empty());
    }

    #[test]
    fn recovery_at_eighteen_minutes_marks_m60_m30_m20_passed() {
        let current = 1080;
        let emitted = HashSet::new();

        let passed = recovery_passed_thresholds(current, &emitted);
        assert_eq!(
            passed,
            vec![
                WarningThreshold::M60,
                WarningThreshold::M30,
                WarningThreshold::M20,
            ]
        );
    }

    #[test]
    fn recovery_preserves_already_emitted_information() {
        let current = 1080;
        let mut emitted = HashSet::new();
        emitted.insert(WarningThreshold::M30);

        let passed = recovery_passed_thresholds(current, &emitted);
        assert_eq!(passed, vec![WarningThreshold::M60, WarningThreshold::M20]);
    }

    #[test]
    fn recovery_keeps_lower_future_thresholds_eligible() {
        let current = 1080;
        let emitted = HashSet::new();

        let passed = recovery_passed_thresholds(current, &emitted);
        assert!(!passed.contains(&WarningThreshold::M10));
        assert!(!passed.contains(&WarningThreshold::M3));
    }

    #[test]
    fn recovery_at_deadline_returns_no_warning_transitions() {
        let current = 0;
        let emitted = HashSet::new();

        let passed = recovery_passed_thresholds(current, &emitted);
        assert!(passed.is_empty());
    }

    #[test]
    fn deadline_predicate_matches_contract_boundary() {
        assert!(!deadline_is_due(1));
        assert!(deadline_is_due(0));
        assert!(deadline_is_due(-1));
    }
}
