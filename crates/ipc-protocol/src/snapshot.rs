//! Wire DTO mapping for system status snapshot and domain entities.

use serde::{Deserialize, Serialize};

use crate::ids::{WireMessageId, WireTimerId};

/// Desired Internet filtering policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DesiredInternetStateDto {
    Unrestricted,
    Blocked,
}

impl From<palka_core::DesiredInternetState> for DesiredInternetStateDto {
    fn from(s: palka_core::DesiredInternetState) -> Self {
        match s {
            palka_core::DesiredInternetState::Unrestricted => Self::Unrestricted,
            palka_core::DesiredInternetState::Blocked => Self::Blocked,
        }
    }
}

/// Observed state of the physical filtering gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InternetStateDto {
    Unknown,
    Unrestricted,
    Blocked,
}

impl From<palka_core::InternetState> for InternetStateDto {
    fn from(s: palka_core::InternetState) -> Self {
        match s {
            palka_core::InternetState::Unknown => Self::Unknown,
            palka_core::InternetState::Unrestricted => Self::Unrestricted,
            palka_core::InternetState::Blocked => Self::Blocked,
        }
    }
}

/// Volatile OS shutdown execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShutdownStateDto {
    Idle,
    Scheduled,
    InProgress,
}

impl From<palka_core::ShutdownState> for ShutdownStateDto {
    fn from(s: palka_core::ShutdownState) -> Self {
        match s {
            palka_core::ShutdownState::Idle => Self::Idle,
            palka_core::ShutdownState::Scheduled => Self::Scheduled,
            palka_core::ShutdownState::InProgress => Self::InProgress,
        }
    }
}

/// Health status category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HealthStatusDto {
    Healthy,
    Degraded,
    Critical,
}

impl From<palka_core::HealthStatus> for HealthStatusDto {
    fn from(h: palka_core::HealthStatus) -> Self {
        match h {
            palka_core::HealthStatus::Healthy => Self::Healthy,
            palka_core::HealthStatus::Degraded => Self::Degraded,
            palka_core::HealthStatus::Critical => Self::Critical,
        }
    }
}

/// Service health diagnostics DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceHealthDto {
    pub status: HealthStatusDto,
    pub uptime_seconds: u64,
    pub internet_gate_healthy: bool,
    pub persistence_healthy: bool,
    pub telegram_connected: bool,
    pub active_tray_sessions: u32,
    #[serde(deserialize_with = "crate::codec::deserialize_required_nullable")]
    pub last_error: Option<String>,
}

impl From<&palka_core::ServiceHealth> for ServiceHealthDto {
    fn from(h: &palka_core::ServiceHealth) -> Self {
        Self {
            status: h.status.into(),
            uptime_seconds: h.uptime_seconds,
            internet_gate_healthy: h.internet_gate_healthy,
            persistence_healthy: h.persistence_healthy,
            telegram_connected: h.telegram_connected,
            active_tray_sessions: h.active_tray_sessions,
            last_error: h.last_error.clone(),
        }
    }
}

/// Supported scheduled action kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionKindDto {
    BlockInternet,
    ShutdownComputer,
}

impl From<palka_core::ActionKind> for ActionKindDto {
    fn from(k: palka_core::ActionKind) -> Self {
        match k {
            palka_core::ActionKind::BlockInternet => Self::BlockInternet,
            palka_core::ActionKind::ShutdownComputer => Self::ShutdownComputer,
        }
    }
}

/// Initiator of a command or state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InitiatorDto {
    ParentTelegram { user_id: u64 },
    ParentLocalPin,
}

impl From<palka_core::Initiator> for InitiatorDto {
    fn from(i: palka_core::Initiator) -> Self {
        match i {
            palka_core::Initiator::ParentTelegram { user_id } => Self::ParentTelegram { user_id },
            palka_core::Initiator::ParentLocalPin => Self::ParentLocalPin,
        }
    }
}

/// Lifecycle execution state of a scheduled action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum ActionExecutionStateDto {
    Pending,
    Executing,
    Completed,
    Failed { reason: String },
    Missed,
}

impl From<&palka_core::ActionExecutionState> for ActionExecutionStateDto {
    fn from(s: &palka_core::ActionExecutionState) -> Self {
        match s {
            palka_core::ActionExecutionState::Pending => Self::Pending,
            palka_core::ActionExecutionState::Executing => Self::Executing,
            palka_core::ActionExecutionState::Completed => Self::Completed,
            palka_core::ActionExecutionState::Failed { reason } => Self::Failed {
                reason: reason.clone(),
            },
            palka_core::ActionExecutionState::Missed => Self::Missed,
        }
    }
}

/// Scheduled action DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledActionDto {
    pub id: WireTimerId,
    pub action_kind: ActionKindDto,
    pub deadline: i64,
    pub created_at: i64,
    pub created_by: InitiatorDto,
    pub emitted_thresholds: Vec<u32>,
    pub execution_state: ActionExecutionStateDto,
}

impl From<&palka_core::ScheduledAction> for ScheduledActionDto {
    fn from(a: &palka_core::ScheduledAction) -> Self {
        let mut thresholds: Vec<u32> = a.emitted_thresholds.iter().map(|t| t.seconds()).collect();
        thresholds.sort_unstable();
        Self {
            id: WireTimerId::from_core(a.id),
            action_kind: a.action_kind.into(),
            deadline: a.deadline.0.0,
            created_at: a.created_at.0,
            created_by: a.created_by.into(),
            emitted_thresholds: thresholds,
            execution_state: (&a.execution_state).into(),
        }
    }
}

/// Aggregated system status snapshot DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusSnapshotDto {
    pub desired_internet_state: DesiredInternetStateDto,
    pub observed_internet_state: InternetStateDto,
    pub shutdown_state: ShutdownStateDto,
    pub active_actions: Vec<ScheduledActionDto>,
    pub health: ServiceHealthDto,
    pub target_child_sid: String,
    pub timestamp: i64,
}

impl From<&palka_core::StatusSnapshot> for StatusSnapshotDto {
    fn from(s: &palka_core::StatusSnapshot) -> Self {
        Self {
            desired_internet_state: s.desired_internet_state.into(),
            observed_internet_state: s.observed_internet_state.into(),
            shutdown_state: s.shutdown_state.into(),
            active_actions: s
                .active_actions
                .iter()
                .map(ScheduledActionDto::from)
                .collect(),
            health: (&s.health).into(),
            target_child_sid: s.target_child_sid.clone(),
            timestamp: s.timestamp.0,
        }
    }
}

/// Reason triggering an Internet policy transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StateChangeReasonDto {
    TimerExpired { timer_id: WireTimerId },
    ImmediateCommand { initiator: InitiatorDto },
    ManualRestore { initiator: InitiatorDto },
    StartupRestoration,
    PlatformSync,
}

impl From<&palka_core::StateChangeReason> for StateChangeReasonDto {
    fn from(r: &palka_core::StateChangeReason) -> Self {
        match r {
            palka_core::StateChangeReason::TimerExpired { timer_id } => Self::TimerExpired {
                timer_id: WireTimerId::from_core(*timer_id),
            },
            palka_core::StateChangeReason::ImmediateCommand { initiator } => {
                Self::ImmediateCommand {
                    initiator: (*initiator).into(),
                }
            }
            palka_core::StateChangeReason::ManualRestore { initiator } => Self::ManualRestore {
                initiator: (*initiator).into(),
            },
            palka_core::StateChangeReason::StartupRestoration => Self::StartupRestoration,
            palka_core::StateChangeReason::PlatformSync => Self::PlatformSync,
        }
    }
}

/// Warning event DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarningEventDto {
    pub timer_id: WireTimerId,
    pub action_kind: ActionKindDto,
    pub threshold_seconds: u32,
    pub deadline: i64,
    pub emitted_at: i64,
}

impl From<&palka_core::WarningEvent> for WarningEventDto {
    fn from(w: &palka_core::WarningEvent) -> Self {
        Self {
            timer_id: WireTimerId::from_core(w.timer_id),
            action_kind: w.action_kind.into(),
            threshold_seconds: w.threshold.seconds(),
            deadline: w.deadline.0.0,
            emitted_at: w.emitted_at.0,
        }
    }
}

/// Sender of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MessageSenderDto {
    Parent,
    Child,
}

impl From<palka_core::MessageSender> for MessageSenderDto {
    fn from(s: palka_core::MessageSender) -> Self {
        match s {
            palka_core::MessageSender::Parent => Self::Parent,
            palka_core::MessageSender::Child => Self::Child,
        }
    }
}

/// Delivery status of a chat message across transport hops.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum DeliveryStatusDto {
    Pending,
    AcceptedByService,
    AcceptedByTelegram,
    DeliveredToTray,
    Failed { reason: String },
}

impl From<&palka_core::DeliveryStatus> for DeliveryStatusDto {
    fn from(s: &palka_core::DeliveryStatus) -> Self {
        match s {
            palka_core::DeliveryStatus::Pending => Self::Pending,
            palka_core::DeliveryStatus::AcceptedByService => Self::AcceptedByService,
            palka_core::DeliveryStatus::AcceptedByTelegram => Self::AcceptedByTelegram,
            palka_core::DeliveryStatus::DeliveredToTray => Self::DeliveredToTray,
            palka_core::DeliveryStatus::Failed { reason } => Self::Failed {
                reason: reason.clone(),
            },
        }
    }
}

/// Chat message DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessageDto {
    pub id: WireMessageId,
    pub sender: MessageSenderDto,
    pub text: String,
    pub timestamp: i64,
    pub delivery_status: DeliveryStatusDto,
}

impl From<&palka_core::ChatMessage> for ChatMessageDto {
    fn from(m: &palka_core::ChatMessage) -> Self {
        Self {
            id: WireMessageId::from_core(m.id),
            sender: m.sender.into(),
            text: m.text.clone(),
            timestamp: m.timestamp.0,
            delivery_status: (&m.delivery_status).into(),
        }
    }
}
