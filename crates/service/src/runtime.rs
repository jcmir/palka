//! Service Runtime Orchestration for PALKA.
//!
//! Implements the normative V1 Service Runtime Orchestration contract defined in
//! `docs/016-service-runtime-orchestration-contract.md`.
//!
//! Architectural Invariants:
//! CORE DECIDES.
//! SERVICE ENFORCES.
//! PLATFORM EXECUTES.
//! TRAY DISPLAYS.
//! TELEGRAM REQUESTS.
//! RUNTIME SERIALIZES AUTHORITATIVE MUTATION.

use crate::bootstrap::BootstrappedServiceState;
use crate::persistence::{
    InternetRetry, OutboxEntryId, PersistentState, TelegramOutboxEntry, TelegramPayload,
};
use crate::state_store::{StateFileStore, StateStoreError};
use palka_core::{
    ActionExecutionState, ActionKind, ChatMessage, Deadline, DeliveryStatus, DesiredInternetState,
    Event, HealthStatus, Initiator, InternetState, MessageId, MessageSender, ScheduledAction,
    ServiceHealth, ShutdownState, StateChangeReason, StatusSnapshot, TimerId, UtcDateTime,
    WarningEvent, WarningThreshold, creation_due_thresholds, creation_passed_thresholds,
    crossed_warning_thresholds, execution_failure_transition, execution_success_transition,
    recovery_overdue_transition, recovery_passed_thresholds, runtime_deadline_transition,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, spawn};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Internal subscription identifier for runtime event subscribers.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SubscriptionId(pub(crate) u64);

/// Active typed event subscription handle returned to internal consumers.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct EventSubscription {
    pub(crate) subscription_id: SubscriptionId,
    pub(crate) initial_snapshot: StatusSnapshot,
    pub(crate) receiver: Receiver<Event>,
}

// ============================================================================
// 1. ABSTRACT PORTS & TRAITS
// ============================================================================

/// Error type for abstract platform operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError {
    pub reason: String,
}

impl PlatformError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Platform error: {}", self.reason)
    }
}

impl std::error::Error for PlatformError {}

/// Abstract port for network restriction enforcement (WFP boundary).
pub trait InternetGate: Send + Sync {
    fn current_state(&self, child_sid: &str) -> Result<InternetState, PlatformError>;
    fn block_internet(&self, child_sid: &str) -> Result<(), PlatformError>;
    fn unblock_internet(&self, child_sid: &str) -> Result<(), PlatformError>;
}

/// Abstract port for OS power control operations (Windows Power boundary).
pub trait PowerController: Send + Sync {
    fn initiate_shutdown(&self) -> Result<(), PlatformError>;
}

/// Abstract clock port supplying dual-clock references: UTC wall clock and monotonic time.
pub trait RuntimeClock: Send + Sync {
    fn utc_now(&self) -> UtcDateTime;
    fn monotonic_now(&self) -> Instant;
}

/// Production implementation of `RuntimeClock` using OS clocks.
#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl RuntimeClock for SystemClock {
    fn utc_now(&self) -> UtcDateTime {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        UtcDateTime(millis)
    }

    fn monotonic_now(&self) -> Instant {
        Instant::now()
    }
}

/// Abstract port for deterministic 128-bit identifier generation.
pub trait IdSource: Send + Sync {
    fn next_timer_id(&self) -> TimerId;
    fn next_outbox_id(&self) -> OutboxEntryId;
}

/// Abstract port for Internet reconciliation backoff retry calculations.
pub trait InternetRetryPolicy: Send + Sync {
    fn delay_for_attempt(&self, attempt_count: u32) -> Duration;
}

/// Abstract port for persisting `PersistentState`.
pub(crate) trait RuntimeStateStore: Send + Sync {
    fn save(&self, state: &PersistentState) -> Result<(), StateStoreError>;
}

/// Adapter connecting canonical `StateFileStore` to `RuntimeStateStore`.
#[derive(Debug, Clone)]
pub(crate) struct StateFileStoreAdapter {
    pub store: StateFileStore,
}

impl StateFileStoreAdapter {
    pub(crate) fn new(store: StateFileStore) -> Self {
        Self { store }
    }
}

impl RuntimeStateStore for StateFileStoreAdapter {
    fn save(&self, state: &PersistentState) -> Result<(), StateStoreError> {
        self.store.save(state)
    }
}

// ============================================================================
// 2. SUB-SECOND DEADLINE CONVERSION HELPER
// ============================================================================

/// Converts a millisecond difference (`deadline_ms - now_ms`) to remaining seconds for domain predicates.
///
/// Guarantees that any positive sub-second future duration (e.g. +1ms to +999ms) evaluates to
/// `> 0` seconds (specifically `1`), preserving the invariant that future deadlines never truncate
/// to overdue (`<= 0`).
/// For non-positive deltas (`0` or negative), returns standard division (`delta_ms / 1000`),
/// guaranteeing `<= 0`.
pub fn remaining_seconds_from_delta_ms(delta_ms: i64) -> i64 {
    if delta_ms > 0 {
        (delta_ms + 999) / 1000
    } else {
        delta_ms / 1000
    }
}

// ============================================================================
// 3. ERROR TAXONOMY
// ============================================================================

#[derive(Debug)]
pub enum RuntimeConstructionError {
    InvalidConfiguration(String),
}

impl fmt::Display for RuntimeConstructionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(msg) => write!(f, "Runtime construction error: {msg}"),
        }
    }
}

impl std::error::Error for RuntimeConstructionError {}

#[derive(Debug)]
pub enum StartupRecoveryError {
    Persistence(StateStoreError),
    Fatal(String),
}

impl fmt::Display for StartupRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persistence(err) => write!(f, "Startup recovery persistence error: {err}"),
            Self::Fatal(msg) => write!(f, "Startup recovery fatal error: {msg}"),
        }
    }
}

impl std::error::Error for StartupRecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence(err) => Some(err),
            Self::Fatal(_) => None,
        }
    }
}

impl From<StateStoreError> for StartupRecoveryError {
    fn from(err: StateStoreError) -> Self {
        Self::Persistence(err)
    }
}

#[derive(Debug)]
pub enum SchedulerError {
    ClockJump(String),
    Channel(String),
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockJump(msg) => write!(f, "Scheduler clock jump error: {msg}"),
            Self::Channel(msg) => write!(f, "Scheduler channel communication error: {msg}"),
        }
    }
}

impl std::error::Error for SchedulerError {}

#[derive(Debug)]
pub enum WorkerError {
    Panicked(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked(msg) => write!(f, "Worker panicked: {msg}"),
        }
    }
}

impl std::error::Error for WorkerError {}

#[derive(Debug)]
pub enum TeardownError {
    JoinFailed(String),
    Persistence(StateStoreError),
}

impl fmt::Display for TeardownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JoinFailed(msg) => write!(f, "Teardown join failure: {msg}"),
            Self::Persistence(err) => write!(f, "Teardown final persistence error: {err}"),
        }
    }
}

impl std::error::Error for TeardownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::JoinFailed(_) => None,
            Self::Persistence(err) => Some(err),
        }
    }
}

/// Unified typed error taxonomy for the Service Runtime.
#[derive(Debug)]
pub enum ServiceRuntimeError {
    Construction(RuntimeConstructionError),
    StartupRecovery(StartupRecoveryError),
    Persistence(StateStoreError),
    Platform(PlatformError),
    Scheduler(SchedulerError),
    Worker(WorkerError),
    Teardown(TeardownError),
    ActionNotFound(TimerId),
    CancellationForbidden(String),
    InvalidInput(String),
    Stopping,
    SubscriptionIdExhausted,
    SubscriptionCapacityExhausted,
}

impl fmt::Display for ServiceRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Construction(err) => write!(f, "{err}"),
            Self::StartupRecovery(err) => write!(f, "{err}"),
            Self::Persistence(err) => write!(f, "Runtime persistence error: {err}"),
            Self::Platform(err) => write!(f, "Runtime platform error: {err}"),
            Self::Scheduler(err) => write!(f, "{err}"),
            Self::Worker(err) => write!(f, "{err}"),
            Self::Teardown(err) => write!(f, "{err}"),
            Self::ActionNotFound(id) => write!(f, "Action timer not found: {:?}", id),
            Self::CancellationForbidden(msg) => write!(f, "Cancellation forbidden: {msg}"),
            Self::InvalidInput(msg) => write!(f, "Invalid runtime input: {msg}"),
            Self::Stopping => write!(f, "Service runtime is stopping: new requests rejected"),
            Self::SubscriptionIdExhausted => write!(f, "Subscription ID allocation exhausted"),
            Self::SubscriptionCapacityExhausted => {
                write!(
                    f,
                    "Subscription capacity exhausted: active tray sessions cannot exceed u32::MAX"
                )
            }
        }
    }
}

impl std::error::Error for ServiceRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Construction(err) => Some(err),
            Self::StartupRecovery(err) => Some(err),
            Self::Persistence(err) => Some(err),
            Self::Platform(err) => Some(err),
            Self::Scheduler(err) => Some(err),
            Self::Worker(err) => Some(err),
            Self::Teardown(err) => Some(err),
            _ => None,
        }
    }
}

impl From<RuntimeConstructionError> for ServiceRuntimeError {
    fn from(err: RuntimeConstructionError) -> Self {
        Self::Construction(err)
    }
}

impl From<StartupRecoveryError> for ServiceRuntimeError {
    fn from(err: StartupRecoveryError) -> Self {
        Self::StartupRecovery(err)
    }
}

impl From<StateStoreError> for ServiceRuntimeError {
    fn from(err: StateStoreError) -> Self {
        Self::Persistence(err)
    }
}

impl From<PlatformError> for ServiceRuntimeError {
    fn from(err: PlatformError) -> Self {
        Self::Platform(err)
    }
}

impl From<TeardownError> for ServiceRuntimeError {
    fn from(err: TeardownError) -> Self {
        Self::Teardown(err)
    }
}

// ============================================================================
// 4. READINESS SNAPSHOT
// ============================================================================

/// Typed result of the service runtime readiness assessment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupReadiness {
    Ready(StatusSnapshot),
    Degraded(StatusSnapshot),
}

impl StartupReadiness {
    pub fn snapshot(&self) -> &StatusSnapshot {
        match self {
            Self::Ready(s) | Self::Degraded(s) => s,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded(_))
    }
}

// ============================================================================
// 5. MONOTONIC TIMER ANCHOR
// ============================================================================

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct MonotonicTimerAnchor {
    pub timer_id: TimerId,
    pub action_kind: ActionKind,
    pub utc_deadline: Deadline,
    pub monotonic_target: Instant,
    pub original_duration_seconds: u64,
    pub monotonic_start: Instant,
    pub last_evaluated_remaining_seconds: u64,
}

// ============================================================================
// 6. RUNTIME COMMAND MESSAGES
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerCancellationResult {
    Cancelled,
    AlreadyAbsent,
    TimerKindMismatch,
}

enum RuntimeCommand {
    ScheduleAction {
        action_kind: ActionKind,
        duration_seconds: u32,
        initiator: Initiator,
        reply: Sender<Result<TimerId, ServiceRuntimeError>>,
    },
    CancelExactTimer {
        timer_id: TimerId,
        expected_action_kind: ActionKind,
        initiator: Initiator,
        reply: Sender<Result<TimerCancellationResult, ServiceRuntimeError>>,
    },
    ImmediateInternetBlock {
        initiator: Initiator,
        reply: Sender<Result<(), ServiceRuntimeError>>,
    },
    RestoreInternet {
        initiator: Initiator,
        reply: Sender<Result<(), ServiceRuntimeError>>,
    },
    AckTelegram {
        entry_id: OutboxEntryId,
        reply: Sender<Result<bool, ServiceRuntimeError>>,
    },
    QueryStatus {
        reply: Sender<StatusSnapshot>,
    },
    SubscribeEvents {
        reply: Sender<Result<EventSubscription, ServiceRuntimeError>>,
    },
    UnsubscribeEvents {
        subscription_id: SubscriptionId,
        reply: Sender<Result<bool, ServiceRuntimeError>>,
    },
    SendChildMessage {
        text: String,
        reply: Sender<Result<MessageId, ServiceRuntimeError>>,
    },
    PublishParentMessage {
        message: ChatMessage,
        reply: Sender<Result<(), ServiceRuntimeError>>,
    },
    #[cfg(test)]
    Tick {
        reply: Sender<()>,
    },
    Stop {
        reply: Sender<Result<(), TeardownError>>,
    },
}

// ============================================================================
// 7. AUTHORITATIVE SINGLE-WRITER COORDINATOR
// ============================================================================

struct ServiceRuntimeCoordinator<S, G, P, C, I, R> {
    bootstrapped: BootstrappedServiceState,
    state: PersistentState,
    observed_internet_state: InternetState,
    shutdown_state: ShutdownState,
    health: ServiceHealth,
    store: S,
    gate: G,
    power: P,
    clock: C,
    id_source: I,
    retry_policy: R,
    monotonic_timers: HashMap<TimerId, MonotonicTimerAnchor>,
    next_retry_at: Option<Instant>,
    stopping: bool,
    #[cfg(test)]
    call_log: Option<Arc<Mutex<Vec<String>>>>,
    monotonic_start: Instant,
    stop_requested: Arc<AtomicBool>,
    platform_effect_gate: Arc<Mutex<()>>,
    #[cfg(test)]
    pre_effect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    pending_durable_candidate: Option<PersistentState>,
    persistence_error: Option<String>,
    internet_gate_error: Option<String>,
    power_error: Option<String>,
    retry_policy_error: Option<String>,
    subscribers: HashMap<SubscriptionId, SyncSender<Event>>,
    next_subscription_id: Option<u64>,
}

fn checked_prospective_active_session_count(
    current_len: usize,
) -> Result<u32, ServiceRuntimeError> {
    let prospective_len = current_len
        .checked_add(1)
        .ok_or(ServiceRuntimeError::SubscriptionCapacityExhausted)?;
    u32::try_from(prospective_len).map_err(|_| ServiceRuntimeError::SubscriptionCapacityExhausted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MeaningfulHealthSignature {
    pub status: HealthStatus,
    pub internet_gate_healthy: bool,
    pub persistence_healthy: bool,
    pub telegram_connected: bool,
    pub active_tray_sessions: u32,
    pub last_error: Option<String>,
}

fn derive_shutdown_state(
    current_state: ShutdownState,
    active_actions: &[ScheduledAction],
) -> ShutdownState {
    if current_state == ShutdownState::InProgress {
        ShutdownState::InProgress
    } else if active_actions.iter().any(|a| {
        a.action_kind == ActionKind::ShutdownComputer
            && matches!(
                a.execution_state,
                ActionExecutionState::Pending | ActionExecutionState::Executing
            )
    }) {
        ShutdownState::Scheduled
    } else {
        ShutdownState::Idle
    }
}

impl<S, G, P, C, I, R> ServiceRuntimeCoordinator<S, G, P, C, I, R>
where
    S: RuntimeStateStore,
    G: InternetGate,
    P: PowerController,
    C: RuntimeClock,
    I: IdSource,
    R: InternetRetryPolicy,
{
    fn new(
        bootstrapped: BootstrappedServiceState,
        store: S,
        gate: G,
        power: P,
        clock: C,
        id_source: I,
        retry_policy: R,
        #[cfg(test)] call_log: Option<Arc<Mutex<Vec<String>>>>,
        stop_requested: Arc<AtomicBool>,
        platform_effect_gate: Arc<Mutex<()>>,
        #[cfg(test)] pre_effect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<(Self, StartupReadiness), ServiceRuntimeError> {
        let state = bootstrapped.state.clone();
        let monotonic_start = clock.monotonic_now();

        let mut coordinator = Self {
            bootstrapped,
            state,
            observed_internet_state: InternetState::Unknown,
            shutdown_state: ShutdownState::Idle,
            health: ServiceHealth {
                status: HealthStatus::Degraded,
                internet_gate_healthy: true,
                persistence_healthy: true,
                telegram_connected: false,
                active_tray_sessions: 0,
                uptime_seconds: 0,
                last_error: None,
            },
            store,
            gate,
            power,
            clock,
            id_source,
            retry_policy,
            monotonic_timers: HashMap::new(),
            next_retry_at: None,
            stopping: false,
            #[cfg(test)]
            call_log,
            monotonic_start,
            stop_requested,
            platform_effect_gate,
            #[cfg(test)]
            pre_effect_hook,
            pending_durable_candidate: None,
            persistence_error: None,
            internet_gate_error: None,
            power_error: None,
            retry_policy_error: None,
            subscribers: HashMap::new(),
            next_subscription_id: Some(1),
        };

        let readiness = coordinator.perform_startup_recovery()?;
        Ok((coordinator, readiness))
    }

    #[cfg(test)]
    fn log_event(&self, event: &str) {
        if let Some(log) = &self.call_log {
            if let Ok(mut l) = log.lock() {
                l.push(event.to_string());
            }
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn log_event(&self, _event: &str) {}

    fn recompute_health_status(&mut self) {
        if !self.health.persistence_healthy {
            self.health.status = HealthStatus::Critical;
        } else if !self.health.internet_gate_healthy
            || !self.health.telegram_connected
            || self.power_error.is_some()
            || self.retry_policy_error.is_some()
        {
            self.health.status = HealthStatus::Degraded;
        } else {
            self.health.status = HealthStatus::Healthy;
        }

        if let Some(err) = &self.persistence_error {
            self.health.last_error = Some(err.clone());
        } else if let Some(err) = &self.retry_policy_error {
            self.health.last_error = Some(err.clone());
        } else if let Some(err) = &self.power_error {
            self.health.last_error = Some(err.clone());
        } else if let Some(err) = &self.internet_gate_error {
            self.health.last_error = Some(err.clone());
        } else {
            self.health.last_error = None;
        }
    }
    fn meaningful_health_signature(&self) -> MeaningfulHealthSignature {
        MeaningfulHealthSignature {
            status: self.health.status,
            internet_gate_healthy: self.health.internet_gate_healthy,
            persistence_healthy: self.health.persistence_healthy,
            telegram_connected: self.health.telegram_connected,
            active_tray_sessions: self.health.active_tray_sessions,
            last_error: self.health.last_error.clone(),
        }
    }

    fn mutate_health_and_maybe_emit<F>(&mut self, mutate: F)
    where
        F: FnOnce(&mut Self),
    {
        let before = self.meaningful_health_signature();
        mutate(self);
        self.recompute_health_status();
        let after = self.meaningful_health_signature();
        if before != after {
            let health = self.build_service_health_snapshot();
            let _ = self.emit_event(Event::ServiceHealthUpdated { health });
        }
    }

    fn sync_shutdown_state_local(&mut self) -> (ShutdownState, ShutdownState) {
        let prev = self.shutdown_state;
        self.shutdown_state =
            derive_shutdown_state(self.shutdown_state, &self.state.active_actions);
        (prev, self.shutdown_state)
    }

    fn maybe_emit_shutdown_state_changed(
        &mut self,
        previous: ShutdownState,
        current: ShutdownState,
    ) {
        if previous != current {
            let _ = self.emit_event(Event::ShutdownStateChanged { previous, current });
        }
    }

    fn maybe_emit_internet_policy_changed(
        &mut self,
        previous_desired: DesiredInternetState,
        previous_observed: InternetState,
        reason: StateChangeReason,
    ) {
        let current_desired = self.state.desired_internet_state;
        let current_observed = self.observed_internet_state;
        if (previous_desired, previous_observed) != (current_desired, current_observed) {
            let _ = self.emit_event(Event::InternetPolicyChanged {
                desired: current_desired,
                observed: current_observed,
                reason,
            });
        }
    }

    fn mark_persistence_failure(&mut self, err: &StateStoreError) {
        self.mutate_health_and_maybe_emit(|s| {
            s.health.persistence_healthy = false;
            s.persistence_error = Some(format!("State store error: {err}"));
        });
    }

    fn mark_persistence_success(&mut self) {
        self.mutate_health_and_maybe_emit(|s| {
            s.health.persistence_healthy = true;
            s.persistence_error = None;
        });
    }

    fn mark_gate_failure(&mut self, err_msg: String) {
        self.mutate_health_and_maybe_emit(|s| {
            s.health.internet_gate_healthy = false;
            s.internet_gate_error = Some(err_msg);
        });
    }

    fn mark_gate_success(&mut self) {
        self.mutate_health_and_maybe_emit(|s| {
            s.health.internet_gate_healthy = true;
            s.internet_gate_error = None;
        });
    }

    fn flush_pending_durable_candidate(&mut self) -> Result<(), StateStoreError> {
        if let Some(pending) = self.pending_durable_candidate.clone() {
            match self.store.save(&pending) {
                Ok(()) => {
                    self.state = pending;
                    self.pending_durable_candidate = None;
                    self.mark_persistence_success();
                    Ok(())
                }
                Err(e) => {
                    self.mark_persistence_failure(&e);
                    Err(e)
                }
            }
        } else {
            Ok(())
        }
    }

    fn commit_authoritative_state(
        &mut self,
        candidate: PersistentState,
    ) -> Result<(), StateStoreError> {
        match self.store.save(&candidate) {
            Ok(()) => {
                self.state = candidate;
                self.pending_durable_candidate = None;
                self.mark_persistence_success();
                Ok(())
            }
            Err(e) => {
                self.mark_persistence_failure(&e);
                Err(e)
            }
        }
    }

    fn commit_post_side_effect_candidate(
        &mut self,
        candidate: PersistentState,
    ) -> Result<(), StateStoreError> {
        self.state = candidate.clone();
        match self.store.save(&candidate) {
            Ok(()) => {
                self.pending_durable_candidate = None;
                self.mark_persistence_success();
                Ok(())
            }
            Err(e) => {
                self.pending_durable_candidate = Some(candidate);
                self.mark_persistence_failure(&e);
                Err(e)
            }
        }
    }

    fn is_stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    /// Executes the normative startup recovery sequence (Sections 7, 8, 17, 18 of contract).
    fn perform_startup_recovery(&mut self) -> Result<StartupReadiness, ServiceRuntimeError> {
        let now_utc = self.clock.utc_now();
        let now_mono = self.clock.monotonic_now();
        let mut candidate = self.state.clone();
        let mut needs_recovery_commit = false;

        let mut actions_to_remove = Vec::new();
        let mut recovered_overdue_block_ids = Vec::new();
        let mut missed_shutdown_snapshots = Vec::new();

        // 1. Process active actions recovered from state.json
        for action in &mut candidate.active_actions {
            let delta_ms = action.deadline.0.0 - now_utc.0;
            let remaining_seconds = remaining_seconds_from_delta_ms(delta_ms);

            if remaining_seconds <= 0 {
                // Action is overdue
                match action.action_kind {
                    ActionKind::BlockInternet => match &action.execution_state {
                        ActionExecutionState::Pending => {
                            match recovery_overdue_transition(
                                ActionKind::BlockInternet,
                                &action.execution_state,
                                remaining_seconds,
                            ) {
                                Some(next @ ActionExecutionState::Executing) => {
                                    action.execution_state = next;
                                    candidate.desired_internet_state =
                                        DesiredInternetState::Blocked;
                                    needs_recovery_commit = true;
                                    recovered_overdue_block_ids.push(action.id);
                                }
                                other => {
                                    return Err(ServiceRuntimeError::StartupRecovery(
                                        StartupRecoveryError::Fatal(format!(
                                            "Contract failure: recovery_overdue_transition for Pending BlockInternet returned {:?}",
                                            other
                                        )),
                                    ));
                                }
                            }
                        }
                        ActionExecutionState::Executing => {
                            if candidate.desired_internet_state != DesiredInternetState::Blocked {
                                candidate.desired_internet_state = DesiredInternetState::Blocked;
                                needs_recovery_commit = true;
                            }
                            recovered_overdue_block_ids.push(action.id);
                        }
                        ActionExecutionState::Failed { .. } => {
                            // Historical failed actions are preserved, do NOT complete them
                        }
                        _ => {}
                    },
                    ActionKind::ShutdownComputer => {
                        match recovery_overdue_transition(
                            action.action_kind,
                            &action.execution_state,
                            remaining_seconds,
                        ) {
                            Some(next @ ActionExecutionState::Missed) => {
                                action.execution_state = next;
                                actions_to_remove.push(action.id);

                                missed_shutdown_snapshots.push(action.clone());

                                let entry_id = self.id_source.next_outbox_id();
                                candidate.telegram_outbox.push(TelegramOutboxEntry {
                                    entry_id,
                                    payload: TelegramPayload::ServiceNotification {
                                        text: "Scheduled shutdown was missed while service was offline"
                                            .to_string(),
                                    },
                                    attempt_count: 0,
                                    last_error: None,
                                });
                                needs_recovery_commit = true;
                            }
                            _ => {}
                        }
                    }
                }
            } else {
                // Action is future: mark passed offline thresholds without outbox notification
                let passed =
                    recovery_passed_thresholds(remaining_seconds, &action.emitted_thresholds);
                if !passed.is_empty() {
                    for t in passed {
                        action.emitted_thresholds.insert(t);
                    }
                    needs_recovery_commit = true;
                }

                // Register volatile monotonic timer anchor
                // Exact sub-second monotonic recovery (Section 13)
                let duration_seconds = (remaining_seconds as u64).max(1);
                let target_instant = now_mono + Duration::from_millis(delta_ms as u64);
                self.monotonic_timers.insert(
                    action.id,
                    MonotonicTimerAnchor {
                        timer_id: action.id,
                        action_kind: action.action_kind,
                        utc_deadline: action.deadline,
                        monotonic_target: target_instant,
                        original_duration_seconds: duration_seconds,
                        monotonic_start: now_mono,
                        last_evaluated_remaining_seconds: duration_seconds,
                    },
                );
            }
        }

        // Remove terminal missed actions
        if !actions_to_remove.is_empty() {
            candidate
                .active_actions
                .retain(|a| !actions_to_remove.contains(&a.id));
        }

        // Durable Recovery Commit: must persist before any platform reconciliation
        if needs_recovery_commit {
            self.log_event("save:recovery");
            self.commit_authoritative_state(candidate.clone())
                .map_err(ServiceRuntimeError::Persistence)?;
        }

        // Emit MissedDeadlineOccurred events after durable recovery commit
        for missed_snapshot in missed_shutdown_snapshots {
            let _ = self.emit_event(Event::MissedDeadlineOccurred {
                action: missed_snapshot,
                reason: "Scheduled shutdown was missed while service was offline".to_string(),
            });
        }

        // Startup Shutdown Aggregate synchronization & event
        let (prev_shutdown, curr_shutdown) = self.sync_shutdown_state_local();
        self.maybe_emit_shutdown_state_changed(prev_shutdown, curr_shutdown);

        // 2. Initial Internet Reconciliation
        let prev_desired = self.state.desired_internet_state;
        let prev_observed = self.observed_internet_state;

        let child_sid = self.bootstrapped.config.child_sid.clone();
        let reconciliation_result = match self.state.desired_internet_state {
            DesiredInternetState::Blocked => {
                self.log_event("gate:block_internet");
                self.gate.block_internet(&child_sid)
            }
            DesiredInternetState::Unrestricted => {
                self.log_event("gate:unblock_internet");
                self.gate.unblock_internet(&child_sid)
            }
        };

        let current_state_result = self.gate.current_state(&child_sid);

        match &current_state_result {
            Ok(obs) => self.observed_internet_state = *obs,
            Err(_) => self.observed_internet_state = InternetState::Unknown,
        }

        self.maybe_emit_internet_policy_changed(
            prev_desired,
            prev_observed,
            StateChangeReason::StartupRestoration,
        );

        let mut degraded = false;
        let mut last_error_msg = None;

        let desired_matches = match (self.state.desired_internet_state, &current_state_result) {
            (DesiredInternetState::Blocked, Ok(InternetState::Blocked)) => true,
            (DesiredInternetState::Unrestricted, Ok(InternetState::Unrestricted)) => true,
            _ => false,
        };

        match (reconciliation_result, current_state_result) {
            (Ok(()), Ok(_)) if desired_matches => {
                self.mark_gate_success();
                let mut update = self.state.clone();
                let mut save_needed = false;

                if update.internet_retry.is_some() {
                    update.internet_retry = None;
                    save_needed = true;
                }

                if !recovered_overdue_block_ids.is_empty() {
                    let mut completed_ids = Vec::new();
                    for act in &mut update.active_actions {
                        if recovered_overdue_block_ids.contains(&act.id) {
                            if let Some(ActionExecutionState::Completed) =
                                execution_success_transition(&act.execution_state)
                            {
                                completed_ids.push(act.id);
                            }
                        }
                    }
                    if !completed_ids.is_empty() {
                        update
                            .active_actions
                            .retain(|a| !completed_ids.contains(&a.id));
                        save_needed = true;
                    }
                }

                if save_needed {
                    self.log_event("save:startup_reconciled");
                    self.commit_post_side_effect_candidate(update)
                        .map_err(ServiceRuntimeError::Persistence)?;
                }
            }
            (Ok(()), Ok(obs)) => {
                // Verification mismatch: mutation succeeded but observed does not match desired
                degraded = true;
                last_error_msg = Some(format!(
                    "Observed Internet state {:?} does not match desired state {:?}",
                    obs, self.state.desired_internet_state
                ));
            }
            (Err(err), _) => {
                degraded = true;
                last_error_msg = Some(err.reason);
            }
            (_, Err(err)) => {
                degraded = true;
                last_error_msg = Some(err.reason);
            }
        }

        if degraded {
            let err_text = last_error_msg
                .clone()
                .unwrap_or_else(|| "Internet startup reconciliation failed".to_string());
            self.mark_gate_failure(err_text.clone());

            // Persist retry metadata and enqueue ServiceNotification without rolling back desired state
            let mut update = self.state.clone();
            let attempt = update
                .internet_retry
                .as_ref()
                .map(|r| r.attempt_count + 1)
                .unwrap_or(1);
            update.internet_retry = Some(InternetRetry {
                attempt_count: attempt,
                last_error: Some(err_text.clone()),
            });

            // Mark overdue block actions as Failed { reason } using core transition (Section 4)
            for act in &mut update.active_actions {
                if recovered_overdue_block_ids.contains(&act.id) {
                    let next = execution_failure_transition(&act.execution_state, err_text.clone())
                        .ok_or_else(|| {
                            ServiceRuntimeError::StartupRecovery(StartupRecoveryError::Fatal(format!(
                                "Contract failure: execution_failure_transition returned None for recovered overdue block action {:?}",
                                act.id
                            )))
                        })?;
                    act.execution_state = next;
                }
            }

            let entry_id = self.id_source.next_outbox_id();
            update.telegram_outbox.push(TelegramOutboxEntry {
                entry_id,
                payload: TelegramPayload::ServiceNotification {
                    text: format!("Internet startup reconciliation failed: {}", err_text),
                },
                attempt_count: 0,
                last_error: None,
            });

            self.log_event("save:retry_metadata");
            self.commit_post_side_effect_candidate(update)
                .map_err(ServiceRuntimeError::Persistence)?;

            // Schedule first monotonic retry
            self.schedule_next_internet_retry(attempt);
        }

        let snapshot = self.build_status_snapshot();
        if snapshot.health.status == HealthStatus::Degraded {
            Ok(StartupReadiness::Degraded(snapshot))
        } else if snapshot.health.status == HealthStatus::Healthy {
            Ok(StartupReadiness::Ready(snapshot))
        } else {
            Err(ServiceRuntimeError::StartupRecovery(
                StartupRecoveryError::Fatal("Startup failed with critical health".to_string()),
            ))
        }
    }

    fn schedule_next_internet_retry(&mut self, attempt: u32) {
        let delay = self.retry_policy.delay_for_attempt(attempt);
        if delay.is_zero() {
            // Defend against zero-delay busy loop (Section 9)
            self.mutate_health_and_maybe_emit(|s| {
                s.retry_policy_error =
                    Some("InternetRetryPolicy returned invalid zero delay".to_string());
            });
            self.next_retry_at = None;
        } else {
            self.mutate_health_and_maybe_emit(|s| {
                s.retry_policy_error = None;
            });
            self.next_retry_at = Some(self.clock.monotonic_now() + delay);
        }
    }

    fn build_service_health_snapshot(&self) -> ServiceHealth {
        let uptime_seconds = self
            .clock
            .monotonic_now()
            .saturating_duration_since(self.monotonic_start)
            .as_secs();

        let mut health = self.health.clone();
        health.uptime_seconds = uptime_seconds;
        health.telegram_connected = false;
        health
    }

    fn build_status_snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            desired_internet_state: self.state.desired_internet_state,
            observed_internet_state: self.observed_internet_state,
            shutdown_state: self.shutdown_state,
            active_actions: self.state.active_actions.clone(),
            health: self.build_service_health_snapshot(),
            target_child_sid: self.bootstrapped.config.child_sid.clone(),
            timestamp: self.clock.utc_now(),
        }
    }

    fn checked_active_session_count(&self) -> Result<u32, ServiceRuntimeError> {
        u32::try_from(self.subscribers.len())
            .map_err(|_| ServiceRuntimeError::SubscriptionCapacityExhausted)
    }

    fn try_send_to_subscribers(
        &self,
        event: &Event,
        excluded_subscription: Option<SubscriptionId>,
    ) -> Vec<SubscriptionId> {
        let mut failed = Vec::new();
        for (id, tx) in &self.subscribers {
            if Some(*id) == excluded_subscription {
                continue;
            }
            match tx.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    failed.push(*id);
                }
            }
        }
        failed
    }

    fn reconcile_subscriber_registry(
        &mut self,
        excluded_subscription: Option<SubscriptionId>,
    ) -> Result<(), ServiceRuntimeError> {
        loop {
            let target_count = self.checked_active_session_count()?;
            let count_changed = self.health.active_tray_sessions != target_count;
            if !count_changed {
                return Ok(());
            }
            self.health.active_tray_sessions = target_count;
            let event = Event::ServiceHealthUpdated {
                health: self.build_service_health_snapshot(),
            };
            let failed = self.try_send_to_subscribers(&event, excluded_subscription);
            if failed.is_empty() {
                return Ok(());
            }
            for id in failed {
                self.subscribers.remove(&id);
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn emit_event(&mut self, event: Event) -> Result<(), ServiceRuntimeError> {
        #[cfg(test)]
        self.log_event(&format!("event:{:?}", event));
        let failed = self.try_send_to_subscribers(&event, None);
        if !failed.is_empty() {
            for id in failed {
                self.subscribers.remove(&id);
            }
            self.reconcile_subscriber_registry(None)?;
        }
        Ok(())
    }

    fn handle_subscribe_events(
        &mut self,
        reply: Sender<Result<EventSubscription, ServiceRuntimeError>>,
    ) {
        // 1. Preflight: SubscriptionId
        let candidate_id = match self.next_subscription_id {
            Some(id) => id,
            None => {
                let _ = reply.send(Err(ServiceRuntimeError::SubscriptionIdExhausted));
                return;
            }
        };
        let subscription_id = SubscriptionId(candidate_id);
        if self.subscribers.contains_key(&subscription_id) {
            let _ = reply.send(Err(ServiceRuntimeError::SubscriptionIdExhausted));
            return;
        }

        // 2. Preflight: Active session capacity
        if let Err(e) = checked_prospective_active_session_count(self.subscribers.len()) {
            let _ = reply.send(Err(e));
            return;
        }

        // 3. Register channel and advance allocator
        let (tx, rx) = sync_channel::<Event>(64);
        self.subscribers.insert(subscription_id, tx);
        self.next_subscription_id = candidate_id.checked_add(1);

        // 4. Reconcile with exclusion of new subscription
        if let Err(e) = self.reconcile_subscriber_registry(Some(subscription_id)) {
            self.subscribers.remove(&subscription_id);
            let _ = self.reconcile_subscriber_registry(None);
            let _ = reply.send(Err(e));
            return;
        }

        // 5. Build final snapshot (contains final stable count)
        let initial_snapshot = self.build_status_snapshot();
        let subscription = EventSubscription {
            subscription_id,
            initial_snapshot,
            receiver: rx,
        };

        // 6. Send reply, handling disconnect rollback
        if reply.send(Ok(subscription)).is_err() {
            self.subscribers.remove(&subscription_id);
            let _ = self.reconcile_subscriber_registry(None);
        }
    }

    fn handle_unsubscribe_events(
        &mut self,
        subscription_id: SubscriptionId,
        reply: Sender<Result<bool, ServiceRuntimeError>>,
    ) {
        let existed = self.subscribers.remove(&subscription_id).is_some();
        if existed {
            if let Err(e) = self.reconcile_subscriber_registry(None) {
                let _ = reply.send(Err(e));
                return;
            }
        }
        let _ = reply.send(Ok(existed));
    }

    /// Handles incoming messages and dispatches scheduled events.
    fn handle_command(&mut self, cmd: RuntimeCommand) {
        if self.stopping {
            match cmd {
                RuntimeCommand::QueryStatus { reply } => {
                    self.handle_query_status(reply);
                }
                RuntimeCommand::Stop { reply } => {
                    let flush_res = if self.pending_durable_candidate.is_some() {
                        self.flush_pending_durable_candidate()
                            .map_err(TeardownError::Persistence)
                    } else {
                        Ok(())
                    };
                    let _ = reply.send(flush_res);
                }
                #[cfg(test)]
                RuntimeCommand::Tick { reply } => {
                    let _ = reply.send(());
                }
                RuntimeCommand::ScheduleAction { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::CancelExactTimer { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::ImmediateInternetBlock { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::RestoreInternet { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::AckTelegram { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::SubscribeEvents { reply } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::UnsubscribeEvents { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::SendChildMessage { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
                RuntimeCommand::PublishParentMessage { reply, .. } => {
                    let _ = reply.send(Err(ServiceRuntimeError::Stopping));
                }
            }
            return;
        }

        match cmd {
            RuntimeCommand::ScheduleAction {
                action_kind,
                duration_seconds,
                initiator,
                reply,
            } => {
                let res = self.handle_schedule_action(action_kind, duration_seconds, initiator);
                let _ = reply.send(res);
            }
            RuntimeCommand::CancelExactTimer {
                timer_id,
                expected_action_kind,
                initiator,
                reply,
            } => {
                let res = self.handle_cancel_exact_timer(timer_id, expected_action_kind, initiator);
                let _ = reply.send(res);
            }
            RuntimeCommand::ImmediateInternetBlock { initiator, reply } => {
                let res = self.handle_immediate_block(initiator);
                let _ = reply.send(res);
            }
            RuntimeCommand::RestoreInternet { initiator, reply } => {
                let res = self.handle_restore_internet(initiator);
                let _ = reply.send(res);
            }
            RuntimeCommand::AckTelegram { entry_id, reply } => {
                let res = self.handle_ack_telegram(entry_id);
                let _ = reply.send(res);
            }
            RuntimeCommand::SendChildMessage { text, reply } => {
                let res = self.handle_send_child_message(text);
                let _ = reply.send(res);
            }
            RuntimeCommand::PublishParentMessage { message, reply } => {
                let res = self.handle_publish_parent_message(message);
                let _ = reply.send(res);
            }
            RuntimeCommand::QueryStatus { reply } => {
                self.handle_query_status(reply);
            }
            RuntimeCommand::SubscribeEvents { reply } => {
                self.handle_subscribe_events(reply);
            }
            RuntimeCommand::UnsubscribeEvents {
                subscription_id,
                reply,
            } => {
                self.handle_unsubscribe_events(subscription_id, reply);
            }
            #[cfg(test)]
            RuntimeCommand::Tick { reply } => {
                self.process_clock_and_events();
                let _ = reply.send(());
            }
            RuntimeCommand::Stop { reply } => {
                self.stopping = true;
                self.subscribers.clear();
                self.health.active_tray_sessions = 0;
                let flush_res = if self.pending_durable_candidate.is_some() {
                    self.flush_pending_durable_candidate()
                        .map_err(TeardownError::Persistence)
                } else {
                    Ok(())
                };
                let _ = reply.send(flush_res);
            }
        }
    }

    fn handle_query_status(&self, reply: Sender<StatusSnapshot>) {
        let snap = self.build_status_snapshot();
        let _ = reply.send(snap);
    }

    fn handle_schedule_action(
        &mut self,
        action_kind: ActionKind,
        duration_seconds: u32,
        initiator: Initiator,
    ) -> Result<TimerId, ServiceRuntimeError> {
        let now_utc = self.clock.utc_now();
        let now_mono = self.clock.monotonic_now();
        let timer_id = self.id_source.next_timer_id();
        let deadline_ms = now_utc.0 + (duration_seconds as i64 * 1000);
        let deadline = Deadline(UtcDateTime(deadline_ms));

        let mut emitted_thresholds = std::collections::HashSet::new();
        let passed = creation_passed_thresholds(duration_seconds.into());
        for p in passed {
            emitted_thresholds.insert(p);
        }

        let due = creation_due_thresholds(duration_seconds.into());
        let mut new_outbox = Vec::new();
        for d in &due {
            emitted_thresholds.insert(*d);
            let entry_id = self.id_source.next_outbox_id();
            new_outbox.push(TelegramOutboxEntry {
                entry_id,
                payload: TelegramPayload::ServiceNotification {
                    text: format!(
                        "Warning: Action {:?} has {} seconds remaining",
                        action_kind,
                        d.seconds()
                    ),
                },
                attempt_count: 0,
                last_error: None,
            });
        }

        let scheduled_action = ScheduledAction {
            id: timer_id,
            action_kind,
            deadline,
            created_at: now_utc,
            created_by: initiator,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds,
        };

        let mut candidate = self.state.clone();
        candidate.active_actions.push(scheduled_action.clone());
        candidate.telegram_outbox.extend(new_outbox);

        self.log_event("save:schedule_action");
        self.commit_authoritative_state(candidate)
            .map_err(ServiceRuntimeError::Persistence)?;

        let target_instant = now_mono + Duration::from_secs(duration_seconds.into());
        self.monotonic_timers.insert(
            timer_id,
            MonotonicTimerAnchor {
                timer_id,
                action_kind,
                utc_deadline: deadline,
                monotonic_target: target_instant,
                original_duration_seconds: duration_seconds.into(),
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: duration_seconds.into(),
            },
        );

        // Emit TimerScheduled with exact persisted ScheduledAction
        let persisted_action = self
            .state
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .cloned()
            .unwrap_or(scheduled_action);
        let _ = self.emit_event(Event::TimerScheduled {
            action: persisted_action,
        });

        // Emit creation-time due warnings
        for threshold in due {
            let _ = self.emit_event(Event::WarningThresholdReached {
                event: WarningEvent {
                    timer_id,
                    action_kind,
                    threshold,
                    deadline,
                    emitted_at: now_utc,
                },
            });
        }

        // If ShutdownComputer, synchronize aggregate and emit if changed
        if action_kind == ActionKind::ShutdownComputer {
            let (prev, curr) = self.sync_shutdown_state_local();
            self.maybe_emit_shutdown_state_changed(prev, curr);
        }

        Ok(timer_id)
    }

    fn handle_cancel_exact_timer(
        &mut self,
        timer_id: TimerId,
        expected_action_kind: ActionKind,
        _initiator: Initiator,
    ) -> Result<TimerCancellationResult, ServiceRuntimeError> {
        // Step 1: Exact TimerId lookup
        let action = match self.state.active_actions.iter().find(|a| a.id == timer_id) {
            Some(a) => a,
            None => return Ok(TimerCancellationResult::AlreadyAbsent),
        };

        // Step 2: Expected ActionKind validation
        if action.action_kind != expected_action_kind {
            return Ok(TimerCancellationResult::TimerKindMismatch);
        }

        // Step 3: Execution state validation - only Pending is cancellable
        if !matches!(action.execution_state, ActionExecutionState::Pending) {
            return Err(ServiceRuntimeError::CancellationForbidden(format!(
                "Action in state {:?} cannot be cancelled; only Pending actions are cancellable",
                action.execution_state
            )));
        }

        // Step 4: Shutdown-only monotonic deadline & anchor validation
        if expected_action_kind == ActionKind::ShutdownComputer {
            let anchor = match self.monotonic_timers.get(&timer_id) {
                Some(a) => a,
                None => {
                    return Err(ServiceRuntimeError::CancellationForbidden(
                        "Monotonic timer anchor unavailable for pending shutdown action"
                            .to_string(),
                    ));
                }
            };

            if anchor.action_kind != action.action_kind {
                return Err(ServiceRuntimeError::CancellationForbidden(
                    "Monotonic timer anchor kind mismatch for pending shutdown action".to_string(),
                ));
            }

            let now_mono = self.clock.monotonic_now();
            if now_mono >= anchor.monotonic_target {
                return Err(ServiceRuntimeError::CancellationForbidden(
                    "Shutdown cancellation boundary has passed".to_string(),
                ));
            }
        }

        // Step 5: Candidate construction & durable commit
        let mut candidate = self.state.clone();
        let initial_count = candidate.active_actions.len();
        candidate.active_actions.retain(|a| a.id != timer_id);
        if candidate.active_actions.len() + 1 != initial_count {
            return Err(ServiceRuntimeError::CancellationForbidden(
                "Failed to target exactly one action for cancellation".to_string(),
            ));
        }

        self.log_event("save:cancel_timer");
        self.commit_authoritative_state(candidate)
            .map_err(ServiceRuntimeError::Persistence)?;

        // Step 6: Monotonic anchor removal
        self.monotonic_timers.remove(&timer_id);

        // Step 7: Typed broadcast event (best-effort, live at-most-once)
        let _ = self.emit_event(Event::TimerCancelled {
            id: timer_id,
            action_kind: expected_action_kind,
        });

        // If ShutdownComputer, synchronize aggregate and emit if changed
        if expected_action_kind == ActionKind::ShutdownComputer {
            let (prev, curr) = self.sync_shutdown_state_local();
            self.maybe_emit_shutdown_state_changed(prev, curr);
        }

        // Step 8: Return Cancelled
        Ok(TimerCancellationResult::Cancelled)
    }

    fn handle_immediate_block(&mut self, initiator: Initiator) -> Result<(), ServiceRuntimeError> {
        let prev_desired = self.state.desired_internet_state;
        let prev_observed = self.observed_internet_state;

        let mut candidate = self.state.clone();
        candidate.desired_internet_state = DesiredInternetState::Blocked;

        self.log_event("save:immediate_block");
        self.commit_authoritative_state(candidate)
            .map_err(ServiceRuntimeError::Persistence)?;

        #[cfg(test)]
        if let Some(ref hook) = self.pre_effect_hook {
            hook();
        }

        let child_sid = self.bootstrapped.config.child_sid.clone();
        let (block_res, current_res) = {
            let _gate = self.platform_effect_gate.lock().unwrap();
            if self.is_stop_requested() {
                return Err(ServiceRuntimeError::Stopping);
            }
            self.log_event("gate:block_internet");
            let b_res = self.gate.block_internet(&child_sid);
            let c_res = self.gate.current_state(&child_sid);
            (b_res, c_res)
        };

        if let Ok(obs) = &current_res {
            self.observed_internet_state = *obs;
        } else {
            self.observed_internet_state = InternetState::Unknown;
        }

        self.maybe_emit_internet_policy_changed(
            prev_desired,
            prev_observed,
            StateChangeReason::ImmediateCommand { initiator },
        );

        match (block_res, current_res) {
            (Ok(()), Ok(obs)) if obs == InternetState::Blocked => {
                self.mark_gate_success();
                if self.state.internet_retry.is_some() {
                    let mut c = self.state.clone();
                    c.internet_retry = None;
                    self.log_event("save:clear_retry");
                    self.commit_post_side_effect_candidate(c)
                        .map_err(ServiceRuntimeError::Persistence)?;
                }
                Ok(())
            }
            (Ok(()), Ok(obs)) => {
                // Section 6: Verification mismatch is a failure!
                let err_msg = format!(
                    "Immediate block verification mismatch: observed {:?}, expected Blocked",
                    obs
                );
                self.handle_immediate_internet_failure(err_msg)
            }
            (Err(err), _) => self.handle_immediate_internet_failure(err.reason),
            (_, Err(err)) => self.handle_immediate_internet_failure(err.reason),
        }
    }

    fn handle_immediate_internet_failure(
        &mut self,
        err_msg: String,
    ) -> Result<(), ServiceRuntimeError> {
        self.mark_gate_failure(err_msg.clone());

        let mut c = self.state.clone();
        let attempt = c
            .internet_retry
            .as_ref()
            .map(|r| r.attempt_count + 1)
            .unwrap_or(1);
        c.internet_retry = Some(InternetRetry {
            attempt_count: attempt,
            last_error: Some(err_msg.clone()),
        });

        let entry_id = self.id_source.next_outbox_id();
        c.telegram_outbox.push(TelegramOutboxEntry {
            entry_id,
            payload: TelegramPayload::ServiceNotification {
                text: format!("Immediate internet block failed: {}", err_msg),
            },
            attempt_count: 0,
            last_error: None,
        });

        self.log_event("save:retry_metadata");
        let save_res = self.commit_post_side_effect_candidate(c);
        self.schedule_next_internet_retry(attempt);

        if let Err(e) = save_res {
            return Err(ServiceRuntimeError::Persistence(e));
        }
        Err(ServiceRuntimeError::Platform(PlatformError::new(err_msg)))
    }

    fn handle_restore_internet(&mut self, initiator: Initiator) -> Result<(), ServiceRuntimeError> {
        let prev_desired = self.state.desired_internet_state;
        let prev_observed = self.observed_internet_state;

        // Candidate: Durable-Before-Side-Effect
        let mut candidate = self.state.clone();
        candidate.desired_internet_state = DesiredInternetState::Unrestricted;

        self.log_event("save:restore_internet");
        self.commit_authoritative_state(candidate)
            .map_err(ServiceRuntimeError::Persistence)?;

        #[cfg(test)]
        if let Some(ref hook) = self.pre_effect_hook {
            hook();
        }

        let child_sid = self.bootstrapped.config.child_sid.clone();
        let (unblock_res, current_res) = {
            let _gate = self.platform_effect_gate.lock().unwrap();
            if self.is_stop_requested() {
                return Err(ServiceRuntimeError::Stopping);
            }
            self.log_event("gate:unblock_internet");
            let u_res = self.gate.unblock_internet(&child_sid);
            let c_res = self.gate.current_state(&child_sid);
            (u_res, c_res)
        };

        if let Ok(obs) = &current_res {
            self.observed_internet_state = *obs;
        } else {
            self.observed_internet_state = InternetState::Unknown;
        }

        self.maybe_emit_internet_policy_changed(
            prev_desired,
            prev_observed,
            StateChangeReason::ManualRestore { initiator },
        );

        match (unblock_res, current_res) {
            (Ok(()), Ok(obs)) if obs == InternetState::Unrestricted => {
                self.mark_gate_success();
                if self.state.internet_retry.is_some() {
                    let mut c = self.state.clone();
                    c.internet_retry = None;
                    self.log_event("save:clear_retry");
                    self.commit_post_side_effect_candidate(c)
                        .map_err(ServiceRuntimeError::Persistence)?;
                }
                Ok(())
            }
            (Ok(()), Ok(obs)) => {
                // Verification mismatch is a failure (Section 6)
                let err_msg = format!(
                    "Restore internet verification mismatch: observed {:?}, expected Unrestricted",
                    obs
                );
                self.handle_restore_internet_failure(err_msg)
            }
            (Err(err), _) => self.handle_restore_internet_failure(err.reason),
            (_, Err(err)) => self.handle_restore_internet_failure(err.reason),
        }
    }

    fn handle_restore_internet_failure(
        &mut self,
        err_msg: String,
    ) -> Result<(), ServiceRuntimeError> {
        // DO NOT ROLLBACK desired_internet_state
        self.mark_gate_failure(err_msg.clone());

        let mut c = self.state.clone();
        let attempt = c
            .internet_retry
            .as_ref()
            .map(|r| r.attempt_count + 1)
            .unwrap_or(1);
        c.internet_retry = Some(InternetRetry {
            attempt_count: attempt,
            last_error: Some(err_msg.clone()),
        });

        let entry_id = self.id_source.next_outbox_id();
        c.telegram_outbox.push(TelegramOutboxEntry {
            entry_id,
            payload: TelegramPayload::ServiceNotification {
                text: format!("Internet restoration failed: {}", err_msg),
            },
            attempt_count: 0,
            last_error: None,
        });

        self.log_event("save:retry_metadata");
        let save_res = self.commit_post_side_effect_candidate(c);
        self.schedule_next_internet_retry(attempt);

        if let Err(e) = save_res {
            return Err(ServiceRuntimeError::Persistence(e));
        }
        Err(ServiceRuntimeError::Platform(PlatformError::new(err_msg)))
    }

    fn handle_ack_telegram(
        &mut self,
        entry_id: OutboxEntryId,
    ) -> Result<bool, ServiceRuntimeError> {
        if let Some(pos) = self
            .state
            .telegram_outbox
            .iter()
            .position(|e| e.entry_id == entry_id)
        {
            let mut candidate = self.state.clone();
            candidate.telegram_outbox.remove(pos);

            self.log_event("save:ack_telegram");
            self.commit_authoritative_state(candidate)
                .map_err(ServiceRuntimeError::Persistence)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn handle_send_child_message(
        &mut self,
        text: String,
    ) -> Result<MessageId, ServiceRuntimeError> {
        let byte_len = text.as_bytes().len();
        if byte_len == 0 || byte_len > 4096 || text.trim().is_empty() {
            return Err(ServiceRuntimeError::InvalidInput(
                "Child chat message text must be 1..=4096 UTF-8 bytes and non-whitespace"
                    .to_string(),
            ));
        }

        let entry_id = self.id_source.next_outbox_id();
        let message_id = MessageId(entry_id.0);
        let now_utc = self.clock.utc_now();

        let chat_message = ChatMessage {
            id: message_id,
            sender: MessageSender::Child,
            text,
            timestamp: now_utc,
            delivery_status: DeliveryStatus::AcceptedByService,
        };

        let mut candidate = self.state.clone();
        candidate.telegram_outbox.push(TelegramOutboxEntry {
            entry_id,
            payload: TelegramPayload::Chat {
                message: chat_message,
            },
            attempt_count: 0,
            last_error: None,
        });

        self.log_event("save:send_child_message");
        self.commit_authoritative_state(candidate)
            .map_err(ServiceRuntimeError::Persistence)?;

        Ok(message_id)
    }

    fn handle_publish_parent_message(
        &mut self,
        message: ChatMessage,
    ) -> Result<(), ServiceRuntimeError> {
        if message.sender != MessageSender::Parent {
            return Err(ServiceRuntimeError::InvalidInput(
                "Parent chat message must have MessageSender::Parent".to_string(),
            ));
        }

        let _ = self.emit_event(Event::ChatMessageReceived { message });
        Ok(())
    }

    /// Evaluates monotonic time thresholds, deadlines, and retry timers.
    fn process_clock_and_events(&mut self) {
        if self.is_stop_requested() {
            return;
        }

        let now_mono = self.clock.monotonic_now();
        let mut expired_anchors = Vec::new();

        // 1. Evaluate warning thresholds (Sections 12 & 19: do not advance cursor on failure!)
        let timer_keys: Vec<_> = self.monotonic_timers.keys().copied().collect();
        for timer_id in timer_keys {
            let (is_expired, action_kind, anchor_deadline, previous, current, crossed) = {
                if let Some(anchor) = self.monotonic_timers.get(&timer_id) {
                    if now_mono >= anchor.monotonic_target {
                        (
                            true,
                            anchor.action_kind,
                            anchor.utc_deadline,
                            0,
                            0,
                            Vec::new(),
                        )
                    } else {
                        let elapsed = now_mono.saturating_duration_since(anchor.monotonic_start);
                        let remaining = anchor
                            .original_duration_seconds
                            .saturating_sub(elapsed.as_secs());
                        let previous = anchor.last_evaluated_remaining_seconds;
                        let current = remaining;
                        let crossed = if previous > current {
                            if let Some(action) =
                                self.state.active_actions.iter().find(|a| a.id == timer_id)
                            {
                                crossed_warning_thresholds(
                                    previous as i64,
                                    current as i64,
                                    &action.emitted_thresholds,
                                )
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        };
                        (
                            false,
                            anchor.action_kind,
                            anchor.utc_deadline,
                            previous,
                            current,
                            crossed,
                        )
                    }
                } else {
                    continue;
                }
            };

            if is_expired {
                expired_anchors.push((timer_id, action_kind));
            } else if !crossed.is_empty() {
                let mut candidate = self.state.clone();
                if let Some(act) = candidate
                    .active_actions
                    .iter_mut()
                    .find(|a| a.id == timer_id)
                {
                    for t in &crossed {
                        act.emitted_thresholds.insert(*t);
                        let entry_id = self.id_source.next_outbox_id();
                        candidate.telegram_outbox.push(TelegramOutboxEntry {
                            entry_id,
                            payload: TelegramPayload::ServiceNotification {
                                text: format!(
                                    "Warning: Action {:?} has {} seconds remaining",
                                    act.action_kind, current
                                ),
                            },
                            attempt_count: 0,
                            last_error: None,
                        });
                    }

                    self.log_event("save:runtime_warning");
                    match self.commit_authoritative_state(candidate) {
                        Ok(()) => {
                            // ONLY advance the anchor cursor after save succeeds!
                            if let Some(anchor) = self.monotonic_timers.get_mut(&timer_id) {
                                anchor.last_evaluated_remaining_seconds = current;
                            }
                            let now_utc = self.clock.utc_now();
                            for threshold in crossed {
                                let _ = self.emit_event(Event::WarningThresholdReached {
                                    event: WarningEvent {
                                        timer_id,
                                        action_kind,
                                        threshold,
                                        deadline: anchor_deadline,
                                        emitted_at: now_utc,
                                    },
                                });
                            }
                        }
                        Err(_) => {
                            // Persistence failure: mark unhealthy, do NOT advance cursor
                        }
                    }
                }
            } else if previous > current {
                if let Some(anchor) = self.monotonic_timers.get_mut(&timer_id) {
                    anchor.last_evaluated_remaining_seconds = current;
                }
            }
        }

        // 2. Process expired deadlines (Section 4: DO NOT remove anchor before durable transition!)
        for (timer_id, action_kind) in expired_anchors {
            if self.is_stop_requested() {
                return;
            }
            match action_kind {
                ActionKind::BlockInternet => {
                    self.execute_scheduled_internet_deadline(timer_id);
                }
                ActionKind::ShutdownComputer => {
                    self.execute_scheduled_shutdown_deadline(timer_id);
                }
            }
        }

        // 3. Process Internet Retry if due
        if let Some(retry_at) = self.next_retry_at {
            if now_mono >= retry_at {
                if !self.health.persistence_healthy {
                    // Prevent autonomous retry side effects while persistence is Critical.
                    // Keep next_retry_at so that once persistence recovers, retry will fire!
                } else {
                    self.next_retry_at = None;
                    self.process_internet_reconciliation_retry();
                }
            }
        }
    }

    fn execute_scheduled_internet_deadline(&mut self, timer_id: TimerId) {
        if self.is_stop_requested() {
            return;
        }

        let prev_desired = self.state.desired_internet_state;
        let prev_observed = self.observed_internet_state;

        let mut candidate = self.state.clone();
        let act = match candidate
            .active_actions
            .iter_mut()
            .find(|a| a.id == timer_id)
        {
            Some(a) => a,
            None => return,
        };

        let executing_state = match runtime_deadline_transition(&act.execution_state, 0) {
            Some(s @ ActionExecutionState::Executing) => s,
            _ => return,
        };

        act.execution_state = executing_state;
        candidate.desired_internet_state = DesiredInternetState::Blocked;

        self.log_event("save:scheduled_internet_executing");
        if self.commit_authoritative_state(candidate).is_err() {
            return;
        }

        self.monotonic_timers.remove(&timer_id);

        let _ = self.emit_event(Event::TimerExpired {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
        });

        #[cfg(test)]
        if let Some(ref hook) = self.pre_effect_hook {
            hook();
        }

        let child_sid = self.bootstrapped.config.child_sid.clone();
        let (block_res, current_res) = {
            let _gate = self.platform_effect_gate.lock().unwrap();
            if self.is_stop_requested() {
                return;
            }
            self.log_event("gate:block_internet");
            let b_res = self.gate.block_internet(&child_sid);
            let c_res = self.gate.current_state(&child_sid);
            (b_res, c_res)
        };

        if let Ok(obs) = &current_res {
            self.observed_internet_state = *obs;
        } else {
            self.observed_internet_state = InternetState::Unknown;
        }

        self.maybe_emit_internet_policy_changed(
            prev_desired,
            prev_observed,
            StateChangeReason::TimerExpired { timer_id },
        );

        let mut post_candidate = self.state.clone();
        match (block_res, current_res) {
            (Ok(()), Ok(obs)) if obs == InternetState::Blocked => {
                self.mark_gate_success();
                if let Some(act) = post_candidate
                    .active_actions
                    .iter_mut()
                    .find(|a| a.id == timer_id)
                {
                    if let Some(ActionExecutionState::Completed) =
                        execution_success_transition(&act.execution_state)
                    {
                        post_candidate.active_actions.retain(|a| a.id != timer_id);
                    }
                }
                post_candidate.internet_retry = None;
                self.log_event("save:scheduled_internet_completed");
                let _ = self.commit_post_side_effect_candidate(post_candidate);
            }
            (Ok(()), Ok(obs)) => {
                let err_msg = format!(
                    "Scheduled block verification mismatch: observed {:?}, expected Blocked",
                    obs
                );
                self.mark_gate_failure(err_msg.clone());
                if let Some(act) = post_candidate
                    .active_actions
                    .iter_mut()
                    .find(|a| a.id == timer_id)
                {
                    if let Some(next) =
                        execution_failure_transition(&act.execution_state, err_msg.clone())
                    {
                        act.execution_state = next;
                    }
                }
                let attempt = post_candidate
                    .internet_retry
                    .as_ref()
                    .map(|r| r.attempt_count + 1)
                    .unwrap_or(1);
                post_candidate.internet_retry = Some(InternetRetry {
                    attempt_count: attempt,
                    last_error: Some(err_msg.clone()),
                });
                let entry_id = self.id_source.next_outbox_id();
                post_candidate.telegram_outbox.push(TelegramOutboxEntry {
                    entry_id,
                    payload: TelegramPayload::ServiceNotification {
                        text: format!("Scheduled internet block failed: {}", err_msg),
                    },
                    attempt_count: 0,
                    last_error: None,
                });
                self.log_event("save:scheduled_internet_failed");
                let _ = self.commit_post_side_effect_candidate(post_candidate);
                self.schedule_next_internet_retry(attempt);
            }
            (Err(err), _) | (_, Err(err)) => {
                self.mark_gate_failure(err.reason.clone());
                if let Some(act) = post_candidate
                    .active_actions
                    .iter_mut()
                    .find(|a| a.id == timer_id)
                {
                    if let Some(next) =
                        execution_failure_transition(&act.execution_state, err.reason.clone())
                    {
                        act.execution_state = next;
                    }
                }
                let attempt = post_candidate
                    .internet_retry
                    .as_ref()
                    .map(|r| r.attempt_count + 1)
                    .unwrap_or(1);
                post_candidate.internet_retry = Some(InternetRetry {
                    attempt_count: attempt,
                    last_error: Some(err.reason.clone()),
                });
                let entry_id = self.id_source.next_outbox_id();
                post_candidate.telegram_outbox.push(TelegramOutboxEntry {
                    entry_id,
                    payload: TelegramPayload::ServiceNotification {
                        text: format!("Scheduled internet block failed: {}", err.reason),
                    },
                    attempt_count: 0,
                    last_error: None,
                });
                self.log_event("save:scheduled_internet_failed");
                let _ = self.commit_post_side_effect_candidate(post_candidate);
                self.schedule_next_internet_retry(attempt);
            }
        }
    }

    fn execute_scheduled_shutdown_deadline(&mut self, timer_id: TimerId) {
        if self.is_stop_requested() {
            return;
        }

        let mut candidate = self.state.clone();
        let act = match candidate
            .active_actions
            .iter_mut()
            .find(|a| a.id == timer_id)
        {
            Some(a) => a,
            None => return,
        };

        let executing_state = match runtime_deadline_transition(&act.execution_state, 0) {
            Some(s @ ActionExecutionState::Executing) => s,
            _ => return,
        };

        act.execution_state = executing_state;

        self.log_event("save:scheduled_shutdown_executing");
        if self.commit_authoritative_state(candidate).is_err() {
            return;
        }

        self.monotonic_timers.remove(&timer_id);

        let _ = self.emit_event(Event::TimerExpired {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
        });

        #[cfg(test)]
        if let Some(ref hook) = self.pre_effect_hook {
            hook();
        }

        let power_res = {
            let _gate = self.platform_effect_gate.lock().unwrap();
            if self.is_stop_requested() {
                return;
            }
            self.log_event("power:initiate_shutdown");
            self.power.initiate_shutdown()
        };

        let mut post_candidate = self.state.clone();
        if let Some(act) = post_candidate
            .active_actions
            .iter_mut()
            .find(|a| a.id == timer_id)
        {
            match power_res {
                Ok(()) => {
                    self.mutate_health_and_maybe_emit(|s| {
                        s.power_error = None;
                    });

                    let prev_shutdown = self.shutdown_state;
                    self.shutdown_state = ShutdownState::InProgress;
                    self.maybe_emit_shutdown_state_changed(
                        prev_shutdown,
                        ShutdownState::InProgress,
                    );

                    if let Some(ActionExecutionState::Completed) =
                        execution_success_transition(&act.execution_state)
                    {
                        post_candidate.active_actions.retain(|a| a.id != timer_id);
                    }

                    self.log_event("save:scheduled_shutdown_completed");
                    let _ = self.commit_post_side_effect_candidate(post_candidate);
                }
                Err(err) => {
                    self.mutate_health_and_maybe_emit(|s| {
                        s.power_error = Some(err.reason.clone());
                    });

                    if let Some(next) =
                        execution_failure_transition(&act.execution_state, err.reason.clone())
                    {
                        act.execution_state = next;
                    }

                    let entry_id = self.id_source.next_outbox_id();
                    post_candidate.telegram_outbox.push(TelegramOutboxEntry {
                        entry_id,
                        payload: TelegramPayload::ServiceNotification {
                            text: format!("Scheduled shutdown failed: {}", err.reason),
                        },
                        attempt_count: 0,
                        last_error: None,
                    });

                    let previous_shutdown_state = self.shutdown_state;
                    let target_shutdown_state =
                        derive_shutdown_state(self.shutdown_state, &post_candidate.active_actions);

                    // Synchronize local aggregate state BEFORE commit_post_side_effect_candidate can fail
                    self.shutdown_state = target_shutdown_state;

                    self.log_event("save:scheduled_shutdown_failed");
                    let save_res = self.commit_post_side_effect_candidate(post_candidate);
                    if save_res.is_ok() {
                        self.maybe_emit_shutdown_state_changed(
                            previous_shutdown_state,
                            target_shutdown_state,
                        );
                    }
                }
            }
        }
    }

    fn process_internet_reconciliation_retry(&mut self) {
        let prev_desired = self.state.desired_internet_state;
        let prev_observed = self.observed_internet_state;

        #[cfg(test)]
        if let Some(ref hook) = self.pre_effect_hook {
            hook();
        }

        let child_sid = self.bootstrapped.config.child_sid.clone();
        let (res, current_res) = {
            let _gate = self.platform_effect_gate.lock().unwrap();
            if self.is_stop_requested() {
                return;
            }
            let res = match self.state.desired_internet_state {
                DesiredInternetState::Blocked => {
                    self.log_event("gate:block_internet");
                    self.gate.block_internet(&child_sid)
                }
                DesiredInternetState::Unrestricted => {
                    self.log_event("gate:unblock_internet");
                    self.gate.unblock_internet(&child_sid)
                }
            };
            let current_res = self.gate.current_state(&child_sid);
            (res, current_res)
        };

        if let Ok(obs) = &current_res {
            self.observed_internet_state = *obs;
        } else {
            self.observed_internet_state = InternetState::Unknown;
        }

        self.maybe_emit_internet_policy_changed(
            prev_desired,
            prev_observed,
            StateChangeReason::PlatformSync,
        );

        let desired_matches = match (self.state.desired_internet_state, &current_res) {
            (DesiredInternetState::Blocked, Ok(InternetState::Blocked)) => true,
            (DesiredInternetState::Unrestricted, Ok(InternetState::Unrestricted)) => true,
            _ => false,
        };

        match (res, current_res) {
            (Ok(()), Ok(_)) if desired_matches => {
                let mut candidate = self.state.clone();
                candidate.internet_retry = None;

                // Resolve executing BlockInternet actions per Section 7, requiring core Completed transition.
                // Do NOT silently delete Failed actions!
                if self.state.desired_internet_state == DesiredInternetState::Blocked {
                    candidate.active_actions.retain(|a| {
                        if a.action_kind == ActionKind::BlockInternet {
                            execution_success_transition(&a.execution_state)
                                != Some(ActionExecutionState::Completed)
                        } else {
                            true
                        }
                    });
                }

                self.mark_gate_success();
                self.log_event("save:retry_success");
                let _ = self.commit_post_side_effect_candidate(candidate);
            }
            (Ok(()), Ok(obs)) => {
                // Verification mismatch is failure! (Section 8)
                let err_msg = format!(
                    "Reconciliation retry mismatch: observed {:?}, expected {:?}",
                    obs, self.state.desired_internet_state
                );
                self.record_retry_failure(err_msg);
            }
            (Err(err), _) => {
                self.record_retry_failure(err.reason);
            }
            (_, Err(err)) => {
                self.record_retry_failure(err.reason);
            }
        }
    }

    fn record_retry_failure(&mut self, err_msg: String) {
        self.mark_gate_failure(err_msg.clone());

        let mut candidate = self.state.clone();
        let attempt = candidate
            .internet_retry
            .as_ref()
            .map(|r| r.attempt_count + 1)
            .unwrap_or(1);
        candidate.internet_retry = Some(InternetRetry {
            attempt_count: attempt,
            last_error: Some(err_msg.clone()),
        });

        let entry_id = self.id_source.next_outbox_id();
        candidate.telegram_outbox.push(TelegramOutboxEntry {
            entry_id,
            payload: TelegramPayload::ServiceNotification {
                text: format!("Internet reconciliation retry failed: {}", err_msg),
            },
            attempt_count: 0,
            last_error: None,
        });

        self.log_event("save:retry_metadata");
        let _ = self.commit_post_side_effect_candidate(candidate);

        self.schedule_next_internet_retry(attempt);
    }

    /// Wake-driven timeout computation: calculates earliest future event or None if no timed work (Section 15).
    fn next_wake_timeout(&self) -> Option<Duration> {
        // Gate autonomous timed re-evaluation while persistence is Critical (Section 6)
        if !self.health.persistence_healthy {
            return None;
        }

        let now_mono = self.clock.monotonic_now();
        let mut earliest: Option<Instant> = None;

        for (timer_id, anchor) in &self.monotonic_timers {
            // 1. Deadline target
            earliest = Some(match earliest {
                Some(e) => e.min(anchor.monotonic_target),
                None => anchor.monotonic_target,
            });

            // 2. Next uncrossed warning threshold
            if let Some(action) = self.state.active_actions.iter().find(|a| a.id == *timer_id) {
                for threshold in WarningThreshold::ALL {
                    let threshold_secs = threshold.seconds() as u64;
                    if anchor.original_duration_seconds > threshold_secs
                        && !action.emitted_thresholds.contains(&threshold)
                    {
                        let offset = anchor.original_duration_seconds - threshold_secs;
                        let threshold_instant =
                            anchor.monotonic_start + Duration::from_secs(offset);
                        earliest = Some(match earliest {
                            Some(e) => e.min(threshold_instant),
                            None => threshold_instant,
                        });
                    }
                }
            }
        }

        // 3. Internet retry event
        if let Some(retry_at) = self.next_retry_at {
            earliest = Some(match earliest {
                Some(e) => e.min(retry_at),
                None => retry_at,
            });
        }

        earliest.map(|target| {
            if target <= now_mono {
                Duration::ZERO
            } else {
                target - now_mono
            }
        })
    }
}

// ============================================================================
// 8. PUBLIC RUNTIME HANDLE & SERVICE RUNTIME
// ============================================================================

/// Thread-safe handle for submitting operations to the authoritative coordinator.
#[derive(Clone)]
pub struct RuntimeHandle {
    command_tx: Sender<RuntimeCommand>,
    ingress: Arc<Mutex<bool>>,
}

impl RuntimeHandle {
    pub fn schedule_internet_block(
        &self,
        duration_seconds: u32,
        initiator: Initiator,
    ) -> Result<TimerId, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::ScheduleAction {
                    action_kind: ActionKind::BlockInternet,
                    duration_seconds,
                    initiator,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    pub fn schedule_shutdown(
        &self,
        duration_seconds: u32,
        initiator: Initiator,
    ) -> Result<TimerId, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::ScheduleAction {
                    action_kind: ActionKind::ShutdownComputer,
                    duration_seconds,
                    initiator,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    #[allow(dead_code)]
    pub(crate) fn cancel_internet_block_timer(
        &self,
        timer_id: TimerId,
        initiator: Initiator,
    ) -> Result<TimerCancellationResult, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::CancelExactTimer {
                    timer_id,
                    expected_action_kind: ActionKind::BlockInternet,
                    initiator,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    #[allow(dead_code)]
    pub(crate) fn cancel_shutdown_timer(
        &self,
        timer_id: TimerId,
        initiator: Initiator,
    ) -> Result<TimerCancellationResult, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::CancelExactTimer {
                    timer_id,
                    expected_action_kind: ActionKind::ShutdownComputer,
                    initiator,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    pub fn immediate_internet_block(
        &self,
        initiator: Initiator,
    ) -> Result<(), ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::ImmediateInternetBlock {
                    initiator,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    pub fn restore_internet(&self, initiator: Initiator) -> Result<(), ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::RestoreInternet {
                    initiator,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    pub fn ack_telegram(&self, entry_id: OutboxEntryId) -> Result<bool, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::AckTelegram {
                    entry_id,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    pub fn query_status(&self) -> Result<StatusSnapshot, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::QueryStatus { reply: reply_tx })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))
    }

    #[allow(dead_code)]
    pub(crate) fn subscribe_events(&self) -> Result<EventSubscription, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::SubscribeEvents { reply: reply_tx })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    #[allow(dead_code)]
    pub(crate) fn unsubscribe_events(
        &self,
        subscription_id: SubscriptionId,
    ) -> Result<bool, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::UnsubscribeEvents {
                    subscription_id,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    #[allow(dead_code)]
    pub(crate) fn send_child_message(
        &self,
        text: String,
    ) -> Result<MessageId, ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::SendChildMessage {
                    text,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    #[allow(dead_code)]
    pub(crate) fn publish_parent_message(
        &self,
        message: ChatMessage,
    ) -> Result<(), ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::PublishParentMessage {
                    message,
                    reply: reply_tx,
                })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))?
    }

    #[cfg(test)]
    pub(crate) fn tick(&self) -> Result<(), ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::Tick { reply: reply_tx })
                .map_err(|e| {
                    ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string()))
                })?;
            reply_rx
        };
        reply_rx
            .recv()
            .map_err(|e| ServiceRuntimeError::Scheduler(SchedulerError::Channel(e.to_string())))
    }
}

/// The authoritative long-lived service runtime instance.
pub struct ServiceRuntime {
    command_tx: Sender<RuntimeCommand>,
    worker_handle: Option<JoinHandle<Result<(), TeardownError>>>,
    ingress: Arc<Mutex<bool>>,
    pub(crate) stop_requested: Arc<AtomicBool>,
    pub(crate) platform_effect_gate: Arc<Mutex<()>>,
    #[cfg(test)]
    pub(crate) stop_effect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    readiness: StartupReadiness,
    handle: RuntimeHandle,
}

impl ServiceRuntime {
    /// Canonical public production constructor.
    ///
    /// Consumes `bootstrapped` by value and derives the canonical state store internally
    /// from `bootstrapped.paths.state()`. Mandatory abstract platform ports must be provided
    /// by the caller/integration layer.
    pub fn start<G, P, C, I, R>(
        bootstrapped: BootstrappedServiceState,
        gate: G,
        power: P,
        clock: C,
        id_source: I,
        retry_policy: R,
    ) -> Result<Self, ServiceRuntimeError>
    where
        G: InternetGate + 'static,
        P: PowerController + 'static,
        C: RuntimeClock + 'static,
        I: IdSource + 'static,
        R: InternetRetryPolicy + 'static,
    {
        let store = StateFileStoreAdapter::new(StateFileStore::new(bootstrapped.paths.state()));
        Self::start_internal(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry_policy,
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_store<S, G, P, C, I, R>(
        bootstrapped: BootstrappedServiceState,
        store: S,
        gate: G,
        power: P,
        clock: C,
        id_source: I,
        retry_policy: R,
        call_log: Option<Arc<Mutex<Vec<String>>>>,
    ) -> Result<Self, ServiceRuntimeError>
    where
        S: RuntimeStateStore + 'static,
        G: InternetGate + 'static,
        P: PowerController + 'static,
        C: RuntimeClock + 'static,
        I: IdSource + 'static,
        R: InternetRetryPolicy + 'static,
    {
        Self::start_internal(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry_policy,
            call_log,
            None,
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_test_hooks<S, G, P, C, I, R>(
        bootstrapped: BootstrappedServiceState,
        store: S,
        gate: G,
        power: P,
        clock: C,
        id_source: I,
        retry_policy: R,
        call_log: Option<Arc<Mutex<Vec<String>>>>,
        pre_effect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
        stop_effect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self, ServiceRuntimeError>
    where
        S: RuntimeStateStore + 'static,
        G: InternetGate + 'static,
        P: PowerController + 'static,
        C: RuntimeClock + 'static,
        I: IdSource + 'static,
        R: InternetRetryPolicy + 'static,
    {
        Self::start_internal(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry_policy,
            call_log,
            pre_effect_hook,
            stop_effect_hook,
        )
    }

    fn start_internal<S, G, P, C, I, R>(
        bootstrapped: BootstrappedServiceState,
        store: S,
        gate: G,
        power: P,
        clock: C,
        id_source: I,
        retry_policy: R,
        #[cfg(test)] call_log: Option<Arc<Mutex<Vec<String>>>>,
        #[cfg(test)] pre_effect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
        #[cfg(test)] stop_effect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self, ServiceRuntimeError>
    where
        S: RuntimeStateStore + 'static,
        G: InternetGate + 'static,
        P: PowerController + 'static,
        C: RuntimeClock + 'static,
        I: IdSource + 'static,
        R: InternetRetryPolicy + 'static,
    {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let platform_effect_gate = Arc::new(Mutex::new(()));
        let (mut coordinator, readiness) = ServiceRuntimeCoordinator::new(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry_policy,
            #[cfg(test)]
            call_log,
            stop_requested.clone(),
            platform_effect_gate.clone(),
            #[cfg(test)]
            pre_effect_hook,
        )?;

        let (command_tx, command_rx): (Sender<RuntimeCommand>, Receiver<RuntimeCommand>) =
            channel();
        let ingress = Arc::new(Mutex::new(false));

        let worker_handle = spawn(move || -> Result<(), TeardownError> {
            loop {
                let timeout = coordinator.next_wake_timeout();
                let cmd_res = match timeout {
                    Some(t) => command_rx.recv_timeout(t),
                    None => command_rx
                        .recv()
                        .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
                };
                match cmd_res {
                    Ok(cmd) => {
                        let is_stop = matches!(cmd, RuntimeCommand::Stop { .. });
                        coordinator.handle_command(cmd);
                        if is_stop {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        coordinator.process_clock_and_events();
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                }
            }
            Ok(())
        });

        let handle = RuntimeHandle {
            command_tx: command_tx.clone(),
            ingress: ingress.clone(),
        };

        Ok(Self {
            command_tx,
            worker_handle: Some(worker_handle),
            ingress,
            stop_requested,
            platform_effect_gate,
            #[cfg(test)]
            stop_effect_hook,
            readiness,
            handle,
        })
    }

    pub fn readiness(&self) -> &StartupReadiness {
        &self.readiness
    }

    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }

    /// Linearized stop sequence.
    ///
    /// Lock Ordering:
    /// 1. INGRESS (self.ingress)
    /// 2. PLATFORM_EFFECT_GATE (self.platform_effect_gate)
    ///
    /// Coordinator acquires PLATFORM_EFFECT_GATE only during platform entry and never acquires INGRESS.
    /// Neither lock is held while waiting for the command teardown reply or worker join.
    pub fn stop(&mut self) -> Result<(), ServiceRuntimeError> {
        let reply_rx = {
            let mut ingress_guard = self.ingress.lock().unwrap();
            if *ingress_guard {
                return Ok(());
            }
            *ingress_guard = true;

            let (reply_tx, reply_rx) = channel();
            {
                let _effect_guard = self.platform_effect_gate.lock().unwrap();
                self.stop_requested.store(true, Ordering::SeqCst);
                #[cfg(test)]
                if let Some(ref hook) = self.stop_effect_hook {
                    hook();
                }
                let _ = self
                    .command_tx
                    .send(RuntimeCommand::Stop { reply: reply_tx });
            }
            reply_rx
        };

        let teardown_res = reply_rx
            .recv()
            .map_err(|_| {
                TeardownError::JoinFailed("Runtime coordinator dropped teardown reply".to_string())
            })
            .map_err(ServiceRuntimeError::Teardown);

        let join_res = if let Some(worker) = self.worker_handle.take() {
            worker
                .join()
                .map_err(|_| TeardownError::JoinFailed("Runtime coordinator panicked".to_string()))
                .map_err(ServiceRuntimeError::Teardown)
        } else {
            Ok(Ok(()))
        };

        // Worker must ALWAYS be joined before error is returned!
        join_res??;
        match teardown_res {
            Ok(Ok(())) => Ok(()),
            Ok(Err(teardown_err)) => Err(ServiceRuntimeError::Teardown(teardown_err)),
            Err(e) => Err(e),
        }
    }
}

impl Drop for ServiceRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ============================================================================
// 9. NORMATIVE RT-01..RT-20 & CORR-01..CORR-12 TEST MATRIX
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_persistence::PersistentConfig;
    use crate::credentials_persistence::PersistentCredentials;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // --- Deterministic Test Fakes ---

    #[derive(Clone)]
    struct FakeStateStore {
        pub state: Arc<Mutex<PersistentState>>,
        pub save_count: Arc<AtomicU32>,
        pub load_count: Arc<AtomicU32>,
        pub fail_saves: Arc<AtomicBool>,
        pub fail_after_n_saves: Arc<AtomicU32>,
        pub log: Arc<Mutex<Vec<String>>>,
        pub on_save: Arc<Mutex<Option<Box<dyn Fn(&PersistentState) + Send + Sync>>>>,
    }

    impl FakeStateStore {
        fn new(initial: PersistentState, log: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                state: Arc::new(Mutex::new(initial)),
                save_count: Arc::new(AtomicU32::new(0)),
                load_count: Arc::new(AtomicU32::new(0)),
                fail_saves: Arc::new(AtomicBool::new(false)),
                fail_after_n_saves: Arc::new(AtomicU32::new(0)),
                log,
                on_save: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl RuntimeStateStore for FakeStateStore {
        fn save(&self, state: &PersistentState) -> Result<(), StateStoreError> {
            if self.fail_saves.load(Ordering::SeqCst) {
                return Err(StateStoreError::InvalidPath(
                    "Injected save failure".to_string(),
                ));
            }
            let limit = self.fail_after_n_saves.load(Ordering::SeqCst);
            let count = self.save_count.fetch_add(1, Ordering::SeqCst);
            if limit > 0 && count >= limit {
                return Err(StateStoreError::InvalidPath(
                    "Injected save failure after limit".to_string(),
                ));
            }
            *self.state.lock().unwrap() = state.clone();
            self.log
                .lock()
                .unwrap()
                .push(format!("save: {:?}", state.desired_internet_state));
            if let Some(ref cb) = *self.on_save.lock().unwrap() {
                cb(state);
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeClock {
        pub utc: Arc<Mutex<i64>>,
        pub mono: Arc<Mutex<Instant>>,
    }

    impl FakeClock {
        fn new(initial_utc_ms: i64) -> Self {
            Self {
                utc: Arc::new(Mutex::new(initial_utc_ms)),
                mono: Arc::new(Mutex::new(Instant::now())),
            }
        }

        fn advance(&self, duration: Duration) {
            *self.utc.lock().unwrap() += duration.as_millis() as i64;
            *self.mono.lock().unwrap() += duration;
        }

        fn shift_utc_only(&self, delta_ms: i64) {
            *self.utc.lock().unwrap() += delta_ms;
        }
    }

    impl RuntimeClock for FakeClock {
        fn utc_now(&self) -> UtcDateTime {
            UtcDateTime(*self.utc.lock().unwrap())
        }

        fn monotonic_now(&self) -> Instant {
            *self.mono.lock().unwrap()
        }
    }

    #[derive(Clone)]
    struct FakeInternetGate {
        pub current: Arc<Mutex<InternetState>>,
        pub fail_calls: Arc<AtomicBool>,
        pub fail_state_change: Arc<AtomicBool>,
        pub log: Arc<Mutex<Vec<String>>>,
        pub on_block: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>,
    }

    impl FakeInternetGate {
        fn new(initial: InternetState, log: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                current: Arc::new(Mutex::new(initial)),
                fail_calls: Arc::new(AtomicBool::new(false)),
                fail_state_change: Arc::new(AtomicBool::new(false)),
                log,
                on_block: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl InternetGate for FakeInternetGate {
        fn current_state(&self, _child_sid: &str) -> Result<InternetState, PlatformError> {
            if self.fail_calls.load(Ordering::SeqCst) {
                return Err(PlatformError::new("Gate query error"));
            }
            let s = *self.current.lock().unwrap();
            self.log
                .lock()
                .unwrap()
                .push(format!("gate:current_state:{:?}", s));
            Ok(s)
        }

        fn block_internet(&self, _child_sid: &str) -> Result<(), PlatformError> {
            if let Some(ref cb) = *self.on_block.lock().unwrap() {
                cb();
            }
            if self.fail_calls.load(Ordering::SeqCst) {
                return Err(PlatformError::new("Gate block error"));
            }
            if !self.fail_state_change.load(Ordering::SeqCst) {
                *self.current.lock().unwrap() = InternetState::Blocked;
            }
            self.log
                .lock()
                .unwrap()
                .push("gate:block_internet".to_string());
            Ok(())
        }

        fn unblock_internet(&self, _child_sid: &str) -> Result<(), PlatformError> {
            if self.fail_calls.load(Ordering::SeqCst) {
                return Err(PlatformError::new("Gate unblock error"));
            }
            if !self.fail_state_change.load(Ordering::SeqCst) {
                *self.current.lock().unwrap() = InternetState::Unrestricted;
            }
            self.log
                .lock()
                .unwrap()
                .push("gate:unblock_internet".to_string());
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakePowerController {
        pub fail_shutdown: Arc<AtomicBool>,
        pub log: Arc<Mutex<Vec<String>>>,
        pub on_initiate_shutdown: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>,
    }

    impl FakePowerController {
        fn new(log: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                fail_shutdown: Arc::new(AtomicBool::new(false)),
                log,
                on_initiate_shutdown: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl PowerController for FakePowerController {
        fn initiate_shutdown(&self) -> Result<(), PlatformError> {
            if let Some(ref cb) = *self.on_initiate_shutdown.lock().unwrap() {
                cb();
            }
            if self.fail_shutdown.load(Ordering::SeqCst) {
                self.log
                    .lock()
                    .unwrap()
                    .push("power:shutdown_failed".to_string());
                return Err(PlatformError::new("Power shutdown failed"));
            }
            self.log
                .lock()
                .unwrap()
                .push("power:initiate_shutdown".to_string());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeIdSource {
        counter: Arc<AtomicU32>,
    }

    impl FakeIdSource {
        fn new() -> Self {
            Self {
                counter: Arc::new(AtomicU32::new(1)),
            }
        }
    }

    impl IdSource for FakeIdSource {
        fn next_timer_id(&self) -> TimerId {
            let c = self.counter.fetch_add(1, Ordering::SeqCst);
            let mut b = [0u8; 16];
            b[15] = c as u8;
            TimerId(b)
        }

        fn next_outbox_id(&self) -> OutboxEntryId {
            let c = self.counter.fetch_add(1, Ordering::SeqCst);
            let mut b = [0u8; 16];
            b[15] = c as u8;
            OutboxEntryId(b)
        }
    }

    #[derive(Clone, Default)]
    struct TestRetryPolicy {
        delay: Duration,
    }

    impl TestRetryPolicy {
        fn new(delay: Duration) -> Self {
            Self { delay }
        }
    }

    impl InternetRetryPolicy for TestRetryPolicy {
        fn delay_for_attempt(&self, _attempt_count: u32) -> Duration {
            self.delay
        }
    }

    static TEST_DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sample_bootstrapped_state(state: PersistentState) -> BootstrappedServiceState {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "palka_rt_test_{}_{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let paths = crate::persistent_root::canonical_paths_for_test(&dir).unwrap();
        let _ = std::fs::create_dir_all(paths.root());
        BootstrappedServiceState {
            paths,
            config: PersistentConfig {
                child_sid: "S-1-5-21-test".to_string(),
                telegram_allowed_user_ids: vec![123456789],
                telegram_allowed_chat_ids: vec![987654321],
                heartbeat_interval_seconds: 60,
            },
            credentials: PersistentCredentials {
                pin_hash: "$argon2id$v=19$m=65536,t=3,p=1$c2FsdHNhbHQ$dGVzdGhhc2g".to_string(),
                telegram_bot_token_dpapi: vec![1, 2, 3, 4],
            },
            state,
        }
    }

    // --- RT-01..RT-20 Verifications ---

    #[test]
    fn rt_01_bootstrapped_state_consumed_by_value_without_disk_reread() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        assert_eq!(store.load_count.load(Ordering::SeqCst), 0);
        assert!(runtime.readiness().is_degraded());
    }

    #[test]
    fn rt_02_overdue_startup_block_internet_persists_executing_before_gate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([1; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(500000)),
            created_at: UtcDateTime(400000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        let entries = log.lock().unwrap().clone();
        let save_idx = entries.iter().position(|e| e == "save: Blocked").unwrap();
        let gate_idx = entries
            .iter()
            .position(|e| e == "gate:block_internet")
            .unwrap();
        assert!(
            save_idx < gate_idx,
            "Durable save must occur BEFORE platform gate call"
        );

        // Strengthened Section 15: readiness returned only after second save removes resolved action
        assert!(store.save_count.load(Ordering::SeqCst) >= 2);
        let snap = runtime.readiness().snapshot();
        assert_eq!(snap.desired_internet_state, DesiredInternetState::Blocked);
        assert_eq!(snap.observed_internet_state, InternetState::Blocked);
        assert!(
            snap.active_actions.is_empty(),
            "No obsolete overdue BlockInternet action in final snapshot"
        );
    }

    #[test]
    fn rt_03_overdue_startup_shutdown_becomes_durable_missed_without_calling_power() {
        // Section 18: RT-03 must independently prove overdue ShutdownComputer behavior
        // from all 3 recovery states: Pending, Executing, Failed { reason }
        let test_states = vec![
            ActionExecutionState::Pending,
            ActionExecutionState::Executing,
            ActionExecutionState::Failed {
                reason: "previous failure".to_string(),
            },
        ];

        for (i, exec_state) in test_states.into_iter().enumerate() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let mut initial_state = PersistentState {
                desired_internet_state: DesiredInternetState::Unrestricted,
                active_actions: Vec::new(),
                internet_retry: None,
                telegram_outbox: Vec::new(),
            };
            initial_state.active_actions.push(ScheduledAction {
                id: TimerId([(i + 1) as u8; 16]),
                action_kind: ActionKind::ShutdownComputer,
                deadline: Deadline(UtcDateTime(500000)),
                created_at: UtcDateTime(400000),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: exec_state,
            });

            let store = FakeStateStore::new(initial_state.clone(), log.clone());
            let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
            let power = FakePowerController::new(log.clone());
            let clock = FakeClock::new(1000000);
            let id_source = FakeIdSource::new();
            let retry = TestRetryPolicy::new(Duration::from_secs(5));

            let bootstrapped = sample_bootstrapped_state(initial_state);
            let runtime = ServiceRuntime::start_with_store(
                bootstrapped,
                store.clone(),
                gate,
                power,
                clock,
                id_source,
                retry,
                Some(log.clone()),
            )
            .expect("Runtime construction must succeed");

            let entries = log.lock().unwrap().clone();
            assert!(!entries.contains(&"power:initiate_shutdown".to_string()));
            assert!(!entries.contains(&"power:shutdown_failed".to_string()));

            let snap = runtime.handle().query_status().unwrap();
            assert!(
                snap.active_actions.is_empty(),
                "Overdue shutdown must be removed from active_actions"
            );
            let saved_state = store.state.lock().unwrap().clone();
            assert_eq!(
                saved_state.telegram_outbox.len(),
                1,
                "Missed notification must be enqueued"
            );
            assert_eq!(snap.shutdown_state, ShutdownState::Idle);
        }
    }

    #[test]
    fn rt_04_offline_crossed_warning_thresholds_marked_without_retroactive_outbox() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        // Future timer with 25 minutes remaining (1500s). Originally 90m. M60 and M30 crossed while offline.
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([3; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(1000000 + 1500 * 1000)),
            created_at: UtcDateTime(1000000 - 3900 * 1000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.active_actions.len(), 1);
        let emitted = &snap.active_actions[0].emitted_thresholds;
        assert!(emitted.contains(&WarningThreshold::M60));
        assert!(emitted.contains(&WarningThreshold::M30));

        let saved = store.state.lock().unwrap().clone();
        assert!(
            saved.telegram_outbox.is_empty(),
            "Offline crossed thresholds must NOT generate retroactive outbox notifications"
        );
    }

    #[test]
    fn rt_05_live_warning_threshold_and_outbox_committed_atomically_once() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        // Schedule action for 185 seconds (M3 is 180s)
        let _ = runtime
            .handle()
            .schedule_internet_block(185, Initiator::ParentLocalPin)
            .unwrap();
        clock.advance(Duration::from_secs(10));
        runtime.handle().tick().unwrap();

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(
            saved.telegram_outbox.len(),
            1,
            "Exactly one warning notification emitted"
        );
        let action = saved
            .active_actions
            .iter()
            .find(|a| a.action_kind == ActionKind::BlockInternet)
            .unwrap();
        assert!(
            action.emitted_thresholds.contains(&WarningThreshold::M3),
            "Threshold atomically marked in candidate"
        );

        // Subsequent evaluation does not duplicate warning
        clock.advance(Duration::from_secs(10));
        runtime.handle().tick().unwrap();
        let saved2 = store.state.lock().unwrap().clone();
        assert_eq!(saved2.telegram_outbox.len(), 1, "Warning not duplicated");
    }

    #[test]
    fn rt_06_immediate_block_persists_desired_blocked_before_gate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        runtime
            .handle()
            .immediate_internet_block(Initiator::ParentLocalPin)
            .unwrap();

        let entries = log.lock().unwrap().clone();
        let save_idx = entries.iter().position(|e| e == "save: Blocked").unwrap();
        let gate_idx = entries
            .iter()
            .position(|e| e == "gate:block_internet")
            .unwrap();
        assert!(save_idx < gate_idx);
        assert_eq!(store.state.lock().unwrap().active_actions.len(), 0);
    }

    #[test]
    fn rt_07_restore_persists_unrestricted_before_gate_and_no_rollback_on_gate_error() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate.clone(),
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        // Fail gate unblock
        gate.fail_calls.store(true, Ordering::SeqCst);
        let res = runtime.handle().restore_internet(Initiator::ParentLocalPin);
        assert!(res.is_err());

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(
            saved.desired_internet_state,
            DesiredInternetState::Unrestricted
        );
        assert!(saved.internet_retry.is_some());
    }

    #[test]
    fn rt_08_scheduled_block_deadline_persists_executing_before_gate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_internet_block(10, Initiator::ParentLocalPin)
            .unwrap();
        log.lock().unwrap().clear();

        // Advance clock past deadline
        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        let entries = log.lock().unwrap().clone();
        let save_idx = entries.iter().position(|e| e == "save: Blocked").unwrap();
        let gate_idx = entries
            .iter()
            .position(|e| e == "gate:block_internet")
            .unwrap();
        assert!(save_idx < gate_idx);
    }

    #[test]
    fn rt_09_successful_runtime_shutdown_follows_durable_executing_ok_inprogress_terminal_removal()
    {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .unwrap();
        log.lock().unwrap().clear();

        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // Section 20: explicitly prove ordered operations in log
        let entries = log.lock().unwrap().clone();
        let executing_save_idx = entries
            .iter()
            .position(|e| e == "save:scheduled_shutdown_executing")
            .unwrap();
        let power_idx = entries
            .iter()
            .position(|e| e == "power:initiate_shutdown")
            .unwrap();
        let completed_save_idx = entries
            .iter()
            .position(|e| e == "save:scheduled_shutdown_completed")
            .unwrap();

        assert!(executing_save_idx < power_idx);
        assert!(power_idx < completed_save_idx);

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.shutdown_state, ShutdownState::InProgress);
        assert!(snap.active_actions.is_empty());
    }

    #[test]
    fn rt_10_shutdown_power_failure_persists_failed_and_no_inprogress() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        power.fail_shutdown.store(true, Ordering::SeqCst);

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .unwrap();

        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.shutdown_state, ShutdownState::Idle);

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(saved.active_actions.len(), 1);
        assert!(matches!(
            saved.active_actions[0].execution_state,
            ActionExecutionState::Failed { .. }
        ));
    }

    #[test]
    fn rt_11_persistence_failure_before_side_effect_prevents_side_effect() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        store.fail_saves.store(true, Ordering::SeqCst);
        let res = runtime
            .handle()
            .immediate_internet_block(Initiator::ParentLocalPin);
        assert!(res.is_err());

        let entries = log.lock().unwrap().clone();
        assert!(!entries.contains(&"gate:block_internet".to_string()));
    }

    #[test]
    fn rt_12_concurrent_submissions_serialize_without_competing_writes() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let handle = runtime.handle().clone();
        let mut threads = Vec::new();
        for i in 0..10 {
            let h = handle.clone();
            threads.push(std::thread::spawn(move || {
                h.schedule_internet_block(100 + i, Initiator::ParentLocalPin)
            }));
        }

        for t in threads {
            let res = t.join().unwrap();
            assert!(res.is_ok());
        }

        let snap = handle.query_status().unwrap();
        assert_eq!(snap.active_actions.len(), 10);
    }

    #[test]
    fn rt_13_status_snapshot_represents_desired_observed_mismatch() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate.clone(),
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        // Case 1: desired Blocked / observed Unrestricted
        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.desired_internet_state, DesiredInternetState::Blocked);
        assert_eq!(snap.observed_internet_state, InternetState::Unrestricted);

        // Case 2: desired Unrestricted / observed Blocked
        *gate.current.lock().unwrap() = InternetState::Blocked;
        let _ = runtime.handle().restore_internet(Initiator::ParentLocalPin);
        let snap2 = runtime.handle().query_status().unwrap();
        assert_eq!(
            snap2.desired_internet_state,
            DesiredInternetState::Unrestricted
        );
        assert_eq!(snap2.observed_internet_state, InternetState::Blocked);
    }

    #[test]
    fn rt_14_wall_clock_shifts_do_not_distort_monotonic_timer_behavior() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_internet_block(100, Initiator::ParentLocalPin)
            .unwrap();

        // Shift wall clock backward by 1 hour (monotonic does not change)
        clock.shift_utc_only(-3600 * 1000);
        runtime.handle().tick().unwrap();
        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(
            snap.active_actions.len(),
            1,
            "Timer must not expire or extend from backward wall-clock jump"
        );

        // Shift wall clock forward by 1 hour
        clock.shift_utc_only(7200 * 1000);
        runtime.handle().tick().unwrap();
        let snap2 = runtime.handle().query_status().unwrap();
        assert_eq!(
            snap2.active_actions.len(),
            1,
            "Timer must not expire early from forward wall-clock jump"
        );
    }

    #[test]
    fn rt_15_future_timers_survive_service_teardown_unchanged() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_internet_block(3600, Initiator::ParentLocalPin)
            .unwrap();
        runtime.stop().expect("Stop must succeed");

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(saved.active_actions.len(), 1);
        assert_eq!(
            saved.active_actions[0].action_kind,
            ActionKind::BlockInternet
        );
    }

    #[test]
    fn rt_16_service_teardown_does_not_unblock_internet() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        runtime.stop().expect("Stop must succeed");

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(
            saved.desired_internet_state,
            DesiredInternetState::Blocked,
            "Teardown must preserve desired Blocked state"
        );
        let entries = log.lock().unwrap().clone();
        assert!(
            !entries.contains(&"gate:unblock_internet".to_string()),
            "Teardown must not call unblock_internet"
        );
    }

    #[test]
    fn rt_17_new_mutation_after_stopping_begins_is_rejected() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let handle = runtime.handle().clone();
        runtime.stop().expect("Stop must succeed");

        let res = handle.schedule_internet_block(100, Initiator::ParentLocalPin);
        assert!(matches!(res, Err(ServiceRuntimeError::Stopping)));
    }

    #[test]
    fn rt_18_all_runtime_workers_joined_when_teardown_returns() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        runtime.stop().expect("Stop must succeed");
        assert!(runtime.worker_handle.is_none());
    }

    #[test]
    fn rt_19_readiness_barrier_reported_after_recovery_persistence() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([19; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(500000)),
            created_at: UtcDateTime(400000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        assert!(runtime.readiness().is_degraded());
        assert_eq!(
            store.state.lock().unwrap().desired_internet_state,
            DesiredInternetState::Blocked
        );
        assert!(store.state.lock().unwrap().active_actions.is_empty());
    }

    #[test]
    fn rt_20_existing_outbox_survives_restart_and_exact_entry_id_ack_removes_it() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let entry1 = OutboxEntryId([10; 16]);
        let entry2 = OutboxEntryId([20; 16]);
        initial_state.telegram_outbox.push(TelegramOutboxEntry {
            entry_id: entry1,
            payload: TelegramPayload::ServiceNotification {
                text: "msg1".to_string(),
            },
            attempt_count: 0,
            last_error: None,
        });
        initial_state.telegram_outbox.push(TelegramOutboxEntry {
            entry_id: entry2,
            payload: TelegramPayload::ServiceNotification {
                text: "msg2".to_string(),
            },
            attempt_count: 0,
            last_error: None,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let handle = runtime.handle();
        // Ack exact entry1
        let acked = handle.ack_telegram(entry1).unwrap();
        assert!(acked);

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(saved.telegram_outbox.len(), 1);
        assert_eq!(saved.telegram_outbox[0].entry_id, entry2);

        // Ack unknown entry
        let unknown = OutboxEntryId([99; 16]);
        let ack_unknown = handle.ack_telegram(unknown).unwrap();
        assert!(!ack_unknown);
        let saved2 = store.state.lock().unwrap().clone();
        assert_eq!(saved2.telegram_outbox.len(), 1);
    }

    // --- CORR-01..CORR-12 Regression Verifications ---

    #[test]
    fn corr_01_immediate_block_mismatch_fails_and_schedules_retry() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let res = runtime
            .handle()
            .immediate_internet_block(Initiator::ParentLocalPin);
        assert!(
            res.is_err(),
            "Immediate block on observed mismatch must NOT return Ok"
        );

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.desired_internet_state, DesiredInternetState::Blocked);
        assert_eq!(snap.observed_internet_state, InternetState::Unrestricted);
        assert_eq!(snap.health.status, HealthStatus::Degraded);

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(saved.desired_internet_state, DesiredInternetState::Blocked);
        assert!(saved.internet_retry.is_some());
        assert_eq!(saved.telegram_outbox.len(), 1);
    }

    #[test]
    fn corr_02_restore_internet_mismatch_fails_and_schedules_retry() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let res = runtime.handle().restore_internet(Initiator::ParentLocalPin);
        assert!(
            res.is_err(),
            "Restore internet on observed mismatch must NOT return Ok"
        );

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(
            snap.desired_internet_state,
            DesiredInternetState::Unrestricted
        );
        assert_eq!(snap.observed_internet_state, InternetState::Blocked);
        assert_eq!(snap.health.status, HealthStatus::Degraded);

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(
            saved.desired_internet_state,
            DesiredInternetState::Unrestricted
        );
        assert!(saved.internet_retry.is_some());
        assert_eq!(saved.telegram_outbox.len(), 1);
    }

    #[test]
    fn corr_03_scheduled_block_mismatch_persists_failed_and_schedules_retry() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_internet_block(10, Initiator::ParentLocalPin)
            .unwrap();

        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(saved.desired_internet_state, DesiredInternetState::Blocked);
        assert_eq!(saved.active_actions.len(), 1);
        assert!(matches!(
            saved.active_actions[0].execution_state,
            ActionExecutionState::Failed { .. }
        ));
        assert!(saved.internet_retry.is_some());
        assert_eq!(saved.telegram_outbox.len(), 1);
    }

    #[test]
    fn corr_04_retry_mismatch_keeps_retry_scheduled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: Some(InternetRetry {
                attempt_count: 1,
                last_error: Some("prior error".to_string()),
            }),
            telegram_outbox: Vec::new(),
        };
        initial_state.telegram_outbox.clear();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        clock.advance(Duration::from_secs(6));
        runtime.handle().tick().unwrap();

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(
            saved.internet_retry.as_ref().unwrap().attempt_count,
            3,
            "Retry attempt counter must be incremented"
        );
        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.health.status, HealthStatus::Degraded);
    }

    #[test]
    fn corr_05_warning_persistence_failure_does_not_lose_threshold() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_internet_block(185, Initiator::ParentLocalPin)
            .unwrap();

        // Advance past M3 threshold (180s) while save fails
        clock.advance(Duration::from_secs(10));
        store.fail_saves.store(true, Ordering::SeqCst);
        runtime.handle().tick().unwrap();

        let snap1 = runtime.handle().query_status().unwrap();
        assert_eq!(
            snap1.health.status,
            HealthStatus::Critical,
            "Persistence failure must be observable"
        );
        assert_eq!(store.state.lock().unwrap().telegram_outbox.len(), 0);

        // Restore save capability: threshold must not be lost and must persist on next evaluation
        store.fail_saves.store(false, Ordering::SeqCst);
        runtime.handle().tick().unwrap();

        let saved = store.state.lock().unwrap().clone();
        assert_eq!(
            saved.telegram_outbox.len(),
            1,
            "Warning threshold must be recovered and persisted exactly once"
        );
        let action = saved
            .active_actions
            .iter()
            .find(|a| a.action_kind == ActionKind::BlockInternet)
            .unwrap();
        assert!(action.emitted_thresholds.contains(&WarningThreshold::M3));

        // Further tick does not duplicate it
        runtime.handle().tick().unwrap();
        assert_eq!(store.state.lock().unwrap().telegram_outbox.len(), 1);
    }

    #[test]
    fn corr_06_shutdown_terminal_save_failure_is_observable() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .unwrap();

        // Advance to deadline, allow first save (Executing), but fail the terminal completed save
        clock.advance(Duration::from_secs(12));
        // Inject failure on next save
        store.fail_saves.store(true, Ordering::SeqCst);
        runtime.handle().tick().unwrap();

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.health.status, HealthStatus::Critical);
        assert!(!snap.health.persistence_healthy);
    }

    #[test]
    fn corr_07_internet_retry_save_failure_does_not_claim_healthy() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: Some(InternetRetry {
                attempt_count: 1,
                last_error: Some("prior error".to_string()),
            }),
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate.clone(),
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        // Now gate recovers and transitions to Blocked
        gate.fail_state_change.store(false, Ordering::SeqCst);
        *gate.current.lock().unwrap() = InternetState::Blocked;

        // Fail save when clearing retry in background
        store.fail_saves.store(true, Ordering::SeqCst);
        clock.advance(Duration::from_secs(6));
        runtime.handle().tick().unwrap();

        let snap = runtime.handle().query_status().unwrap();
        assert_ne!(
            snap.health.status,
            HealthStatus::Healthy,
            "Must NOT claim Healthy if clearing retry metadata save failed"
        );
        assert!(!snap.health.persistence_healthy);
    }

    #[test]
    fn corr_08_subsecond_future_deadline_is_not_overdue() {
        // Section 14 regression test: +1ms, +999ms, 0ms, -1ms
        assert_eq!(remaining_seconds_from_delta_ms(1), 1);
        assert_eq!(remaining_seconds_from_delta_ms(999), 1);
        assert_eq!(remaining_seconds_from_delta_ms(0), 0);
        assert_eq!(remaining_seconds_from_delta_ms(-1), 0);
        assert_eq!(remaining_seconds_from_delta_ms(-1000), -1);

        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        // Deadline is +1 ms in the future relative to clock at 1000000
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([88; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(1000001)),
            created_at: UtcDateTime(900000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(
            snap.active_actions.len(),
            1,
            "+1ms future deadline must remain future"
        );
        assert_eq!(
            snap.active_actions[0].execution_state,
            ActionExecutionState::Pending
        );
    }

    #[test]
    fn corr_09_subsecond_shutdown_cancellation_allowed() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let timer_id = TimerId([89; 16]);
        // Deadline is +1 ms in the future relative to clock at 1000000
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(1000001)),
            created_at: UtcDateTime(900000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        // 1ms before deadline: cancellation MUST succeed
        let res = runtime
            .handle()
            .cancel_shutdown_timer(timer_id, Initiator::ParentLocalPin);
        assert_eq!(
            res.unwrap(),
            TimerCancellationResult::Cancelled,
            "+1ms before deadline cancellation must be allowed"
        );

        assert!(store.state.lock().unwrap().active_actions.is_empty());
    }

    #[test]
    fn corr_10_executing_scheduled_action_cannot_be_cancelled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let timer_id1 = TimerId([91; 16]);
        let timer_id2 = TimerId([92; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id1,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(900000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Executing,
        });
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id2,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(900000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Failed {
                reason: "err".to_string(),
            },
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let res1 = runtime
            .handle()
            .cancel_internet_block_timer(timer_id1, Initiator::ParentLocalPin);
        assert!(
            matches!(res1, Err(ServiceRuntimeError::CancellationForbidden(_))),
            "Executing action cannot be cancelled"
        );

        let res2 = runtime
            .handle()
            .cancel_shutdown_timer(timer_id2, Initiator::ParentLocalPin);
        assert!(
            matches!(res2, Err(ServiceRuntimeError::CancellationForbidden(_))),
            "Failed action cannot be cancelled"
        );
    }

    #[test]
    fn corr_11_no_fake_platform_gate_default_produces_false_ready_enforcement() {
        // Section 3: Verify that ServiceRuntime::start requires abstract mandatory platform ports
        // from the caller rather than silently substituting fake platform adapters.
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let bootstrapped = sample_bootstrapped_state(initial_state);

        // A real/caller provided gate reflecting the actual network state (Unrestricted)
        let gate = FakeInternetGate::new(
            InternetState::Unrestricted,
            Arc::new(Mutex::new(Vec::new())),
        );
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(Arc::new(Mutex::new(Vec::new())));
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let runtime = ServiceRuntime::start(bootstrapped, gate, power, clock, id_source, retry)
            .expect("Runtime construction must succeed");

        // Must NOT falsely report Ready when enforcement did not occur on platform
        assert!(
            runtime.readiness().is_degraded(),
            "Cannot claim Ready when observed does not match desired Blocked"
        );
    }

    #[test]
    fn corr_12_zero_retry_delay_does_not_cause_busy_loop() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_state_change.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        // Malicious or misconfigured zero-delay retry policy
        let retry = TestRetryPolicy::new(Duration::ZERO);

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        // Runtime degraded, zero-delay rejected, no busy spin scheduled
        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.health.status, HealthStatus::Degraded);
        assert!(
            snap.health
                .last_error
                .as_ref()
                .unwrap()
                .contains("zero delay")
        );
    }

    #[test]
    fn corr_13_startup_overdue_block_internet_success_terminalizes_action_before_readiness() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([13; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(500000)),
            created_at: UtcDateTime(400000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state.clone());
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        assert!(store.save_count.load(Ordering::SeqCst) >= 2);
        let entries = log.lock().unwrap().clone();
        let first_save_idx = entries.iter().position(|e| e == "save:recovery").unwrap();
        let gate_idx = entries
            .iter()
            .position(|e| e == "gate:block_internet")
            .unwrap();
        let cleanup_save_idx = entries
            .iter()
            .position(|e| e == "save:startup_reconciled")
            .unwrap();
        assert!(first_save_idx < gate_idx);
        assert!(gate_idx < cleanup_save_idx);

        let snap = runtime.readiness().snapshot();
        assert_eq!(snap.desired_internet_state, DesiredInternetState::Blocked);
        assert_eq!(snap.observed_internet_state, InternetState::Blocked);
        assert!(snap.active_actions.is_empty());
        assert!(store.state.lock().unwrap().active_actions.is_empty());

        // Also test terminal cleanup save failure: startup MUST NOT report Ready/Degraded
        let log_fail = Arc::new(Mutex::new(Vec::new()));
        let store_fail = FakeStateStore::new(initial_state.clone(), log_fail.clone());
        store_fail.fail_after_n_saves.store(1, Ordering::SeqCst);
        let gate_fail = FakeInternetGate::new(InternetState::Blocked, log_fail.clone());
        let power_fail = FakePowerController::new(log_fail.clone());
        let clock_fail = FakeClock::new(1000000);
        let id_source_fail = FakeIdSource::new();
        let retry_fail = TestRetryPolicy::new(Duration::from_secs(5));
        let bootstrapped_fail = sample_bootstrapped_state(initial_state);

        let res = ServiceRuntime::start_with_store(
            bootstrapped_fail,
            store_fail,
            gate_fail,
            power_fail,
            clock_fail,
            id_source_fail,
            retry_fail,
            Some(log_fail),
        );
        assert!(
            matches!(res, Err(ServiceRuntimeError::Persistence(_))),
            "Terminal cleanup save failure must return typed persistence failure"
        );
    }

    #[test]
    fn corr_14_persisted_overdue_executing_block_internet_is_resolved_on_startup() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([14; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(500000)),
            created_at: UtcDateTime(400000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Executing,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        assert!(runtime.readiness().is_degraded());
        let snap = runtime.readiness().snapshot();
        assert_eq!(snap.desired_internet_state, DesiredInternetState::Blocked);
        assert_eq!(snap.observed_internet_state, InternetState::Blocked);
        assert!(snap.active_actions.is_empty());
        assert!(store.state.lock().unwrap().active_actions.is_empty());
    }

    #[test]
    fn corr_15_deadline_save_failure_retains_timer_eligibility() {
        // Part A: BlockInternet deadline
        {
            let log = Arc::new(Mutex::new(Vec::new()));
            let gate_log = Arc::new(Mutex::new(Vec::new()));
            let initial_state = PersistentState {
                desired_internet_state: DesiredInternetState::Unrestricted,
                active_actions: Vec::new(),
                internet_retry: None,
                telegram_outbox: Vec::new(),
            };
            let store = FakeStateStore::new(initial_state.clone(), log.clone());
            let gate = FakeInternetGate::new(InternetState::Unrestricted, gate_log.clone());
            let power = FakePowerController::new(log.clone());
            let clock = FakeClock::new(1000000);
            let id_source = FakeIdSource::new();
            let retry = TestRetryPolicy::new(Duration::from_secs(5));

            let bootstrapped = sample_bootstrapped_state(initial_state);
            let runtime = ServiceRuntime::start_with_store(
                bootstrapped,
                store.clone(),
                gate.clone(),
                power,
                clock.clone(),
                id_source,
                retry,
                Some(log),
            )
            .expect("Runtime construction must succeed");

            let timer_id = runtime
                .handle()
                .schedule_internet_block(10, Initiator::ParentLocalPin)
                .expect("Schedule must succeed");

            clock.advance(Duration::from_secs(11));

            // Fail the Executing save
            store.fail_saves.store(true, Ordering::SeqCst);
            runtime.handle().tick().unwrap();

            // Gate must NOT have been called
            let entries = gate_log.lock().unwrap().clone();
            assert!(
                !entries.iter().any(|e| e == "gate:block_internet"),
                "Gate must NOT be called when pre-side-effect save fails"
            );

            // Action remains Pending in authoritative state
            let current_action = store
                .state
                .lock()
                .unwrap()
                .active_actions
                .iter()
                .find(|a| a.id == timer_id)
                .cloned()
                .expect("Action must still exist in authoritative state");
            assert_eq!(
                current_action.execution_state,
                ActionExecutionState::Pending
            );

            // Persistence health becomes Critical
            let snap = runtime.handle().query_status().unwrap();
            assert_eq!(snap.health.status, HealthStatus::Critical);

            // Recover store and explicitly re-evaluate
            store.fail_saves.store(false, Ordering::SeqCst);
            runtime.handle().tick().unwrap();

            // Executing persisted and gate invoked exactly once
            let entries2 = gate_log.lock().unwrap().clone();
            let gate_calls = entries2
                .iter()
                .filter(|e| *e == "gate:block_internet")
                .count();
            assert_eq!(
                gate_calls, 1,
                "InternetGate must be invoked exactly once after recovery"
            );
        }

        // Part B: Shutdown deadline
        {
            let log = Arc::new(Mutex::new(Vec::new()));
            let power_log = Arc::new(Mutex::new(Vec::new()));
            let initial_state = PersistentState {
                desired_internet_state: DesiredInternetState::Unrestricted,
                active_actions: Vec::new(),
                internet_retry: None,
                telegram_outbox: Vec::new(),
            };
            let store = FakeStateStore::new(initial_state.clone(), log.clone());
            let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
            let power = FakePowerController::new(power_log.clone());
            let clock = FakeClock::new(1000000);
            let id_source = FakeIdSource::new();
            let retry = TestRetryPolicy::new(Duration::from_secs(5));

            let bootstrapped = sample_bootstrapped_state(initial_state);
            let runtime = ServiceRuntime::start_with_store(
                bootstrapped,
                store.clone(),
                gate,
                power.clone(),
                clock.clone(),
                id_source,
                retry,
                Some(log),
            )
            .expect("Runtime construction must succeed");

            let timer_id = runtime
                .handle()
                .schedule_shutdown(10, Initiator::ParentLocalPin)
                .expect("Schedule must succeed");

            clock.advance(Duration::from_secs(11));

            // Fail the Executing save
            store.fail_saves.store(true, Ordering::SeqCst);
            runtime.handle().tick().unwrap();

            // PowerController must NOT have been called
            let entries = power_log.lock().unwrap().clone();
            assert!(
                !entries.iter().any(|e| e == "power:initiate_shutdown"),
                "PowerController must NOT have been called when pre-side-effect save fails"
            );

            // Action remains Pending
            let current_action = store
                .state
                .lock()
                .unwrap()
                .active_actions
                .iter()
                .find(|a| a.id == timer_id)
                .cloned()
                .expect("Action must still exist");
            assert_eq!(
                current_action.execution_state,
                ActionExecutionState::Pending
            );

            // Recover store and explicitly re-evaluate
            store.fail_saves.store(false, Ordering::SeqCst);
            runtime.handle().tick().unwrap();

            // PowerController invoked exactly once
            let entries2 = power_log.lock().unwrap().clone();
            let power_calls = entries2
                .iter()
                .filter(|e| *e == "power:initiate_shutdown")
                .count();
            assert_eq!(
                power_calls, 1,
                "PowerController must be invoked exactly once after recovery"
            );
        }
    }

    #[test]
    fn corr_16_warning_persistence_failure_does_not_create_zero_timeout_spin() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let (mut coordinator, _) = ServiceRuntimeCoordinator::new(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(())),
            None,
        )
        .expect("Coordinator construction must succeed");

        let _ = coordinator
            .handle_schedule_action(ActionKind::BlockInternet, 185, Initiator::ParentLocalPin)
            .unwrap();

        // Advance past 180s threshold
        clock.advance(Duration::from_secs(10));
        store.fail_saves.store(true, Ordering::SeqCst);

        // Process clock: save fails
        coordinator.process_clock_and_events();

        // Must be persistence Critical
        assert_eq!(coordinator.health.status, HealthStatus::Critical);
        assert!(!coordinator.health.persistence_healthy);

        // Next autonomous wake timeout MUST NOT be zero (must be None to gate spin)
        let timeout = coordinator.next_wake_timeout();
        assert_eq!(
            timeout, None,
            "next_wake_timeout must return None while persistence is Critical to prevent busy-spinning"
        );

        // Threshold remains un-emitted
        assert_eq!(store.state.lock().unwrap().telegram_outbox.len(), 0);

        // Restore store and explicitly trigger permitted evaluation
        store.fail_saves.store(false, Ordering::SeqCst);
        coordinator.process_clock_and_events();

        // Warning + emitted threshold persisted exactly once
        assert_eq!(coordinator.health.status, HealthStatus::Degraded);
        assert!(coordinator.health.persistence_healthy);
        let saved = store.state.lock().unwrap().clone();
        assert_eq!(saved.telegram_outbox.len(), 1);
        assert!(
            saved.active_actions[0]
                .emitted_thresholds
                .contains(&WarningThreshold::M3)
        );
    }

    #[test]
    fn corr_17_aggregate_health_cannot_be_healthy_while_persistence_unhealthy() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let (mut coordinator, _) = ServiceRuntimeCoordinator::new(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(())),
            None,
        )
        .expect("Coordinator construction must succeed");

        // Mark persistence failure
        let err = StateStoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
        coordinator.mark_persistence_failure(&err);
        assert_eq!(coordinator.health.status, HealthStatus::Critical);

        // Simulate subsequent successful InternetGate operation
        coordinator.mark_gate_success();

        // Aggregate status MUST NOT become Healthy
        assert_ne!(coordinator.health.status, HealthStatus::Healthy);
        assert_eq!(
            coordinator.health.status,
            HealthStatus::Critical,
            "Aggregate status must remain Critical while persistence is unhealthy"
        );
    }

    #[test]
    fn corr_18_telegram_disconnected_produces_operational_degraded_health() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        assert!(
            runtime.readiness().is_degraded(),
            "Runtime readiness must be Degraded because Telegram is disconnected"
        );

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.health.telegram_connected, false);
        assert_eq!(snap.health.persistence_healthy, true);
        assert_eq!(snap.health.internet_gate_healthy, true);
        assert_eq!(snap.health.status, HealthStatus::Degraded);

        // Runtime remains fully operational
        let res = runtime
            .handle()
            .immediate_internet_block(Initiator::ParentLocalPin);
        assert!(
            res.is_ok(),
            "Runtime must remain operational while Degraded"
        );
    }

    #[test]
    fn corr_19_stop_mutation_submission_boundary_is_linearized() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state.clone());
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let handle = runtime.handle().clone();

        // 1. Mutation legitimately crossing before Stop succeeds and gets reply
        let pre_res = handle.immediate_internet_block(Initiator::ParentLocalPin);
        assert!(pre_res.is_ok());

        // 2. Linearized Stop
        runtime.stop().expect("Stop must succeed");

        // 3. Mutation attempting submission after Stop returns ServiceRuntimeError::Stopping immediately
        let post_res = handle.restore_internet(Initiator::ParentLocalPin);
        assert!(
            matches!(post_res, Err(ServiceRuntimeError::Stopping)),
            "Mutation after stop must be rejected with Stopping"
        );

        let post_cancel =
            handle.cancel_internet_block_timer(TimerId([1; 16]), Initiator::ParentLocalPin);
        assert!(
            matches!(post_cancel, Err(ServiceRuntimeError::Stopping)),
            "Cancel timer after stop must be rejected with Stopping"
        );

        let post_query = handle.query_status();
        assert!(
            matches!(post_query, Err(ServiceRuntimeError::Stopping)),
            "Query status after stop must be rejected with Stopping"
        );

        // Worker handle is joined
        assert!(runtime.worker_handle.is_none());

        // Concurrency check: multiple concurrent mutations during Stop
        let mut runtime2 = ServiceRuntime::start_with_store(
            sample_bootstrapped_state(initial_state.clone()),
            FakeStateStore::new(
                PersistentState {
                    desired_internet_state: DesiredInternetState::Unrestricted,
                    active_actions: Vec::new(),
                    internet_retry: None,
                    telegram_outbox: Vec::new(),
                },
                Arc::new(Mutex::new(Vec::new())),
            ),
            FakeInternetGate::new(
                InternetState::Unrestricted,
                Arc::new(Mutex::new(Vec::new())),
            ),
            FakePowerController::new(Arc::new(Mutex::new(Vec::new()))),
            FakeClock::new(1000000),
            FakeIdSource::new(),
            TestRetryPolicy::new(Duration::from_secs(5)),
            None,
        )
        .unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut worker_threads = Vec::new();
        for _ in 0..4 {
            let b = barrier.clone();
            let h = runtime2.handle().clone();
            worker_threads.push(spawn(move || {
                b.wait();
                h.immediate_internet_block(Initiator::ParentLocalPin)
            }));
        }
        barrier.wait();
        let _ = runtime2.stop();
        for wt in worker_threads {
            let res = wt.join().unwrap();
            match res {
                Ok(()) => {}
                Err(ServiceRuntimeError::Stopping) => {}
                Err(e) => panic!("Unexpected error during stop race: {:?}", e),
            }
        }
    }

    #[test]
    fn corr_20_subsecond_monotonic_deadlines_preserve_millisecond_duration() {
        // Verify +1 ms deadline
        {
            let log = Arc::new(Mutex::new(Vec::new()));
            let mut initial_state = PersistentState {
                desired_internet_state: DesiredInternetState::Unrestricted,
                active_actions: Vec::new(),
                internet_retry: None,
                telegram_outbox: Vec::new(),
            };
            let timer_id = TimerId([20; 16]);
            let startup_utc = 1000000;
            let deadline_utc = startup_utc + 1; // +1 ms
            initial_state.active_actions.push(ScheduledAction {
                id: timer_id,
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(deadline_utc)),
                created_at: UtcDateTime(startup_utc - 1000),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            });

            let store = FakeStateStore::new(initial_state.clone(), log.clone());
            let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
            let power = FakePowerController::new(log.clone());
            let clock = FakeClock::new(startup_utc);
            let id_source = FakeIdSource::new();
            let retry = TestRetryPolicy::new(Duration::from_secs(5));

            let bootstrapped = sample_bootstrapped_state(initial_state);
            let runtime = ServiceRuntime::start_with_store(
                bootstrapped,
                store.clone(),
                gate.clone(),
                power,
                clock.clone(),
                id_source,
                retry,
                Some(log.clone()),
            )
            .expect("Runtime construction must succeed");

            // Advance monotonic clock by 500 microseconds (less than 1 ms)
            *clock.mono.lock().unwrap() += Duration::from_micros(500);
            runtime.handle().tick().unwrap();

            // Action must remain Pending! Not due yet!
            let action = store
                .state
                .lock()
                .unwrap()
                .active_actions
                .iter()
                .find(|a| a.id == timer_id)
                .cloned()
                .unwrap();
            assert_eq!(action.execution_state, ActionExecutionState::Pending);

            // Advance another 500 microseconds to reach exact 1 ms
            *clock.mono.lock().unwrap() += Duration::from_micros(500);
            runtime.handle().tick().unwrap();

            // Deadline is now due! Must NOT require 1 full second!
            let entries = log.lock().unwrap().clone();
            assert!(
                entries.iter().any(|e| e == "gate:block_internet"),
                "Deadline of +1ms must execute at 1ms, not 1000ms"
            );
        }

        // Verify +999 ms deadline
        {
            let log = Arc::new(Mutex::new(Vec::new()));
            let mut initial_state = PersistentState {
                desired_internet_state: DesiredInternetState::Unrestricted,
                active_actions: Vec::new(),
                internet_retry: None,
                telegram_outbox: Vec::new(),
            };
            let timer_id = TimerId([21; 16]);
            let startup_utc = 1000000;
            let deadline_utc = startup_utc + 999; // +999 ms
            initial_state.active_actions.push(ScheduledAction {
                id: timer_id,
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(deadline_utc)),
                created_at: UtcDateTime(startup_utc - 1000),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            });

            let store = FakeStateStore::new(initial_state.clone(), log.clone());
            let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
            let power = FakePowerController::new(log.clone());
            let clock = FakeClock::new(startup_utc);
            let id_source = FakeIdSource::new();
            let retry = TestRetryPolicy::new(Duration::from_secs(5));

            let bootstrapped = sample_bootstrapped_state(initial_state);
            let runtime = ServiceRuntime::start_with_store(
                bootstrapped,
                store.clone(),
                gate.clone(),
                power,
                clock.clone(),
                id_source,
                retry,
                Some(log.clone()),
            )
            .expect("Runtime construction must succeed");

            // Advance monotonic clock by 998 ms
            *clock.mono.lock().unwrap() += Duration::from_millis(998);
            runtime.handle().tick().unwrap();

            // Action remains Pending
            let action = store
                .state
                .lock()
                .unwrap()
                .active_actions
                .iter()
                .find(|a| a.id == timer_id)
                .cloned()
                .unwrap();
            assert_eq!(action.execution_state, ActionExecutionState::Pending);

            // Advance by 1 ms to reach 999 ms
            *clock.mono.lock().unwrap() += Duration::from_millis(1);
            runtime.handle().tick().unwrap();

            // Deadline is now due at 999 ms, NOT at 1000 ms
            let entries = log.lock().unwrap().clone();
            assert!(
                entries.iter().any(|e| e == "gate:block_internet"),
                "Deadline of +999ms must execute at 999ms, not 1000ms"
            );
        }
    }

    #[test]
    fn corr_21_post_side_effect_block_cleanup_retained_and_not_bypassed() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate.clone(),
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        let timer_id = runtime
            .handle()
            .schedule_internet_block(10, Initiator::ParentLocalPin)
            .unwrap();

        // 1 save so far: schedule_action
        // Next save (2) will be scheduled_block_executing (must succeed)
        // Subsequent save (3) will be scheduled_block_completed (must fail)
        store.fail_after_n_saves.store(2, Ordering::SeqCst);

        // Advance clock past deadline
        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // Required proof 1: gate invoked exactly once
        let gate_block_count = log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "gate:block_internet")
            .count();
        assert_eq!(
            gate_block_count, 1,
            "InternetGate must be invoked exactly once"
        );

        // Required proof 2: runtime does NOT claim the timer durably completed
        let snap = runtime.handle().query_status().unwrap();
        assert!(
            !snap.health.persistence_healthy,
            "Persistence must be marked unhealthy"
        );
        assert_eq!(
            snap.health.status,
            HealthStatus::Critical,
            "Health must be Critical"
        );

        // Required proof 3 & 4: an unrelated subsequent command cannot persist an older Executing snapshot
        // While store is failing, mutations are rejected by the barrier
        let res = runtime
            .handle()
            .schedule_internet_block(100, Initiator::ParentLocalPin);
        assert!(
            res.is_err(),
            "Subsequent mutation must be rejected while persistence fails"
        );

        // Required proof 5: after the recovery mechanism, durable state contains no orphan Executing action
        // Store becomes writable again
        store.fail_after_n_saves.store(0, Ordering::SeqCst);
        store.fail_saves.store(false, Ordering::SeqCst);

        // Now subsequent command succeeds and flushes the pending cleanup candidate
        let res2 = runtime
            .handle()
            .schedule_internet_block(100, Initiator::ParentLocalPin);
        assert!(res2.is_ok(), "Mutation succeeds after store recovers");

        // Durable state check: timer_id must NOT exist as Executing
        let disk_state = store.state.lock().unwrap();
        let orphan_executing = disk_state.active_actions.iter().any(|a| a.id == timer_id);
        assert!(
            !orphan_executing,
            "Durable state must contain no orphan Executing action"
        );

        // Gate was still invoked only once
        let gate_block_count_after = log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "gate:block_internet")
            .count();
        assert_eq!(
            gate_block_count_after, 1,
            "InternetGate was not invoked again"
        );
    }

    #[test]
    fn corr_22_post_side_effect_shutdown_durability_and_barrier() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power.clone(),
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        let shutdown_timer_id = runtime
            .handle()
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .unwrap();

        // Save 1: schedule
        // Save 2: executing (succeeds)
        // Save 3: terminal cleanup (fails)
        store.fail_after_n_saves.store(2, Ordering::SeqCst);

        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // Shutdown was called
        let power_count = log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "power:initiate_shutdown")
            .count();
        assert_eq!(power_count, 1);

        // Keep ShutdownState::InProgress truthful
        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.shutdown_state, ShutdownState::InProgress);
        // Mark persistence Critical
        assert_eq!(snap.health.status, HealthStatus::Critical);
        assert!(!snap.health.persistence_healthy);

        // Unrelated mutation rejected while persistence broken
        let res = runtime
            .handle()
            .schedule_internet_block(200, Initiator::ParentLocalPin);
        assert!(res.is_err());

        // Store recovers
        store.fail_after_n_saves.store(0, Ordering::SeqCst);
        store.fail_saves.store(false, Ordering::SeqCst);

        // Unrelated mutation flushes pending candidate and succeeds
        let res2 = runtime
            .handle()
            .schedule_internet_block(200, Initiator::ParentLocalPin);
        assert!(res2.is_ok());

        let disk_state = store.state.lock().unwrap();
        let orphan_shutdown = disk_state
            .active_actions
            .iter()
            .any(|a| a.id == shutdown_timer_id);
        assert!(
            !orphan_shutdown,
            "Durable state must not contain stale Executing shutdown action"
        );

        let snap2 = runtime.handle().query_status().unwrap();
        assert_eq!(
            snap2.shutdown_state,
            ShutdownState::InProgress,
            "Shutdown state remains truthful"
        );
        assert!(snap2.health.persistence_healthy, "Persistence recovered");
    }

    #[test]
    fn corr_23_recovery_liveness_after_transient_persistence_failure() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        // 1. Transient persistence failure
        store.fail_saves.store(true, Ordering::SeqCst);
        let fail_res = runtime
            .handle()
            .schedule_internet_block(50, Initiator::ParentLocalPin);
        assert!(fail_res.is_err());

        let snap1 = runtime.handle().query_status().unwrap();
        assert_eq!(snap1.health.status, HealthStatus::Critical);
        assert!(!snap1.health.persistence_healthy);

        // 2. Store becomes writable again
        store.fail_saves.store(false, Ordering::SeqCst);

        // 3. Legitimate authoritative save succeeds (e.g. cancel, ack, schedule, immediate_block)
        let ok_res = runtime
            .handle()
            .schedule_internet_block(20, Initiator::ParentLocalPin);
        assert!(ok_res.is_ok());

        // 4. Persistence must be considered recovered and scheduler operation eligible
        let snap2 = runtime.handle().query_status().unwrap();
        assert!(
            snap2.health.persistence_healthy,
            "Persistence must be recovered"
        );
        assert_ne!(
            snap2.health.status,
            HealthStatus::Critical,
            "Status must no longer be Critical"
        );

        // Scheduler eligibility: advance clock, timer fires!
        clock.advance(Duration::from_secs(25));
        runtime.handle().tick().unwrap();

        let gate_blocks = log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "gate:block_internet")
            .count();
        assert_eq!(
            gate_blocks, 1,
            "Scheduled timer must fire now that persistence is recovered"
        );
    }

    #[test]
    fn corr_24_zero_delay_retry_never_lowers_critical_health() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::ZERO);

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let (mut coordinator, _) = ServiceRuntimeCoordinator::new(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(())),
            None,
        )
        .expect("Coordinator construction must succeed");

        // Persistence failure
        let err = StateStoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk error"));
        coordinator.mark_persistence_failure(&err);
        assert!(!coordinator.health.persistence_healthy);
        assert_eq!(coordinator.health.status, HealthStatus::Critical);

        // Zero-delay retry evaluation
        coordinator.schedule_next_internet_retry(1);

        // Invariant: persistence_healthy=false, status=Critical, next_retry_at=None
        assert!(!coordinator.health.persistence_healthy);
        assert_eq!(
            coordinator.health.status,
            HealthStatus::Critical,
            "Health status must remain Critical even with zero-delay retry policy"
        );
        assert_eq!(coordinator.next_retry_at, None);
    }

    #[test]
    fn corr_25_stop_preempts_new_timer_platform_side_effects() {
        // Part A: Shutdown timer deadline stopped before PowerController
        {
            let log = Arc::new(Mutex::new(Vec::new()));
            let initial_state = PersistentState {
                desired_internet_state: DesiredInternetState::Unrestricted,
                active_actions: Vec::new(),
                internet_retry: None,
                telegram_outbox: Vec::new(),
            };
            let store = FakeStateStore::new(initial_state.clone(), log.clone());
            let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
            let power = FakePowerController::new(log.clone());
            let clock = FakeClock::new(1000000);
            let id_source = FakeIdSource::new();
            let retry = TestRetryPolicy::new(Duration::from_secs(5));

            let shutdown_action_id = TimerId([25; 16]);
            let now_utc = clock.utc_now();
            let deadline = Deadline(UtcDateTime(now_utc.0 + 10_000));

            let mut state_with_action = initial_state.clone();
            state_with_action.active_actions.push(ScheduledAction {
                id: shutdown_action_id,
                action_kind: ActionKind::ShutdownComputer,
                deadline,
                created_at: now_utc,
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            });
            *store.state.lock().unwrap() = state_with_action.clone();

            let bootstrapped = sample_bootstrapped_state(state_with_action);
            let mut runtime = ServiceRuntime::start_with_store(
                bootstrapped,
                store.clone(),
                gate.clone(),
                power.clone(),
                clock.clone(),
                id_source.clone(),
                retry.clone(),
                Some(log.clone()),
            )
            .expect("Runtime construction must succeed");

            let stop_token = runtime.stop_requested.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let b_clone = barrier.clone();

            // When save:scheduled_shutdown_executing happens:
            // set stop_requested = true before power.initiate_shutdown!
            *store.on_save.lock().unwrap() =
                Some(Box::new(move |saved_state: &PersistentState| {
                    if saved_state.active_actions.iter().any(|a| {
                        a.id == shutdown_action_id
                            && matches!(a.execution_state, ActionExecutionState::Executing)
                    }) {
                        stop_token.store(true, Ordering::SeqCst);
                        b_clone.wait();
                    }
                }));

            // Advance clock to trigger deadline
            clock.advance(Duration::from_secs(12));

            let h = runtime.handle().clone();
            let tick_handle = spawn(move || {
                let _ = h.tick();
            });

            barrier.wait();

            // Stop the runtime
            let stop_res = runtime.stop();
            assert!(stop_res.is_ok(), "Worker must terminate and join cleanly");
            let _ = tick_handle.join();

            // Prove C: PowerController is NOT called
            let power_invocations = log
                .lock()
                .unwrap()
                .iter()
                .filter(|e| *e == "power:initiate_shutdown")
                .count();
            assert_eq!(
                power_invocations, 0,
                "PowerController must NOT be called when stop requested"
            );

            // Prove E: Persisted Executing state remains safe for restart semantics
            let disk_state = store.state.lock().unwrap().clone();
            let action_on_disk = disk_state
                .active_actions
                .iter()
                .find(|a| a.id == shutdown_action_id)
                .expect("Action must be persisted on disk");
            assert!(
                matches!(
                    action_on_disk.execution_state,
                    ActionExecutionState::Executing
                ),
                "Persisted state must be Executing"
            );

            // Re-boot service: overdue Executing shutdown must become Missed and NOT shut down
            clock.advance(Duration::from_secs(10));
            let bootstrapped2 = sample_bootstrapped_state(disk_state);
            let log2 = Arc::new(Mutex::new(Vec::new()));
            let power2 = FakePowerController::new(log2.clone());
            let store2 = FakeStateStore::new(bootstrapped2.state.clone(), log2.clone());
            let runtime2 = ServiceRuntime::start_with_store(
                bootstrapped2,
                store2,
                gate,
                power2,
                clock,
                id_source,
                retry,
                Some(log2.clone()),
            )
            .expect("Re-start must succeed");

            let power_restart_invocations = log2
                .lock()
                .unwrap()
                .iter()
                .filter(|e| *e == "power:initiate_shutdown")
                .count();
            assert_eq!(
                power_restart_invocations, 0,
                "Restart must NOT shut down fresh boot"
            );

            let snap_restart = runtime2.handle().query_status().unwrap();
            let missed_on_restart = snap_restart
                .active_actions
                .iter()
                .any(|a| a.id == shutdown_action_id);
            assert!(
                !missed_on_restart,
                "Missed shutdown must be retired from active actions"
            );
        }

        // Part B: InternetGate equivalent
        {
            let log = Arc::new(Mutex::new(Vec::new()));
            let initial_state = PersistentState {
                desired_internet_state: DesiredInternetState::Unrestricted,
                active_actions: Vec::new(),
                internet_retry: None,
                telegram_outbox: Vec::new(),
            };
            let store = FakeStateStore::new(initial_state.clone(), log.clone());
            let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
            let power = FakePowerController::new(log.clone());
            let clock = FakeClock::new(1000000);
            let id_source = FakeIdSource::new();
            let retry = TestRetryPolicy::new(Duration::from_secs(5));

            let block_action_id = TimerId([26; 16]);
            let now_utc = clock.utc_now();
            let deadline = Deadline(UtcDateTime(now_utc.0 + 10_000));

            let mut state_with_action = initial_state.clone();
            state_with_action.active_actions.push(ScheduledAction {
                id: block_action_id,
                action_kind: ActionKind::BlockInternet,
                deadline,
                created_at: now_utc,
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            });
            *store.state.lock().unwrap() = state_with_action.clone();

            let bootstrapped = sample_bootstrapped_state(state_with_action);
            let mut runtime = ServiceRuntime::start_with_store(
                bootstrapped,
                store.clone(),
                gate.clone(),
                power,
                clock.clone(),
                id_source,
                retry,
                Some(log.clone()),
            )
            .expect("Runtime construction must succeed");

            let stop_token = runtime.stop_requested.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let b_clone = barrier.clone();

            *store.on_save.lock().unwrap() =
                Some(Box::new(move |saved_state: &PersistentState| {
                    if saved_state.active_actions.iter().any(|a| {
                        a.id == block_action_id
                            && matches!(a.execution_state, ActionExecutionState::Executing)
                    }) {
                        stop_token.store(true, Ordering::SeqCst);
                        b_clone.wait();
                    }
                }));

            clock.advance(Duration::from_secs(12));

            let h = runtime.handle().clone();
            let tick_handle = spawn(move || {
                let _ = h.tick();
            });

            barrier.wait();

            let stop_res = runtime.stop();
            assert!(stop_res.is_ok());
            let _ = tick_handle.join();

            let gate_calls = log
                .lock()
                .unwrap()
                .iter()
                .filter(|e| *e == "gate:block_internet")
                .count();
            assert_eq!(
                gate_calls, 0,
                "InternetGate::block_internet must NOT be called when stop requested"
            );
        }
    }

    #[test]
    fn corr_26_power_failure_health_regression() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power.clone(),
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        let timer_id = runtime
            .handle()
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .unwrap();

        // Inject power failure
        power.fail_shutdown.store(true, Ordering::SeqCst);

        // Trigger deadline
        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // Status checks:
        let snap = runtime.handle().query_status().unwrap();
        // ShutdownState != InProgress
        assert_ne!(snap.shutdown_state, ShutdownState::InProgress);
        assert_eq!(snap.shutdown_state, ShutdownState::Idle);

        // StatusSnapshot remains Degraded (persistence is healthy, gate is healthy, power failed)
        assert_eq!(snap.health.status, HealthStatus::Degraded);
        assert!(snap.health.persistence_healthy);

        // last_error still contains the power failure (not erased by persistence success!)
        let last_err = snap.health.last_error.expect("last_error must be present");
        assert!(
            last_err.contains("Power shutdown failed"),
            "last_error must contain power failure, got: {}",
            last_err
        );

        // Action is durably saved as Failed
        let disk_state = store.state.lock().unwrap();
        let act = disk_state
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .expect("Failed action must be in durable state");
        assert!(matches!(
            act.execution_state,
            ActionExecutionState::Failed { .. }
        ));
    }

    #[test]
    fn corr_27_failed_block_action_preserved_across_reconciliation() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let failed_action_id = TimerId([27; 16]);
        let executing_action_id = TimerId([28; 16]);
        let now_utc = 1000000;

        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: vec![
                ScheduledAction {
                    id: failed_action_id,
                    action_kind: ActionKind::BlockInternet,
                    deadline: Deadline(UtcDateTime(now_utc - 5000)),
                    created_at: UtcDateTime(now_utc - 10000),
                    created_by: Initiator::ParentLocalPin,
                    emitted_thresholds: std::collections::HashSet::new(),
                    execution_state: ActionExecutionState::Failed {
                        reason: "gate timeout".to_string(),
                    },
                },
                ScheduledAction {
                    id: executing_action_id,
                    action_kind: ActionKind::BlockInternet,
                    deadline: Deadline(UtcDateTime(now_utc - 1000)),
                    created_at: UtcDateTime(now_utc - 10000),
                    created_by: Initiator::ParentLocalPin,
                    emitted_thresholds: std::collections::HashSet::new(),
                    execution_state: ActionExecutionState::Executing,
                },
            ],
            internet_retry: Some(InternetRetry {
                attempt_count: 2,
                last_error: Some("prior failure".to_string()),
            }),
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        // Gate starts unrestricted but succeeds in blocking
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(now_utc);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let snap = runtime.handle().query_status().unwrap();
        // 1. Current Internet observed Blocked
        assert_eq!(snap.observed_internet_state, InternetState::Blocked);

        // 2. Retry metadata cleared
        let disk_state = store.state.lock().unwrap();
        assert_eq!(disk_state.internet_retry, None);

        // 3. Failed scheduled action is NOT silently deleted
        let failed_exists = disk_state
            .active_actions
            .iter()
            .any(|a| a.id == failed_action_id);
        assert!(
            failed_exists,
            "Persisted Failed BlockInternet action must NOT be deleted"
        );

        // 4. Executing BlockInternet transitioned to Completed via core and only then removed
        let executing_exists = disk_state
            .active_actions
            .iter()
            .any(|a| a.id == executing_action_id);
        assert!(
            !executing_exists,
            "Executing BlockInternet action must be removed upon completion"
        );
    }

    #[test]
    fn corr_28_startup_executing_persists_blocked_before_gate_call() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let now_utc = 1000000;
        let action_id = TimerId([29; 16]);

        // Persisted: BlockInternet / Executing + desired Unrestricted
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![ScheduledAction {
                id: action_id,
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(now_utc - 5000)), // Overdue
                created_at: UtcDateTime(now_utc - 10000),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Executing,
            }],
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(now_utc);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        let entries = log.lock().unwrap().clone();
        let recovery_save_idx = entries
            .iter()
            .position(|e| e == "save:recovery")
            .expect("save:recovery must occur");
        let gate_block_idx = entries
            .iter()
            .position(|e| e == "gate:block_internet")
            .expect("gate:block_internet must occur");

        assert!(
            recovery_save_idx < gate_block_idx,
            "save:recovery must happen BEFORE gate:block_internet"
        );

        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.desired_internet_state, DesiredInternetState::Blocked);
        assert_eq!(snap.observed_internet_state, InternetState::Blocked);
    }

    #[test]
    fn corr_29_query_status_is_strictly_read_only() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_internet_block(10, Initiator::ParentLocalPin)
            .unwrap();

        // 1 save so far: schedule_action.
        // Save 2: scheduled_block_executing (succeeds)
        // Save 3: terminal cleanup (fails)
        store.fail_after_n_saves.store(2, Ordering::SeqCst);

        // Advance past deadline and tick
        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // Terminal save failed: pending durable candidate created, health is Critical
        let snap_before = runtime.handle().query_status().unwrap();
        assert!(!snap_before.health.persistence_healthy);
        assert_eq!(snap_before.health.status, HealthStatus::Critical);

        let initial_save_count = store.save_count.load(Ordering::SeqCst);
        let log_count_before = log.lock().unwrap().len();

        // Restore underlying store writability
        store.fail_after_n_saves.store(0, Ordering::SeqCst);
        store.fail_saves.store(false, Ordering::SeqCst);

        // Call QueryStatus multiple times
        let snap1 = runtime.handle().query_status().unwrap();
        let snap2 = runtime.handle().query_status().unwrap();

        // Assert: ZERO saves performed by QueryStatus
        let save_count_after = store.save_count.load(Ordering::SeqCst);
        assert_eq!(
            save_count_after, initial_save_count,
            "QueryStatus must perform ZERO StateStore saves"
        );

        // Assert: ZERO platform operations performed
        let log_count_after = log.lock().unwrap().len();
        assert_eq!(
            log_count_after, log_count_before,
            "QueryStatus must perform ZERO platform calls"
        );

        // Assert: pending candidate remains pending and health remains Critical (QueryStatus does not recover)
        assert!(!snap1.health.persistence_healthy);
        assert_eq!(snap1.health.status, HealthStatus::Critical);
        assert!(!snap2.health.persistence_healthy);

        // Then perform an authorized mutating path and prove pending candidate is flushed
        let mutate_res = runtime
            .handle()
            .schedule_internet_block(100, Initiator::ParentLocalPin);
        assert!(
            mutate_res.is_ok(),
            "Authorized mutation flushes pending candidate and succeeds"
        );

        let snap_after = runtime.handle().query_status().unwrap();
        assert!(
            snap_after.health.persistence_healthy,
            "Persistence recovered after mutating path"
        );
        assert_ne!(snap_after.health.status, HealthStatus::Critical);
    }

    #[test]
    fn corr_30_teardown_dirty_success_flushes_pending_candidate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        let timer_id = runtime
            .handle()
            .schedule_internet_block(10, Initiator::ParentLocalPin)
            .unwrap();

        // Save 1: schedule
        // Save 2: executing (succeeds)
        // Save 3: terminal cleanup (fails)
        store.fail_after_n_saves.store(2, Ordering::SeqCst);

        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // Side effect succeeded, terminal save failed => pending_durable_candidate exists
        let snap_before = runtime.handle().query_status().unwrap();
        assert!(!snap_before.health.persistence_healthy);

        // Store becomes writable again
        store.fail_after_n_saves.store(0, Ordering::SeqCst);
        store.fail_saves.store(false, Ordering::SeqCst);

        let gate_blocks_before = log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "gate:block_internet")
            .count();

        // NO unrelated mutation submitted -> call stop()
        let stop_res = runtime.stop();
        assert!(stop_res.is_ok(), "Stop must succeed when store is writable");

        // Worker handle joined
        assert!(runtime.worker_handle.is_none());

        // Durable state contains resolved terminal result (timer_id removed)
        let disk_state = store.state.lock().unwrap();
        let action_exists = disk_state.active_actions.iter().any(|a| a.id == timer_id);
        assert!(
            !action_exists,
            "Durable state must contain resolved terminal result"
        );

        // No extra platform side effects invoked by teardown
        let gate_blocks_after = log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "gate:block_internet")
            .count();
        assert_eq!(
            gate_blocks_before, gate_blocks_after,
            "Teardown must not invoke extra platform side effects"
        );
    }

    #[test]
    fn corr_31_teardown_dirty_failure_propagates_error() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        let _ = runtime
            .handle()
            .schedule_internet_block(10, Initiator::ParentLocalPin)
            .unwrap();

        // Save 2: executing (succeeds)
        // Save 3+: fails
        store.fail_after_n_saves.store(2, Ordering::SeqCst);

        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // Pending candidate exists, store remains unwritable
        let saves_before = store.save_count.load(Ordering::SeqCst);

        let stop_res = runtime.stop();
        // Worker must be joined
        assert!(
            runtime.worker_handle.is_none(),
            "Worker thread must always be joined"
        );

        // stop returns ServiceRuntimeError::Teardown(TeardownError::Persistence(...))
        assert!(
            stop_res.is_err(),
            "Stop MUST return error when final flush fails"
        );
        match stop_res.unwrap_err() {
            ServiceRuntimeError::Teardown(TeardownError::Persistence(_)) => {}
            other => panic!("Expected Teardown(Persistence), got: {:?}", other),
        }

        // Final flush attempted exactly once
        let saves_after = store.save_count.load(Ordering::SeqCst);
        assert_eq!(
            saves_after,
            saves_before + 1,
            "Final flush attempted exactly once"
        );
    }

    #[test]
    fn corr_32_shutdown_error_save_failure_retains_failed_candidate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        power.fail_shutdown.store(true, Ordering::SeqCst); // Power fails
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        let timer_id = runtime
            .handle()
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .unwrap();

        // Save 1: schedule (succeeds)
        // Save 2: executing (succeeds)
        // Save 3: failure outcome save (fails)
        store.fail_after_n_saves.store(2, Ordering::SeqCst);

        clock.advance(Duration::from_secs(12));
        runtime.handle().tick().unwrap();

        // 1. ShutdownState is NOT InProgress
        let snap = runtime.handle().query_status().unwrap();
        assert_ne!(snap.shutdown_state, ShutdownState::InProgress);

        // 2. Power error remains visible
        assert_eq!(snap.health.status, HealthStatus::Critical);
        assert!(!snap.health.persistence_healthy);
        assert!(snap.health.last_error.is_some());

        // 3. Authoritative in-memory candidate contains Failed action
        let act = snap
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .expect("Action must be present");
        assert!(matches!(
            act.execution_state,
            ActionExecutionState::Failed { .. }
        ));

        // Store recovers
        store.fail_after_n_saves.store(0, Ordering::SeqCst);
        store.fail_saves.store(false, Ordering::SeqCst);

        // Flush via teardown
        let stop_res = runtime.stop();
        assert!(stop_res.is_ok());

        // 4. Exact Failed action and ServiceNotification become durable
        let disk_state = store.state.lock().unwrap();
        let disk_act = disk_state
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .expect("Failed action must be durable on disk");
        assert!(matches!(
            disk_act.execution_state,
            ActionExecutionState::Failed { .. }
        ));

        let notification_exists = disk_state.telegram_outbox.iter().any(|e| match &e.payload {
            TelegramPayload::ServiceNotification { text } => {
                text.contains("Scheduled shutdown failed")
            }
            _ => false,
        });
        assert!(
            notification_exists,
            "Shutdown failure notification must be durable on disk"
        );
    }

    #[test]
    fn corr_33_internet_retry_failure_save_failure_retains_outcome() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_calls.store(true, Ordering::SeqCst); // Gate call fails on startup
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        // Fail subsequent save of retry outcome
        store.fail_saves.store(true, Ordering::SeqCst);

        // Advance clock past retry delay
        clock.advance(Duration::from_secs(6));
        runtime.handle().tick().unwrap();

        // 1. attempt_count incremented in pending candidate (1 -> 2)
        let snap = runtime.handle().query_status().unwrap();
        assert!(!snap.health.persistence_healthy);
        assert_eq!(snap.health.status, HealthStatus::Critical);

        // 2. Desired state unchanged
        assert_eq!(snap.desired_internet_state, DesiredInternetState::Blocked);

        // Store recovers
        store.fail_saves.store(false, Ordering::SeqCst);

        // Flush via teardown
        let stop_res = runtime.stop();
        assert!(stop_res.is_ok());

        // 3. Durable state contains updated attempt count, error and ServiceNotification
        let disk_state = store.state.lock().unwrap();
        let disk_retry = disk_state
            .internet_retry
            .as_ref()
            .expect("Retry metadata must be durable");
        assert_eq!(
            disk_retry.attempt_count, 2,
            "attempt_count must be incremented to 2"
        );

        let notification_exists = disk_state.telegram_outbox.iter().any(|e| match &e.payload {
            TelegramPayload::ServiceNotification { text } => {
                text.contains("Internet reconciliation retry failed")
            }
            _ => false,
        });
        assert!(
            notification_exists,
            "Reconciliation retry failure notification must be durable"
        );
    }

    #[test]
    fn corr_34_internet_retry_success_cleanup_save_failure_retains_candidate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let executing_id = TimerId([34; 16]);
        let failed_id = TimerId([35; 16]);

        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: vec![
                ScheduledAction {
                    id: executing_id,
                    action_kind: ActionKind::BlockInternet,
                    deadline: Deadline(UtcDateTime(2000000)),
                    created_at: UtcDateTime(800000),
                    created_by: Initiator::ParentLocalPin,
                    emitted_thresholds: std::collections::HashSet::new(),
                    execution_state: ActionExecutionState::Executing,
                },
                ScheduledAction {
                    id: failed_id,
                    action_kind: ActionKind::BlockInternet,
                    deadline: Deadline(UtcDateTime(700000)),
                    created_at: UtcDateTime(600000),
                    created_by: Initiator::ParentLocalPin,
                    emitted_thresholds: std::collections::HashSet::new(),
                    execution_state: ActionExecutionState::Failed {
                        reason: "Historical failure".to_string(),
                    },
                },
            ],
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_calls.store(true, Ordering::SeqCst); // Gate fails on startup => schedules retry
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate.clone(),
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        // Advance clock past retry delay
        clock.advance(Duration::from_secs(6));

        // Gate recovers and confirms Blocked
        gate.fail_calls.store(false, Ordering::SeqCst);
        *gate.current.lock().unwrap() = InternetState::Blocked;

        // Fail save of retry success cleanup
        store.fail_saves.store(true, Ordering::SeqCst);

        runtime.handle().tick().unwrap();

        // 1. Observed Blocked
        let snap = runtime.handle().query_status().unwrap();
        assert_eq!(snap.observed_internet_state, InternetState::Blocked);
        // Persistence Critical
        assert!(!snap.health.persistence_healthy);

        // 2. Candidate in-memory has executing action removed, failed action preserved
        assert!(!snap.active_actions.iter().any(|a| a.id == executing_id));
        assert!(snap.active_actions.iter().any(|a| a.id == failed_id));

        // Store recovers
        store.fail_saves.store(false, Ordering::SeqCst);

        // Teardown flushes exact candidate
        let stop_res = runtime.stop();
        assert!(stop_res.is_ok());

        let disk_state = store.state.lock().unwrap();
        assert_eq!(
            disk_state.internet_retry, None,
            "internet_retry must be cleared on disk"
        );
        assert!(
            !disk_state
                .active_actions
                .iter()
                .any(|a| a.id == executing_id),
            "Executing action must be removed"
        );
        assert!(
            disk_state.active_actions.iter().any(|a| a.id == failed_id),
            "Historical failed action must be preserved"
        );
    }

    #[test]
    fn corr_35_immediate_internet_failure_outcome_candidate_retained() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate.clone(),
            power,
            clock,
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        // Gate will fail on subsequent calls
        gate.fail_calls.store(true, Ordering::SeqCst);

        // Save 1 (pre-side-effect desired Blocked) succeeds
        // Save 2 (retry metadata + outbox) fails
        store.fail_after_n_saves.store(1, Ordering::SeqCst);

        let res = runtime
            .handle()
            .immediate_internet_block(Initiator::ParentLocalPin);

        // Caller receives Persistence error because post-side-effect save failed
        assert!(
            res.is_err(),
            "Caller must receive error when outcome save fails"
        );
        match res.unwrap_err() {
            ServiceRuntimeError::Persistence(_) => {}
            other => panic!(
                "Expected ServiceRuntimeError::Persistence, got: {:?}",
                other
            ),
        }

        // Desired state on disk remains Blocked (pre-side-effect save succeeded)
        let disk_state_mid = store.state.lock().unwrap().clone();
        assert_eq!(
            disk_state_mid.desired_internet_state,
            DesiredInternetState::Blocked
        );

        // Snapshot shows persistence Critical
        let snap = runtime.handle().query_status().unwrap();
        assert!(!snap.health.persistence_healthy);
        assert_eq!(snap.health.status, HealthStatus::Critical);

        // Store recovers
        store.fail_after_n_saves.store(0, Ordering::SeqCst);
        store.fail_saves.store(false, Ordering::SeqCst);

        // Teardown flushes retry metadata + notification
        let stop_res = runtime.stop();
        assert!(stop_res.is_ok());

        let disk_state_final = store.state.lock().unwrap();
        assert_eq!(
            disk_state_final.desired_internet_state,
            DesiredInternetState::Blocked
        );
        assert!(
            disk_state_final.internet_retry.is_some(),
            "Retry metadata must be durable"
        );

        let notification_exists =
            disk_state_final
                .telegram_outbox
                .iter()
                .any(|e| match &e.payload {
                    TelegramPayload::ServiceNotification { text } => {
                        text.contains("Immediate internet block failed")
                    }
                    _ => false,
                });
        assert!(
            notification_exists,
            "Notification must be durable without rolling desired state back"
        );
    }

    #[test]
    fn corr_36_startup_core_authority() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let overdue_id = TimerId([36; 16]);

        // Scenario A: overdue BlockInternet Pending -> recovery_overdue_transition authorizes Executing -> durable Executing + desired Blocked precedes gate
        let initial_state_a = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![ScheduledAction {
                id: overdue_id,
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(500)),
                created_at: UtcDateTime(100),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            }],
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store_a = FakeStateStore::new(initial_state_a.clone(), log.clone());
        let gate_a = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power_a = FakePowerController::new(log.clone());
        let clock_a = FakeClock::new(1000); // Overdue (now 1000 > deadline 500)
        let id_source_a = FakeIdSource::new();
        let retry_a = TestRetryPolicy::new(Duration::from_secs(5));

        let first_save_executed = Arc::new(Mutex::new(None));
        let first_save_clone = first_save_executed.clone();
        *store_a.on_save.lock().unwrap() = Some(Box::new(move |s: &PersistentState| {
            let mut guard = first_save_clone.lock().unwrap();
            if guard.is_none() {
                *guard = Some(s.clone());
            }
        }));

        let bootstrapped_a = sample_bootstrapped_state(initial_state_a);
        let mut runtime_a = ServiceRuntime::start_with_store(
            bootstrapped_a,
            store_a.clone(),
            gate_a.clone(),
            power_a,
            clock_a,
            id_source_a,
            retry_a,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        // Assert Part A:
        // 1. First durable save occurred before gate call
        let first_save = first_save_executed
            .lock()
            .unwrap()
            .clone()
            .expect("First recovery save must have occurred");
        assert_eq!(
            first_save.desired_internet_state,
            DesiredInternetState::Blocked,
            "First save must persist desired Blocked"
        );
        let saved_action = first_save
            .active_actions
            .iter()
            .find(|a| a.id == overdue_id)
            .expect("Action must be present in first save");
        assert_eq!(
            saved_action.execution_state,
            ActionExecutionState::Executing,
            "Core authorized Executing state must be persisted in first save"
        );

        // Verify order in call log: save:recovery must precede gate:block_internet
        let events = log.lock().unwrap().clone();
        let recovery_save_idx = events
            .iter()
            .position(|e| e == "save:recovery")
            .expect("save:recovery must exist");
        let gate_block_idx = events
            .iter()
            .position(|e| e == "gate:block_internet")
            .expect("gate:block_internet must exist");
        assert!(
            recovery_save_idx < gate_block_idx,
            "save:recovery must precede gate:block_internet"
        );

        // Upon gate success, action is terminalized to Completed and removed
        let snap_a = runtime_a.handle().query_status().unwrap();
        assert!(
            !snap_a.active_actions.iter().any(|a| a.id == overdue_id),
            "Action must be Completed and removed on gate success"
        );
        assert_eq!(snap_a.observed_internet_state, InternetState::Blocked);
        runtime_a.stop().unwrap();

        // Scenario B: Same recovered action -> startup gate failure -> execution_failure_transition authorizes Failed -> Failed + retry + notification durable
        let initial_state_b = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![ScheduledAction {
                id: overdue_id,
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(500)),
                created_at: UtcDateTime(100),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            }],
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let log_b = Arc::new(Mutex::new(Vec::new()));
        let store_b = FakeStateStore::new(initial_state_b.clone(), log_b.clone());
        let gate_b = FakeInternetGate::new(InternetState::Unrestricted, log_b.clone());
        gate_b.fail_calls.store(true, Ordering::SeqCst); // Force gate failure
        let power_b = FakePowerController::new(log_b.clone());
        let clock_b = FakeClock::new(1000);
        let id_source_b = FakeIdSource::new();
        let retry_b = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped_b = sample_bootstrapped_state(initial_state_b);
        let mut runtime_b = ServiceRuntime::start_with_store(
            bootstrapped_b,
            store_b.clone(),
            gate_b,
            power_b,
            clock_b,
            id_source_b,
            retry_b,
            Some(log_b),
        )
        .expect("Runtime construction must succeed even in degraded state");

        // Assert Part B:
        // Startup reconciliation failed -> execution_failure_transition authorizes Failed
        let disk_b = store_b.state.lock().unwrap().clone();
        let failed_action = disk_b
            .active_actions
            .iter()
            .find(|a| a.id == overdue_id)
            .expect("Failed action must be preserved in active_actions");
        match &failed_action.execution_state {
            ActionExecutionState::Failed { reason } => {
                assert!(
                    reason.contains("Gate"),
                    "Failed reason must reflect platform error: {}",
                    reason
                );
            }
            other => panic!("Expected ActionExecutionState::Failed, got: {:?}", other),
        }

        assert!(
            disk_b.internet_retry.is_some(),
            "Retry metadata must be durable"
        );
        assert_eq!(disk_b.internet_retry.as_ref().unwrap().attempt_count, 1);
        let has_notification = disk_b.telegram_outbox.iter().any(|e| match &e.payload {
            TelegramPayload::ServiceNotification { text } => {
                text.contains("Internet startup reconciliation failed")
            }
            _ => false,
        });
        assert!(
            has_notification,
            "ServiceNotification must be durable in outbox"
        );

        let snap_b = runtime_b.handle().query_status().unwrap();
        assert_eq!(snap_b.health.status, HealthStatus::Degraded);
        runtime_b.stop().unwrap();
    }

    #[test]
    fn corr_37_stop_wins_platform_effect_entry_race() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let shutdown_id = TimerId([37; 16]);

        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![ScheduledAction {
                id: shutdown_id,
                action_kind: ActionKind::ShutdownComputer,
                deadline: Deadline(UtcDateTime(2000)),
                created_at: UtcDateTime(1000),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            }],
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let (worker_reached_tx, worker_reached_rx) = channel();
        let (worker_release_tx, worker_release_rx) = channel();
        let worker_release_rx = Arc::new(Mutex::new(worker_release_rx));

        let pre_effect_hook: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let rx = worker_release_rx;
            move || {
                let _ = worker_reached_tx.send(());
                let guard = rx.lock().unwrap();
                let _ = guard.recv();
            }
        });

        let (stop_entered_tx, _stop_entered_rx) = channel();
        let stop_effect_hook: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let tx = worker_release_tx;
            move || {
                let _ = tx.send(());
                let _ = stop_entered_tx.send(());
            }
        });

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_test_hooks(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
            Some(pre_effect_hook),
            Some(stop_effect_hook),
        )
        .expect("Runtime construction must succeed");

        // 1. Advance clock past deadline
        clock.advance(Duration::from_secs(5));

        // Trigger worker timer evaluation on background thread
        let handle = runtime.handle().clone();
        let tick_handle = spawn(move || {
            let _ = handle.tick();
        });

        // 2 & 3. Worker evaluates timer, persists Executing to store, reaches immediately BEFORE platform-effect entry
        worker_reached_rx
            .recv()
            .expect("Worker must reach before platform effect hook");

        // Verify Step 2: Executing was durably saved before platform effect entry
        {
            let disk = store.state.lock().unwrap().clone();
            let act = disk
                .active_actions
                .iter()
                .find(|a| a.id == shutdown_id)
                .expect("Action must be present on disk");
            assert_eq!(
                act.execution_state,
                ActionExecutionState::Executing,
                "Action must be durably Executing before platform effect entry"
            );
        }

        // 4 & 5. Actual runtime.stop() crosses/wins platform_effect_gate boundary, setting stop_requested
        // and releasing worker
        let stop_res = runtime.stop();
        assert!(stop_res.is_ok(), "runtime.stop() must succeed");

        let _ = tick_handle.join();

        // 6. PowerController invocation count remains 0
        let events = log.lock().unwrap().clone();
        let shutdown_invocations = events
            .iter()
            .filter(|e| e.contains("power:initiate_shutdown"))
            .count();
        assert_eq!(
            shutdown_invocations, 0,
            "PowerController must NOT be invoked when Stop wins boundary"
        );

        // 9. Durable action in store remains Executing
        {
            let disk = store.state.lock().unwrap().clone();
            let act = disk
                .active_actions
                .iter()
                .find(|a| a.id == shutdown_id)
                .expect("Action must remain on disk");
            assert_eq!(
                act.execution_state,
                ActionExecutionState::Executing,
                "Durable action must remain Executing"
            );
        }

        // 10. Restart converts it to Missed without PowerController
        let restart_state = store.state.lock().unwrap().clone();
        let restart_log = Arc::new(Mutex::new(Vec::new()));
        let restart_store = FakeStateStore::new(restart_state.clone(), restart_log.clone());
        let restart_gate = FakeInternetGate::new(InternetState::Unrestricted, restart_log.clone());
        let restart_power = FakePowerController::new(restart_log.clone());
        let restart_clock = FakeClock::new(7000);
        let restart_id_source = FakeIdSource::new();
        let restart_retry = TestRetryPolicy::new(Duration::from_secs(5));
        let restart_bootstrapped = sample_bootstrapped_state(restart_state);

        let mut restart_runtime = ServiceRuntime::start_with_store(
            restart_bootstrapped,
            restart_store.clone(),
            restart_gate,
            restart_power,
            restart_clock,
            restart_id_source,
            restart_retry,
            Some(restart_log.clone()),
        )
        .expect("Restart runtime construction must succeed");

        let restart_events = restart_log.lock().unwrap().clone();
        assert!(
            !restart_events
                .iter()
                .any(|e| e.contains("power:initiate_shutdown")),
            "Restart must NOT invoke PowerController for overdue Executing shutdown"
        );

        let restart_disk = restart_store.state.lock().unwrap().clone();
        assert!(
            !restart_disk
                .active_actions
                .iter()
                .any(|a| a.id == shutdown_id),
            "Missed shutdown must be removed from active_actions"
        );
        let notification_present = restart_disk
            .telegram_outbox
            .iter()
            .any(|e| match &e.payload {
                TelegramPayload::ServiceNotification { text } => {
                    text.contains("Scheduled shutdown was missed")
                }
                _ => false,
            });
        assert!(
            notification_present,
            "Notification of missed shutdown must be persisted"
        );

        restart_runtime.stop().unwrap();
    }

    #[test]
    fn corr_38_platform_effect_wins_boundary_before_stop() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let shutdown_id = TimerId([38; 16]);

        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: vec![ScheduledAction {
                id: shutdown_id,
                action_kind: ActionKind::ShutdownComputer,
                deadline: Deadline(UtcDateTime(2000)),
                created_at: UtcDateTime(1000),
                created_by: Initiator::ParentLocalPin,
                emitted_thresholds: std::collections::HashSet::new(),
                execution_state: ActionExecutionState::Pending,
            }],
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let (in_effect_tx, in_effect_rx) = channel();
        let (finish_effect_tx, finish_effect_rx) = channel();
        let finish_effect_rx = Arc::new(Mutex::new(finish_effect_rx));

        // Hook inside platform operation: called WHILE holding platform_effect_gate!
        *power.on_initiate_shutdown.lock().unwrap() = Some(Box::new({
            let rx = finish_effect_rx;
            move || {
                let _ = in_effect_tx.send(());
                let guard = rx.lock().unwrap();
                let _ = guard.recv();
            }
        }));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed");

        // Advance clock past deadline
        clock.advance(Duration::from_secs(5));

        // Trigger evaluation on background thread
        let handle = runtime.handle().clone();
        let tick_handle = spawn(move || {
            let _ = handle.tick();
        });

        // Wait until worker is INSIDE initiate_shutdown (holding platform_effect_gate)
        in_effect_rx
            .recv()
            .expect("Worker must enter platform effect");

        // Worker is currently holding platform_effect_gate!
        // Now call runtime.stop() in another thread: it will acquire ingress,
        // then try to acquire platform_effect_gate and wait for the already-entered attempt!
        let (stop_done_tx, stop_done_rx) = channel();
        let mut runtime_stop_wrapper = Some(runtime);
        let stop_thread = spawn(move || {
            let mut r = runtime_stop_wrapper.take().unwrap();
            let res = r.stop();
            let _ = stop_done_tx.send(res);
        });

        // Release the platform effect so it completes
        finish_effect_tx
            .send(())
            .expect("Must signal platform effect to finish");

        // Stop waits for that already-entered attempt and completes without deadlock
        let stop_result = stop_done_rx
            .recv()
            .expect("Stop thread must report completion");
        assert!(stop_result.is_ok(), "Stop must succeed without deadlock");

        let _ = stop_thread.join();
        let _ = tick_handle.join();

        // Verifications:
        // 1. One platform attempt completed
        let events = log.lock().unwrap().clone();
        let shutdown_count = events
            .iter()
            .filter(|e| e == &"power:initiate_shutdown")
            .count();
        assert_eq!(
            shutdown_count, 1,
            "Exactly one platform attempt must complete"
        );

        // 2. No second platform attempt started
        assert_eq!(
            events.iter().filter(|e| e.starts_with("power:")).count(),
            1,
            "No second platform attempt must start"
        );
    }

    // ========================================================================
    // SLICE 3A TEST FIXTURES & 19 TESTS
    // ========================================================================

    fn sample_initial_state() -> PersistentState {
        PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        }
    }

    fn setup_test_runtime() -> (ServiceRuntime, FakeClock, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("Runtime construction must succeed");

        (runtime, clock, log)
    }

    fn setup_test_coordinator() -> ServiceRuntimeCoordinator<
        FakeStateStore,
        FakeInternetGate,
        FakePowerController,
        FakeClock,
        FakeIdSource,
        TestRetryPolicy,
    > {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let platform_effect_gate = Arc::new(Mutex::new(()));

        let (coordinator, _) = ServiceRuntimeCoordinator::new(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
            stop_requested,
            platform_effect_gate,
            None,
        )
        .expect("Coordinator construction must succeed");

        coordinator
    }

    #[test]
    fn test_ipc37_typed_event_broadcaster_independent_of_call_log() {
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), Arc::new(Mutex::new(Vec::new())));
        let gate = FakeInternetGate::new(
            InternetState::Unrestricted,
            Arc::new(Mutex::new(Vec::new())),
        );
        let power = FakePowerController::new(Arc::new(Mutex::new(Vec::new())));
        let clock = FakeClock::new(1000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        // Start runtime with call_log = None
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            None,
        )
        .expect("Runtime construction must succeed without call_log");

        let handle = runtime.handle().clone();
        let sub = handle
            .subscribe_events()
            .expect("Subscription must succeed with call_log=None");
        assert_eq!(sub.subscription_id, SubscriptionId(1));
        assert_eq!(sub.initial_snapshot.health.active_tray_sessions, 1);

        let unsub_res = handle
            .unsubscribe_events(sub.subscription_id)
            .expect("Unsubscribe must succeed");
        assert!(unsub_res);

        let stop_res = runtime.stop();
        assert!(stop_res.is_ok());
    }

    #[test]
    fn test_ipc38_subscribe_registration_is_atomic() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub = handle.subscribe_events().expect("Subscribe must succeed");
        assert_eq!(sub.subscription_id, SubscriptionId(1));

        let status = handle.query_status().expect("Query status must succeed");
        assert_eq!(status.health.active_tray_sessions, 1);

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc39_initial_snapshot_contains_final_stable_session_count() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub1 = handle.subscribe_events().expect("Subscribe 1 must succeed");
        assert_eq!(sub1.initial_snapshot.health.active_tray_sessions, 1);

        let sub2 = handle.subscribe_events().expect("Subscribe 2 must succeed");
        assert_eq!(sub2.initial_snapshot.health.active_tray_sessions, 2);

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc39_full_existing_subscriber_pruned_before_new_snapshot() {
        let mut coordinator = setup_test_coordinator();

        // 1. Register subscriber A
        let (reply_tx_a, reply_rx_a) = channel();
        coordinator.handle_subscribe_events(reply_tx_a);
        let sub_a = reply_rx_a.recv().unwrap().unwrap();
        assert_eq!(sub_a.subscription_id, SubscriptionId(1));
        assert_eq!(coordinator.health.active_tray_sessions, 1);

        // 2. Fill subscriber A queue to capacity (64 events)
        for _ in 0..64 {
            let ev = Event::ServiceHealthUpdated {
                health: coordinator.build_service_health_snapshot(),
            };
            let _ = coordinator.emit_event(ev);
        }
        assert_eq!(coordinator.subscribers.len(), 1);

        // 3. Register subscriber B:
        // Reconciling with existing subscriber A attempts to send ServiceHealthUpdated to A.
        // A's queue (already 64) cannot accept the event -> TrySendError::Full -> A is pruned!
        let (reply_tx_b, reply_rx_b) = channel();
        coordinator.handle_subscribe_events(reply_tx_b);
        let sub_b = reply_rx_b.recv().unwrap().unwrap();
        assert_eq!(sub_b.subscription_id, SubscriptionId(2));

        // 4. B initial snapshot must report active_tray_sessions == 1 (not 2!)
        assert_eq!(sub_b.initial_snapshot.health.active_tray_sessions, 1);

        // 5. B receiver must be empty (no self-registration event)
        assert!(sub_b.receiver.try_recv().is_err());

        // 6. Coordinator subscribers has only B
        assert_eq!(coordinator.subscribers.len(), 1);
        assert!(!coordinator.subscribers.contains_key(&SubscriptionId(1)));
        assert!(coordinator.subscribers.contains_key(&SubscriptionId(2)));
    }

    #[test]
    fn test_ipc40_event_after_subscribe_barrier_delivered_without_gap() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub_a = handle.subscribe_events().expect("Subscribe A must succeed");
        let sub_b = handle.subscribe_events().expect("Subscribe B must succeed");

        assert_eq!(sub_b.initial_snapshot.health.active_tray_sessions, 2);
        // B receiver is empty right after subscription
        assert!(sub_b.receiver.try_recv().is_err());

        // Unsubscribe A is an operation strictly AFTER B's barrier
        let unsub_res = handle
            .unsubscribe_events(sub_a.subscription_id)
            .expect("Unsubscribe A must succeed");
        assert!(unsub_res);

        // B receiver must receive the health update
        let ev = sub_b
            .receiver
            .try_recv()
            .expect("Event must be in B's receiver");
        match ev {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.active_tray_sessions, 1);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc41_queue_capacity_exact_64_and_65th_removes_subscriber() {
        let mut coordinator = setup_test_coordinator();

        let (reply_tx, reply_rx) = channel();
        coordinator.handle_subscribe_events(reply_tx);
        let sub = reply_rx.recv().unwrap().unwrap();

        // Send 64 events without draining receiver
        for i in 0..64 {
            let ev = Event::ServiceHealthUpdated {
                health: coordinator.build_service_health_snapshot(),
            };
            let res = coordinator.emit_event(ev);
            assert!(res.is_ok());
            assert_eq!(
                coordinator.subscribers.len(),
                1,
                "Must remain subscribed at event {}",
                i + 1
            );
        }

        // 65th event triggers Full and removes subscriber
        let ev65 = Event::ServiceHealthUpdated {
            health: coordinator.build_service_health_snapshot(),
        };
        let res65 = coordinator.emit_event(ev65);
        assert!(res65.is_ok());
        assert_eq!(
            coordinator.subscribers.len(),
            0,
            "65th event must prune subscriber"
        );
        assert_eq!(coordinator.health.active_tray_sessions, 0);

        // Drain receiver: must contain exactly 64 events (65th was rejected, no drop-oldest)
        let mut count = 0;
        while sub.receiver.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 64, "Receiver must contain exactly 64 queued events");
    }

    #[test]
    fn test_ipc42_disconnected_receiver_removed_on_next_event() {
        let mut coordinator = setup_test_coordinator();

        let (reply_tx, reply_rx) = channel();
        coordinator.handle_subscribe_events(reply_tx);
        let sub = reply_rx.recv().unwrap().unwrap();
        assert_eq!(coordinator.subscribers.len(), 1);

        // Drop the receiver
        drop(sub.receiver);

        // Emit next event: triggers Disconnected
        let ev = Event::ServiceHealthUpdated {
            health: coordinator.build_service_health_snapshot(),
        };
        let res = coordinator.emit_event(ev);
        assert!(res.is_ok());
        assert_eq!(coordinator.subscribers.len(), 0);
        assert_eq!(coordinator.health.active_tray_sessions, 0);
    }

    #[test]
    fn test_ipc43_slow_subscriber_never_blocks_coordinator() {
        let mut coordinator = setup_test_coordinator();

        let (reply_tx, reply_rx) = channel();
        coordinator.handle_subscribe_events(reply_tx);
        let sub_a = reply_rx.recv().unwrap().unwrap();
        assert_eq!(coordinator.subscribers.len(), 1);

        // Fill A's queue to exactly 64 using internal test access without causing automatic removal
        if let Some(tx_a) = coordinator.subscribers.get(&sub_a.subscription_id) {
            for _ in 0..64 {
                let ev = Event::ServiceHealthUpdated {
                    health: coordinator.build_service_health_snapshot(),
                };
                assert!(tx_a.try_send(ev).is_ok());
            }
        }

        // Ordinary typed event delivery encounters Full and removes subscriber A via try_send
        let emit_ev = Event::ServiceHealthUpdated {
            health: coordinator.build_service_health_snapshot(),
        };
        let emit_res = coordinator.emit_event(emit_ev);
        assert!(emit_res.is_ok());
        assert_eq!(coordinator.subscribers.len(), 0);
        assert_eq!(coordinator.health.active_tray_sessions, 0);

        // Immediately execute another coordinator operation (QueryStatus) and verify completion
        let (status_tx, status_rx) = channel();
        coordinator.handle_query_status(status_tx);
        let status = status_rx.recv().unwrap();
        assert_eq!(status.health.active_tray_sessions, 0);
    }

    #[test]
    fn test_ipc44_registry_length_is_only_session_count_source() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            0
        );

        let s1 = handle.subscribe_events().unwrap();
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            1
        );

        let s2 = handle.subscribe_events().unwrap();
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            2
        );

        let s3 = handle.subscribe_events().unwrap();
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            3
        );

        handle.unsubscribe_events(s2.subscription_id).unwrap();
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            2
        );

        handle.unsubscribe_events(s1.subscription_id).unwrap();
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            1
        );

        handle.unsubscribe_events(s3.subscription_id).unwrap();
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            0
        );

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc44_unsubscribe_is_idempotent() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub = handle.subscribe_events().unwrap();
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            1
        );

        let first = handle.unsubscribe_events(sub.subscription_id).unwrap();
        assert!(first);
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            0
        );

        let second = handle.unsubscribe_events(sub.subscription_id).unwrap();
        assert!(!second);
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            0
        );

        let unknown = handle.unsubscribe_events(SubscriptionId(9999)).unwrap();
        assert!(!unknown);
        assert_eq!(
            handle.query_status().unwrap().health.active_tray_sessions,
            0
        );

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc44_auto_remove_then_unsubscribe_does_not_change_count() {
        let mut coordinator = setup_test_coordinator();

        let (reply_tx, reply_rx) = channel();
        coordinator.handle_subscribe_events(reply_tx);
        let sub = reply_rx.recv().unwrap().unwrap();

        // Drop receiver and trigger auto-removal via event emission
        drop(sub.receiver);
        let _ = coordinator.emit_event(Event::ServiceHealthUpdated {
            health: coordinator.build_service_health_snapshot(),
        });
        assert_eq!(coordinator.subscribers.len(), 0);
        assert_eq!(coordinator.health.active_tray_sessions, 0);

        // Now explicitly unsubscribe the already auto-removed ID
        let (unsub_tx, unsub_rx) = channel();
        coordinator.handle_unsubscribe_events(sub.subscription_id, unsub_tx);
        let unsub_res = unsub_rx.recv().unwrap().unwrap();
        assert!(!unsub_res);
        assert_eq!(coordinator.health.active_tray_sessions, 0);
    }

    #[test]
    fn test_subscription_id_exhaustion_fails_without_mutation() {
        let mut coordinator = setup_test_coordinator();
        coordinator.next_subscription_id = None;

        let (reply_tx, reply_rx) = channel();
        coordinator.handle_subscribe_events(reply_tx);
        let res = reply_rx.recv().unwrap();

        match res {
            Err(ServiceRuntimeError::SubscriptionIdExhausted) => {}
            other => panic!("Expected SubscriptionIdExhausted, got {:?}", other),
        }
        assert_eq!(coordinator.subscribers.len(), 0);
        assert_eq!(coordinator.health.active_tray_sessions, 0);
    }

    #[test]
    fn test_subscription_id_collision_fails_without_mutation() {
        let mut coordinator = setup_test_coordinator();

        // Artificially insert SubscriptionId(1)
        let (dummy_tx, dummy_rx) = sync_channel(64);
        coordinator.subscribers.insert(SubscriptionId(1), dummy_tx);
        coordinator.health.active_tray_sessions = 1;
        coordinator.next_subscription_id = Some(1);

        let (reply_tx, reply_rx) = channel();
        coordinator.handle_subscribe_events(reply_tx);
        let res = reply_rx.recv().unwrap();

        match res {
            Err(ServiceRuntimeError::SubscriptionIdExhausted) => {}
            other => panic!(
                "Expected SubscriptionIdExhausted on collision, got {:?}",
                other
            ),
        }
        assert_eq!(coordinator.subscribers.len(), 1);
        assert_eq!(coordinator.health.active_tray_sessions, 1);
        assert_eq!(coordinator.next_subscription_id, Some(1));
        assert!(coordinator.subscribers.contains_key(&SubscriptionId(1)));

        // Verify existing sender entry was not replaced
        let test_ev = Event::ServiceHealthUpdated {
            health: coordinator.build_service_health_snapshot(),
        };
        let _ = coordinator.emit_event(test_ev);
        assert!(dummy_rx.try_recv().is_ok());
    }

    #[test]
    fn test_subscription_capacity_overflow_fails_without_mutation() {
        // Normal max boundary: (u32::MAX - 1) as usize must return Ok(u32::MAX)
        let normal_boundary = (u32::MAX - 1) as usize;
        let normal_res = checked_prospective_active_session_count(normal_boundary);
        assert_eq!(normal_res.unwrap(), u32::MAX);

        // Overflow boundary: u32::MAX as usize must return SubscriptionCapacityExhausted
        let overflow_boundary = u32::MAX as usize;
        let overflow_res = checked_prospective_active_session_count(overflow_boundary);
        match overflow_res {
            Err(ServiceRuntimeError::SubscriptionCapacityExhausted) => {}
            other => panic!(
                "Expected SubscriptionCapacityExhausted at u32::MAX, got {:?}",
                other
            ),
        }

        // usize::MAX boundary must return SubscriptionCapacityExhausted
        let usize_max_res = checked_prospective_active_session_count(usize::MAX);
        match usize_max_res {
            Err(ServiceRuntimeError::SubscriptionCapacityExhausted) => {}
            other => panic!(
                "Expected SubscriptionCapacityExhausted at usize::MAX, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_subscribe_reply_failure_rolls_back_registration_and_health() {
        let mut coordinator = setup_test_coordinator();

        // 1. Register subscriber A successfully
        let (tx1, rx1) = channel();
        coordinator.handle_subscribe_events(tx1);
        let sub_a = rx1.recv().unwrap().unwrap();
        assert_eq!(coordinator.health.active_tray_sessions, 1);

        // 2. Drain A receiver
        while sub_a.receiver.try_recv().is_ok() {}

        // 3. Attempt subscriber B with dropped reply receiver
        let (tx2, rx2) = channel();
        drop(rx2); // caller disappeared
        coordinator.handle_subscribe_events(tx2);

        // 4. Coordinator must have rolled back B's registration and reconciled health
        assert_eq!(coordinator.subscribers.len(), 1);
        assert_eq!(coordinator.health.active_tray_sessions, 1);
        assert!(coordinator.subscribers.contains_key(&sub_a.subscription_id));
        assert!(!coordinator.subscribers.contains_key(&SubscriptionId(2)));

        // 5. A receives the final reconciliation event showing active_tray_sessions == 1
        let mut last_session_count = None;
        while let Ok(ev) = sub_a.receiver.try_recv() {
            if let Event::ServiceHealthUpdated { health } = ev {
                last_session_count = Some(health.active_tray_sessions);
            }
        }
        assert_eq!(last_session_count, Some(1));
    }

    #[test]
    fn test_new_subscriber_receives_no_self_registration_health_event() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub1 = handle.subscribe_events().unwrap();
        let sub2 = handle.subscribe_events().unwrap();

        // sub2 received NO event from its own registration
        assert!(sub2.receiver.try_recv().is_err());

        // sub1 did receive the event for sub2's registration
        let ev = sub1.receiver.try_recv().expect("sub1 must receive update");
        match ev {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.active_tray_sessions, 2);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_unsubscribe_emits_health_to_remaining_subscribers() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub1 = handle.subscribe_events().unwrap();
        let sub2 = handle.subscribe_events().unwrap();
        let sub3 = handle.subscribe_events().unwrap();

        // Drain sub2 and sub3
        while sub2.receiver.try_recv().is_ok() {}
        while sub3.receiver.try_recv().is_ok() {}

        handle.unsubscribe_events(sub1.subscription_id).unwrap();

        let ev2 = sub2
            .receiver
            .try_recv()
            .expect("sub2 must receive health event");
        match ev2 {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.active_tray_sessions, 2);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }

        let ev3 = sub3
            .receiver
            .try_recv()
            .expect("sub3 must receive health event");
        match ev3 {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.active_tray_sessions, 2);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_health_stabilization_converges_to_final_registry_count() {
        let mut coordinator = setup_test_coordinator();

        let (tx1, rx1) = channel();
        coordinator.handle_subscribe_events(tx1);
        let sub1 = rx1.recv().unwrap().unwrap();

        let (tx2, rx2) = channel();
        coordinator.handle_subscribe_events(tx2);
        let sub2 = rx2.recv().unwrap().unwrap();

        let (tx3, rx3) = channel();
        coordinator.handle_subscribe_events(tx3);
        let sub3 = rx3.recv().unwrap().unwrap();

        assert_eq!(coordinator.health.active_tray_sessions, 3);

        // Drain sub1, sub2, and sub3
        while sub1.receiver.try_recv().is_ok() {}
        while sub2.receiver.try_recv().is_ok() {}
        while sub3.receiver.try_recv().is_ok() {}

        // Fill sub2 queue directly to 64 (will encounter Full)
        if let Some(tx2) = coordinator.subscribers.get(&sub2.subscription_id) {
            for _ in 0..64 {
                let _ = tx2.try_send(Event::ServiceHealthUpdated {
                    health: coordinator.build_service_health_snapshot(),
                });
            }
        }

        // Drop sub3 receiver (will encounter Disconnected)
        drop(sub3.receiver);

        // Emit an event to trigger delivery, pruning of sub2 & sub3, and iterative health reconciliation
        let emit_ev = Event::ServiceHealthUpdated {
            health: coordinator.build_service_health_snapshot(),
        };
        let emit_res = coordinator.emit_event(emit_ev);
        assert!(emit_res.is_ok());

        // Final stable state: exactly survivor sub1 remains, health == 1
        assert_eq!(coordinator.subscribers.len(), 1);
        assert_eq!(coordinator.health.active_tray_sessions, 1);
        assert!(coordinator.subscribers.contains_key(&sub1.subscription_id));

        // Survivor sub1 receives final health update showing active_tray_sessions == 1
        let mut final_session_count = None;
        while let Ok(ev) = sub1.receiver.try_recv() {
            if let Event::ServiceHealthUpdated { health } = ev {
                final_session_count = Some(health.active_tray_sessions);
            }
        }
        assert_eq!(final_session_count, Some(1));

        drop(sub2);
    }

    #[test]
    fn test_runtime_stop_clears_subscribers_and_disconnects_receivers() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub1 = handle.subscribe_events().unwrap();
        let sub2 = handle.subscribe_events().unwrap();

        runtime.stop().expect("Runtime stop must succeed");

        // Drain any buffered events and verify channel disconnected
        while sub1.receiver.try_recv().is_ok() {}
        while sub2.receiver.try_recv().is_ok() {}

        assert!(sub1.receiver.recv().is_err());
        assert!(sub2.receiver.recv().is_err());
    }

    // ========================================================================
    // SLICE 3B: EXACT TIMER CANCELLATION TESTS (26 tests)
    // ========================================================================

    #[test]
    fn test_ipc71_cancel_internet_block_absent_returns_already_absent() {
        let (mut runtime, _clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let absent_id = TimerId([0xEE; 16]);
        let res = handle.cancel_internet_block_timer(absent_id, Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::AlreadyAbsent);

        // No persistence save logged
        let logs = log.lock().unwrap().clone();
        assert!(!logs.iter().any(|l| l.contains("cancel_timer")));

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc71_cancel_shutdown_absent_returns_already_absent() {
        let (mut runtime, _clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let absent_id = TimerId([0xEE; 16]);
        let res = handle.cancel_shutdown_timer(absent_id, Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::AlreadyAbsent);

        // No persistence save logged
        let logs = log.lock().unwrap().clone();
        assert!(!logs.iter().any(|l| l.contains("cancel_timer")));

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc72_cancel_internet_with_shutdown_id_returns_timer_kind_mismatch() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let shutdown_id = handle
            .schedule_shutdown(300, Initiator::ParentLocalPin)
            .expect("Schedule shutdown must succeed");

        // Attempting to cancel shutdown timer via cancel_internet_block_timer returns TimerKindMismatch
        let res = handle.cancel_internet_block_timer(shutdown_id, Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::TimerKindMismatch);

        // Shutdown timer still exists in status snapshot
        let snap = handle.query_status().unwrap();
        assert_eq!(snap.active_actions.len(), 1);
        assert_eq!(snap.active_actions[0].id, shutdown_id);
        assert_eq!(
            snap.active_actions[0].action_kind,
            ActionKind::ShutdownComputer
        );

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc72_cancel_shutdown_with_internet_id_returns_timer_kind_mismatch() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let block_id = handle
            .schedule_internet_block(300, Initiator::ParentLocalPin)
            .expect("Schedule internet block must succeed");

        // Attempting to cancel internet block timer via cancel_shutdown_timer returns TimerKindMismatch
        let res = handle.cancel_shutdown_timer(block_id, Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::TimerKindMismatch);

        // BlockInternet timer still exists in status snapshot
        let snap = handle.query_status().unwrap();
        assert_eq!(snap.active_actions.len(), 1);
        assert_eq!(snap.active_actions[0].id, block_id);
        assert_eq!(
            snap.active_actions[0].action_kind,
            ActionKind::BlockInternet
        );

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc73_repeated_internet_cancellation_is_idempotent() {
        let (mut runtime, _clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        let block_id = handle
            .schedule_internet_block(300, Initiator::ParentLocalPin)
            .expect("Schedule internet block must succeed");

        // First cancellation -> Cancelled
        let res1 = handle.cancel_internet_block_timer(block_id, Initiator::ParentLocalPin);
        assert_eq!(res1.unwrap(), TimerCancellationResult::Cancelled);

        // Second cancellation -> AlreadyAbsent
        let res2 = handle.cancel_internet_block_timer(block_id, Initiator::ParentLocalPin);
        assert_eq!(res2.unwrap(), TimerCancellationResult::AlreadyAbsent);

        // Exactly one save:cancel_timer logged
        let logs = log.lock().unwrap().clone();
        let cancel_saves = logs.iter().filter(|l| *l == "save:cancel_timer").count();
        assert_eq!(cancel_saves, 1);

        // Exactly one TimerCancelled event received
        let mut cancel_event_count = 0;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { id, action_kind } = ev {
                assert_eq!(id, block_id);
                assert_eq!(action_kind, ActionKind::BlockInternet);
                cancel_event_count += 1;
            }
        }
        assert_eq!(cancel_event_count, 1);

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc73_repeated_shutdown_cancellation_is_idempotent() {
        let (mut runtime, _clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        let shutdown_id = handle
            .schedule_shutdown(300, Initiator::ParentLocalPin)
            .expect("Schedule shutdown must succeed");

        // First cancellation -> Cancelled
        let res1 = handle.cancel_shutdown_timer(shutdown_id, Initiator::ParentLocalPin);
        assert_eq!(res1.unwrap(), TimerCancellationResult::Cancelled);

        // Second cancellation -> AlreadyAbsent
        let res2 = handle.cancel_shutdown_timer(shutdown_id, Initiator::ParentLocalPin);
        assert_eq!(res2.unwrap(), TimerCancellationResult::AlreadyAbsent);

        // Exactly one save:cancel_timer logged
        let logs = log.lock().unwrap().clone();
        let cancel_saves = logs.iter().filter(|l| *l == "save:cancel_timer").count();
        assert_eq!(cancel_saves, 1);

        // Exactly one TimerCancelled event received
        let mut cancel_event_count = 0;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { id, action_kind } = ev {
                assert_eq!(id, shutdown_id);
                assert_eq!(action_kind, ActionKind::ShutdownComputer);
                cancel_event_count += 1;
            }
        }
        assert_eq!(cancel_event_count, 1);

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc74_shutdown_cancel_before_monotonic_deadline_succeeds() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let shutdown_id = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("Schedule shutdown must succeed");

        // Advance 5 seconds (target is 10s from start)
        clock.advance(Duration::from_secs(5));

        let res = handle.cancel_shutdown_timer(shutdown_id, Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::Cancelled);

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc74_shutdown_cancel_at_exact_monotonic_deadline_rejected() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let shutdown_id = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("Schedule shutdown must succeed");

        // Advance exactly 10 seconds to hit monotonic target
        clock.advance(Duration::from_secs(10));

        let res = handle.cancel_shutdown_timer(shutdown_id, Initiator::ParentLocalPin);
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc74_shutdown_cancel_after_monotonic_deadline_rejected() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let shutdown_id = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("Schedule shutdown must succeed");

        // Advance 15 seconds past monotonic target
        clock.advance(Duration::from_secs(15));

        let res = handle.cancel_shutdown_timer(shutdown_id, Initiator::ParentLocalPin);
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc74_utc_backward_jump_does_not_reopen_expired_shutdown_cancellation() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let shutdown_id = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("Schedule shutdown must succeed");

        // Advance monotonic clock past target (12s)
        clock.advance(Duration::from_secs(12));

        // Shift UTC wall clock backward by 100 seconds
        clock.shift_utc_only(-100_000);

        // Cancellation must still be rejected based on monotonic target authority
        let res = handle.cancel_shutdown_timer(shutdown_id, Initiator::ParentLocalPin);
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));

        let _ = runtime.stop();
    }

    #[test]
    fn test_ipc74_utc_forward_jump_does_not_prematurely_reject_unexpired_shutdown_cancellation() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let shutdown_id = handle
            .schedule_shutdown(100, Initiator::ParentLocalPin)
            .expect("Schedule shutdown must succeed");

        // Monotonic time is still at 0 (well before 100s target), but shift UTC forward by 1000s
        clock.shift_utc_only(1_000_000);

        // Cancellation must still succeed based on monotonic target authority
        let res = handle.cancel_shutdown_timer(shutdown_id, Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::Cancelled);

        let _ = runtime.stop();
    }

    #[test]
    fn test_exact_timer_id_isolation_among_multiple_actions() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        let id_a = handle
            .schedule_internet_block(60, Initiator::ParentLocalPin)
            .unwrap();
        let id_b = handle
            .schedule_internet_block(120, Initiator::ParentLocalPin)
            .unwrap();
        let id_c = handle
            .schedule_shutdown(180, Initiator::ParentLocalPin)
            .unwrap();

        // Cancel B
        let res_b = handle.cancel_internet_block_timer(id_b, Initiator::ParentLocalPin);
        assert_eq!(res_b.unwrap(), TimerCancellationResult::Cancelled);

        // Verify A and C remain in state
        let snap = handle.query_status().unwrap();
        let ids: Vec<_> = snap.active_actions.iter().map(|a| a.id).collect();
        assert!(ids.contains(&id_a));
        assert!(!ids.contains(&id_b));
        assert!(ids.contains(&id_c));

        // Event emitted only for B
        let mut cancelled_ids = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { id, .. } = ev {
                cancelled_ids.push(id);
            }
        }
        assert_eq!(cancelled_ids, vec![id_b]);

        // Cancel C
        let res_c = handle.cancel_shutdown_timer(id_c, Initiator::ParentLocalPin);
        assert_eq!(res_c.unwrap(), TimerCancellationResult::Cancelled);

        // A still remains
        let snap2 = handle.query_status().unwrap();
        let ids2: Vec<_> = snap2.active_actions.iter().map(|a| a.id).collect();
        assert_eq!(ids2, vec![id_a]);

        let _ = runtime.stop();
    }

    #[test]
    fn test_successful_internet_timer_cancellation_removes_action_and_anchor() {
        let mut coordinator = setup_test_coordinator();

        let timer_id = coordinator
            .handle_schedule_action(ActionKind::BlockInternet, 60, Initiator::ParentLocalPin)
            .unwrap();

        assert_eq!(coordinator.state.active_actions.len(), 1);
        assert!(coordinator.monotonic_timers.contains_key(&timer_id));

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::BlockInternet,
            Initiator::ParentLocalPin,
        );
        assert_eq!(res.unwrap(), TimerCancellationResult::Cancelled);

        assert_eq!(coordinator.state.active_actions.len(), 0);
        assert!(!coordinator.monotonic_timers.contains_key(&timer_id));
    }

    #[test]
    fn test_successful_shutdown_timer_cancellation_removes_action_and_anchor() {
        let mut coordinator = setup_test_coordinator();

        let timer_id = coordinator
            .handle_schedule_action(ActionKind::ShutdownComputer, 60, Initiator::ParentLocalPin)
            .unwrap();

        assert_eq!(coordinator.state.active_actions.len(), 1);
        assert!(coordinator.monotonic_timers.contains_key(&timer_id));

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::ShutdownComputer,
            Initiator::ParentLocalPin,
        );
        assert_eq!(res.unwrap(), TimerCancellationResult::Cancelled);

        assert_eq!(coordinator.state.active_actions.len(), 0);
        assert!(!coordinator.monotonic_timers.contains_key(&timer_id));
    }

    #[test]
    fn test_executing_action_cancellation_forbidden() {
        let mut coordinator = setup_test_coordinator();
        let timer_id = TimerId([0x11; 16]);

        coordinator.state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Executing,
        });

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::BlockInternet,
            Initiator::ParentLocalPin,
        );
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));
        assert_eq!(coordinator.state.active_actions.len(), 1);
    }

    #[test]
    fn test_failed_action_cancellation_forbidden() {
        let mut coordinator = setup_test_coordinator();
        let timer_id = TimerId([0x22; 16]);

        coordinator.state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Failed {
                reason: "Power controller error".to_string(),
            },
        });

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::ShutdownComputer,
            Initiator::ParentLocalPin,
        );
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));
        assert_eq!(coordinator.state.active_actions.len(), 1);
    }

    #[test]
    fn test_completed_action_cancellation_forbidden() {
        let mut coordinator = setup_test_coordinator();
        let timer_id = TimerId([0x33; 16]);

        coordinator.state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Completed,
        });

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::BlockInternet,
            Initiator::ParentLocalPin,
        );
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));
        assert_eq!(coordinator.state.active_actions.len(), 1);
    }

    #[test]
    fn test_missed_action_cancellation_forbidden() {
        let mut coordinator = setup_test_coordinator();
        let timer_id = TimerId([0x44; 16]);

        coordinator.state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Missed,
        });

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::ShutdownComputer,
            Initiator::ParentLocalPin,
        );
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));
        assert_eq!(coordinator.state.active_actions.len(), 1);
    }

    #[test]
    fn test_shutdown_cancel_missing_monotonic_anchor_fails_closed() {
        let mut coordinator = setup_test_coordinator();
        let timer_id = TimerId([0x55; 16]);

        // Action exists in authoritative state, but monotonic anchor is absent
        coordinator.state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::ShutdownComputer,
            Initiator::ParentLocalPin,
        );
        match res {
            Err(ServiceRuntimeError::CancellationForbidden(msg)) => {
                assert!(msg.contains("Monotonic timer anchor unavailable"));
            }
            other => panic!("Expected CancellationForbidden, got {:?}", other),
        }
        assert_eq!(coordinator.state.active_actions.len(), 1);
    }

    #[test]
    fn test_shutdown_cancel_mismatched_anchor_kind_fails_closed() {
        let mut coordinator = setup_test_coordinator();
        let timer_id = TimerId([0x66; 16]);
        let now_mono = coordinator.clock.monotonic_now();

        coordinator.state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        // Anchor has mismatched ActionKind::BlockInternet
        coordinator.monotonic_timers.insert(
            timer_id,
            MonotonicTimerAnchor {
                timer_id,
                action_kind: ActionKind::BlockInternet,
                utc_deadline: Deadline(UtcDateTime(2000000)),
                monotonic_target: now_mono + Duration::from_secs(60),
                original_duration_seconds: 60,
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: 60,
            },
        );

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::ShutdownComputer,
            Initiator::ParentLocalPin,
        );
        match res {
            Err(ServiceRuntimeError::CancellationForbidden(msg)) => {
                assert!(msg.contains("Monotonic timer anchor kind mismatch"));
            }
            other => panic!("Expected CancellationForbidden, got {:?}", other),
        }
        assert_eq!(coordinator.state.active_actions.len(), 1);
        assert!(coordinator.monotonic_timers.contains_key(&timer_id));
    }

    #[test]
    fn test_persistence_failure_leaves_action_and_anchor_unmodified_without_event() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };
        let timer_id = TimerId([0x77; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime construction must succeed");

        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        // Inject save failure
        store.fail_saves.store(true, Ordering::SeqCst);

        // Cancellation attempt fails with Persistence error
        let res_fail = handle.cancel_internet_block_timer(timer_id, Initiator::ParentLocalPin);
        assert!(matches!(res_fail, Err(ServiceRuntimeError::Persistence(_))));

        // Action is still present in status snapshot
        let snap = handle.query_status().unwrap();
        assert_eq!(snap.active_actions.len(), 1);
        assert_eq!(snap.active_actions[0].id, timer_id);

        // No TimerCancelled event emitted
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { .. } = ev {
                panic!("TimerCancelled event must NOT be emitted on persistence failure");
            }
        }

        // Retry without save failure succeeds
        store.fail_saves.store(false, Ordering::SeqCst);
        let res_ok = handle.cancel_internet_block_timer(timer_id, Initiator::ParentLocalPin);
        assert_eq!(res_ok.unwrap(), TimerCancellationResult::Cancelled);

        // Action is now removed
        let snap2 = handle.query_status().unwrap();
        assert_eq!(snap2.active_actions.len(), 0);

        // Exactly one TimerCancelled event received
        let mut cancel_event_count = 0;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { id, .. } = ev {
                assert_eq!(id, timer_id);
                cancel_event_count += 1;
            }
        }
        assert_eq!(cancel_event_count, 1);

        let _ = runtime.stop();
    }

    #[test]
    fn test_durable_commit_precedes_timer_cancelled_event() {
        let mut coordinator = setup_test_coordinator();

        let (sub_tx, sub_rx) = channel();
        coordinator.handle_subscribe_events(sub_tx);
        let sub = sub_rx.recv().unwrap().unwrap();

        let timer_id = coordinator
            .handle_schedule_action(ActionKind::BlockInternet, 60, Initiator::ParentLocalPin)
            .unwrap();

        // Drain subscription receiver
        while sub.receiver.try_recv().is_ok() {}

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::BlockInternet,
            Initiator::ParentLocalPin,
        );
        assert_eq!(res.unwrap(), TimerCancellationResult::Cancelled);

        // State is already committed and anchor removed
        assert!(coordinator.state.active_actions.is_empty());
        assert!(!coordinator.monotonic_timers.contains_key(&timer_id));

        // TimerCancelled is in the receiver
        let ev = sub.receiver.try_recv().expect("Event must be delivered");
        match ev {
            Event::TimerCancelled { id, action_kind } => {
                assert_eq!(id, timer_id);
                assert_eq!(action_kind, ActionKind::BlockInternet);
            }
            other => panic!("Expected TimerCancelled, got {:?}", other),
        }
    }

    #[test]
    fn test_already_absent_emits_no_timer_cancelled_event() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        let res =
            handle.cancel_internet_block_timer(TimerId([0x99; 16]), Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::AlreadyAbsent);

        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { .. } = ev {
                panic!("No TimerCancelled event must be emitted on AlreadyAbsent");
            }
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_timer_kind_mismatch_emits_no_timer_cancelled_event() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        let timer_id = handle
            .schedule_shutdown(60, Initiator::ParentLocalPin)
            .unwrap();

        // Drain receiver
        while sub.receiver.try_recv().is_ok() {}

        let res = handle.cancel_internet_block_timer(timer_id, Initiator::ParentLocalPin);
        assert_eq!(res.unwrap(), TimerCancellationResult::TimerKindMismatch);

        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { .. } = ev {
                panic!("No TimerCancelled event must be emitted on TimerKindMismatch");
            }
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_cancellation_forbidden_emits_no_timer_cancelled_event() {
        let mut coordinator = setup_test_coordinator();

        let (sub_tx, sub_rx) = channel();
        coordinator.handle_subscribe_events(sub_tx);
        let sub = sub_rx.recv().unwrap().unwrap();

        let timer_id = TimerId([0xAA; 16]);
        coordinator.state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(2000000)),
            created_at: UtcDateTime(1000000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Executing,
        });

        while sub.receiver.try_recv().is_ok() {}

        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::BlockInternet,
            Initiator::ParentLocalPin,
        );
        assert!(matches!(
            res,
            Err(ServiceRuntimeError::CancellationForbidden(_))
        ));

        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerCancelled { .. } = ev {
                panic!("No TimerCancelled event must be emitted on CancellationForbidden");
            }
        }
    }

    #[test]
    fn test_timer_cancelled_delivery_failure_does_not_change_cancelled_result() {
        let mut coordinator = setup_test_coordinator();

        let (sub_tx, sub_rx) = channel();
        coordinator.handle_subscribe_events(sub_tx);
        let sub = sub_rx.recv().unwrap().unwrap();

        let timer_id = coordinator
            .handle_schedule_action(ActionKind::BlockInternet, 60, Initiator::ParentLocalPin)
            .unwrap();

        // Drop subscriber's receiver to simulate disconnected delivery
        drop(sub.receiver);

        // Cancellation must still succeed durably and return Cancelled
        let res = coordinator.handle_cancel_exact_timer(
            timer_id,
            ActionKind::BlockInternet,
            Initiator::ParentLocalPin,
        );
        assert_eq!(res.unwrap(), TimerCancellationResult::Cancelled);

        // Action and anchor are removed
        assert!(coordinator.state.active_actions.is_empty());
        assert!(!coordinator.monotonic_timers.contains_key(&timer_id));
    }

    // ============================================================================
    // SLICE 3C: CHAT RUNTIME SEAMS TESTS
    // ============================================================================

    #[test]
    fn test_send_child_message_empty_text_rejected() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let res = handle.send_child_message("".to_string());
        assert!(matches!(res, Err(ServiceRuntimeError::InvalidInput(_))));

        let snap = handle.query_status().unwrap();
        assert_eq!(snap.active_actions.len(), 0);

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_whitespace_only_text_rejected() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let whitespace_samples = [" ", "\t", "\n", "\r\n", "   \t \n \r  "];
        for sample in whitespace_samples {
            let res = handle.send_child_message(sample.to_string());
            assert!(matches!(res, Err(ServiceRuntimeError::InvalidInput(_))));
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_4096_bytes_accepted() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let text = "a".repeat(4096);
        let res = handle.send_child_message(text);
        assert!(res.is_ok());

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_4097_bytes_rejected() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let text = "a".repeat(4097);
        let res = handle.send_child_message(text);
        assert!(matches!(res, Err(ServiceRuntimeError::InvalidInput(_))));

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_utf8_multibyte_boundary_accepted_and_rejected() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        // "🦀" is 4 UTF-8 bytes. 1024 * 4 = 4096 bytes -> Valid
        let valid_crab = "🦀".repeat(1024);
        assert_eq!(valid_crab.as_bytes().len(), 4096);
        let res_ok = handle.send_child_message(valid_crab);
        assert!(res_ok.is_ok());

        // 1024 crabs + "a" = 4097 bytes -> Invalid
        let invalid_crab = format!("{}a", "🦀".repeat(1024));
        assert_eq!(invalid_crab.as_bytes().len(), 4097);
        let res_err = handle.send_child_message(invalid_crab);
        assert!(matches!(res_err, Err(ServiceRuntimeError::InvalidInput(_))));

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_preserves_leading_trailing_whitespace() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime start must succeed");

        let handle = runtime.handle().clone();
        let raw_text = "  \t Hello from Child! \n  ";
        let res = handle.send_child_message(raw_text.to_string());
        assert!(res.is_ok());

        let persisted = store.state.lock().unwrap();
        assert_eq!(persisted.telegram_outbox.len(), 1);
        match &persisted.telegram_outbox[0].payload {
            TelegramPayload::Chat { message } => {
                assert_eq!(message.text, raw_text);
            }
            other => panic!("Expected TelegramPayload::Chat, got {:?}", other),
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_outbox_entry_id_raw_bytes_mapped_to_message_id() {
        let mut coordinator = setup_test_coordinator();

        let message_id = coordinator
            .handle_send_child_message("Test mapping".to_string())
            .unwrap();

        assert_eq!(coordinator.state.telegram_outbox.len(), 1);
        let entry = &coordinator.state.telegram_outbox[0];
        assert_eq!(message_id, MessageId(entry.entry_id.0));

        match &entry.payload {
            TelegramPayload::Chat { message } => {
                assert_eq!(message.id, MessageId(entry.entry_id.0));
                assert_eq!(message.id, message_id);
            }
            other => panic!("Expected Chat payload, got {:?}", other),
        }
    }

    #[test]
    fn test_send_child_message_uses_service_runtime_clock_utc() {
        let mut coordinator = setup_test_coordinator();
        *coordinator.clock.utc.lock().unwrap() = 1700000000000;

        let _ = coordinator
            .handle_send_child_message("Clock test".to_string())
            .unwrap();

        let entry = &coordinator.state.telegram_outbox[0];
        match &entry.payload {
            TelegramPayload::Chat { message } => {
                assert_eq!(message.timestamp, UtcDateTime(1700000000000));
            }
            other => panic!("Expected Chat payload, got {:?}", other),
        }
    }

    #[test]
    fn test_send_child_message_sets_sender_child_and_accepted_by_service() {
        let mut coordinator = setup_test_coordinator();

        let _ = coordinator
            .handle_send_child_message("Shape test".to_string())
            .unwrap();

        let entry = &coordinator.state.telegram_outbox[0];
        assert_eq!(entry.attempt_count, 0);
        assert_eq!(entry.last_error, None);

        match &entry.payload {
            TelegramPayload::Chat { message } => {
                assert_eq!(message.sender, MessageSender::Child);
                assert_eq!(message.delivery_status, DeliveryStatus::AcceptedByService);
                assert_eq!(message.text, "Shape test");
            }
            other => panic!("Expected Chat payload, got {:?}", other),
        }
    }

    #[test]
    fn test_send_child_message_appends_to_outbox_and_preserves_existing_entries() {
        let mut coordinator = setup_test_coordinator();

        let existing_entry_id = OutboxEntryId([0xEE; 16]);
        coordinator.state.telegram_outbox.push(TelegramOutboxEntry {
            entry_id: existing_entry_id,
            payload: TelegramPayload::ServiceNotification {
                text: "Prior notification".to_string(),
            },
            attempt_count: 2,
            last_error: Some("Temporary network error".to_string()),
        });

        let msg_id = coordinator
            .handle_send_child_message("Appended message".to_string())
            .unwrap();

        assert_eq!(coordinator.state.telegram_outbox.len(), 2);
        assert_eq!(
            coordinator.state.telegram_outbox[0].entry_id,
            existing_entry_id
        );
        assert_eq!(coordinator.state.telegram_outbox[0].attempt_count, 2);

        let second = &coordinator.state.telegram_outbox[1];
        assert_eq!(second.entry_id.0, msg_id.0);
        assert_eq!(second.attempt_count, 0);
    }

    #[test]
    fn test_send_child_message_durable_commit_precedes_reply() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime start must succeed");

        let handle = runtime.handle().clone();
        let res = handle.send_child_message("Durable test".to_string());
        let msg_id = res.expect("Send child message must succeed");

        // When reply is received, persistent store already contains the message
        let persisted = store.state.lock().unwrap();
        assert_eq!(persisted.telegram_outbox.len(), 1);
        assert_eq!(persisted.telegram_outbox[0].entry_id.0, msg_id.0);

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_persistence_failure_leaves_state_unmodified() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime start must succeed");

        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        // Inject save failure
        store.fail_saves.store(true, Ordering::SeqCst);

        let res = handle.send_child_message("Fail persistence".to_string());
        assert!(matches!(res, Err(ServiceRuntimeError::Persistence(_))));

        // Authoritative store state has no outbox entries
        assert!(store.state.lock().unwrap().telegram_outbox.is_empty());

        // No ChatMessageReceived event emitted
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::ChatMessageReceived { .. } = ev {
                panic!("ChatMessageReceived must not be emitted on persistence failure");
            }
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_retry_after_persistence_failure_allocates_next_id() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime start must succeed");

        let handle = runtime.handle().clone();

        // Injected failure
        store.fail_saves.store(true, Ordering::SeqCst);
        let res_fail = handle.send_child_message("Retry message".to_string());
        assert!(matches!(res_fail, Err(ServiceRuntimeError::Persistence(_))));

        // Clear failure and retry
        store.fail_saves.store(false, Ordering::SeqCst);
        let res_ok = handle.send_child_message("Retry message".to_string());
        let msg_id = res_ok.expect("Retry must succeed");

        let persisted = store.state.lock().unwrap();
        assert_eq!(persisted.telegram_outbox.len(), 1);
        assert_eq!(persisted.telegram_outbox[0].entry_id.0, msg_id.0);

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_emits_no_chat_message_received_event() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().unwrap();

        let res = handle.send_child_message("Outbound child message".to_string());
        assert!(res.is_ok());

        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::ChatMessageReceived { .. } = ev {
                panic!("SendChildMessage must NOT emit Event::ChatMessageReceived (no echo)");
            }
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_send_child_message_after_stopping_begins_is_rejected() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let _ = runtime.stop();

        let res = handle.send_child_message("Message after stop".to_string());
        assert!(matches!(res, Err(ServiceRuntimeError::Stopping)));
    }

    #[test]
    fn test_publish_parent_message_with_child_sender_rejected() {
        let mut coordinator = setup_test_coordinator();

        let child_msg = ChatMessage {
            id: MessageId([0x11; 16]),
            sender: MessageSender::Child,
            text: "Spoofed child sender".to_string(),
            timestamp: UtcDateTime(1000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };

        let res = coordinator.handle_publish_parent_message(child_msg);
        assert!(matches!(res, Err(ServiceRuntimeError::InvalidInput(_))));
        assert!(coordinator.state.telegram_outbox.is_empty());
    }

    #[test]
    fn test_publish_parent_message_delivers_exact_message_to_subscriber() {
        let mut coordinator = setup_test_coordinator();

        let (sub_tx, sub_rx) = channel();
        coordinator.handle_subscribe_events(sub_tx);
        let sub = sub_rx.recv().unwrap().unwrap();

        let parent_msg = ChatMessage {
            id: MessageId([0x22; 16]),
            sender: MessageSender::Parent,
            text: "Hello from parent in Telegram!".to_string(),
            timestamp: UtcDateTime(12345678),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };

        let res = coordinator.handle_publish_parent_message(parent_msg.clone());
        assert!(res.is_ok());

        let ev = sub
            .receiver
            .try_recv()
            .expect("Event must be delivered to subscriber");
        match ev {
            Event::ChatMessageReceived { message } => {
                assert_eq!(message, parent_msg);
            }
            other => panic!("Expected ChatMessageReceived, got {:?}", other),
        }
    }

    #[test]
    fn test_publish_parent_message_with_zero_subscribers_succeeds_without_persistence() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        };

        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1000000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(5));

        let bootstrapped = sample_bootstrapped_state(initial_state);
        let mut runtime = ServiceRuntime::start_with_store(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("Runtime start must succeed");

        let handle = runtime.handle().clone();

        let parent_msg = ChatMessage {
            id: MessageId([0x33; 16]),
            sender: MessageSender::Parent,
            text: "Parent msg with zero subscribers".to_string(),
            timestamp: UtcDateTime(1000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };

        let res = handle.publish_parent_message(parent_msg);
        assert!(res.is_ok());

        // Zero saves, zero outbox mutations
        assert_eq!(store.save_count.load(Ordering::SeqCst), 0);
        assert!(store.state.lock().unwrap().telegram_outbox.is_empty());

        let _ = runtime.stop();
    }

    #[test]
    fn test_publish_parent_message_preserves_original_timestamp_and_status() {
        let mut coordinator = setup_test_coordinator();

        let (sub_tx, sub_rx) = channel();
        coordinator.handle_subscribe_events(sub_tx);
        let sub = sub_rx.recv().unwrap().unwrap();

        let parent_msg = ChatMessage {
            id: MessageId([0x44; 16]),
            sender: MessageSender::Parent,
            text: "Timestamp status test".to_string(),
            timestamp: UtcDateTime(9876543210),
            delivery_status: DeliveryStatus::DeliveredToTray,
        };

        let res = coordinator.handle_publish_parent_message(parent_msg.clone());
        assert!(res.is_ok());

        let ev = sub.receiver.try_recv().expect("Event must be delivered");
        match ev {
            Event::ChatMessageReceived { message } => {
                assert_eq!(message.timestamp, UtcDateTime(9876543210));
                assert_eq!(message.delivery_status, DeliveryStatus::DeliveredToTray);
            }
            other => panic!("Expected ChatMessageReceived, got {:?}", other),
        }
    }

    #[test]
    fn test_publish_parent_message_does_not_mutate_state_or_outbox() {
        let mut coordinator = setup_test_coordinator();

        let initial_state_clone = coordinator.state.clone();

        let parent_msg = ChatMessage {
            id: MessageId([0x55; 16]),
            sender: MessageSender::Parent,
            text: "No mutation test".to_string(),
            timestamp: UtcDateTime(1000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };

        let res = coordinator.handle_publish_parent_message(parent_msg);
        assert!(res.is_ok());

        assert_eq!(coordinator.state, initial_state_clone);
    }

    #[test]
    fn test_publish_parent_message_offline_is_never_replayed_to_future_subscribers() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let msg_p1 = ChatMessage {
            id: MessageId([0x61; 16]),
            sender: MessageSender::Parent,
            text: "P1: Sent while offline".to_string(),
            timestamp: UtcDateTime(1000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };

        // Publish P1 with 0 subscribers
        let res1 = handle.publish_parent_message(msg_p1);
        assert!(res1.is_ok());

        // Later, subscriber connects
        let sub = handle.subscribe_events().unwrap();

        // Verify sub receives no historical P1
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::ChatMessageReceived { message } = ev {
                if message.id == MessageId([0x61; 16]) {
                    panic!("Historical P1 message must NEVER be replayed to new subscribers");
                }
            }
        }

        // Publish P2 after subscription
        let msg_p2 = ChatMessage {
            id: MessageId([0x62; 16]),
            sender: MessageSender::Parent,
            text: "P2: Sent while online".to_string(),
            timestamp: UtcDateTime(2000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };
        let res2 = handle.publish_parent_message(msg_p2.clone());
        assert!(res2.is_ok());

        // Receiver gets exactly P2
        let ev = sub.receiver.try_recv().expect("P2 must be delivered");
        match ev {
            Event::ChatMessageReceived { message } => {
                assert_eq!(message, msg_p2);
            }
            other => panic!("Expected ChatMessageReceived(P2), got {:?}", other),
        }

        let _ = runtime.stop();
    }

    #[test]
    fn test_publish_parent_message_nonblocking_with_disconnected_subscriber() {
        let mut coordinator = setup_test_coordinator();

        let (sub_tx, sub_rx) = channel();
        coordinator.handle_subscribe_events(sub_tx);
        let sub = sub_rx.recv().unwrap().unwrap();

        // Disconnect subscriber
        drop(sub.receiver);

        let parent_msg = ChatMessage {
            id: MessageId([0x77; 16]),
            sender: MessageSender::Parent,
            text: "Disconnected sub message".to_string(),
            timestamp: UtcDateTime(1000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };

        let res = coordinator.handle_publish_parent_message(parent_msg);
        assert!(res.is_ok());

        // Disconnected subscriber is pruned
        assert_eq!(coordinator.subscribers.len(), 0);
    }

    #[test]
    fn test_publish_parent_message_nonblocking_with_full_subscriber_queue() {
        let mut coordinator = setup_test_coordinator();

        let (sub_tx, sub_rx) = channel();
        coordinator.handle_subscribe_events(sub_tx);
        let sub = sub_rx.recv().unwrap().unwrap();

        // Fill 64-event queue
        for i in 0..64 {
            let msg = ChatMessage {
                id: MessageId([i as u8; 16]),
                sender: MessageSender::Parent,
                text: format!("Queue fill {i}"),
                timestamp: UtcDateTime(1000000 + i as i64),
                delivery_status: DeliveryStatus::AcceptedByTelegram,
            };
            let res = coordinator.handle_publish_parent_message(msg);
            assert!(res.is_ok());
        }

        // 65th event causes overflow pruning without blocking
        let overflow_msg = ChatMessage {
            id: MessageId([0xFF; 16]),
            sender: MessageSender::Parent,
            text: "Overflow message".to_string(),
            timestamp: UtcDateTime(2000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };
        let res_overflow = coordinator.handle_publish_parent_message(overflow_msg);
        assert!(res_overflow.is_ok());

        // Full subscriber was pruned
        assert_eq!(coordinator.subscribers.len(), 0);

        drop(sub.receiver);
    }

    #[test]
    fn test_publish_parent_message_after_stopping_begins_is_rejected() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let _ = runtime.stop();

        let parent_msg = ChatMessage {
            id: MessageId([0x88; 16]),
            sender: MessageSender::Parent,
            text: "Stopping parent message".to_string(),
            timestamp: UtcDateTime(1000000),
            delivery_status: DeliveryStatus::AcceptedByTelegram,
        };

        let res = handle.publish_parent_message(parent_msg);
        assert!(matches!(res, Err(ServiceRuntimeError::Stopping)));
    }

    // ========================================================================
    // SLICE 3D: RUNTIME EVENT EMISSION COMPLETION TESTS (54 tests)
    // ========================================================================

    fn create_test_coordinator_custom(
        bootstrapped: BootstrappedServiceState,
        store: FakeStateStore,
        gate: FakeInternetGate,
        power: FakePowerController,
        clock: FakeClock,
        id_source: FakeIdSource,
        retry: TestRetryPolicy,
        log: Option<Arc<Mutex<Vec<String>>>>,
    ) -> Result<
        (
            ServiceRuntimeCoordinator<
                FakeStateStore,
                FakeInternetGate,
                FakePowerController,
                FakeClock,
                FakeIdSource,
                TestRetryPolicy,
            >,
            StartupReadiness,
        ),
        ServiceRuntimeError,
    > {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let platform_effect_gate = Arc::new(Mutex::new(()));
        ServiceRuntimeCoordinator::new(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            log,
            stop_requested,
            platform_effect_gate,
            None,
        )
    }

    fn subscribe_to_coordinator(
        coordinator: &mut ServiceRuntimeCoordinator<
            FakeStateStore,
            FakeInternetGate,
            FakePowerController,
            FakeClock,
            FakeIdSource,
            TestRetryPolicy,
        >,
    ) -> EventSubscription {
        let (tx, rx) = channel();
        coordinator.handle_subscribe_events(tx);
        rx.recv().unwrap().unwrap()
    }

    fn query_coordinator_status(
        coordinator: &ServiceRuntimeCoordinator<
            FakeStateStore,
            FakeInternetGate,
            FakePowerController,
            FakeClock,
            FakeIdSource,
            TestRetryPolicy,
        >,
    ) -> StatusSnapshot {
        let (tx, rx) = channel();
        coordinator.handle_query_status(tx);
        rx.recv().unwrap()
    }

    // 1. Schedule Internet emits TimerScheduled after durable save
    #[test]
    fn test_schedule_internet_emits_timer_scheduled_after_durable_save() {
        let (mut runtime, _clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let timer_id = handle
            .schedule_internet_block(700, Initiator::ParentLocalPin)
            .expect("schedule ok");

        let ev = sub.receiver.try_recv().expect("TimerScheduled event");
        match ev {
            Event::TimerScheduled { action } => {
                assert_eq!(action.id, timer_id);
                assert_eq!(action.action_kind, ActionKind::BlockInternet);
                assert_eq!(action.created_by, Initiator::ParentLocalPin);
                assert_eq!(action.execution_state, ActionExecutionState::Pending);
            }
            other => panic!("Expected TimerScheduled, got {:?}", other),
        }

        let logs = log.lock().unwrap().clone();
        let _save_idx = logs
            .iter()
            .position(|l| l.contains("save:"))
            .expect("must save");
        let _ = runtime.stop();
    }

    // 2. Schedule Shutdown emits TimerScheduled and ShutdownStateChanged
    #[test]
    fn test_schedule_shutdown_emits_timer_scheduled_and_shutdown_state_changed() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let timer_id = handle
            .schedule_shutdown(700, Initiator::ParentLocalPin)
            .expect("schedule ok");

        let ev1 = sub.receiver.try_recv().expect("first event");
        match ev1 {
            Event::TimerScheduled { action } => {
                assert_eq!(action.id, timer_id);
                assert_eq!(action.action_kind, ActionKind::ShutdownComputer);
            }
            other => panic!("Expected TimerScheduled, got {:?}", other),
        }

        let ev2 = sub.receiver.try_recv().expect("second event");
        match ev2 {
            Event::ShutdownStateChanged { previous, current } => {
                assert_eq!(previous, ShutdownState::Idle);
                assert_eq!(current, ShutdownState::Scheduled);
            }
            other => panic!("Expected ShutdownStateChanged, got {:?}", other),
        }
        let _ = runtime.stop();
    }

    // 3. Schedule action persistence failure emits no TimerScheduled
    #[test]
    fn test_schedule_action_persistence_failure_emits_no_timer_scheduled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        store.fail_saves.store(true, Ordering::SeqCst);
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator creation ok");

        let sub = subscribe_to_coordinator(&mut coordinator);

        let res = coordinator.handle_schedule_action(
            ActionKind::BlockInternet,
            700,
            Initiator::ParentLocalPin,
        );
        assert!(res.is_err());

        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(
                ev,
                Event::TimerScheduled { .. } | Event::WarningThresholdReached { .. }
            ) {
                panic!("No schedule event should be emitted on save failure");
            }
        }
    }

    // 4. Schedule action subscriber full does not fail schedule
    #[test]
    fn test_schedule_action_subscriber_full_does_not_fail_schedule() {
        let mut coordinator = setup_test_coordinator();
        let sub = subscribe_to_coordinator(&mut coordinator);

        if let Some(tx) = coordinator.subscribers.get(&sub.subscription_id) {
            for _ in 0..64 {
                let _ = tx.try_send(Event::ServiceHealthUpdated {
                    health: coordinator.build_service_health_snapshot(),
                });
            }
        }

        let res = coordinator.handle_schedule_action(
            ActionKind::BlockInternet,
            700,
            Initiator::ParentLocalPin,
        );
        assert!(res.is_ok());
        assert_eq!(coordinator.subscribers.len(), 0);
    }

    // 5. Schedule with creation due warning emits WarningThresholdReached
    #[test]
    fn test_schedule_with_creation_due_warning_emits_warning_threshold_reached() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let timer_id = handle
            .schedule_internet_block(180, Initiator::ParentLocalPin)
            .expect("schedule ok");

        let ev1 = sub.receiver.try_recv().expect("TimerScheduled");
        assert!(matches!(ev1, Event::TimerScheduled { .. }));

        let ev2 = sub.receiver.try_recv().expect("WarningThresholdReached");
        match ev2 {
            Event::WarningThresholdReached { event } => {
                assert_eq!(event.timer_id, timer_id);
                assert_eq!(event.threshold, WarningThreshold::M3);
            }
            other => panic!("Expected WarningThresholdReached, got {:?}", other),
        }
        let _ = runtime.stop();
    }

    // 6. Schedule with creation passed warning does not emit warning event
    #[test]
    fn test_schedule_with_creation_passed_warning_does_not_emit_warning_event() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let timer_id = handle
            .schedule_internet_block(200, Initiator::ParentLocalPin)
            .expect("schedule ok");

        let ev1 = sub.receiver.try_recv().expect("TimerScheduled");
        assert!(matches!(ev1, Event::TimerScheduled { .. }));

        let mut warning_thresholds = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::WarningThresholdReached { event } = ev {
                assert_eq!(event.timer_id, timer_id);
                warning_thresholds.push(event.threshold);
            }
        }
        assert!(
            warning_thresholds.is_empty(),
            "No creation due warning for 200s timer"
        );
        let _ = runtime.stop();
    }

    // 7. Runtime warning emitted after durable state save
    #[test]
    fn test_runtime_warning_emitted_after_durable_state_save() {
        let (mut runtime, clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let timer_id = handle
            .schedule_internet_block(200, Initiator::ParentLocalPin)
            .expect("schedule ok");

        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(30));
        runtime.handle().tick().unwrap();

        let mut received = false;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::WarningThresholdReached { event } = ev {
                if event.timer_id == timer_id && event.threshold == WarningThreshold::M3 {
                    received = true;
                }
            }
        }
        assert!(received);

        let logs = log.lock().unwrap().clone();
        assert!(logs.iter().any(|l| l.contains("save:runtime_warning")));
        let _ = runtime.stop();
    }

    // 8. Runtime warning persistence failure emits no warning and does not advance cursor
    #[test]
    fn test_runtime_warning_persistence_failure_emits_no_warning_and_does_not_advance_cursor() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator creation ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let timer_id = coordinator
            .handle_schedule_action(ActionKind::BlockInternet, 200, Initiator::ParentLocalPin)
            .expect("schedule ok");

        while sub.receiver.try_recv().is_ok() {}

        store.fail_saves.store(true, Ordering::SeqCst);

        clock.advance(Duration::from_secs(30));
        coordinator.process_clock_and_events();

        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::WarningThresholdReached { .. }) {
                panic!("No warning event must be emitted on save failure");
            }
        }

        let anchor = coordinator.monotonic_timers.get(&timer_id).unwrap();
        assert_eq!(anchor.last_evaluated_remaining_seconds, 200);
    }

    // 9. Runtime warning no duplicate events for already emitted threshold
    #[test]
    fn test_runtime_warning_no_duplicate_events_for_already_emitted_threshold() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let timer_id = handle
            .schedule_internet_block(200, Initiator::ParentLocalPin)
            .expect("schedule ok");

        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(30));
        runtime.handle().tick().unwrap();

        let mut count_m3 = 0;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::WarningThresholdReached { event } = ev {
                if event.timer_id == timer_id && event.threshold == WarningThreshold::M3 {
                    count_m3 += 1;
                }
            }
        }
        assert_eq!(count_m3, 1);

        clock.advance(Duration::from_secs(10));
        runtime.handle().tick().unwrap();

        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::WarningThresholdReached { event } = ev {
                if event.timer_id == timer_id && event.threshold == WarningThreshold::M3 {
                    panic!("Duplicate warning threshold reached event!");
                }
            }
        }
        let _ = runtime.stop();
    }

    // 10. Runtime warning multiple thresholds emitted in deterministic order
    #[test]
    fn test_runtime_warning_multiple_thresholds_emitted_in_deterministic_order() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let timer_id = handle
            .schedule_internet_block(4000, Initiator::ParentLocalPin)
            .expect("schedule ok");

        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(2500));
        runtime.handle().tick().unwrap();

        let mut thresholds = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::WarningThresholdReached { event } = ev {
                if event.timer_id == timer_id {
                    thresholds.push(event.threshold);
                }
            }
        }

        assert_eq!(
            thresholds,
            vec![WarningThreshold::M60, WarningThreshold::M30]
        );
        let _ = runtime.stop();
    }

    // 11. Internet deadline emits TimerExpired before gate block
    #[test]
    fn test_internet_deadline_emits_timer_expired_before_gate_block() {
        let (mut runtime, clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe");

        let timer_id = handle
            .schedule_internet_block(10, Initiator::ParentLocalPin)
            .expect("schedule ok");

        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(15));
        runtime.handle().tick().unwrap();

        let mut timer_expired_seen = false;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerExpired { id, action_kind } = ev {
                assert_eq!(id, timer_id);
                assert_eq!(action_kind, ActionKind::BlockInternet);
                timer_expired_seen = true;
            }
        }
        assert!(timer_expired_seen);

        let logs = log.lock().unwrap();
        assert!(logs.contains(&"gate:block_internet".to_string()));
        let _ = runtime.stop();
    }

    // 12. Shutdown deadline emits TimerExpired before power initiate
    #[test]
    fn test_shutdown_deadline_emits_timer_expired_before_power_initiate() {
        let (mut runtime, clock, log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe");

        let timer_id = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("schedule ok");

        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(15));
        runtime.handle().tick().unwrap();

        let mut timer_expired_seen = false;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::TimerExpired { id, action_kind } = ev {
                assert_eq!(id, timer_id);
                assert_eq!(action_kind, ActionKind::ShutdownComputer);
                timer_expired_seen = true;
            }
        }
        assert!(timer_expired_seen);

        let logs = log.lock().unwrap();
        assert!(logs.contains(&"power:initiate_shutdown".to_string()));
        let _ = runtime.stop();
    }

    // 13. Deadline Executing persistence failure emits no TimerExpired
    #[test]
    fn test_deadline_executing_persistence_failure_emits_no_timer_expired() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator creation ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let _timer_id = coordinator
            .handle_schedule_action(ActionKind::BlockInternet, 10, Initiator::ParentLocalPin)
            .expect("schedule ok");

        while sub.receiver.try_recv().is_ok() {}

        store.fail_saves.store(true, Ordering::SeqCst);

        clock.advance(Duration::from_secs(15));
        coordinator.process_clock_and_events();

        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::TimerExpired { .. }) {
                panic!("TimerExpired must not be emitted on Executing save failure");
            }
        }
    }

    // 14. Deadline event delivery failure does not suppress side effect
    #[test]
    fn test_deadline_event_delivery_failure_does_not_suppress_side_effect() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator creation ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let _timer_id = coordinator
            .handle_schedule_action(ActionKind::BlockInternet, 10, Initiator::ParentLocalPin)
            .expect("schedule ok");

        drop(sub.receiver);

        clock.advance(Duration::from_secs(15));
        coordinator.process_clock_and_events();

        let logs = log.lock().unwrap();
        assert!(logs.contains(&"gate:block_internet".to_string()));
    }

    // 15. Startup recovery does not emit retroactive TimerExpired
    #[test]
    fn test_startup_recovery_does_not_emit_retroactive_timer_expired() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([0x11; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(500_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator creation ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.process_clock_and_events();

        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(
                ev,
                Event::TimerExpired { .. } | Event::WarningThresholdReached { .. }
            ) {
                panic!(
                    "No retroactive TimerExpired or Warning event during/after startup recovery"
                );
            }
        }
    }

    // 16. Startup recovery pending shutdown missed emits MissedDeadlineOccurred
    #[test]
    fn test_startup_recovery_pending_shutdown_missed_emits_missed_deadline_occurred() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        let timer_id = TimerId([0x22; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(500_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (_coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let logs = log.lock().unwrap().clone();
        let missed_event = logs
            .iter()
            .find(|l| l.contains("event:MissedDeadlineOccurred"))
            .expect("Must emit MissedDeadlineOccurred");
        assert!(missed_event.contains("Scheduled shutdown was missed while service was offline"));
    }

    // 17. Startup recovery executing shutdown missed emits MissedDeadlineOccurred
    #[test]
    fn test_startup_recovery_executing_shutdown_missed_emits_missed_deadline_occurred() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        let timer_id = TimerId([0x23; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(500_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Executing,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (_coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let logs = log.lock().unwrap().clone();
        assert!(
            logs.iter()
                .any(|l| l.contains("event:MissedDeadlineOccurred"))
        );
    }

    // 18. Startup recovery failed shutdown missed emits MissedDeadlineOccurred
    #[test]
    fn test_startup_recovery_failed_shutdown_missed_emits_missed_deadline_occurred() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        let timer_id = TimerId([0x24; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(500_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Failed {
                reason: "prior failure".to_string(),
            },
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (_coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let logs = log.lock().unwrap().clone();
        assert!(
            logs.iter()
                .any(|l| l.contains("event:MissedDeadlineOccurred"))
        );
    }

    // 19. Startup recovery missed deadline preserves action snapshot and safe reason
    #[test]
    fn test_startup_recovery_missed_deadline_preserves_action_snapshot_and_safe_reason() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        let timer_id = TimerId([0x25; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(500_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (_coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let logs = log.lock().unwrap().clone();
        let missed_event = logs
            .iter()
            .find(|l| l.contains("event:MissedDeadlineOccurred"))
            .expect("Must emit MissedDeadlineOccurred");
        assert!(missed_event.contains("Scheduled shutdown was missed while service was offline"));
        assert!(missed_event.contains("ShutdownComputer"));
        assert!(missed_event.contains("Missed"));
    }

    // 20. Startup recovery multiple missed shutdowns emitted in deterministic order
    #[test]
    fn test_startup_recovery_multiple_missed_shutdowns_emitted_in_deterministic_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        let timer1 = TimerId([0x26; 16]);
        let timer2 = TimerId([0x27; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer1,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(500_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        initial_state.active_actions.push(ScheduledAction {
            id: timer2,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(600_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentTelegram { user_id: 42 },
            execution_state: ActionExecutionState::Executing,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (_coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let logs = log.lock().unwrap().clone();
        let missed_events: Vec<_> = logs
            .iter()
            .filter(|l| l.contains("event:MissedDeadlineOccurred"))
            .collect();
        assert_eq!(missed_events.len(), 2);
    }

    // 21. Startup recovery persistence failure emits no missed deadline event
    #[test]
    fn test_startup_recovery_persistence_failure_emits_no_missed_deadline_event() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([0x28; 16]),
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(500_000)),
            created_at: UtcDateTime(400_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        store.fail_saves.store(true, Ordering::SeqCst);
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let res = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        );
        assert!(res.is_err());
    }

    // 22. Shutdown aggregate startup future shutdown derives Scheduled
    #[test]
    fn test_shutdown_aggregate_startup_future_shutdown_derives_scheduled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([0x29; 16]),
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2_000_000)),
            created_at: UtcDateTime(1_000_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        assert_eq!(coordinator.shutdown_state, ShutdownState::Scheduled);
        let snap = query_coordinator_status(&coordinator);
        assert_eq!(snap.shutdown_state, ShutdownState::Scheduled);
    }

    // 23. Shutdown aggregate first schedule emits Idle to Scheduled
    #[test]
    fn test_shutdown_aggregate_first_schedule_emits_idle_to_scheduled() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let _ = handle
            .schedule_shutdown(700, Initiator::ParentLocalPin)
            .expect("schedule ok");

        let _timer_ev = sub.receiver.try_recv().expect("TimerScheduled");
        let state_ev = sub.receiver.try_recv().expect("ShutdownStateChanged");
        match state_ev {
            Event::ShutdownStateChanged { previous, current } => {
                assert_eq!(previous, ShutdownState::Idle);
                assert_eq!(current, ShutdownState::Scheduled);
            }
            other => panic!("Expected ShutdownStateChanged, got {:?}", other),
        }
        let _ = runtime.stop();
    }

    // 24. Shutdown aggregate second schedule emits no duplicate ShutdownStateChanged
    #[test]
    fn test_shutdown_aggregate_second_schedule_emits_no_duplicate_shutdown_state_changed() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let _ = handle
            .schedule_shutdown(700, Initiator::ParentLocalPin)
            .expect("first schedule");
        while sub.receiver.try_recv().is_ok() {}

        let _ = handle
            .schedule_shutdown(1200, Initiator::ParentLocalPin)
            .expect("second schedule");

        let mut state_changed = false;
        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::ShutdownStateChanged { .. }) {
                state_changed = true;
            }
        }
        assert!(
            !state_changed,
            "Must not emit duplicate ShutdownStateChanged"
        );
        let _ = runtime.stop();
    }

    // 25. Shutdown aggregate cancel one when another remains stays Scheduled
    #[test]
    fn test_shutdown_aggregate_cancel_one_when_another_remains_stays_scheduled() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let id1 = handle
            .schedule_shutdown(700, Initiator::ParentLocalPin)
            .expect("sched 1");
        let _id2 = handle
            .schedule_shutdown(1200, Initiator::ParentLocalPin)
            .expect("sched 2");
        while sub.receiver.try_recv().is_ok() {}

        let res = handle
            .cancel_shutdown_timer(id1, Initiator::ParentLocalPin)
            .expect("cancel ok");
        assert_eq!(res, TimerCancellationResult::Cancelled);

        let mut state_changed = false;
        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::ShutdownStateChanged { .. }) {
                state_changed = true;
            }
        }
        assert!(
            !state_changed,
            "Must stay Scheduled, no state changed event"
        );
        let snap = handle.query_status().expect("status ok");
        assert_eq!(snap.shutdown_state, ShutdownState::Scheduled);
        let _ = runtime.stop();
    }

    // 26. Shutdown aggregate cancel last emits Scheduled to Idle
    #[test]
    fn test_shutdown_aggregate_cancel_last_emits_scheduled_to_idle() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let id1 = handle
            .schedule_shutdown(700, Initiator::ParentLocalPin)
            .expect("sched 1");
        while sub.receiver.try_recv().is_ok() {}

        let res = handle
            .cancel_shutdown_timer(id1, Initiator::ParentLocalPin)
            .expect("cancel ok");
        assert_eq!(res, TimerCancellationResult::Cancelled);

        let ev1 = sub.receiver.try_recv().expect("TimerCancelled");
        assert!(matches!(ev1, Event::TimerCancelled { .. }));

        let ev2 = sub.receiver.try_recv().expect("ShutdownStateChanged");
        match ev2 {
            Event::ShutdownStateChanged { previous, current } => {
                assert_eq!(previous, ShutdownState::Scheduled);
                assert_eq!(current, ShutdownState::Idle);
            }
            other => panic!("Expected ShutdownStateChanged, got {:?}", other),
        }
        let snap = handle.query_status().expect("status ok");
        assert_eq!(snap.shutdown_state, ShutdownState::Idle);
        let _ = runtime.stop();
    }

    // 27. Shutdown aggregate deadline Pending to Executing emits no aggregate event
    #[test]
    fn test_shutdown_aggregate_deadline_pending_to_executing_emits_no_aggregate_event() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let _ = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("sched");
        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(15));
        runtime.handle().tick().unwrap();

        let mut events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            events.push(ev);
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TimerExpired { .. }))
        );
        assert!(!events.iter().any(|e| matches!(
            e,
            Event::ShutdownStateChanged {
                previous: ShutdownState::Scheduled,
                current: ShutdownState::Scheduled
            }
        )));
        let _ = runtime.stop();
    }

    // 28. Shutdown aggregate power success emits Scheduled to InProgress
    #[test]
    fn test_shutdown_aggregate_power_success_emits_scheduled_to_inprogress() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        let _ = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("sched");
        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(15));
        runtime.handle().tick().unwrap();

        let mut state_changed = None;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::ShutdownStateChanged { previous, current } = ev {
                state_changed = Some((previous, current));
            }
        }
        assert_eq!(
            state_changed,
            Some((ShutdownState::Scheduled, ShutdownState::InProgress))
        );
        let snap = handle.query_status().expect("status ok");
        assert_eq!(snap.shutdown_state, ShutdownState::InProgress);
        let _ = runtime.stop();
    }

    // 29. Shutdown aggregate completed removal preserves InProgress sticky
    #[test]
    fn test_shutdown_aggregate_completed_removal_preserves_inprogress_sticky() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let _ = handle
            .schedule_shutdown(10, Initiator::ParentLocalPin)
            .expect("sched");

        clock.advance(Duration::from_secs(15));
        runtime.handle().tick().unwrap();

        let snap = handle.query_status().expect("status ok");
        assert_eq!(snap.shutdown_state, ShutdownState::InProgress);
        assert_eq!(snap.active_actions.len(), 0);

        clock.advance(Duration::from_secs(10));
        runtime.handle().tick().unwrap();

        let snap2 = handle.query_status().expect("status ok");
        assert_eq!(snap2.shutdown_state, ShutdownState::InProgress);
        let _ = runtime.stop();
    }

    // 30. Shutdown aggregate power failure emits Scheduled to Idle only after persistence
    #[test]
    fn test_shutdown_aggregate_power_failure_emits_scheduled_to_idle_only_after_persistence() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        power.fail_shutdown.store(true, Ordering::SeqCst);
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let _ = coordinator
            .handle_schedule_action(ActionKind::ShutdownComputer, 10, Initiator::ParentLocalPin)
            .expect("sched");
        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(15));
        coordinator.process_clock_and_events();

        let mut events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            events.push(ev);
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TimerExpired { .. }))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ShutdownStateChanged {
                previous: ShutdownState::Scheduled,
                current: ShutdownState::Idle
            }
        )));
        assert_eq!(coordinator.shutdown_state, ShutdownState::Idle);
    }

    // 31. Shutdown aggregate power failure with other pending shutdown stays Scheduled
    #[test]
    fn test_shutdown_aggregate_power_failure_with_other_pending_shutdown_stays_scheduled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        power.fail_shutdown.store(true, Ordering::SeqCst);
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let _ = coordinator
            .handle_schedule_action(ActionKind::ShutdownComputer, 10, Initiator::ParentLocalPin)
            .expect("sched 1");
        let _ = coordinator
            .handle_schedule_action(ActionKind::ShutdownComputer, 100, Initiator::ParentLocalPin)
            .expect("sched 2");
        while sub.receiver.try_recv().is_ok() {}

        clock.advance(Duration::from_secs(15));
        coordinator.process_clock_and_events();

        let mut state_changed = false;
        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::ShutdownStateChanged { .. }) {
                state_changed = true;
            }
        }
        assert!(!state_changed, "Must stay Scheduled, no state event");
        assert_eq!(coordinator.shutdown_state, ShutdownState::Scheduled);
    }

    // 32. Internet policy immediate block emits InternetPolicyChanged with ImmediateCommand reason
    #[test]
    fn test_internet_policy_immediate_block_emits_internet_policy_changed_with_immediate_command_reason()
     {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        handle
            .immediate_internet_block(Initiator::ParentLocalPin)
            .expect("block ok");

        let ev = sub.receiver.try_recv().expect("InternetPolicyChanged");
        match ev {
            Event::InternetPolicyChanged {
                desired,
                observed,
                reason,
            } => {
                assert_eq!(desired, DesiredInternetState::Blocked);
                assert_eq!(observed, InternetState::Blocked);
                assert_eq!(
                    reason,
                    StateChangeReason::ImmediateCommand {
                        initiator: Initiator::ParentLocalPin
                    }
                );
            }
            other => panic!("Expected InternetPolicyChanged, got {:?}", other),
        }
        let _ = runtime.stop();
    }

    // 33. Internet policy immediate block platform failure emits mismatch event
    #[test]
    fn test_internet_policy_immediate_block_platform_failure_emits_mismatch_event() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        gate.fail_calls.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let res = coordinator.handle_immediate_block(Initiator::ParentLocalPin);
        assert!(res.is_err());

        let ev = sub
            .receiver
            .try_recv()
            .expect("InternetPolicyChanged mismatch event");
        match ev {
            Event::InternetPolicyChanged {
                desired,
                observed,
                reason,
            } => {
                assert_eq!(desired, DesiredInternetState::Blocked);
                assert_eq!(observed, InternetState::Unknown);
                assert_eq!(
                    reason,
                    StateChangeReason::ImmediateCommand {
                        initiator: Initiator::ParentLocalPin
                    }
                );
            }
            other => panic!("Expected InternetPolicyChanged, got {:?}", other),
        }
    }

    // 34. Internet policy manual restore emits InternetPolicyChanged with ManualRestore reason
    #[test]
    fn test_internet_policy_manual_restore_emits_internet_policy_changed_with_manual_restore_reason()
     {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        handle
            .immediate_internet_block(Initiator::ParentLocalPin)
            .expect("block ok");

        let sub = handle.subscribe_events().expect("subscribe ok");
        handle
            .restore_internet(Initiator::ParentLocalPin)
            .expect("restore ok");

        let ev = sub.receiver.try_recv().expect("InternetPolicyChanged");
        match ev {
            Event::InternetPolicyChanged {
                desired,
                observed,
                reason,
            } => {
                assert_eq!(desired, DesiredInternetState::Unrestricted);
                assert_eq!(observed, InternetState::Unrestricted);
                assert_eq!(
                    reason,
                    StateChangeReason::ManualRestore {
                        initiator: Initiator::ParentLocalPin
                    }
                );
            }
            other => panic!("Expected InternetPolicyChanged, got {:?}", other),
        }
        let _ = runtime.stop();
    }

    // 35. Internet policy manual restore platform failure emits mismatch event
    #[test]
    fn test_internet_policy_manual_restore_platform_failure_emits_mismatch_event() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.desired_internet_state = DesiredInternetState::Blocked;
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        gate.fail_calls.store(true, Ordering::SeqCst);
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let res = coordinator.handle_restore_internet(Initiator::ParentLocalPin);
        assert!(res.is_err());

        let ev = sub.receiver.try_recv().expect("mismatch event");
        match ev {
            Event::InternetPolicyChanged {
                desired,
                observed,
                reason,
            } => {
                assert_eq!(desired, DesiredInternetState::Unrestricted);
                assert_eq!(observed, InternetState::Unknown);
                assert_eq!(
                    reason,
                    StateChangeReason::ManualRestore {
                        initiator: Initiator::ParentLocalPin
                    }
                );
            }
            other => panic!("Expected InternetPolicyChanged, got {:?}", other),
        }
    }

    // 36. Internet policy scheduled block emits InternetPolicyChanged with TimerExpired reason
    #[test]
    fn test_internet_policy_scheduled_block_emits_internet_policy_changed_with_timer_expired_reason()
     {
        let log = Arc::new(Mutex::new(Vec::new()));
        let timer_id = TimerId([36; 16]);
        let mut initial_state = sample_initial_state();
        initial_state.desired_internet_state = DesiredInternetState::Unrestricted;
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(1_300_000)),
            created_at: UtcDateTime(1_000_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate.clone(),
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        // Establish pre-deadline state: desired is Unrestricted, observed is Blocked
        coordinator.observed_internet_state = InternetState::Blocked;
        *gate.current.lock().unwrap() = InternetState::Blocked;

        // Explicit pre-deadline assertions: desired is Unrestricted, observed is Blocked
        assert_eq!(
            coordinator.state.desired_internet_state,
            DesiredInternetState::Unrestricted
        );
        assert_eq!(coordinator.observed_internet_state, InternetState::Blocked);
        let pre_act = coordinator
            .state
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .expect("target action present before deadline");
        assert_eq!(pre_act.action_kind, ActionKind::BlockInternet);
        assert_eq!(pre_act.execution_state, ActionExecutionState::Pending);

        let now_mono = clock.monotonic_now();
        coordinator.monotonic_timers.insert(
            timer_id,
            MonotonicTimerAnchor {
                timer_id,
                action_kind: ActionKind::BlockInternet,
                utc_deadline: Deadline(UtcDateTime(1_300_000)),
                monotonic_target: now_mono,
                original_duration_seconds: 300,
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: 300,
            },
        );

        let sub = subscribe_to_coordinator(&mut coordinator);
        while sub.receiver.try_recv().is_ok() {}

        // Execute the exact scheduled BlockInternet deadline
        coordinator.execute_scheduled_internet_deadline(timer_id);

        let mut policy_events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::InternetPolicyChanged {
                desired,
                observed,
                reason,
            } = ev
            {
                policy_events.push((desired, observed, reason));
            }
        }

        // Event assertions: exactly 1 event with matching payload
        assert_eq!(
            policy_events.len(),
            1,
            "Must emit exactly one InternetPolicyChanged for desired-only pair change"
        );
        assert_eq!(
            policy_events[0],
            (
                DesiredInternetState::Blocked,
                InternetState::Blocked,
                StateChangeReason::TimerExpired { timer_id }
            )
        );

        // Post-deadline state assertions: desired is Blocked, observed is Blocked
        assert_eq!(
            coordinator.state.desired_internet_state,
            DesiredInternetState::Blocked
        );
        assert_eq!(coordinator.observed_internet_state, InternetState::Blocked);
    }

    // 37. Internet policy startup restoration emits event on initial state change
    #[test]
    fn test_internet_policy_startup_restoration_emits_event_on_initial_state_change() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.desired_internet_state = DesiredInternetState::Blocked;
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (_coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let logs = log.lock().unwrap().clone();
        let policy_event = logs
            .iter()
            .find(|l| l.contains("event:InternetPolicyChanged"))
            .expect("Must emit InternetPolicyChanged");
        assert!(policy_event.contains("StartupRestoration"));
    }

    // 38. Internet policy platform sync emits event on observed change
    #[test]
    fn test_internet_policy_platform_sync_emits_event_on_observed_change() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.desired_internet_state = DesiredInternetState::Blocked;
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate.clone(),
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator.observed_internet_state = InternetState::Unrestricted;
        *gate.current.lock().unwrap() = InternetState::Blocked;

        coordinator.process_internet_reconciliation_retry();

        let mut sync_event = None;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::InternetPolicyChanged {
                desired,
                observed,
                reason,
            } = ev
            {
                sync_event = Some((desired, observed, reason));
            }
        }

        assert_eq!(
            sync_event,
            Some((
                DesiredInternetState::Blocked,
                InternetState::Blocked,
                StateChangeReason::PlatformSync
            ))
        );
    }

    // 39. Internet policy no change emits no duplicate event
    #[test]
    fn test_internet_policy_no_change_emits_no_duplicate_event() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        handle
            .restore_internet(Initiator::ParentLocalPin)
            .expect("restore ok");

        let mut policy_count = 0;
        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::InternetPolicyChanged { .. }) {
                policy_count += 1;
            }
        }
        assert_eq!(
            policy_count, 0,
            "No event when desired and observed were already unrestricted"
        );
        let _ = runtime.stop();
    }

    // 40. Health persistence failure emits ServiceHealthUpdated
    #[test]
    fn test_health_persistence_failure_emits_service_health_updated() {
        let mut coordinator = setup_test_coordinator();
        let sub = subscribe_to_coordinator(&mut coordinator);

        let err = StateStoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
        coordinator.mark_persistence_failure(&err);

        let ev = sub.receiver.try_recv().expect("ServiceHealthUpdated");
        match ev {
            Event::ServiceHealthUpdated { health } => {
                assert!(!health.persistence_healthy);
                assert_eq!(health.status, HealthStatus::Critical);
                assert!(health.last_error.unwrap().contains("disk full"));
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }
    }

    // 41. Health persistence recovery emits ServiceHealthUpdated
    #[test]
    fn test_health_persistence_recovery_emits_service_health_updated() {
        let mut coordinator = setup_test_coordinator();
        let err = StateStoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
        coordinator.mark_persistence_failure(&err);

        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.mark_persistence_success();

        let ev = sub.receiver.try_recv().expect("ServiceHealthUpdated");
        match ev {
            Event::ServiceHealthUpdated { health } => {
                assert!(health.persistence_healthy);
                assert_eq!(health.last_error, None);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }
    }

    // 42. Health gate failure emits ServiceHealthUpdated
    #[test]
    fn test_health_gate_failure_emits_service_health_updated() {
        let mut coordinator = setup_test_coordinator();
        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator.mark_gate_failure("WFP timeout".to_string());

        let ev = sub.receiver.try_recv().expect("ServiceHealthUpdated");
        match ev {
            Event::ServiceHealthUpdated { health } => {
                assert!(!health.internet_gate_healthy);
                assert_eq!(health.last_error, Some("WFP timeout".to_string()));
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }
    }

    // 43. Health gate recovery emits ServiceHealthUpdated
    #[test]
    fn test_health_gate_recovery_emits_service_health_updated() {
        let mut coordinator = setup_test_coordinator();
        coordinator.mark_gate_failure("WFP timeout".to_string());

        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.mark_gate_success();

        let ev = sub.receiver.try_recv().expect("ServiceHealthUpdated");
        match ev {
            Event::ServiceHealthUpdated { health } => {
                assert!(health.internet_gate_healthy);
                assert_eq!(health.last_error, None);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }
    }

    // 44. Health uptime advance alone emits no health event
    #[test]
    fn test_health_uptime_advance_alone_emits_no_health_event() {
        let (mut runtime, clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();
        let sub = handle.subscribe_events().expect("subscribe ok");

        clock.advance(Duration::from_secs(100));
        runtime.handle().tick().unwrap();

        let mut health_ev_seen = false;
        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::ServiceHealthUpdated { .. }) {
                health_ev_seen = true;
            }
        }
        assert!(
            !health_ev_seen,
            "Uptime advance alone must not emit health event"
        );
        let _ = runtime.stop();
    }

    // 45. Health emitted event contains fresh uptime seconds
    #[test]
    fn test_health_emitted_event_contains_fresh_uptime_seconds() {
        let mut coordinator = setup_test_coordinator();
        coordinator.clock.advance(Duration::from_secs(123));

        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.mark_gate_failure("test error".to_string());

        let ev = sub.receiver.try_recv().expect("ServiceHealthUpdated");
        match ev {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.uptime_seconds, 123);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }
    }

    // 46. Health event delivery failure does not alter runtime operation result
    #[test]
    fn test_health_event_delivery_failure_does_not_alter_runtime_operation_result() {
        let mut coordinator = setup_test_coordinator();
        let sub = subscribe_to_coordinator(&mut coordinator);
        drop(sub.receiver);

        coordinator.mark_gate_failure("dropped sub test".to_string());
        assert_eq!(coordinator.subscribers.len(), 0);
        assert!(!coordinator.health.internet_gate_healthy);
    }

    // 47. Shutdown aggregate startup future executing shutdown derives Scheduled
    #[test]
    fn test_shutdown_aggregate_startup_future_executing_shutdown_derives_scheduled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([0x30; 16]),
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2_000_000)),
            created_at: UtcDateTime(1_000_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Executing,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        assert_eq!(coordinator.shutdown_state, ShutdownState::Scheduled);
    }

    // 48. Shutdown aggregate failed action alone derives Idle
    #[test]
    fn test_shutdown_aggregate_failed_action_alone_derives_idle() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([0x31; 16]),
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(2_000_000)),
            created_at: UtcDateTime(1_000_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Failed {
                reason: "err".to_string(),
            },
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock,
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        assert_eq!(coordinator.shutdown_state, ShutdownState::Idle);
    }

    // 49. Shutdown power failure persistence failure keeps snapshot consistent and suppresses state event
    #[test]
    fn test_shutdown_power_failure_persistence_failure_keeps_snapshot_consistent_and_suppresses_state_event()
     {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        power.fail_shutdown.store(true, Ordering::SeqCst);
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store.clone(),
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        let sub = subscribe_to_coordinator(&mut coordinator);
        let timer_id = coordinator
            .handle_schedule_action(ActionKind::ShutdownComputer, 10, Initiator::ParentLocalPin)
            .expect("sched ok");
        while sub.receiver.try_recv().is_ok() {}

        // Power fails on deadline, and subsequent save of Failed candidate fails
        // Save 1: Schedule (1)
        // Save 2: Executing (2)
        // Save 3: Failed outcome (fails) -> fail_after_n_saves = 2
        store.fail_after_n_saves.store(2, Ordering::SeqCst);

        clock.advance(Duration::from_secs(15));
        coordinator.process_clock_and_events();

        let snap = query_coordinator_status(&coordinator);
        assert_eq!(snap.active_actions.len(), 1);
        assert_eq!(snap.active_actions[0].id, timer_id);
        assert!(matches!(
            snap.active_actions[0].execution_state,
            ActionExecutionState::Failed { .. }
        ));
        assert_eq!(snap.shutdown_state, ShutdownState::Idle);
        assert!(!snap.health.persistence_healthy);
        assert_eq!(snap.health.status, HealthStatus::Critical);

        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::ShutdownStateChanged { .. }) {
                panic!(
                    "Must not emit ShutdownStateChanged for failed non-durable persistence transition"
                );
            }
        }
    }

    // 50. Internet policy platform sync failure emits final observed pair
    #[test]
    fn test_internet_policy_platform_sync_failure_emits_final_observed_pair() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.desired_internet_state = DesiredInternetState::Blocked;
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate.clone(),
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log),
        )
        .expect("coordinator ok");

        gate.fail_calls.store(true, Ordering::SeqCst);
        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.process_internet_reconciliation_retry();

        let mut sync_event = None;
        while let Ok(ev) = sub.receiver.try_recv() {
            if let Event::InternetPolicyChanged {
                desired,
                observed,
                reason,
            } = ev
            {
                sync_event = Some((desired, observed, reason));
            }
        }

        assert_eq!(
            sync_event,
            Some((
                DesiredInternetState::Blocked,
                InternetState::Unknown,
                StateChangeReason::PlatformSync
            ))
        );
    }

    // 51. Health power error set and clear emit meaningful updates
    #[test]
    fn test_health_power_error_set_and_clear_emit_meaningful_updates() {
        let mut coordinator = setup_test_coordinator();
        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator.mutate_health_and_maybe_emit(|s| {
            s.power_error = Some("Power err".to_string());
        });

        let ev1 = sub.receiver.try_recv().expect("power error health update");
        match ev1 {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.last_error, Some("Power err".to_string()));
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }

        coordinator.mutate_health_and_maybe_emit(|s| {
            s.power_error = None;
        });

        let ev2 = sub
            .receiver
            .try_recv()
            .expect("power error cleared health update");
        match ev2 {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.last_error, None);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }
    }

    // 52. Health retry policy error set and clear emit meaningful updates
    #[test]
    fn test_health_retry_policy_error_set_and_clear_emit_meaningful_updates() {
        let mut coordinator = setup_test_coordinator();
        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator.mutate_health_and_maybe_emit(|s| {
            s.retry_policy_error = Some("Zero delay error".to_string());
        });

        let ev1 = sub
            .receiver
            .try_recv()
            .expect("retry policy error health update");
        match ev1 {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.last_error, Some("Zero delay error".to_string()));
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }

        coordinator.mutate_health_and_maybe_emit(|s| {
            s.retry_policy_error = None;
        });

        let ev2 = sub
            .receiver
            .try_recv()
            .expect("retry policy error cleared health update");
        match ev2 {
            Event::ServiceHealthUpdated { health } => {
                assert_eq!(health.last_error, None);
            }
            other => panic!("Expected ServiceHealthUpdated, got {:?}", other),
        }
    }

    // 53. Health subscriber count event semantics preserved
    #[test]
    fn test_health_subscriber_count_event_semantics_preserved() {
        let (mut runtime, _clock, _log) = setup_test_runtime();
        let handle = runtime.handle().clone();

        let sub1 = handle.subscribe_events().expect("sub1 ok");
        assert_eq!(sub1.initial_snapshot.health.active_tray_sessions, 1);

        let sub2 = handle.subscribe_events().expect("sub2 ok");
        assert_eq!(sub2.initial_snapshot.health.active_tray_sessions, 2);

        let _ = sub1;
        let _ = sub2;
        let _ = runtime.stop();
    }

    // 54. Health pruning reconciliation does not recurse or storm
    #[test]
    fn test_health_pruning_reconciliation_does_not_recurse_or_storm() {
        let mut coordinator = setup_test_coordinator();

        let mut healthy_subs = Vec::new();
        for i in 0..5 {
            let (tx, rx) = channel();
            coordinator.handle_subscribe_events(tx);
            let sub = rx.recv().unwrap().unwrap();
            if i < 3 {
                drop(sub.receiver);
            } else {
                healthy_subs.push(sub);
            }
        }

        coordinator.mutate_health_and_maybe_emit(|s| {
            s.health.internet_gate_healthy = false;
            s.internet_gate_error = Some("Gate err".to_string());
        });

        assert_eq!(coordinator.subscribers.len(), healthy_subs.len());
        assert_eq!(
            coordinator.health.active_tray_sessions,
            healthy_subs.len() as u32
        );
    }

    // 55. Internet deadline missing action stale anchor emits no timer expired and no gate call
    #[test]
    fn test_internet_deadline_missing_action_stale_anchor_emits_no_timer_expired_and_no_gate_call()
    {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        // Insert a stale anchor for an absent timer
        let stale_id = TimerId([99; 16]);
        let now_mono = clock.monotonic_now();
        coordinator.monotonic_timers.insert(
            stale_id,
            MonotonicTimerAnchor {
                timer_id: stale_id,
                action_kind: ActionKind::BlockInternet,
                utc_deadline: Deadline(UtcDateTime(1_000_000)),
                monotonic_target: now_mono,
                original_duration_seconds: 10,
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: 10,
            },
        );

        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.execute_scheduled_internet_deadline(stale_id);

        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::TimerExpired { .. }) {
                panic!("Must not emit TimerExpired for missing action");
            }
        }

        let logs = log.lock().unwrap().clone();
        assert!(
            !logs.iter().any(|l| l.contains("gate:block_internet")),
            "Must not call InternetGate for missing action"
        );
    }

    // 56. Shutdown deadline failed action stale anchor emits no timer expired and no power call
    #[test]
    fn test_shutdown_deadline_failed_action_stale_anchor_emits_no_timer_expired_and_no_power_call()
    {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        let timer_id = TimerId([7; 16]);
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::ShutdownComputer,
            deadline: Deadline(UtcDateTime(1_500_000)),
            created_at: UtcDateTime(1_000_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Failed {
                reason: "Prior failure".to_string(),
            },
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let now_mono = clock.monotonic_now();
        coordinator.monotonic_timers.insert(
            timer_id,
            MonotonicTimerAnchor {
                timer_id,
                action_kind: ActionKind::ShutdownComputer,
                utc_deadline: Deadline(UtcDateTime(1_500_000)),
                monotonic_target: now_mono,
                original_duration_seconds: 10,
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: 10,
            },
        );

        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.execute_scheduled_shutdown_deadline(timer_id);

        while let Ok(ev) = sub.receiver.try_recv() {
            if matches!(ev, Event::TimerExpired { .. }) {
                panic!("Must not emit TimerExpired for Failed action");
            }
        }

        let logs = log.lock().unwrap().clone();
        assert!(
            !logs.iter().any(|l| l.contains("power:initiate_shutdown")),
            "Must not call PowerController for Failed action"
        );

        let act = coordinator
            .state
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .expect("action present");
        assert!(
            matches!(act.execution_state, ActionExecutionState::Failed { .. }),
            "Action execution state must remain Failed"
        );
    }

    // 57. Immediate block event order policy then gate health then clear retry persistence
    #[test]
    fn test_immediate_block_event_order_policy_then_gate_health_then_clear_retry_persistence() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let initial_state = sample_initial_state();
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        coordinator.state.internet_retry = Some(InternetRetry {
            attempt_count: 1,
            last_error: Some("Prior gate error".to_string()),
        });

        // Make gate unhealthy initially
        coordinator.mark_gate_failure("Prior gate error".to_string());
        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator
            .handle_immediate_block(Initiator::ParentLocalPin)
            .expect("block ok");

        let mut events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            events.push(ev);
        }

        let policy_idx = events
            .iter()
            .position(|e| matches!(e, Event::InternetPolicyChanged { .. }))
            .expect("policy event emitted");
        let health_idx = events
            .iter()
            .position(|e| matches!(e, Event::ServiceHealthUpdated { .. }))
            .expect("health event emitted");
        assert!(
            policy_idx < health_idx,
            "InternetPolicyChanged must precede ServiceHealthUpdated"
        );

        let logs = log.lock().unwrap().clone();
        let gate_call_idx = logs
            .iter()
            .position(|l| l.contains("gate:block_internet"))
            .expect("gate called");
        let clear_retry_idx = logs
            .iter()
            .position(|l| l.contains("save:clear_retry"))
            .expect("clear retry saved");
        assert!(
            gate_call_idx < clear_retry_idx,
            "Clear retry save must follow gate operation"
        );
    }

    // 58. Restore internet event order policy then gate health then clear retry persistence
    #[test]
    fn test_restore_internet_event_order_policy_then_gate_health_then_clear_retry_persistence() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.desired_internet_state = DesiredInternetState::Blocked;
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        coordinator.state.internet_retry = Some(InternetRetry {
            attempt_count: 1,
            last_error: Some("Prior gate error".to_string()),
        });

        coordinator.mark_gate_failure("Prior gate error".to_string());
        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator
            .handle_restore_internet(Initiator::ParentLocalPin)
            .expect("restore ok");

        let mut events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            events.push(ev);
        }

        let policy_idx = events
            .iter()
            .position(|e| matches!(e, Event::InternetPolicyChanged { .. }))
            .expect("policy event emitted");
        let health_idx = events
            .iter()
            .position(|e| matches!(e, Event::ServiceHealthUpdated { .. }))
            .expect("health event emitted");
        assert!(
            policy_idx < health_idx,
            "InternetPolicyChanged must precede ServiceHealthUpdated"
        );

        let logs = log.lock().unwrap().clone();
        let gate_call_idx = logs
            .iter()
            .position(|l| l.contains("gate:unblock_internet"))
            .expect("gate called");
        let clear_retry_idx = logs
            .iter()
            .position(|l| l.contains("save:clear_retry"))
            .expect("clear retry saved");
        assert!(
            gate_call_idx < clear_retry_idx,
            "Clear retry save must follow gate operation"
        );
    }

    // 59. Scheduled internet success event order policy then gate health then completed persistence
    #[test]
    fn test_scheduled_internet_success_event_order_policy_then_gate_health_then_completed_persistence()
     {
        let log = Arc::new(Mutex::new(Vec::new()));
        let timer_id = TimerId([101; 16]);
        let mut initial_state = sample_initial_state();
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(1_300_000)),
            created_at: UtcDateTime(1_000_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        let now_mono = clock.monotonic_now();
        coordinator.monotonic_timers.insert(
            timer_id,
            MonotonicTimerAnchor {
                timer_id,
                action_kind: ActionKind::BlockInternet,
                utc_deadline: Deadline(UtcDateTime(1_300_000)),
                monotonic_target: now_mono,
                original_duration_seconds: 300,
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: 300,
            },
        );

        coordinator.mark_gate_failure("Prior gate error".to_string());
        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator.execute_scheduled_internet_deadline(timer_id);

        let mut events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            events.push(ev);
        }

        let timer_expired_idx = events
            .iter()
            .position(|e| matches!(e, Event::TimerExpired { .. }))
            .expect("TimerExpired emitted");
        let policy_idx = events
            .iter()
            .position(|e| matches!(e, Event::InternetPolicyChanged { .. }))
            .expect("InternetPolicyChanged emitted");
        let health_idx = events
            .iter()
            .position(|e| matches!(e, Event::ServiceHealthUpdated { .. }))
            .expect("ServiceHealthUpdated emitted");

        assert!(
            timer_expired_idx < policy_idx,
            "TimerExpired must precede InternetPolicyChanged"
        );
        assert!(
            policy_idx < health_idx,
            "InternetPolicyChanged must precede ServiceHealthUpdated"
        );

        let logs = log.lock().unwrap().clone();
        let completed_save_idx = logs
            .iter()
            .position(|l| l.contains("save:scheduled_internet_completed"))
            .expect("completed save");
        let gate_call_idx = logs
            .iter()
            .position(|l| l.contains("gate:block_internet"))
            .expect("gate called");
        assert!(
            gate_call_idx < completed_save_idx,
            "Completed save must follow gate call"
        );

        assert!(
            !coordinator
                .state
                .active_actions
                .iter()
                .any(|a| a.id == timer_id),
            "Completed action must be removed"
        );
    }

    // 60. Scheduled internet failure event order policy then gate health then failed persistence
    #[test]
    fn test_scheduled_internet_failure_event_order_policy_then_gate_health_then_failed_persistence()
    {
        let log = Arc::new(Mutex::new(Vec::new()));
        let timer_id = TimerId([102; 16]);
        let mut initial_state = sample_initial_state();
        initial_state.active_actions.push(ScheduledAction {
            id: timer_id,
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(1_300_000)),
            created_at: UtcDateTime(1_000_000),
            created_by: Initiator::ParentLocalPin,
            execution_state: ActionExecutionState::Pending,
            emitted_thresholds: std::collections::HashSet::new(),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Unrestricted, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate.clone(),
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        gate.fail_calls.store(true, Ordering::SeqCst);

        let now_mono = clock.monotonic_now();
        coordinator.monotonic_timers.insert(
            timer_id,
            MonotonicTimerAnchor {
                timer_id,
                action_kind: ActionKind::BlockInternet,
                utc_deadline: Deadline(UtcDateTime(1_300_000)),
                monotonic_target: now_mono,
                original_duration_seconds: 300,
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: 300,
            },
        );

        let sub = subscribe_to_coordinator(&mut coordinator);
        coordinator.execute_scheduled_internet_deadline(timer_id);

        let mut events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            events.push(ev);
        }

        let timer_expired_idx = events
            .iter()
            .position(|e| matches!(e, Event::TimerExpired { .. }))
            .expect("TimerExpired emitted");
        let policy_idx = events
            .iter()
            .position(|e| matches!(e, Event::InternetPolicyChanged { .. }))
            .expect("InternetPolicyChanged emitted");
        let health_idx = events
            .iter()
            .position(|e| matches!(e, Event::ServiceHealthUpdated { .. }))
            .expect("ServiceHealthUpdated emitted");

        assert!(
            timer_expired_idx < policy_idx,
            "TimerExpired must precede InternetPolicyChanged"
        );
        assert!(
            policy_idx < health_idx,
            "InternetPolicyChanged must precede ServiceHealthUpdated"
        );

        let act = coordinator
            .state
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .expect("action retained");
        assert!(
            matches!(act.execution_state, ActionExecutionState::Failed { .. }),
            "Action state must be Failed"
        );
    }

    // 61. Platform sync success event order policy then gate health then retry success persistence
    #[test]
    fn test_platform_sync_success_event_order_policy_then_gate_health_then_retry_success_persistence()
     {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut initial_state = sample_initial_state();
        initial_state.desired_internet_state = DesiredInternetState::Blocked;
        initial_state.internet_retry = Some(InternetRetry {
            attempt_count: 1,
            last_error: Some("Prior gate error".to_string()),
        });
        let store = FakeStateStore::new(initial_state.clone(), log.clone());
        let gate = FakeInternetGate::new(InternetState::Blocked, log.clone());
        let power = FakePowerController::new(log.clone());
        let clock = FakeClock::new(1_000_000);
        let id_source = FakeIdSource::new();
        let retry = TestRetryPolicy::new(Duration::from_secs(10));
        let bootstrapped = sample_bootstrapped_state(initial_state);

        let (mut coordinator, _) = create_test_coordinator_custom(
            bootstrapped,
            store,
            gate,
            power,
            clock.clone(),
            id_source,
            retry,
            Some(log.clone()),
        )
        .expect("coordinator ok");

        // Set observed state to Unrestricted to induce a change on sync
        coordinator.observed_internet_state = InternetState::Unrestricted;
        coordinator.mark_gate_failure("Prior gate error".to_string());
        let sub = subscribe_to_coordinator(&mut coordinator);

        coordinator.process_internet_reconciliation_retry();

        let mut events = Vec::new();
        while let Ok(ev) = sub.receiver.try_recv() {
            events.push(ev);
        }

        let policy_idx = events
            .iter()
            .position(|e| matches!(e, Event::InternetPolicyChanged { .. }))
            .expect("InternetPolicyChanged emitted");
        let health_idx = events
            .iter()
            .position(|e| matches!(e, Event::ServiceHealthUpdated { .. }))
            .expect("ServiceHealthUpdated emitted");

        assert!(
            policy_idx < health_idx,
            "InternetPolicyChanged must precede ServiceHealthUpdated"
        );

        let logs = log.lock().unwrap().clone();
        let gate_call_idx = logs
            .iter()
            .position(|l| l.contains("gate:block_internet"))
            .expect("gate called");
        let retry_save_idx = logs
            .iter()
            .position(|l| l.contains("save:retry_success"))
            .expect("retry success saved");
        assert!(
            gate_call_idx < retry_save_idx,
            "Retry success save must follow gate call"
        );
    }
}
