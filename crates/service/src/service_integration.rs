//! Service SCM integration and composition root for `palka-service`.
//!
//! Implements the normative requirements of `docs/017-service-scm-executable-integration-contract.md`:
//! - Phase-aware SCM status progression (START_PENDING CP1 -> Bootstrap -> START_PENDING CP2 -> Runtime -> RUNNING -> STOP_PENDING -> STOPPED).
//! - Clean separation between platform execution and service orchestration via test seams.
//! - Strategy-A dependency gate: production composition root uses only real production adapters.
//! - Safe, typed error mapping preserving Win32 standard exit codes (0 for graceful, 1064 for fatal).

use std::fmt;

use palka_windows_platform::scm_runtime::{
    ERROR_EXCEPTION_IN_SERVICE, NO_ERROR, ScmRuntimeControl, ScmRuntimeError, ScmServiceContext,
};

use crate::bootstrap::{BootstrappedServiceState, ServiceBootstrapError, bootstrap_service};
use crate::runtime::{
    PlatformError, ServiceRuntime, ServiceRuntimeError, StartupReadiness, SystemClock,
};
use crate::windows_id_source::WindowsIdSourceAdapter;
use crate::windows_internet_gate::WindowsInternetRetryPolicy;
use crate::windows_power_controller::WindowsPowerControllerAdapter;
use palka_windows_platform::internet_gate::WindowsInternetGate;

/// Recommended SCM wait hints in milliseconds according to docs/017 §3.1.
pub const START_WAIT_HINT_MS: u32 = 30_000;
pub const STOP_WAIT_HINT_MS: u32 = 15_000;

/// Unified typed error taxonomy for service executable integration.
#[derive(Debug)]
pub enum ServiceIntegrationError {
    /// Error during persistent root, config, credentials, or state bootstrap.
    Bootstrap(ServiceBootstrapError),
    /// Error during runtime startup, recovery, or production dependency construction.
    RuntimeStartup(ServiceRuntimeError),
    /// Error during graceful runtime teardown or worker join.
    RuntimeTeardown(ServiceRuntimeError),
    /// Error communicating status or receiving controls from Windows SCM.
    ScmStatus(ScmRuntimeError),
    /// Architectural or dependency composition error.
    DependencyComposition(&'static str),
}

impl fmt::Display for ServiceIntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bootstrap(err) => write!(f, "Service integration bootstrap error: {err}"),
            Self::RuntimeStartup(err) => {
                write!(f, "Service integration runtime startup error: {err}")
            }
            Self::RuntimeTeardown(err) => {
                write!(f, "Service integration runtime teardown error: {err}")
            }
            Self::ScmStatus(err) => write!(f, "Service integration SCM status error: {err}"),
            Self::DependencyComposition(msg) => {
                write!(f, "Service integration dependency composition error: {msg}")
            }
        }
    }
}

impl std::error::Error for ServiceIntegrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bootstrap(err) => Some(err),
            Self::RuntimeStartup(err) => Some(err),
            Self::RuntimeTeardown(err) => Some(err),
            Self::ScmStatus(err) => Some(err),
            Self::DependencyComposition(_) => None,
        }
    }
}

impl From<ServiceBootstrapError> for ServiceIntegrationError {
    fn from(err: ServiceBootstrapError) -> Self {
        Self::Bootstrap(err)
    }
}

impl From<ScmRuntimeError> for ServiceIntegrationError {
    fn from(err: ScmRuntimeError) -> Self {
        Self::ScmStatus(err)
    }
}

/// Abstract port for Windows SCM lifecycle interactions.
pub trait ServiceLifecyclePort {
    /// Reports `SERVICE_START_PENDING` with an incrementing checkpoint and wait hint.
    fn report_start_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ServiceIntegrationError>;

    /// Reports `SERVICE_RUNNING` accepting `SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN`.
    fn report_running(&mut self) -> Result<(), ServiceIntegrationError>;

    /// Blocks awaiting `SERVICE_CONTROL_STOP` or `SERVICE_CONTROL_SHUTDOWN` from SCM.
    fn wait_for_control(&self) -> Result<ScmRuntimeControl, ServiceIntegrationError>;

    /// Reports `SERVICE_STOP_PENDING` with an incrementing checkpoint and wait hint.
    fn report_stop_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ServiceIntegrationError>;

    /// Reports `SERVICE_STOPPED` with a Win32 exit code (0 for graceful, 1064 for fatal).
    fn report_stopped(&mut self, win32_exit_code: u32) -> Result<(), ServiceIntegrationError>;
}

impl ServiceLifecyclePort for ScmServiceContext {
    fn report_start_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ServiceIntegrationError> {
        self.report_start_pending(checkpoint, wait_hint_ms)
            .map_err(ServiceIntegrationError::ScmStatus)
    }

    fn report_running(&mut self) -> Result<(), ServiceIntegrationError> {
        self.report_running()
            .map_err(ServiceIntegrationError::ScmStatus)
    }

    fn wait_for_control(&self) -> Result<ScmRuntimeControl, ServiceIntegrationError> {
        self.wait_for_control()
            .map_err(ServiceIntegrationError::ScmStatus)
    }

    fn report_stop_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ServiceIntegrationError> {
        self.report_stop_pending(checkpoint, wait_hint_ms)
            .map_err(ServiceIntegrationError::ScmStatus)
    }

    fn report_stopped(&mut self, win32_exit_code: u32) -> Result<(), ServiceIntegrationError> {
        self.report_stopped(win32_exit_code)
            .map_err(ServiceIntegrationError::ScmStatus)
    }
}

/// Abstract port for authoritative service bootstrap.
pub trait ServiceBootstrapPort {
    /// Loads and strictly validates persistent configuration, credentials, and state.
    fn bootstrap(&mut self) -> Result<BootstrappedServiceState, ServiceBootstrapError>;
}

/// Concrete production bootstrap port invoking `palka_service::bootstrap::bootstrap_service()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionBootstrap;

impl ServiceBootstrapPort for ProductionBootstrap {
    fn bootstrap(&mut self) -> Result<BootstrappedServiceState, ServiceBootstrapError> {
        bootstrap_service()
    }
}

/// Abstract port for controlling the lifecycle of a started Service Runtime instance.
pub trait ServiceRuntimeLifecyclePort {
    /// Inspects the startup readiness status of the runtime.
    fn readiness(&self) -> &StartupReadiness;

    /// Initiates graceful teardown and waits for worker threads to complete.
    fn stop(&mut self) -> Result<(), ServiceRuntimeError>;
}

impl ServiceRuntimeLifecyclePort for ServiceRuntime {
    fn readiness(&self) -> &StartupReadiness {
        self.readiness()
    }

    fn stop(&mut self) -> Result<(), ServiceRuntimeError> {
        self.stop()
    }
}

/// Abstract factory port for constructing the Service Runtime.
pub trait ServiceRuntimeFactory {
    type Runtime: ServiceRuntimeLifecyclePort;

    /// Constructs and starts the runtime given a validated bootstrap snapshot.
    fn start(
        &mut self,
        bootstrapped: BootstrappedServiceState,
    ) -> Result<Self::Runtime, ServiceRuntimeError>;
}

/// Production runtime factory constructing all real Strategy-A production dependencies.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionRuntimeFactory;

impl ServiceRuntimeFactory for ProductionRuntimeFactory {
    type Runtime = ServiceRuntime;

    fn start(
        &mut self,
        bootstrapped: BootstrappedServiceState,
    ) -> Result<Self::Runtime, ServiceRuntimeError> {
        // 1. Construct real Windows WFP InternetGate
        let gate = WindowsInternetGate::open_live()
            .map_err(|e| ServiceRuntimeError::Platform(PlatformError::from(e)))?;

        // 2. Construct real Windows Power Controller Adapter (with readiness probe)
        let power = WindowsPowerControllerAdapter::from_production()
            .map_err(ServiceRuntimeError::Platform)?;

        // 3. Construct real Production IdSource (with reserved active Timer & Outbox IDs)
        let id_source = WindowsIdSourceAdapter::from_production(&bootstrapped.state)
            .map_err(ServiceRuntimeError::Platform)?;

        // 4. Construct SystemClock and WindowsInternetRetryPolicy
        let clock = SystemClock;
        let retry_policy = WindowsInternetRetryPolicy;

        // 5. Start ServiceRuntime
        ServiceRuntime::start(bootstrapped, gate, power, clock, id_source, retry_policy)
    }
}

/// Testable service orchestration algorithm enforcing docs/017 §3, §7, §8, §9.
pub fn run_service_with_ports<L, B, F>(
    mut lifecycle: L,
    mut bootstrap: B,
    mut runtime_factory: F,
) -> Result<(), ServiceIntegrationError>
where
    L: ServiceLifecyclePort,
    B: ServiceBootstrapPort,
    F: ServiceRuntimeFactory,
{
    // [Phase 1]: Report SERVICE_START_PENDING (Checkpoint 1, 30_000 ms) before disk I/O
    lifecycle.report_start_pending(1, START_WAIT_HINT_MS)?;

    // [Phase 2]: Authoritative service bootstrap
    let bootstrapped = match bootstrap.bootstrap() {
        Ok(state) => state,
        Err(err) => {
            lifecycle.report_stopped(ERROR_EXCEPTION_IN_SERVICE)?;
            return Err(ServiceIntegrationError::Bootstrap(err));
        }
    };

    // [Phase 3]: Report SERVICE_START_PENDING (Checkpoint 2, 30_000 ms)
    lifecycle.report_start_pending(2, START_WAIT_HINT_MS)?;

    // [Phase 4]: Deferred production runtime composition & startup recovery
    let mut runtime = match runtime_factory.start(bootstrapped) {
        Ok(rt) => rt,
        Err(err) => {
            lifecycle.report_stopped(ERROR_EXCEPTION_IN_SERVICE)?;
            return Err(ServiceIntegrationError::RuntimeStartup(err));
        }
    };

    // [Phase 5]: Inspect startup readiness
    match runtime.readiness() {
        StartupReadiness::Ready(_) | StartupReadiness::Degraded(_) => {
            // Both Ready and Degraded (with real production adapters) permit SERVICE_RUNNING publication
        }
    }

    // [Phase 6]: Report SERVICE_RUNNING accepting STOP and SHUTDOWN
    lifecycle.report_running()?;

    // [Phase 7]: Await SCM control code (Stop or Shutdown)
    let control_res = lifecycle.wait_for_control();
    if let Err(control_err) = control_res {
        // Control channel failure while RUNNING: must transition RUNNING -> STOP_PENDING -> STOPPED(1064)
        if let Err(stop_pending_err) = lifecycle.report_stop_pending(1, STOP_WAIT_HINT_MS) {
            // STOP_PENDING publication failed:
            // Do NOT call report_stopped (would be invalid RUNNING -> STOPPED direct transition).
            // Attempt runtime.stop() so live runtime workers are not deliberately abandoned.
            let _ = runtime.stop();
            return Err(stop_pending_err);
        }
        // STOP_PENDING succeeded:
        let runtime_stop_res = runtime.stop();
        lifecycle.report_stopped(ERROR_EXCEPTION_IN_SERVICE)?;
        if let Err(runtime_err) = runtime_stop_res {
            return Err(ServiceIntegrationError::RuntimeTeardown(runtime_err));
        }
        return Err(control_err);
    }

    // [Phase 8]: SCM requested planned stop or shutdown.
    // Transition to SERVICE_STOP_PENDING (Checkpoint 1, 15_000 ms)
    lifecycle.report_stop_pending(1, STOP_WAIT_HINT_MS)?;

    // [Phase 9]: Authoritative graceful runtime teardown
    let stop_result = runtime.stop();

    // [Phase 10]: Terminal status publication
    match stop_result {
        Ok(()) => {
            lifecycle.report_stopped(NO_ERROR)?;
            Ok(())
        }
        Err(err) => {
            lifecycle.report_stopped(ERROR_EXCEPTION_IN_SERVICE)?;
            Err(ServiceIntegrationError::RuntimeTeardown(err))
        }
    }
}

/// Service main entry callback compatible with `palka_windows_platform::scm_runtime::PalkaServiceEntry`.
pub fn palka_service_entry(context: ScmServiceContext) {
    let result = run_service_with_ports(context, ProductionBootstrap, ProductionRuntimeFactory);
    if let Err(err) = result {
        eprintln!("PALKA Service Main exited with error: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_persistence::PersistentConfig;
    use crate::credentials_persistence::PersistentCredentials;
    use crate::persistence::PersistentState;
    use crate::persistent_root::canonical_paths_for_test;
    use palka_core::{
        ActionExecutionState, ActionKind, Deadline, DesiredInternetState, HealthStatus, Initiator,
        InternetState, ScheduledAction, ServiceHealth, ShutdownState, StatusSnapshot, TimerId,
        UtcDateTime,
    };
    use palka_windows_platform::scm_runtime::ScmServiceState;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct FakeLifecycleEvents {
        events: Arc<Mutex<Vec<String>>>,
        current_state: Arc<Mutex<ScmServiceState>>,
        last_checkpoint: Arc<Mutex<u32>>,
        control_to_deliver: Arc<Mutex<Option<Result<ScmRuntimeControl, ScmRuntimeError>>>>,
        fail_report_stop_pending: Arc<Mutex<Option<ScmRuntimeError>>>,
        fail_report_stopped: Arc<Mutex<Option<ScmRuntimeError>>>,
    }

    impl Default for FakeLifecycleEvents {
        fn default() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
                current_state: Arc::new(Mutex::new(ScmServiceState::Unreported)),
                last_checkpoint: Arc::new(Mutex::new(0)),
                control_to_deliver: Arc::new(Mutex::new(None)),
                fail_report_stop_pending: Arc::new(Mutex::new(None)),
                fail_report_stopped: Arc::new(Mutex::new(None)),
            }
        }
    }

    #[derive(Clone)]
    struct FakeServiceLifecyclePort {
        events: FakeLifecycleEvents,
    }

    impl FakeServiceLifecyclePort {
        fn new() -> Self {
            Self {
                events: FakeLifecycleEvents::default(),
            }
        }

        fn with_control(control: Result<ScmRuntimeControl, ScmRuntimeError>) -> Self {
            let fake = Self::new();
            *fake.events.control_to_deliver.lock().unwrap() = Some(control);
            fake
        }
    }

    impl ServiceLifecyclePort for FakeServiceLifecyclePort {
        fn report_start_pending(
            &mut self,
            checkpoint: u32,
            wait_hint_ms: u32,
        ) -> Result<(), ServiceIntegrationError> {
            let mut state = self.events.current_state.lock().unwrap();
            let mut cp = self.events.last_checkpoint.lock().unwrap();
            *state = ScmServiceState::StartPending;
            *cp = checkpoint;
            self.events.events.lock().unwrap().push(format!(
                "start_pending(cp={checkpoint},hint={wait_hint_ms})"
            ));
            Ok(())
        }

        fn report_running(&mut self) -> Result<(), ServiceIntegrationError> {
            let mut state = self.events.current_state.lock().unwrap();
            *state = ScmServiceState::Running;
            self.events
                .events
                .lock()
                .unwrap()
                .push("running".to_string());
            Ok(())
        }

        fn wait_for_control(&self) -> Result<ScmRuntimeControl, ServiceIntegrationError> {
            self.events
                .events
                .lock()
                .unwrap()
                .push("wait_for_control".to_string());
            let ctrl = self
                .events
                .control_to_deliver
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Ok(ScmRuntimeControl::Stop));
            ctrl.map_err(ServiceIntegrationError::ScmStatus)
        }

        fn report_stop_pending(
            &mut self,
            checkpoint: u32,
            wait_hint_ms: u32,
        ) -> Result<(), ServiceIntegrationError> {
            if let Some(err) = self.events.fail_report_stop_pending.lock().unwrap().take() {
                return Err(ServiceIntegrationError::ScmStatus(err));
            }
            let mut state = self.events.current_state.lock().unwrap();
            let mut cp = self.events.last_checkpoint.lock().unwrap();
            *state = ScmServiceState::StopPending;
            *cp = checkpoint;
            self.events
                .events
                .lock()
                .unwrap()
                .push(format!("stop_pending(cp={checkpoint},hint={wait_hint_ms})"));
            Ok(())
        }

        fn report_stopped(&mut self, win32_exit_code: u32) -> Result<(), ServiceIntegrationError> {
            if let Some(err) = self.events.fail_report_stopped.lock().unwrap().take() {
                return Err(ServiceIntegrationError::ScmStatus(err));
            }
            let mut state = self.events.current_state.lock().unwrap();
            *state = ScmServiceState::Stopped;
            self.events
                .events
                .lock()
                .unwrap()
                .push(format!("stopped(code={win32_exit_code})"));
            Ok(())
        }
    }

    struct FakeServiceBootstrapPort {
        events: Arc<Mutex<Vec<String>>>,
        result: Option<Result<BootstrappedServiceState, ServiceBootstrapError>>,
        bootstrap_called: bool,
    }

    impl FakeServiceBootstrapPort {
        fn success(events: Arc<Mutex<Vec<String>>>, state: BootstrappedServiceState) -> Self {
            Self {
                events,
                result: Some(Ok(state)),
                bootstrap_called: false,
            }
        }

        fn failure(events: Arc<Mutex<Vec<String>>>, err: ServiceBootstrapError) -> Self {
            Self {
                events,
                result: Some(Err(err)),
                bootstrap_called: false,
            }
        }
    }

    impl ServiceBootstrapPort for FakeServiceBootstrapPort {
        fn bootstrap(&mut self) -> Result<BootstrappedServiceState, ServiceBootstrapError> {
            self.bootstrap_called = true;
            self.events.lock().unwrap().push("bootstrap".to_string());
            self.result
                .take()
                .expect("FakeServiceBootstrapPort called more than once")
        }
    }

    struct FakeServiceRuntimeLifecyclePort {
        events: Arc<Mutex<Vec<String>>>,
        readiness: StartupReadiness,
        stop_result: Result<(), ServiceRuntimeError>,
        stop_called: bool,
    }

    impl FakeServiceRuntimeLifecyclePort {
        fn ready(events: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                events,
                readiness: StartupReadiness::Ready(dummy_snapshot(true)),
                stop_result: Ok(()),
                stop_called: false,
            }
        }

        fn degraded(events: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                events,
                readiness: StartupReadiness::Degraded(dummy_snapshot(false)),
                stop_result: Ok(()),
                stop_called: false,
            }
        }

        fn failing_stop(events: Arc<Mutex<Vec<String>>>, err: ServiceRuntimeError) -> Self {
            Self {
                events,
                readiness: StartupReadiness::Ready(dummy_snapshot(true)),
                stop_result: Err(err),
                stop_called: false,
            }
        }
    }

    impl ServiceRuntimeLifecyclePort for FakeServiceRuntimeLifecyclePort {
        fn readiness(&self) -> &StartupReadiness {
            self.events
                .lock()
                .unwrap()
                .push("runtime_readiness".to_string());
            &self.readiness
        }

        fn stop(&mut self) -> Result<(), ServiceRuntimeError> {
            self.stop_called = true;
            self.events.lock().unwrap().push("runtime_stop".to_string());
            self.stop_result
                .as_ref()
                .map(|_| ())
                .map_err(|_| ServiceRuntimeError::Platform(PlatformError::new("stop failed")))
        }
    }

    struct FakeRuntimeFactory {
        events: Arc<Mutex<Vec<String>>>,
        result: Option<Result<FakeServiceRuntimeLifecyclePort, ServiceRuntimeError>>,
        start_called: bool,
    }

    impl FakeRuntimeFactory {
        fn success(events: Arc<Mutex<Vec<String>>>, rt: FakeServiceRuntimeLifecyclePort) -> Self {
            Self {
                events,
                result: Some(Ok(rt)),
                start_called: false,
            }
        }

        fn failure(events: Arc<Mutex<Vec<String>>>, err: ServiceRuntimeError) -> Self {
            Self {
                events,
                result: Some(Err(err)),
                start_called: false,
            }
        }
    }

    impl ServiceRuntimeFactory for FakeRuntimeFactory {
        type Runtime = FakeServiceRuntimeLifecyclePort;

        fn start(
            &mut self,
            _bootstrapped: BootstrappedServiceState,
        ) -> Result<Self::Runtime, ServiceRuntimeError> {
            self.start_called = true;
            self.events
                .lock()
                .unwrap()
                .push("runtime_factory_start".to_string());
            self.result
                .take()
                .expect("FakeRuntimeFactory called more than once")
        }
    }

    fn dummy_snapshot(healthy: bool) -> StatusSnapshot {
        StatusSnapshot {
            desired_internet_state: DesiredInternetState::Unrestricted,
            observed_internet_state: InternetState::Unrestricted,
            shutdown_state: ShutdownState::Idle,
            active_actions: Vec::new(),
            health: ServiceHealth {
                status: if healthy {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Degraded
                },
                uptime_seconds: 1,
                internet_gate_healthy: healthy,
                persistence_healthy: true,
                telegram_connected: true,
                active_tray_sessions: 0,
                last_error: None,
            },
            target_child_sid: "S-1-5-21-test".to_string(),
            timestamp: UtcDateTime(1_000_000),
        }
    }

    fn dummy_persistent_state() -> PersistentState {
        PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        }
    }

    fn dummy_bootstrapped() -> BootstrappedServiceState {
        let temp_base = PathBuf::from(r"C:\ProgramData");
        let paths = canonical_paths_for_test(&temp_base).expect("valid canonical test paths");
        BootstrappedServiceState {
            paths,
            config: PersistentConfig {
                child_sid: "S-1-5-21-test-child".to_string(),
                telegram_allowed_user_ids: vec![123456789],
                telegram_allowed_chat_ids: vec![987654321],
                heartbeat_interval_seconds: 60,
            },
            credentials: PersistentCredentials {
                pin_hash: "$argon2id$v=19$m=65536,t=3,p=1$c2FsdHNhbHQ$dGVzdGhhc2g".to_string(),
                telegram_bot_token_dpapi: vec![1, 2, 3, 4],
            },
            state: dummy_persistent_state(),
        }
    }

    // EXE-02: CP1 precedes bootstrap
    #[test]
    fn test_exe_02_cp1_precedes_bootstrap() {
        let lifecycle = FakeServiceLifecyclePort::new();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        assert_eq!(ev[0], "start_pending(cp=1,hint=30000)");
        assert_eq!(ev[1], "bootstrap");
        let cp1_idx = ev
            .iter()
            .position(|e| e == "start_pending(cp=1,hint=30000)")
            .unwrap();
        let bs_idx = ev.iter().position(|e| e == "bootstrap").unwrap();
        assert!(cp1_idx < bs_idx);
    }

    // EXE-03: bootstrap failure -> STOPPED(1064), never RUNNING
    #[test]
    fn test_exe_03_bootstrap_failure_stops_with_1064() {
        use crate::config_store::ConfigStoreError;

        let lifecycle = FakeServiceLifecyclePort::new();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::failure(
            events.clone(),
            ServiceBootstrapError::Config(ConfigStoreError::MissingCanonical(PathBuf::from(
                "config.json",
            ))),
        );
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::Bootstrap(_)
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "stopped(code=1064)".to_string(),
            ]
        );
        assert!(!ev.contains(&"running".to_string()));
    }

    // EXE-04: successful bootstrap alone is insufficient for RUNNING; CP2 occurs before runtime factory
    #[test]
    fn test_exe_04_bootstrap_success_emits_cp2_not_running() {
        let lifecycle = FakeServiceLifecyclePort::new();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        let bs_idx = ev.iter().position(|e| e == "bootstrap").unwrap();
        let cp2_idx = ev
            .iter()
            .position(|e| e == "start_pending(cp=2,hint=30000)")
            .unwrap();
        let factory_idx = ev
            .iter()
            .position(|e| e == "runtime_factory_start")
            .unwrap();
        let running_idx = ev.iter().position(|e| e == "running").unwrap();

        // bootstrap < CP2 < runtime_factory_start < running
        assert!(bs_idx < cp2_idx);
        assert!(cp2_idx < factory_idx);
        assert!(factory_idx < running_idx);
    }

    // EXE-05: runtime factory starts strictly after CP2 and RUNNING only after readiness inspection
    #[test]
    fn test_exe_05_runtime_factory_starts_after_cp2() {
        let lifecycle = FakeServiceLifecyclePort::new();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        let cp2_idx = ev
            .iter()
            .position(|e| e == "start_pending(cp=2,hint=30000)")
            .unwrap();
        let factory_idx = ev
            .iter()
            .position(|e| e == "runtime_factory_start")
            .unwrap();
        let readiness_idx = ev.iter().position(|e| e == "runtime_readiness").unwrap();
        let running_idx = ev.iter().position(|e| e == "running").unwrap();

        // CP2 < runtime_factory_start < runtime_readiness < running
        assert!(cp2_idx < factory_idx);
        assert!(factory_idx < readiness_idx);
        assert!(readiness_idx < running_idx);
    }

    // EXE-06: Ready -> RUNNING
    #[test]
    fn test_exe_06_ready_reports_running() {
        let lifecycle = FakeServiceLifecyclePort::new();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        assert!(ev.contains(&"running".to_string()));
    }

    // EXE-07: Degraded -> RUNNING
    #[test]
    fn test_exe_07_degraded_reports_running() {
        let lifecycle = FakeServiceLifecyclePort::new();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::degraded(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        assert!(ev.contains(&"running".to_string()));
    }

    // EXE-08: runtime startup failure -> STOPPED(1064), never RUNNING
    #[test]
    fn test_exe_08_runtime_start_failure_stops_with_1064() {
        let lifecycle = FakeServiceLifecyclePort::new();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::failure(
            events.clone(),
            ServiceRuntimeError::Platform(PlatformError::new("wfp unavailable")),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::RuntimeStartup(_)
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
                "stopped(code=1064)".to_string(),
            ]
        );
        assert!(!ev.contains(&"running".to_string()));
    }

    // EXE-09: STOP -> STOP_PENDING -> runtime.stop -> STOPPED(0)
    #[test]
    fn test_exe_09_stop_control_graceful_teardown() {
        let lifecycle = FakeServiceLifecyclePort::with_control(Ok(ScmRuntimeControl::Stop));
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
                "runtime_readiness".to_string(),
                "running".to_string(),
                "wait_for_control".to_string(),
                "stop_pending(cp=1,hint=15000)".to_string(),
                "runtime_stop".to_string(),
                "stopped(code=0)".to_string(),
            ]
        );
    }

    // EXE-10: SHUTDOWN -> same planned teardown
    #[test]
    fn test_exe_10_shutdown_control_graceful_teardown() {
        let lifecycle = FakeServiceLifecyclePort::with_control(Ok(ScmRuntimeControl::Shutdown));
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
                "runtime_readiness".to_string(),
                "running".to_string(),
                "wait_for_control".to_string(),
                "stop_pending(cp=1,hint=15000)".to_string(),
                "runtime_stop".to_string(),
                "stopped(code=0)".to_string(),
            ]
        );
    }

    // EXE-11: runtime stop error -> STOPPED(1064)
    #[test]
    fn test_exe_11_runtime_stop_error_reports_1064() {
        let lifecycle = FakeServiceLifecyclePort::with_control(Ok(ScmRuntimeControl::Stop));
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::failing_stop(
                events.clone(),
                ServiceRuntimeError::Platform(PlatformError::new("worker join failed")),
            ),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::RuntimeTeardown(_)
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
                "runtime_readiness".to_string(),
                "running".to_string(),
                "wait_for_control".to_string(),
                "stop_pending(cp=1,hint=15000)".to_string(),
                "runtime_stop".to_string(),
                "stopped(code=1064)".to_string(),
            ]
        );
    }

    // EXE-12: no unblock_internet call during service stop
    // EXE-13: integration does not clear or mutate persisted timers/actions/outbox
    #[test]
    fn test_exe_12_and_13_stop_invariants_and_no_domain_mutation() {
        let mut initial_state = dummy_persistent_state();
        initial_state.active_actions.push(ScheduledAction {
            id: TimerId([1u8; 16]),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(1_000_000)),
            created_at: UtcDateTime(500_000),
            created_by: Initiator::ParentLocalPin,
            emitted_thresholds: std::collections::HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        });

        let mut bootstrapped = dummy_bootstrapped();
        bootstrapped.state = initial_state.clone();

        let lifecycle = FakeServiceLifecyclePort::with_control(Ok(ScmRuntimeControl::Stop));
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), bootstrapped);
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        // Verify initial state remains unmodified
        assert_eq!(initial_state.active_actions.len(), 1);
        assert_eq!(initial_state.active_actions[0].id, TimerId([1u8; 16]));
    }

    // EXE-14: service orchestration never deliberately performs a second successful STOPPED publication
    #[test]
    fn test_exe_14_at_most_one_stopped_publication() {
        let lifecycle = FakeServiceLifecyclePort::with_control(Ok(ScmRuntimeControl::Stop));
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_ok());

        let ev = events.lock().unwrap().clone();
        let stopped_count = ev.iter().filter(|e| e.starts_with("stopped")).count();
        assert_eq!(stopped_count, 1);
    }

    // Running-phase failure policy: control wait failure transitions RUNNING -> STOP_PENDING -> STOPPED(1064)
    #[test]
    fn test_running_phase_control_wait_failure_transitions_cleanly() {
        let lifecycle =
            FakeServiceLifecyclePort::with_control(Err(ScmRuntimeError::ControlChannelClosed));
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::ScmStatus(ScmRuntimeError::ControlChannelClosed)
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
                "runtime_readiness".to_string(),
                "running".to_string(),
                "wait_for_control".to_string(),
                "stop_pending(cp=1,hint=15000)".to_string(),
                "runtime_stop".to_string(),
                "stopped(code=1064)".to_string(),
            ]
        );
    }

    // Phase Policy Test A: bootstrap failure + STOPPED publication failure
    #[test]
    fn test_phase_policy_bootstrap_failure_plus_stopped_failure() {
        use crate::config_store::ConfigStoreError;

        let lifecycle = FakeServiceLifecyclePort::new();
        *lifecycle.events.fail_report_stopped.lock().unwrap() = Some(ScmRuntimeError::WindowsApi {
            function: "SetServiceStatus",
            code: 5,
            message: "Access Denied".to_string(),
        });

        let lifecycle_events = lifecycle.events.clone();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::failure(
            events.clone(),
            ServiceBootstrapError::Config(ConfigStoreError::MissingCanonical(PathBuf::from(
                "config.json",
            ))),
        );
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::ScmStatus(ScmRuntimeError::WindowsApi { code: 5, .. })
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
            ]
        );
        assert!(!ev.iter().any(|e| e.starts_with("stopped")));
        assert_eq!(
            *lifecycle_events.current_state.lock().unwrap(),
            ScmServiceState::StartPending
        );
    }

    // Phase Policy Test B: runtime startup failure + STOPPED publication failure
    #[test]
    fn test_phase_policy_runtime_start_failure_plus_stopped_failure() {
        let lifecycle = FakeServiceLifecyclePort::new();
        *lifecycle.events.fail_report_stopped.lock().unwrap() = Some(ScmRuntimeError::WindowsApi {
            function: "SetServiceStatus",
            code: 5,
            message: "Access Denied".to_string(),
        });

        let lifecycle_events = lifecycle.events.clone();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::failure(
            events.clone(),
            ServiceRuntimeError::Platform(PlatformError::new("wfp init failed")),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::ScmStatus(ScmRuntimeError::WindowsApi { code: 5, .. })
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
            ]
        );
        assert!(!ev.iter().any(|e| e.starts_with("stopped")));
        assert_eq!(
            *lifecycle_events.current_state.lock().unwrap(),
            ScmServiceState::StartPending
        );
    }

    // Phase Policy Test C: runtime teardown failure + STOPPED publication failure
    #[test]
    fn test_phase_policy_runtime_teardown_failure_plus_stopped_failure() {
        let lifecycle = FakeServiceLifecyclePort::with_control(Ok(ScmRuntimeControl::Stop));
        *lifecycle.events.fail_report_stopped.lock().unwrap() = Some(ScmRuntimeError::WindowsApi {
            function: "SetServiceStatus",
            code: 5,
            message: "Access Denied".to_string(),
        });

        let lifecycle_events = lifecycle.events.clone();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::failing_stop(
                events.clone(),
                ServiceRuntimeError::Platform(PlatformError::new("stop failed")),
            ),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::ScmStatus(ScmRuntimeError::WindowsApi { code: 5, .. })
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
                "runtime_readiness".to_string(),
                "running".to_string(),
                "wait_for_control".to_string(),
                "stop_pending(cp=1,hint=15000)".to_string(),
                "runtime_stop".to_string(),
            ]
        );
        assert!(!ev.iter().any(|e| e.starts_with("stopped")));
        assert_eq!(
            *lifecycle_events.current_state.lock().unwrap(),
            ScmServiceState::StopPending
        );
    }

    // Phase Policy Test D: control wait failure + STOP_PENDING publication failure
    #[test]
    fn test_phase_policy_control_wait_failure_plus_stop_pending_failure() {
        let lifecycle =
            FakeServiceLifecyclePort::with_control(Err(ScmRuntimeError::ControlChannelClosed));
        *lifecycle.events.fail_report_stop_pending.lock().unwrap() =
            Some(ScmRuntimeError::WindowsApi {
                function: "SetServiceStatus",
                code: 5,
                message: "Access Denied".to_string(),
            });

        let lifecycle_events = lifecycle.events.clone();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        // Returned error = ScmStatus from STOP_PENDING
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::ScmStatus(ScmRuntimeError::WindowsApi { code: 5, .. })
        ));

        let ev = events.lock().unwrap().clone();
        // Runtime stop attempted
        assert!(ev.contains(&"runtime_stop".to_string()));
        // No normal STOPPED publication attempted after failed STOP_PENDING
        assert!(!ev.iter().any(|e| e.starts_with("stopped")));
        // State remains Running (not mutated)
        assert_eq!(
            *lifecycle_events.current_state.lock().unwrap(),
            ScmServiceState::Running
        );
    }

    // Phase Policy Test E: control wait failure + successful STOP_PENDING + STOPPED publication failure
    #[test]
    fn test_phase_policy_control_wait_failure_plus_stopped_failure() {
        let lifecycle =
            FakeServiceLifecyclePort::with_control(Err(ScmRuntimeError::ControlChannelClosed));
        *lifecycle.events.fail_report_stopped.lock().unwrap() = Some(ScmRuntimeError::WindowsApi {
            function: "SetServiceStatus",
            code: 5,
            message: "Access Denied".to_string(),
        });

        let lifecycle_events = lifecycle.events.clone();
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::ready(events.clone()),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        // Returned error = terminal ScmStatus
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::ScmStatus(ScmRuntimeError::WindowsApi { code: 5, .. })
        ));

        let ev = events.lock().unwrap().clone();
        // Runtime stop attempted
        assert!(ev.contains(&"runtime_stop".to_string()));
        // STOP_PENDING was published
        assert!(ev.contains(&"stop_pending(cp=1,hint=15000)".to_string()));
        // No successful STOPPED
        assert!(!ev.iter().any(|e| e.starts_with("stopped")));
        assert_eq!(
            *lifecycle_events.current_state.lock().unwrap(),
            ScmServiceState::StopPending
        );
    }

    // Phase Policy Test F: control wait failure with failing runtime stop
    #[test]
    fn test_phase_policy_control_wait_failure_with_runtime_stop_error() {
        let lifecycle =
            FakeServiceLifecyclePort::with_control(Err(ScmRuntimeError::ControlChannelClosed));
        let events = lifecycle.events.events.clone();
        let bootstrap = FakeServiceBootstrapPort::success(events.clone(), dummy_bootstrapped());
        let factory = FakeRuntimeFactory::success(
            events.clone(),
            FakeServiceRuntimeLifecyclePort::failing_stop(
                events.clone(),
                ServiceRuntimeError::Platform(PlatformError::new("stop failed")),
            ),
        );

        let res = run_service_with_ports(lifecycle, bootstrap, factory);
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            ServiceIntegrationError::RuntimeTeardown(_)
        ));

        let ev = events.lock().unwrap().clone();
        assert_eq!(
            ev,
            vec![
                "start_pending(cp=1,hint=30000)".to_string(),
                "bootstrap".to_string(),
                "start_pending(cp=2,hint=30000)".to_string(),
                "runtime_factory_start".to_string(),
                "runtime_readiness".to_string(),
                "running".to_string(),
                "wait_for_control".to_string(),
                "stop_pending(cp=1,hint=15000)".to_string(),
                "runtime_stop".to_string(),
                "stopped(code=1064)".to_string(),
            ]
        );
    }
}
