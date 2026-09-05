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
    ActionExecutionState, ActionKind, Deadline, DesiredInternetState, HealthStatus, Initiator,
    InternetState, ScheduledAction, ServiceHealth, ShutdownState, StatusSnapshot, TimerId,
    UtcDateTime, WarningThreshold, action_state_is_terminal, creation_due_thresholds,
    creation_passed_thresholds, crossed_warning_thresholds, execution_failure_transition,
    execution_success_transition, recovery_overdue_transition, recovery_passed_thresholds,
    runtime_deadline_transition, shutdown_cancel_allowed,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, spawn};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

enum RuntimeCommand {
    ScheduleAction {
        action_kind: ActionKind,
        duration_seconds: u32,
        initiator: Initiator,
        reply: Sender<Result<TimerId, ServiceRuntimeError>>,
    },
    CancelTimer {
        timer_id: TimerId,
        initiator: Initiator,
        reply: Sender<Result<(), ServiceRuntimeError>>,
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
        call_log: Option<Arc<Mutex<Vec<String>>>>,
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
        };

        let readiness = coordinator.perform_startup_recovery()?;
        Ok((coordinator, readiness))
    }

    fn log_event(&self, event: &str) {
        if let Some(log) = &self.call_log {
            if let Ok(mut l) = log.lock() {
                l.push(event.to_string());
            }
        }
    }

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

    fn mark_persistence_failure(&mut self, err: &StateStoreError) {
        self.health.persistence_healthy = false;
        self.persistence_error = Some(format!("State store error: {err}"));
        self.recompute_health_status();
    }

    fn mark_persistence_success(&mut self) {
        self.health.persistence_healthy = true;
        self.persistence_error = None;
        self.recompute_health_status();
    }

    fn mark_gate_failure(&mut self, err_msg: String) {
        self.health.internet_gate_healthy = false;
        self.internet_gate_error = Some(err_msg);
        self.recompute_health_status();
    }

    fn mark_gate_success(&mut self) {
        self.health.internet_gate_healthy = true;
        self.internet_gate_error = None;
        self.recompute_health_status();
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

        // 2. Initial Internet Reconciliation
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
            self.retry_policy_error =
                Some("InternetRetryPolicy returned invalid zero delay".to_string());
            self.next_retry_at = None;
            self.recompute_health_status();
        } else {
            self.retry_policy_error = None;
            self.next_retry_at = Some(self.clock.monotonic_now() + delay);
            self.recompute_health_status();
        }
    }

    fn build_status_snapshot(&self) -> StatusSnapshot {
        let uptime_seconds = self
            .clock
            .monotonic_now()
            .saturating_duration_since(self.monotonic_start)
            .as_secs();

        let mut health = self.health.clone();
        health.uptime_seconds = uptime_seconds;
        health.telegram_connected = false;

        StatusSnapshot {
            desired_internet_state: self.state.desired_internet_state,
            observed_internet_state: self.observed_internet_state,
            shutdown_state: self.shutdown_state,
            active_actions: self.state.active_actions.clone(),
            health,
            target_child_sid: self.bootstrapped.config.child_sid.clone(),
            timestamp: self.clock.utc_now(),
        }
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
                RuntimeCommand::CancelTimer { reply, .. } => {
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
            RuntimeCommand::CancelTimer {
                timer_id,
                initiator,
                reply,
            } => {
                let res = self.handle_cancel_timer(timer_id, initiator);
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
            RuntimeCommand::QueryStatus { reply } => {
                self.handle_query_status(reply);
            }
            #[cfg(test)]
            RuntimeCommand::Tick { reply } => {
                self.process_clock_and_events();
                let _ = reply.send(());
            }
            RuntimeCommand::Stop { reply } => {
                self.stopping = true;
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
            emitted_thresholds,
            execution_state: ActionExecutionState::Pending,
        };

        // Candidate state: Durable-Before-Ack
        let mut candidate = self.state.clone();
        candidate.active_actions.push(scheduled_action);
        candidate.telegram_outbox.extend(new_outbox);

        self.log_event("save:schedule_action");
        self.commit_authoritative_state(candidate)
            .map_err(ServiceRuntimeError::Persistence)?;

        // Register monotonic timer anchor
        let duration = Duration::from_secs(duration_seconds as u64);
        self.monotonic_timers.insert(
            timer_id,
            MonotonicTimerAnchor {
                timer_id,
                action_kind,
                utc_deadline: deadline,
                monotonic_target: now_mono + duration,
                original_duration_seconds: duration_seconds as u64,
                monotonic_start: now_mono,
                last_evaluated_remaining_seconds: duration_seconds as u64,
            },
        );

        Ok(timer_id)
    }

    fn handle_cancel_timer(
        &mut self,
        timer_id: TimerId,
        _initiator: Initiator,
    ) -> Result<(), ServiceRuntimeError> {
        let action = self
            .state
            .active_actions
            .iter()
            .find(|a| a.id == timer_id)
            .ok_or(ServiceRuntimeError::ActionNotFound(timer_id))?;

        // Section 21: only a genuinely pending scheduled timer may use the timer-cancellation path.
        if !matches!(action.execution_state, ActionExecutionState::Pending) {
            return Err(ServiceRuntimeError::CancellationForbidden(format!(
                "Action in state {:?} cannot be cancelled; only Pending actions are cancellable",
                action.execution_state
            )));
        }

        if action.action_kind == ActionKind::ShutdownComputer {
            let now_utc = self.clock.utc_now();
            let delta_ms = action.deadline.0.0 - now_utc.0;
            let remaining = remaining_seconds_from_delta_ms(delta_ms);
            if !shutdown_cancel_allowed(remaining) {
                return Err(ServiceRuntimeError::CancellationForbidden(
                    "Shutdown cancellation boundary has passed".to_string(),
                ));
            }
        }

        if action_state_is_terminal(&action.execution_state) {
            return Err(ServiceRuntimeError::CancellationForbidden(
                "Terminal action cannot be cancelled".to_string(),
            ));
        }

        let mut candidate = self.state.clone();
        candidate.active_actions.retain(|a| a.id != timer_id);

        self.log_event("save:cancel_timer");
        self.commit_authoritative_state(candidate)
            .map_err(ServiceRuntimeError::Persistence)?;
        self.monotonic_timers.remove(&timer_id);

        Ok(())
    }

    fn handle_immediate_block(&mut self, _initiator: Initiator) -> Result<(), ServiceRuntimeError> {
        // Candidate: Durable-Before-Side-Effect
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

        match (block_res, current_res) {
            (Ok(()), Ok(obs)) if obs == InternetState::Blocked => {
                if self.state.internet_retry.is_some() {
                    let mut c = self.state.clone();
                    c.internet_retry = None;
                    self.log_event("save:clear_retry");
                    self.commit_post_side_effect_candidate(c)
                        .map_err(ServiceRuntimeError::Persistence)?;
                }
                self.mark_gate_success();
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

    fn handle_restore_internet(
        &mut self,
        _initiator: Initiator,
    ) -> Result<(), ServiceRuntimeError> {
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

        match (unblock_res, current_res) {
            (Ok(()), Ok(obs)) if obs == InternetState::Unrestricted => {
                if self.state.internet_retry.is_some() {
                    let mut c = self.state.clone();
                    c.internet_retry = None;
                    self.log_event("save:clear_retry");
                    self.commit_post_side_effect_candidate(c)
                        .map_err(ServiceRuntimeError::Persistence)?;
                }
                self.mark_gate_success();
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
            let (is_expired, action_kind, previous, current, crossed) = {
                if let Some(anchor) = self.monotonic_timers.get(&timer_id) {
                    if now_mono >= anchor.monotonic_target {
                        (true, anchor.action_kind, 0, 0, Vec::new())
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
                        (false, anchor.action_kind, previous, current, crossed)
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

                    self.log_event("save:warning_threshold");
                    match self.commit_authoritative_state(candidate) {
                        Ok(()) => {
                            // ONLY advance the anchor cursor after save succeeds!
                            if let Some(anchor) = self.monotonic_timers.get_mut(&timer_id) {
                                anchor.last_evaluated_remaining_seconds = current;
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

        let mut candidate = self.state.clone();
        if let Some(action) = candidate
            .active_actions
            .iter_mut()
            .find(|a| a.id == timer_id)
        {
            if let Some(next_state) = runtime_deadline_transition(&action.execution_state, 0) {
                action.execution_state = next_state;
                candidate.desired_internet_state = DesiredInternetState::Blocked;

                self.log_event("save:scheduled_block_executing");
                if let Err(e) = self.commit_authoritative_state(candidate) {
                    self.mark_persistence_failure(&e);
                    return;
                }

                // Section 4: Only after durable transition succeeds may the volatile anchor be retired
                self.monotonic_timers.remove(&timer_id);

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

                let mut post_candidate = self.state.clone();
                if let Some(act) = post_candidate
                    .active_actions
                    .iter_mut()
                    .find(|a| a.id == timer_id)
                {
                    match (block_res, current_res) {
                        (Ok(()), Ok(obs)) if obs == InternetState::Blocked => {
                            self.observed_internet_state = obs;
                            self.mark_gate_success();
                            if let Some(ActionExecutionState::Completed) =
                                execution_success_transition(&act.execution_state)
                            {
                                // Remove Completed terminal action
                                post_candidate.active_actions.retain(|a| a.id != timer_id);
                                self.log_event("save:scheduled_block_completed");
                                let _ = self.commit_post_side_effect_candidate(post_candidate);
                            }
                        }
                        (Ok(()), Ok(obs)) => {
                            // Verification mismatch! (Section 7)
                            let err_msg = format!(
                                "Scheduled block verification mismatch: observed {:?}, expected Blocked",
                                obs
                            );
                            self.handle_scheduled_block_failure(
                                timer_id,
                                &mut post_candidate,
                                err_msg,
                            );
                        }
                        (Err(err), _) => {
                            self.handle_scheduled_block_failure(
                                timer_id,
                                &mut post_candidate,
                                err.reason,
                            );
                        }
                        (_, Err(err)) => {
                            self.handle_scheduled_block_failure(
                                timer_id,
                                &mut post_candidate,
                                err.reason,
                            );
                        }
                    }
                }
            }
        }
    }

    fn handle_scheduled_block_failure(
        &mut self,
        timer_id: TimerId,
        candidate: &mut PersistentState,
        err_msg: String,
    ) {
        self.mark_gate_failure(err_msg.clone());

        if let Some(act) = candidate
            .active_actions
            .iter_mut()
            .find(|a| a.id == timer_id)
        {
            if let Some(next) = execution_failure_transition(&act.execution_state, err_msg.clone())
            {
                act.execution_state = next;
            }
        }

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
                text: format!("Scheduled internet block failed: {}", err_msg),
            },
            attempt_count: 0,
            last_error: None,
        });

        self.log_event("save:scheduled_block_failed");
        let _ = self.commit_post_side_effect_candidate(candidate.clone());

        self.schedule_next_internet_retry(attempt);
    }

    fn execute_scheduled_shutdown_deadline(&mut self, timer_id: TimerId) {
        if self.is_stop_requested() {
            return;
        }

        let mut candidate = self.state.clone();
        if let Some(action) = candidate
            .active_actions
            .iter_mut()
            .find(|a| a.id == timer_id)
        {
            if let Some(next_state) = runtime_deadline_transition(&action.execution_state, 0) {
                action.execution_state = next_state;

                self.log_event("save:scheduled_shutdown_executing");
                if let Err(e) = self.commit_authoritative_state(candidate) {
                    self.mark_persistence_failure(&e);
                    return;
                }

                // Section 4: Only after durable transition succeeds may the volatile anchor be retired
                self.monotonic_timers.remove(&timer_id);

                #[cfg(test)]
                if let Some(ref hook) = self.pre_effect_hook {
                    hook();
                }

                let shutdown_res = {
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
                    match shutdown_res {
                        Ok(()) => {
                            self.power_error = None;
                            self.shutdown_state = ShutdownState::InProgress;
                            if let Some(ActionExecutionState::Completed) =
                                execution_success_transition(&act.execution_state)
                            {
                                post_candidate.active_actions.retain(|a| a.id != timer_id);
                                self.log_event("save:scheduled_shutdown_completed");
                                let _ = self.commit_post_side_effect_candidate(post_candidate);
                            }
                        }
                        Err(err) => {
                            // Do NOT set InProgress
                            self.power_error = Some(err.reason.clone());
                            self.recompute_health_status();

                            if let Some(next) = execution_failure_transition(
                                &act.execution_state,
                                err.reason.clone(),
                            ) {
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

                            self.log_event("save:scheduled_shutdown_failed");
                            let _ = self.commit_post_side_effect_candidate(post_candidate);
                        }
                    }
                }
            }
        }
    }

    fn process_internet_reconciliation_retry(&mut self) {
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

                self.log_event("save:retry_success");
                let _ = self.commit_post_side_effect_candidate(candidate);
                self.mark_gate_success();
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

    pub fn cancel_timer(
        &self,
        timer_id: TimerId,
        initiator: Initiator,
    ) -> Result<(), ServiceRuntimeError> {
        let reply_rx = {
            let guard = self.ingress.lock().unwrap();
            if *guard {
                return Err(ServiceRuntimeError::Stopping);
            }
            let (reply_tx, reply_rx) = channel();
            self.command_tx
                .send(RuntimeCommand::CancelTimer {
                    timer_id,
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
        call_log: Option<Arc<Mutex<Vec<String>>>>,
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
            .cancel_timer(timer_id, Initiator::ParentLocalPin);
        assert!(
            res.is_ok(),
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
            .cancel_timer(timer_id1, Initiator::ParentLocalPin);
        assert!(
            matches!(res1, Err(ServiceRuntimeError::CancellationForbidden(_))),
            "Executing action cannot be cancelled"
        );

        let res2 = runtime
            .handle()
            .cancel_timer(timer_id2, Initiator::ParentLocalPin);
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

        let post_cancel = handle.cancel_timer(TimerId([1; 16]), Initiator::ParentLocalPin);
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
}
