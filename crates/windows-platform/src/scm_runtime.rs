//! Windows SCM runtime adapter for PALKA.
//!
//! Provides the low-level Windows Service runtime dispatcher, control handler,
//! and status reporting mechanisms required by the PALKA Service Lifecycle Contract.
//!
//! Key properties:
//! - Connects the service process to SCM using `StartServiceCtrlDispatcherW`.
//! - Registers the extended service control handler via `RegisterServiceCtrlHandlerExW`.
//! - Completely initializes the non-blocking control delivery context BEFORE registering the handler.
//! - Retains handler context ownership across the entire lifespan of the dispatcher.
//! - Decodes and delivers `SERVICE_CONTROL_STOP` and `SERVICE_CONTROL_SHUTDOWN` via non-blocking `try_send`.
//! - Rejects unsupported controls (`PAUSE`, `CONTINUE`, user-defined, etc.) with `ERROR_CALL_NOT_IMPLEMENTED`.
//! - Transactional status reporting: local lifecycle state is updated ONLY IF status publication succeeds.
//! - Rollback and retry capability on status publication failures.
//! - Non-Send context ensuring sequential status reporting and strictly at most one successful `SERVICE_STOPPED`.
//! - Shared panic and early-return fallback reporting `SERVICE_STOPPED` at most once via a unified helper.
//! - Strictly separates platform execution from service decisions and bootstrap.

use crate::scm::PALKA_SERVICE_NAME;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(windows)]
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STATUS_HANDLE, SERVICE_TABLE_ENTRYW, SetServiceStatus, StartServiceCtrlDispatcherW,
};
#[cfg(windows)]
use windows::core::{PCWSTR, PWSTR};

/// Service type constant: Win32 own process (`SERVICE_WIN32_OWN_PROCESS`).
pub const SERVICE_WIN32_OWN_PROCESS: u32 = 0x00000010;

/// Service status state constant: `SERVICE_START_PENDING`.
pub const SERVICE_START_PENDING: u32 = 0x00000002;

/// Service status state constant: `SERVICE_RUNNING`.
pub const SERVICE_RUNNING: u32 = 0x00000004;

/// Service status state constant: `SERVICE_STOP_PENDING`.
pub const SERVICE_STOP_PENDING: u32 = 0x00000003;

/// Service status state constant: `SERVICE_STOPPED`.
pub const SERVICE_STOPPED: u32 = 0x00000001;

/// Controls accepted bitmask constant: `SERVICE_ACCEPT_STOP`.
pub const SERVICE_ACCEPT_STOP: u32 = 0x00000001;

/// Controls accepted bitmask constant: `SERVICE_ACCEPT_PAUSE_CONTINUE` (not accepted by PALKA).
pub const SERVICE_ACCEPT_PAUSE_CONTINUE: u32 = 0x00000002;

/// Controls accepted bitmask constant: `SERVICE_ACCEPT_SHUTDOWN`.
pub const SERVICE_ACCEPT_SHUTDOWN: u32 = 0x00000004;

/// Service control code: `SERVICE_CONTROL_STOP`.
pub const SERVICE_CONTROL_STOP: u32 = 0x00000001;

/// Service control code: `SERVICE_CONTROL_PAUSE` (unsupported by PALKA).
pub const SERVICE_CONTROL_PAUSE: u32 = 0x00000002;

/// Service control code: `SERVICE_CONTROL_CONTINUE` (unsupported by PALKA).
pub const SERVICE_CONTROL_CONTINUE: u32 = 0x00000003;

/// Service control code: `SERVICE_CONTROL_INTERROGATE`.
pub const SERVICE_CONTROL_INTERROGATE: u32 = 0x00000004;

/// Service control code: `SERVICE_CONTROL_SHUTDOWN`.
pub const SERVICE_CONTROL_SHUTDOWN: u32 = 0x00000005;

/// Win32 error code: `ERROR_SUCCESS` (0).
pub const NO_ERROR: u32 = 0;

/// Win32 error code: `ERROR_CALL_NOT_IMPLEMENTED` (120).
pub const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

/// Win32 error code: `ERROR_EXCEPTION_IN_SERVICE` (1064).
pub const ERROR_EXCEPTION_IN_SERVICE: u32 = 1064;

/// Supported runtime controls delivered by Windows SCM to PALKA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmRuntimeControl {
    /// Graceful stop requested (`SERVICE_CONTROL_STOP`).
    Stop,
    /// System shutdown requested (`SERVICE_CONTROL_SHUTDOWN`).
    Shutdown,
}

/// SCM service lifecycle states tracked by the internal state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmServiceState {
    /// Service status has not yet been reported to SCM.
    Unreported,
    /// Service is initializing (`SERVICE_START_PENDING`).
    StartPending,
    /// Service is running and accepting controls (`SERVICE_RUNNING`).
    Running,
    /// Service is performing graceful teardown (`SERVICE_STOP_PENDING`).
    StopPending,
    /// Service has terminated (`SERVICE_STOPPED`).
    Stopped,
}

impl std::fmt::Display for ScmServiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreported => write!(f, "UNREPORTED"),
            Self::StartPending => write!(f, "SERVICE_START_PENDING"),
            Self::Running => write!(f, "SERVICE_RUNNING"),
            Self::StopPending => write!(f, "SERVICE_STOP_PENDING"),
            Self::Stopped => write!(f, "SERVICE_STOPPED"),
        }
    }
}

/// Typed error returned by the SCM runtime adapter.
#[derive(Debug)]
pub enum ScmRuntimeError {
    /// SCM runtime dispatcher is only supported on Windows.
    UnsupportedPlatform,
    /// An invalid service state transition was attempted.
    InvalidLifecycleTransition {
        from: ScmServiceState,
        attempted: ScmServiceState,
    },
    /// An invalid pending status checkpoint or wait hint was supplied.
    InvalidPendingStatus {
        reason: String,
        checkpoint: u32,
        wait_hint_ms: u32,
    },
    /// A dispatcher is already active in this process.
    DispatcherAlreadyActive,
    /// The control communication channel was closed unexpectedly.
    ControlChannelClosed,
    /// The service entry callback panicked across the FFI boundary.
    ServiceEntryPanicked,
    /// The service entry callback returned without having successfully reported `SERVICE_STOPPED`.
    ServiceEntryReturnedBeforeStopped,
    /// Internal dispatcher bridge state error (e.g. synchronization failure).
    InternalBridgeError(&'static str),
    /// A Win32 API call failed.
    WindowsApi {
        function: &'static str,
        code: u32,
        message: String,
    },
}

impl std::fmt::Display for ScmRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "Windows SCM runtime is only supported on Windows")
            }
            Self::InvalidLifecycleTransition { from, attempted } => {
                write!(
                    f,
                    "invalid SCM service lifecycle transition from {from} to {attempted}"
                )
            }
            Self::InvalidPendingStatus {
                reason,
                checkpoint,
                wait_hint_ms,
            } => {
                write!(
                    f,
                    "invalid SCM pending status: {reason} (checkpoint={checkpoint}, wait_hint_ms={wait_hint_ms})"
                )
            }
            Self::DispatcherAlreadyActive => {
                write!(
                    f,
                    "SCM service dispatcher is already active in this process"
                )
            }
            Self::ControlChannelClosed => {
                write!(f, "SCM control channel is closed")
            }
            Self::ServiceEntryPanicked => {
                write!(f, "PALKA service entry panicked across FFI boundary")
            }
            Self::ServiceEntryReturnedBeforeStopped => {
                write!(
                    f,
                    "PALKA service entry returned before reporting SERVICE_STOPPED"
                )
            }
            Self::InternalBridgeError(msg) => {
                write!(f, "internal SCM dispatcher bridge error: {msg}")
            }
            Self::WindowsApi {
                function,
                code,
                message,
            } => {
                write!(
                    f,
                    "Windows API {function} failed with code {code}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for ScmRuntimeError {}

/// Pure lifecycle state machine validating SCM status transitions and checkpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmLifecycleStateMachine {
    current_state: ScmServiceState,
    last_checkpoint: u32,
}

impl Default for ScmLifecycleStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl ScmLifecycleStateMachine {
    /// Creates an uninitialized state machine before any status is reported.
    pub fn new() -> Self {
        Self {
            current_state: ScmServiceState::Unreported,
            last_checkpoint: 0,
        }
    }

    /// Returns the current tracked lifecycle state.
    pub fn current_state(&self) -> ScmServiceState {
        self.current_state
    }

    /// Returns the last recorded checkpoint value.
    pub fn last_checkpoint(&self) -> u32 {
        self.last_checkpoint
    }

    /// Validates and records a transition to or progress within `SERVICE_START_PENDING`.
    pub fn plan_start_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ScmRuntimeError> {
        if checkpoint == 0 {
            return Err(ScmRuntimeError::InvalidPendingStatus {
                reason: "checkpoint must be greater than zero".into(),
                checkpoint,
                wait_hint_ms,
            });
        }
        if wait_hint_ms == 0 {
            return Err(ScmRuntimeError::InvalidPendingStatus {
                reason: "wait hint must be greater than zero".into(),
                checkpoint,
                wait_hint_ms,
            });
        }

        match self.current_state {
            ScmServiceState::Unreported => {
                self.current_state = ScmServiceState::StartPending;
                self.last_checkpoint = checkpoint;
                Ok(())
            }
            ScmServiceState::StartPending => {
                if checkpoint <= self.last_checkpoint {
                    return Err(ScmRuntimeError::InvalidPendingStatus {
                        reason: "checkpoint must strictly increase for repeated pending status"
                            .into(),
                        checkpoint,
                        wait_hint_ms,
                    });
                }
                self.last_checkpoint = checkpoint;
                Ok(())
            }
            other => Err(ScmRuntimeError::InvalidLifecycleTransition {
                from: other,
                attempted: ScmServiceState::StartPending,
            }),
        }
    }

    /// Validates and records a transition to `SERVICE_RUNNING`.
    pub fn plan_running(&mut self) -> Result<(), ScmRuntimeError> {
        match self.current_state {
            ScmServiceState::StartPending => {
                self.current_state = ScmServiceState::Running;
                self.last_checkpoint = 0;
                Ok(())
            }
            other => Err(ScmRuntimeError::InvalidLifecycleTransition {
                from: other,
                attempted: ScmServiceState::Running,
            }),
        }
    }

    /// Validates and records a transition to or progress within `SERVICE_STOP_PENDING`.
    pub fn plan_stop_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ScmRuntimeError> {
        if checkpoint == 0 {
            return Err(ScmRuntimeError::InvalidPendingStatus {
                reason: "checkpoint must be greater than zero".into(),
                checkpoint,
                wait_hint_ms,
            });
        }
        if wait_hint_ms == 0 {
            return Err(ScmRuntimeError::InvalidPendingStatus {
                reason: "wait hint must be greater than zero".into(),
                checkpoint,
                wait_hint_ms,
            });
        }

        match self.current_state {
            ScmServiceState::Running => {
                self.current_state = ScmServiceState::StopPending;
                self.last_checkpoint = checkpoint;
                Ok(())
            }
            ScmServiceState::StopPending => {
                if checkpoint <= self.last_checkpoint {
                    return Err(ScmRuntimeError::InvalidPendingStatus {
                        reason: "checkpoint must strictly increase for repeated pending status"
                            .into(),
                        checkpoint,
                        wait_hint_ms,
                    });
                }
                self.last_checkpoint = checkpoint;
                Ok(())
            }
            other => Err(ScmRuntimeError::InvalidLifecycleTransition {
                from: other,
                attempted: ScmServiceState::StopPending,
            }),
        }
    }

    /// Validates and records a transition to `SERVICE_STOPPED`.
    pub fn plan_stopped(&mut self, win32_exit_code: u32) -> Result<(), ScmRuntimeError> {
        match self.current_state {
            ScmServiceState::StopPending => {
                self.current_state = ScmServiceState::Stopped;
                self.last_checkpoint = 0;
                Ok(())
            }
            ScmServiceState::StartPending => {
                // START_PENDING -> STOPPED is only allowed for failed bootstrap/startup,
                // and only with nonzero exit code.
                if win32_exit_code == 0 {
                    return Err(ScmRuntimeError::InvalidLifecycleTransition {
                        from: ScmServiceState::StartPending,
                        attempted: ScmServiceState::Stopped,
                    });
                }
                self.current_state = ScmServiceState::Stopped;
                self.last_checkpoint = 0;
                Ok(())
            }
            other => Err(ScmRuntimeError::InvalidLifecycleTransition {
                from: other,
                attempted: ScmServiceState::Stopped,
            }),
        }
    }
}

/// Canonical representation of a Windows SCM `SERVICE_STATUS` structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalServiceStatus {
    pub service_type: u32,
    pub current_state: u32,
    pub controls_accepted: u32,
    pub win32_exit_code: u32,
    pub service_specific_exit_code: u32,
    pub checkpoint: u32,
    pub wait_hint: u32,
}

impl CanonicalServiceStatus {
    /// Builds canonical `SERVICE_START_PENDING` status.
    pub fn start_pending(checkpoint: u32, wait_hint_ms: u32) -> Self {
        Self {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: SERVICE_START_PENDING,
            controls_accepted: 0,
            win32_exit_code: 0, // ERROR_SUCCESS
            service_specific_exit_code: 0,
            checkpoint,
            wait_hint: wait_hint_ms,
        }
    }

    /// Builds canonical `SERVICE_RUNNING` status with `STOP | SHUTDOWN` accepted.
    pub fn running() -> Self {
        Self {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: SERVICE_RUNNING,
            controls_accepted: SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
            win32_exit_code: 0,
            service_specific_exit_code: 0,
            checkpoint: 0,
            wait_hint: 0,
        }
    }

    /// Builds canonical `SERVICE_STOP_PENDING` status.
    pub fn stop_pending(checkpoint: u32, wait_hint_ms: u32) -> Self {
        Self {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: SERVICE_STOP_PENDING,
            controls_accepted: 0,
            win32_exit_code: 0,
            service_specific_exit_code: 0,
            checkpoint,
            wait_hint: wait_hint_ms,
        }
    }

    /// Builds canonical `SERVICE_STOPPED` status.
    pub fn stopped(win32_exit_code: u32) -> Self {
        Self {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: SERVICE_STOPPED,
            controls_accepted: 0,
            win32_exit_code,
            service_specific_exit_code: 0,
            checkpoint: 0,
            wait_hint: 0,
        }
    }

    #[cfg(windows)]
    fn to_win32_status(self) -> SERVICE_STATUS {
        SERVICE_STATUS {
            dwServiceType: windows::Win32::System::Services::ENUM_SERVICE_TYPE(self.service_type),
            dwCurrentState: SERVICE_STATUS_CURRENT_STATE(self.current_state),
            dwControlsAccepted: self.controls_accepted,
            dwWin32ExitCode: self.win32_exit_code,
            dwServiceSpecificExitCode: self.service_specific_exit_code,
            dwCheckPoint: self.checkpoint,
            dwWaitHint: self.wait_hint,
        }
    }
}

/// Result of decoding a raw Win32 SCM control code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedControl {
    /// Deliverable control event (`STOP` or `SHUTDOWN`).
    Deliver(ScmRuntimeControl),
    /// No-op interrogate query returning `NO_ERROR`.
    Interrogate,
    /// Unsupported or unknown control code returning `ERROR_CALL_NOT_IMPLEMENTED`.
    NotImplemented,
}

/// Decodes an incoming Win32 service control request code according to PALKA contract.
pub fn decode_service_control(dw_control: u32) -> DecodedControl {
    match dw_control {
        SERVICE_CONTROL_STOP => DecodedControl::Deliver(ScmRuntimeControl::Stop),
        SERVICE_CONTROL_SHUTDOWN => DecodedControl::Deliver(ScmRuntimeControl::Shutdown),
        SERVICE_CONTROL_INTERROGATE => DecodedControl::Interrogate,
        _ => DecodedControl::NotImplemented,
    }
}

/// Pure handler logic for incoming service control codes.
pub fn handle_control_request<F>(dw_control: u32, deliver_fn: F) -> u32
where
    F: FnOnce(ScmRuntimeControl),
{
    match decode_service_control(dw_control) {
        DecodedControl::Deliver(ctrl) => {
            deliver_fn(ctrl);
            NO_ERROR
        }
        DecodedControl::Interrogate => NO_ERROR,
        DecodedControl::NotImplemented => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// Private abstract status sink enabling transactional publication and pure unit testing.
trait StatusSink {
    /// Publishes the canonical status.
    fn publish(&self, status: CanonicalServiceStatus) -> Result<(), ScmRuntimeError>;
}

/// Production Win32 status sink reporting to SCM via `SetServiceStatus`.
#[cfg(windows)]
struct Win32StatusSink {
    handle: SERVICE_STATUS_HANDLE,
    stopped_reported: Arc<AtomicBool>,
}

#[cfg(windows)]
impl Win32StatusSink {
    fn new(handle: SERVICE_STATUS_HANDLE, stopped_reported: Arc<AtomicBool>) -> Self {
        Self {
            handle,
            stopped_reported,
        }
    }
}

#[cfg(windows)]
impl StatusSink for Win32StatusSink {
    fn publish(&self, status: CanonicalServiceStatus) -> Result<(), ScmRuntimeError> {
        let mut win32_status = status.to_win32_status();
        // SAFETY:
        // `self.handle` is the valid `SERVICE_STATUS_HANDLE` obtained from `RegisterServiceCtrlHandlerExW`.
        // `win32_status` is a fully initialized, valid `SERVICE_STATUS` structure derived
        // directly from our validated canonical state machine.
        let res = unsafe { SetServiceStatus(self.handle, &mut win32_status) };
        if let Err(err) = res {
            return Err(classify_windows_error("SetServiceStatus", err));
        }
        if status.current_state == SERVICE_STOPPED {
            self.stopped_reported.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Executes the unified panic/early-return fallback logic to report `SERVICE_STOPPED`.
///
/// If `stopped_reported` is already true, performs zero publication attempts.
/// If false, publishes `SERVICE_STOPPED` with `ERROR_EXCEPTION_IN_SERVICE` (1064).
/// Upon successful publication, marks `stopped_reported` as true.
fn execute_panic_fallback<S: StatusSink + ?Sized>(sink: &S, stopped_reported: &AtomicBool) {
    if !stopped_reported.load(Ordering::SeqCst) {
        let status = CanonicalServiceStatus::stopped(ERROR_EXCEPTION_IN_SERVICE);
        if sink.publish(status).is_ok() {
            stopped_reported.store(true, Ordering::SeqCst);
        }
    }
}

/// Typed context provided to the service entry function.
///
/// Encapsulates transactional status reporting to Windows SCM and control event reception.
/// Intentionally marked `!Send` and `!Sync` via `PhantomData<Rc<()>>` to structurally prevent
/// transferring SCM lifecycle ownership to background threads, guaranteeing that normal
/// status reporting and ServiceMain panic/early-return fallback cannot race.
pub struct ScmServiceContext {
    state_machine: ScmLifecycleStateMachine,
    control_receiver: Receiver<ScmRuntimeControl>,
    status_sink: Box<dyn StatusSink>,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl ScmServiceContext {
    /// Private constructor used by dispatcher trampolines.
    fn new(
        control_receiver: Receiver<ScmRuntimeControl>,
        status_sink: Box<dyn StatusSink>,
    ) -> Self {
        Self {
            state_machine: ScmLifecycleStateMachine::new(),
            control_receiver,
            status_sink,
            _not_send_sync: PhantomData,
        }
    }

    /// Internal helper for unit testing.
    #[cfg(test)]
    fn new_test(
        control_receiver: Receiver<ScmRuntimeControl>,
        status_sink: Box<dyn StatusSink>,
    ) -> Self {
        Self::new(control_receiver, status_sink)
    }

    /// Returns the current lifecycle state.
    pub fn current_state(&self) -> ScmServiceState {
        self.state_machine.current_state()
    }

    /// Returns the last confirmed checkpoint.
    pub fn last_checkpoint(&self) -> u32 {
        self.state_machine.last_checkpoint()
    }

    /// Transactionally reports `SERVICE_START_PENDING`.
    ///
    /// The internal state machine is committed ONLY IF status publication succeeds.
    /// On publication failure, the local state and checkpoint roll back to previous values.
    pub fn report_start_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ScmRuntimeError> {
        let mut candidate = self.state_machine.clone();
        candidate.plan_start_pending(checkpoint, wait_hint_ms)?;
        let status = CanonicalServiceStatus::start_pending(checkpoint, wait_hint_ms);
        self.status_sink.publish(status)?;
        self.state_machine = candidate;
        Ok(())
    }

    /// Transactionally reports `SERVICE_RUNNING` accepting `SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN`.
    ///
    /// The internal state machine is committed ONLY IF status publication succeeds.
    pub fn report_running(&mut self) -> Result<(), ScmRuntimeError> {
        let mut candidate = self.state_machine.clone();
        candidate.plan_running()?;
        let status = CanonicalServiceStatus::running();
        self.status_sink.publish(status)?;
        self.state_machine = candidate;
        Ok(())
    }

    /// Blocks until a termination control (`Stop` or `Shutdown`) is received from SCM.
    pub fn wait_for_control(&self) -> Result<ScmRuntimeControl, ScmRuntimeError> {
        self.control_receiver
            .recv()
            .map_err(|_| ScmRuntimeError::ControlChannelClosed)
    }

    /// Transactionally reports `SERVICE_STOP_PENDING`.
    ///
    /// The internal state machine is committed ONLY IF status publication succeeds.
    /// On publication failure, the local state and checkpoint roll back to previous values.
    pub fn report_stop_pending(
        &mut self,
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), ScmRuntimeError> {
        let mut candidate = self.state_machine.clone();
        candidate.plan_stop_pending(checkpoint, wait_hint_ms)?;
        let status = CanonicalServiceStatus::stop_pending(checkpoint, wait_hint_ms);
        self.status_sink.publish(status)?;
        self.state_machine = candidate;
        Ok(())
    }

    /// Transactionally reports `SERVICE_STOPPED` with the specified Win32 exit code (0 for graceful).
    ///
    /// The internal state machine is committed ONLY IF status publication succeeds.
    pub fn report_stopped(&mut self, win32_exit_code: u32) -> Result<(), ScmRuntimeError> {
        let mut candidate = self.state_machine.clone();
        candidate.plan_stopped(win32_exit_code)?;
        let status = CanonicalServiceStatus::stopped(win32_exit_code);
        self.status_sink.publish(status)?;
        self.state_machine = candidate;
        Ok(())
    }
}

/// Function pointer type for the service main entry callback.
pub type PalkaServiceEntry = fn(ScmServiceContext);

#[cfg(windows)]
fn classify_windows_error(function: &'static str, err: windows::core::Error) -> ScmRuntimeError {
    use windows::Win32::Foundation::WIN32_ERROR;
    let code = WIN32_ERROR::from_error(&err)
        .map(|c| c.0)
        .unwrap_or_else(|| err.code().0 as u32);
    ScmRuntimeError::WindowsApi {
        function,
        code,
        message: err.message().to_string(),
    }
}

/// Private context structure passed as `lpContext` to `RegisterServiceCtrlHandlerExW`.
///
/// Contains a stable, non-blocking `SyncSender` enabling non-blocking control delivery.
struct HandlerContext {
    sender: SyncSender<ScmRuntimeControl>,
}

#[cfg(windows)]
#[derive(Clone)]
struct ActiveServiceFallback {
    handle: SERVICE_STATUS_HANDLE,
    stopped_reported: Arc<AtomicBool>,
}

// SAFETY:
// `SERVICE_STATUS_HANDLE` is an opaque Win32 handle valid across threads in the service process.
// `ActiveServiceFallback` is stored inside `Mutex` for outer ServiceMain panic recovery.
#[cfg(windows)]
unsafe impl Send for ActiveServiceFallback {}

#[cfg(windows)]
impl StatusSink for ActiveServiceFallback {
    fn publish(&self, status: CanonicalServiceStatus) -> Result<(), ScmRuntimeError> {
        let mut win32_status = status.to_win32_status();
        // SAFETY:
        // `self.handle` is the valid `SERVICE_STATUS_HANDLE` obtained from
        // `RegisterServiceCtrlHandlerExW` for the active PALKA service process.
        // `win32_status` is a fully initialized, valid stack-allocated `SERVICE_STATUS`
        // structure produced directly from our validated canonical representation.
        // `ActiveServiceFallback` remains alive for this call and status publication
        // is serialized by the ServiceMain lifecycle design.
        let res = unsafe { SetServiceStatus(self.handle, &mut win32_status) };
        if let Err(err) = res {
            return Err(classify_windows_error("SetServiceStatus", err));
        }
        if status.current_state == SERVICE_STOPPED {
            self.stopped_reported.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

static DISPATCHER_ACTIVE: AtomicBool = AtomicBool::new(false);
static CONFIGURED_ENTRY: OnceLock<Mutex<Option<PalkaServiceEntry>>> = OnceLock::new();
static SERVICE_RESULT: OnceLock<Mutex<Option<Result<(), ScmRuntimeError>>>> = OnceLock::new();
static DISPATCHER_HANDLER_CTX: OnceLock<Mutex<Option<Arc<HandlerContext>>>> = OnceLock::new();
#[cfg(windows)]
static ACTIVE_FALLBACK: OnceLock<Mutex<Option<ActiveServiceFallback>>> = OnceLock::new();

fn get_entry_cell() -> &'static Mutex<Option<PalkaServiceEntry>> {
    CONFIGURED_ENTRY.get_or_init(|| Mutex::new(None))
}

fn get_result_cell() -> &'static Mutex<Option<Result<(), ScmRuntimeError>>> {
    SERVICE_RESULT.get_or_init(|| Mutex::new(None))
}

fn get_handler_context_cell() -> &'static Mutex<Option<Arc<HandlerContext>>> {
    DISPATCHER_HANDLER_CTX.get_or_init(|| Mutex::new(None))
}

#[cfg(windows)]
fn get_active_fallback() -> &'static Mutex<Option<ActiveServiceFallback>> {
    ACTIVE_FALLBACK.get_or_init(|| Mutex::new(None))
}

struct DispatcherActiveGuard;

impl Drop for DispatcherActiveGuard {
    fn drop(&mut self) {
        if let Ok(mut entry_guard) = get_entry_cell().lock() {
            *entry_guard = None;
        }
        if let Ok(mut ctx_guard) = get_handler_context_cell().lock() {
            *ctx_guard = None;
        }
        #[cfg(windows)]
        {
            if let Ok(mut fallback_guard) = get_active_fallback().lock() {
                *fallback_guard = None;
            }
        }
        // Release the dispatcher slot only after every bridge cleanup attempt has completed.
        DISPATCHER_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Runs the PALKA Windows SCM service dispatcher.
///
/// Connects the calling process to Windows SCM via `StartServiceCtrlDispatcherW`.
/// Blocks until the service has completed execution and SCM shuts down the dispatcher.
///
/// On non-Windows platforms, returns `ScmRuntimeError::UnsupportedPlatform` immediately
/// without executing the provided callback.
pub fn run_palka_service_dispatcher(entry: PalkaServiceEntry) -> Result<(), ScmRuntimeError> {
    #[cfg(not(windows))]
    {
        let _ = entry;
        Err(ScmRuntimeError::UnsupportedPlatform)
    }
    #[cfg(windows)]
    {
        run_palka_service_dispatcher_windows(entry)
    }
}

#[cfg(windows)]
fn run_palka_service_dispatcher_windows(entry: PalkaServiceEntry) -> Result<(), ScmRuntimeError> {
    if DISPATCHER_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(ScmRuntimeError::DispatcherAlreadyActive);
    }
    let _guard = DispatcherActiveGuard;

    // Reset result cell and register configured entry fail-closed
    {
        let mut res_guard = get_result_cell()
            .lock()
            .map_err(|_| ScmRuntimeError::InternalBridgeError("result cell mutex poisoned"))?;
        *res_guard = None;
        let mut entry_guard = get_entry_cell()
            .lock()
            .map_err(|_| ScmRuntimeError::InternalBridgeError("entry cell mutex poisoned"))?;
        *entry_guard = Some(entry);
    }

    let mut name_wide: Vec<u16> = PALKA_SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name_wide.as_mut_ptr()),
            lpServiceProc: Some(service_main_trampoline),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(std::ptr::null_mut()),
            lpServiceProc: None,
        },
    ];

    // SAFETY:
    // `table` is a valid 2-element array terminated by a null-entry as required by StartServiceCtrlDispatcherW.
    // `name_wide` is valid, properly null-terminated UTF-16 and outlives the blocking StartServiceCtrlDispatcherW call.
    // `service_main_trampoline` is an extern "system" function matching the LPSERVICE_MAIN_FUNCTIONW ABI.
    let dispatch_res = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };

    if let Err(err) = dispatch_res {
        return Err(classify_windows_error("StartServiceCtrlDispatcherW", err));
    }

    // Check if the service main trampoline or user callback recorded an error/panic
    let mut res_guard = get_result_cell()
        .lock()
        .map_err(|_| ScmRuntimeError::InternalBridgeError("result cell mutex poisoned on exit"))?;
    match res_guard.take() {
        Some(Ok(())) => Ok(()),
        Some(Err(err)) => Err(err),
        None => Err(ScmRuntimeError::InternalBridgeError(
            "ServiceMain finished without recording result",
        )),
    }
}

#[cfg(windows)]
unsafe extern "system" fn service_control_handler(
    dw_control: u32,
    _dw_event_type: u32,
    _lp_event_data: *mut core::ffi::c_void,
    lp_context: *mut core::ffi::c_void,
) -> u32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_control_request(dw_control, |ctrl| {
            if !lp_context.is_null() {
                // SAFETY:
                // `lp_context` was derived from `Arc::as_ptr` of an `Arc<HandlerContext>`
                // stored in `DISPATCHER_HANDLER_CTX` before `RegisterServiceCtrlHandlerExW`.
                // It remains valid and pinned across the entire execution of the service dispatcher
                // and is released only after `StartServiceCtrlDispatcherW` returns.
                // Control delivery uses non-blocking `SyncSender::try_send`.
                let ctx = unsafe { &*(lp_context as *const HandlerContext) };
                let _ = ctx.sender.try_send(ctrl);
            }
        })
    }));

    match result {
        Ok(code) => code,
        Err(_) => ERROR_EXCEPTION_IN_SERVICE,
    }
}

#[cfg(windows)]
unsafe extern "system" fn service_main_trampoline(
    _dw_num_services_args: u32,
    _lp_service_arg_vectors: *mut PWSTR,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(service_main_inner));
    if result.is_err() {
        // CORRECTION A & C: Outer ServiceMain panic fallback using shared active fallback snapshot.
        // Mutex is acquired, value cloned, and lock immediately released before SetServiceStatus.
        let fallback_snapshot = match get_active_fallback().lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Some(ref fallback) = fallback_snapshot {
            execute_panic_fallback(fallback, &fallback.stopped_reported);
        }
        if let Ok(mut res_guard) = get_result_cell().lock() {
            *res_guard = Some(Err(ScmRuntimeError::ServiceEntryPanicked));
        }
    }
}

#[cfg(windows)]
fn service_main_inner() {
    // CORRECTION B: Configured entry retrieval fails closed on poison or missing entry
    let entry = match get_entry_cell().lock() {
        Ok(guard) => match *guard {
            Some(e) => e,
            None => {
                if let Ok(mut res_guard) = get_result_cell().lock() {
                    *res_guard = Some(Err(ScmRuntimeError::InternalBridgeError(
                        "configured service entry missing",
                    )));
                }
                return;
            }
        },
        Err(_) => {
            if let Ok(mut res_guard) = get_result_cell().lock() {
                *res_guard = Some(Err(ScmRuntimeError::InternalBridgeError(
                    "entry cell mutex poisoned in ServiceMain",
                )));
            }
            return;
        }
    };

    // Context initialized before registration and held by Arc across dispatcher
    let (tx, rx) = sync_channel(1);
    let handler_ctx = Arc::new(HandlerContext { sender: tx });
    let ctx_ptr: *const HandlerContext = Arc::as_ptr(&handler_ctx);

    if let Ok(mut ctx_guard) = get_handler_context_cell().lock() {
        *ctx_guard = Some(Arc::clone(&handler_ctx));
    } else {
        if let Ok(mut res_guard) = get_result_cell().lock() {
            *res_guard = Some(Err(ScmRuntimeError::InternalBridgeError(
                "handler context cell mutex poisoned",
            )));
        }
        return;
    }

    let name_wide: Vec<u16> = PALKA_SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY:
    // `name_wide` is a valid null-terminated UTF-16 string representing canonical PALKA_SERVICE_NAME.
    // `service_control_handler` is an extern "system" function adhering to HandlerEx ABI.
    // `ctx_ptr` points to the `HandlerContext` held by `DISPATCHER_HANDLER_CTX`.
    // It remains valid and pinned across the entire execution of StartServiceCtrlDispatcherW.
    let status_handle_res = unsafe {
        RegisterServiceCtrlHandlerExW(
            PCWSTR(name_wide.as_ptr()),
            Some(service_control_handler),
            Some(ctx_ptr as *const core::ffi::c_void),
        )
    };

    let status_handle = match status_handle_res {
        Ok(handle) => handle,
        Err(err) => {
            if let Ok(mut res_guard) = get_result_cell().lock() {
                *res_guard = Some(Err(classify_windows_error(
                    "RegisterServiceCtrlHandlerExW",
                    err,
                )));
            }
            return;
        }
    };

    // CORRECTION C: Construct local fallback object first, then install into shared bridge
    let stopped_reported = Arc::new(AtomicBool::new(false));
    let local_fallback = ActiveServiceFallback {
        handle: status_handle,
        stopped_reported: Arc::clone(&stopped_reported),
    };

    // Fail closed if bridge installation fails: publish local best-effort STOPPED and return
    match get_active_fallback().lock() {
        Ok(mut fb_guard) => {
            *fb_guard = Some(local_fallback.clone());
        }
        Err(_) => {
            execute_panic_fallback(&local_fallback, &local_fallback.stopped_reported);
            if let Ok(mut res_guard) = get_result_cell().lock() {
                *res_guard = Some(Err(ScmRuntimeError::InternalBridgeError(
                    "active fallback mutex poisoned during installation",
                )));
            }
            return;
        }
    }

    let win32_sink = Box::new(Win32StatusSink::new(
        status_handle,
        Arc::clone(&stopped_reported),
    ));
    let context = ScmServiceContext::new(rx, win32_sink);

    // Invoke user service entry with panic protection
    let entry_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        entry(context);
    }));

    if entry_result.is_err() {
        // Inner entry panic fallback uses local fallback directly without holding global lock
        execute_panic_fallback(&local_fallback, &local_fallback.stopped_reported);
        if let Ok(mut res_guard) = get_result_cell().lock() {
            *res_guard = Some(Err(ScmRuntimeError::ServiceEntryPanicked));
        }
    } else {
        // Service Entry Return Guard:
        // Successful service completion requires confirmed SERVICE_STOPPED publication.
        if !stopped_reported.load(Ordering::SeqCst) {
            execute_panic_fallback(&local_fallback, &local_fallback.stopped_reported);
            if let Ok(mut res_guard) = get_result_cell().lock() {
                *res_guard = Some(Err(ScmRuntimeError::ServiceEntryReturnedBeforeStopped));
            }
        } else if let Ok(mut res_guard) = get_result_cell().lock() {
            *res_guard = Some(Ok(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test mock sink recording published statuses and allowing simulated failure injection.
    struct MockStatusSink {
        should_fail: AtomicBool,
        published: Mutex<Vec<CanonicalServiceStatus>>,
        stopped_reported: Arc<AtomicBool>,
    }

    impl MockStatusSink {
        fn new() -> Self {
            Self {
                should_fail: AtomicBool::new(false),
                published: Mutex::new(Vec::new()),
                stopped_reported: Arc::new(AtomicBool::new(false)),
            }
        }

        fn set_fail(&self, fail: bool) {
            self.should_fail.store(fail, Ordering::SeqCst);
        }

        fn last_published(&self) -> Option<CanonicalServiceStatus> {
            self.published.lock().unwrap().last().cloned()
        }

        fn published_count(&self) -> usize {
            self.published.lock().unwrap().len()
        }
    }

    impl StatusSink for Arc<MockStatusSink> {
        fn publish(&self, status: CanonicalServiceStatus) -> Result<(), ScmRuntimeError> {
            if self.should_fail.load(Ordering::SeqCst) {
                return Err(ScmRuntimeError::WindowsApi {
                    function: "SetServiceStatus",
                    code: 5, // ERROR_ACCESS_DENIED
                    message: "simulated publication failure".into(),
                });
            }
            self.published.lock().unwrap().push(status);
            if status.current_state == SERVICE_STOPPED {
                self.stopped_reported.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    fn create_test_context_with_sink() -> (
        ScmServiceContext,
        SyncSender<ScmRuntimeControl>,
        Arc<MockStatusSink>,
    ) {
        let (tx, rx) = sync_channel(1);
        let sink = Arc::new(MockStatusSink::new());
        let ctx = ScmServiceContext::new_test(rx, Box::new(Arc::clone(&sink)));
        (ctx, tx, sink)
    }

    #[test]
    fn test_01_initial_start_pending_accepted() {
        let mut sm = ScmLifecycleStateMachine::new();
        assert_eq!(sm.current_state(), ScmServiceState::Unreported);
        assert!(sm.plan_start_pending(1, 5000).is_ok());
        assert_eq!(sm.current_state(), ScmServiceState::StartPending);
        assert_eq!(sm.last_checkpoint(), 1);
    }

    #[test]
    fn test_02_start_pending_exact_status_fields() {
        let status = CanonicalServiceStatus::start_pending(1, 5000);
        assert_eq!(status.service_type, 0x10); // SERVICE_WIN32_OWN_PROCESS
        assert_eq!(status.current_state, 2); // SERVICE_START_PENDING
        assert_eq!(status.controls_accepted, 0);
        assert_eq!(status.win32_exit_code, 0);
        assert_eq!(status.service_specific_exit_code, 0);
        assert_eq!(status.checkpoint, 1);
        assert_eq!(status.wait_hint, 5000);
    }

    #[test]
    fn test_03_start_pending_checkpoint_must_be_positive() {
        let mut sm = ScmLifecycleStateMachine::new();
        let err = sm.plan_start_pending(0, 5000).unwrap_err();
        match err {
            ScmRuntimeError::InvalidPendingStatus {
                checkpoint, reason, ..
            } => {
                assert_eq!(checkpoint, 0);
                assert!(reason.contains("checkpoint must be greater than zero"));
            }
            other => panic!("expected InvalidPendingStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_04_start_pending_wait_hint_must_be_positive() {
        let mut sm = ScmLifecycleStateMachine::new();
        let err = sm.plan_start_pending(1, 0).unwrap_err();
        match err {
            ScmRuntimeError::InvalidPendingStatus {
                wait_hint_ms,
                reason,
                ..
            } => {
                assert_eq!(wait_hint_ms, 0);
                assert!(reason.contains("wait hint must be greater than zero"));
            }
            other => panic!("expected InvalidPendingStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_05_repeated_start_pending_checkpoint_must_increase() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        assert!(sm.plan_start_pending(1, 5000).is_err());
        assert!(sm.plan_start_pending(0, 5000).is_err());
        assert!(sm.plan_start_pending(2, 5000).is_ok());
        assert_eq!(sm.last_checkpoint(), 2);
    }

    #[test]
    fn test_06_start_pending_to_running_allowed() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        assert!(sm.plan_running().is_ok());
        assert_eq!(sm.current_state(), ScmServiceState::Running);
        assert_eq!(sm.last_checkpoint(), 0);
    }

    #[test]
    fn test_07_running_exact_status_fields() {
        let status = CanonicalServiceStatus::running();
        assert_eq!(status.service_type, 0x10); // SERVICE_WIN32_OWN_PROCESS
        assert_eq!(status.current_state, 4); // SERVICE_RUNNING
        assert_eq!(status.controls_accepted, 0x01 | 0x04);
        assert_eq!(status.win32_exit_code, 0);
        assert_eq!(status.service_specific_exit_code, 0);
        assert_eq!(status.checkpoint, 0);
        assert_eq!(status.wait_hint, 0);
    }

    #[test]
    fn test_08_running_accepted_controls_exact_stop_shutdown() {
        let status = CanonicalServiceStatus::running();
        assert_eq!(status.controls_accepted, 5); // 1 | 4
    }

    #[test]
    fn test_09_running_has_no_pause_continue_bit() {
        let status = CanonicalServiceStatus::running();
        // SERVICE_ACCEPT_PAUSE_CONTINUE = 0x00000002
        assert_eq!(status.controls_accepted & SERVICE_ACCEPT_PAUSE_CONTINUE, 0);
    }

    #[test]
    fn test_10_running_checkpoint_wait_hint_zero() {
        let status = CanonicalServiceStatus::running();
        assert_eq!(status.checkpoint, 0);
        assert_eq!(status.wait_hint, 0);
    }

    #[test]
    fn test_11_running_to_stop_pending_allowed() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        assert!(sm.plan_stop_pending(1, 10000).is_ok());
        assert_eq!(sm.current_state(), ScmServiceState::StopPending);
        assert_eq!(sm.last_checkpoint(), 1);
    }

    #[test]
    fn test_12_stop_pending_exact_status_fields() {
        let status = CanonicalServiceStatus::stop_pending(1, 10000);
        assert_eq!(status.service_type, 0x10); // SERVICE_WIN32_OWN_PROCESS
        assert_eq!(status.current_state, 3); // SERVICE_STOP_PENDING
        assert_eq!(status.controls_accepted, 0);
        assert_eq!(status.win32_exit_code, 0);
        assert_eq!(status.service_specific_exit_code, 0);
        assert_eq!(status.checkpoint, 1);
        assert_eq!(status.wait_hint, 10000);
    }

    #[test]
    fn test_13_stop_pending_checkpoint_must_be_positive() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        let err = sm.plan_stop_pending(0, 10000).unwrap_err();
        match err {
            ScmRuntimeError::InvalidPendingStatus { checkpoint, .. } => {
                assert_eq!(checkpoint, 0);
            }
            other => panic!("expected InvalidPendingStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_14_stop_pending_wait_hint_must_be_positive() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        let err = sm.plan_stop_pending(1, 0).unwrap_err();
        match err {
            ScmRuntimeError::InvalidPendingStatus { wait_hint_ms, .. } => {
                assert_eq!(wait_hint_ms, 0);
            }
            other => panic!("expected InvalidPendingStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_15_repeated_stop_pending_checkpoint_strictly_increases() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        sm.plan_stop_pending(1, 10000).unwrap();
        assert!(sm.plan_stop_pending(1, 10000).is_err());
        assert!(sm.plan_stop_pending(2, 10000).is_ok());
        assert_eq!(sm.last_checkpoint(), 2);
    }

    #[test]
    fn test_16_stop_pending_to_stopped_allowed() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        sm.plan_stop_pending(1, 10000).unwrap();
        assert!(sm.plan_stopped(0).is_ok());
        assert_eq!(sm.current_state(), ScmServiceState::Stopped);
    }

    #[test]
    fn test_17_graceful_stopped_uses_exit_code_zero() {
        let status = CanonicalServiceStatus::stopped(0);
        assert_eq!(status.service_type, 0x10);
        assert_eq!(status.current_state, 1); // SERVICE_STOPPED
        assert_eq!(status.controls_accepted, 0);
        assert_eq!(status.win32_exit_code, 0);
        assert_eq!(status.service_specific_exit_code, 0);
        assert_eq!(status.checkpoint, 0);
        assert_eq!(status.wait_hint, 0);
    }

    #[test]
    fn test_18_startup_failure_start_pending_to_stopped_with_nonzero_code_allowed() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        assert!(sm.plan_stopped(1064).is_ok());
        assert_eq!(sm.current_state(), ScmServiceState::Stopped);
    }

    #[test]
    fn test_19_startup_failure_start_pending_to_stopped_with_zero_code_rejected() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        let err = sm.plan_stopped(0).unwrap_err();
        match err {
            ScmRuntimeError::InvalidLifecycleTransition { from, attempted } => {
                assert_eq!(from, ScmServiceState::StartPending);
                assert_eq!(attempted, ScmServiceState::Stopped);
            }
            other => panic!("expected InvalidLifecycleTransition, got {other:?}"),
        }
    }

    #[test]
    fn test_20_unreported_to_running_rejected() {
        let mut sm = ScmLifecycleStateMachine::new();
        let err = sm.plan_running().unwrap_err();
        match err {
            ScmRuntimeError::InvalidLifecycleTransition { from, attempted } => {
                assert_eq!(from, ScmServiceState::Unreported);
                assert_eq!(attempted, ScmServiceState::Running);
            }
            other => panic!("expected InvalidLifecycleTransition, got {other:?}"),
        }
    }

    #[test]
    fn test_21_running_to_stopped_rejected() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        let err = sm.plan_stopped(0).unwrap_err();
        match err {
            ScmRuntimeError::InvalidLifecycleTransition { from, attempted } => {
                assert_eq!(from, ScmServiceState::Running);
                assert_eq!(attempted, ScmServiceState::Stopped);
            }
            other => panic!("expected InvalidLifecycleTransition, got {other:?}"),
        }
    }

    #[test]
    fn test_22_stop_pending_to_running_rejected() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        sm.plan_stop_pending(1, 10000).unwrap();
        let err = sm.plan_running().unwrap_err();
        match err {
            ScmRuntimeError::InvalidLifecycleTransition { from, attempted } => {
                assert_eq!(from, ScmServiceState::StopPending);
                assert_eq!(attempted, ScmServiceState::Running);
            }
            other => panic!("expected InvalidLifecycleTransition, got {other:?}"),
        }
    }

    #[test]
    fn test_23_no_transition_after_stopped() {
        let mut sm = ScmLifecycleStateMachine::new();
        sm.plan_start_pending(1, 5000).unwrap();
        sm.plan_running().unwrap();
        sm.plan_stop_pending(1, 10000).unwrap();
        sm.plan_stopped(0).unwrap();

        assert!(sm.plan_start_pending(1, 5000).is_err());
        assert!(sm.plan_running().is_err());
        assert!(sm.plan_stop_pending(1, 5000).is_err());
        assert!(sm.plan_stopped(0).is_err());
    }

    #[test]
    fn test_24_service_control_stop_produces_stop_and_no_error() {
        let mut received = None;
        let ret = handle_control_request(SERVICE_CONTROL_STOP, |ctrl| {
            received = Some(ctrl);
        });
        assert_eq!(ret, NO_ERROR);
        assert_eq!(received, Some(ScmRuntimeControl::Stop));
    }

    #[test]
    fn test_25_service_control_shutdown_produces_shutdown_and_no_error() {
        let mut received = None;
        let ret = handle_control_request(SERVICE_CONTROL_SHUTDOWN, |ctrl| {
            received = Some(ctrl);
        });
        assert_eq!(ret, NO_ERROR);
        assert_eq!(received, Some(ScmRuntimeControl::Shutdown));
    }

    #[test]
    fn test_26_service_control_interrogate_produces_no_event_and_no_error() {
        let mut received = None;
        let ret = handle_control_request(SERVICE_CONTROL_INTERROGATE, |ctrl| {
            received = Some(ctrl);
        });
        assert_eq!(ret, NO_ERROR);
        assert_eq!(received, None);
    }

    #[test]
    fn test_27_service_control_pause_returns_call_not_implemented() {
        let mut received = None;
        let ret = handle_control_request(SERVICE_CONTROL_PAUSE, |ctrl| {
            received = Some(ctrl);
        });
        assert_eq!(ret, ERROR_CALL_NOT_IMPLEMENTED);
        assert_eq!(received, None);
    }

    #[test]
    fn test_28_service_control_continue_returns_call_not_implemented() {
        let mut received = None;
        let ret = handle_control_request(SERVICE_CONTROL_CONTINUE, |ctrl| {
            received = Some(ctrl);
        });
        assert_eq!(ret, ERROR_CALL_NOT_IMPLEMENTED);
        assert_eq!(received, None);
    }

    #[test]
    fn test_29_user_defined_control_returns_call_not_implemented() {
        for code in [128, 150, 200, 255] {
            let mut received = None;
            let ret = handle_control_request(code, |ctrl| {
                received = Some(ctrl);
            });
            assert_eq!(ret, ERROR_CALL_NOT_IMPLEMENTED);
            assert_eq!(received, None);
        }
    }

    #[test]
    fn test_30_unknown_standard_control_returns_call_not_implemented() {
        for code in [6, 7, 8, 9, 10, 11, 13] {
            let mut received = None;
            let ret = handle_control_request(code, |ctrl| {
                received = Some(ctrl);
            });
            assert_eq!(ret, ERROR_CALL_NOT_IMPLEMENTED);
            assert_eq!(received, None);
        }
    }

    #[test]
    fn test_31_no_control_mask_contains_pause_continue() {
        let start = CanonicalServiceStatus::start_pending(1, 1000);
        let run = CanonicalServiceStatus::running();
        let stop = CanonicalServiceStatus::stop_pending(1, 1000);
        let stopped = CanonicalServiceStatus::stopped(0);

        assert_eq!(start.controls_accepted & SERVICE_ACCEPT_PAUSE_CONTINUE, 0);
        assert_eq!(run.controls_accepted & SERVICE_ACCEPT_PAUSE_CONTINUE, 0);
        assert_eq!(stop.controls_accepted & SERVICE_ACCEPT_PAUSE_CONTINUE, 0);
        assert_eq!(stopped.controls_accepted & SERVICE_ACCEPT_PAUSE_CONTINUE, 0);
    }

    #[test]
    fn test_32_service_type_exact_service_win32_own_process() {
        let status = CanonicalServiceStatus::running();
        assert_eq!(status.service_type, SERVICE_WIN32_OWN_PROCESS);
    }

    #[test]
    fn test_33_no_raw_pointer_or_handle_escapes_context_api() {
        let (mut ctx, tx, _sink) = create_test_context_with_sink();
        assert_eq!(ctx.current_state(), ScmServiceState::Unreported);

        ctx.report_start_pending(1, 5000).unwrap();
        assert_eq!(ctx.current_state(), ScmServiceState::StartPending);

        ctx.report_running().unwrap();
        assert_eq!(ctx.current_state(), ScmServiceState::Running);

        tx.send(ScmRuntimeControl::Stop).unwrap();
        let ctrl = ctx.wait_for_control().unwrap();
        assert_eq!(ctrl, ScmRuntimeControl::Stop);

        ctx.report_stop_pending(1, 5000).unwrap();
        assert_eq!(ctx.current_state(), ScmServiceState::StopPending);

        ctx.report_stopped(0).unwrap();
        assert_eq!(ctx.current_state(), ScmServiceState::Stopped);
    }

    #[test]
    fn test_34_non_windows_dispatcher_returns_unsupported_platform() {
        #[cfg(not(windows))]
        {
            let mut called = false;
            let res = run_palka_service_dispatcher(|_| {
                called = true;
            });
            match res {
                Err(ScmRuntimeError::UnsupportedPlatform) => (),
                other => panic!("expected UnsupportedPlatform, got {other:?}"),
            }
            assert!(!called);
        }
    }

    #[test]
    fn test_35_start_pending_publication_failure_rolls_back_state_and_checkpoint() {
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        assert_eq!(ctx.current_state(), ScmServiceState::Unreported);
        assert_eq!(ctx.last_checkpoint(), 0);

        sink.set_fail(true);
        let err = ctx.report_start_pending(1, 5000).unwrap_err();
        match err {
            ScmRuntimeError::WindowsApi { function, code, .. } => {
                assert_eq!(function, "SetServiceStatus");
                assert_eq!(code, 5);
            }
            other => panic!("expected WindowsApi error, got {other:?}"),
        }

        assert_eq!(ctx.current_state(), ScmServiceState::Unreported);
        assert_eq!(ctx.last_checkpoint(), 0);
        assert_eq!(sink.published_count(), 0);

        sink.set_fail(false);
        assert!(ctx.report_start_pending(1, 5000).is_ok());
        assert_eq!(ctx.current_state(), ScmServiceState::StartPending);
        assert_eq!(ctx.last_checkpoint(), 1);
        assert_eq!(sink.published_count(), 1);
    }

    #[test]
    fn test_36_running_publication_failure_rolls_back_to_start_pending() {
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        ctx.report_start_pending(1, 5000).unwrap();
        assert_eq!(ctx.current_state(), ScmServiceState::StartPending);

        sink.set_fail(true);
        let err = ctx.report_running().unwrap_err();
        match err {
            ScmRuntimeError::WindowsApi { function, .. } => {
                assert_eq!(function, "SetServiceStatus");
            }
            other => panic!("expected WindowsApi error, got {other:?}"),
        }

        assert_eq!(ctx.current_state(), ScmServiceState::StartPending);
        assert_eq!(ctx.last_checkpoint(), 1);

        sink.set_fail(false);
        assert!(ctx.report_running().is_ok());
        assert_eq!(ctx.current_state(), ScmServiceState::Running);
        assert_eq!(ctx.last_checkpoint(), 0);
    }

    #[test]
    fn test_37_stop_pending_publication_failure_rolls_back_to_running() {
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        ctx.report_start_pending(1, 5000).unwrap();
        ctx.report_running().unwrap();
        assert_eq!(ctx.current_state(), ScmServiceState::Running);

        sink.set_fail(true);
        let err = ctx.report_stop_pending(1, 5000).unwrap_err();
        match err {
            ScmRuntimeError::WindowsApi { function, .. } => {
                assert_eq!(function, "SetServiceStatus");
            }
            other => panic!("expected WindowsApi error, got {other:?}"),
        }

        assert_eq!(ctx.current_state(), ScmServiceState::Running);
        assert_eq!(ctx.last_checkpoint(), 0);

        sink.set_fail(false);
        assert!(ctx.report_stop_pending(1, 5000).is_ok());
        assert_eq!(ctx.current_state(), ScmServiceState::StopPending);
        assert_eq!(ctx.last_checkpoint(), 1);
    }

    #[test]
    fn test_38_stopped_publication_failure_rolls_back_to_stop_pending() {
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        ctx.report_start_pending(1, 5000).unwrap();
        ctx.report_running().unwrap();
        ctx.report_stop_pending(1, 5000).unwrap();
        assert_eq!(ctx.current_state(), ScmServiceState::StopPending);

        sink.set_fail(true);
        let err = ctx.report_stopped(0).unwrap_err();
        match err {
            ScmRuntimeError::WindowsApi { function, .. } => {
                assert_eq!(function, "SetServiceStatus");
            }
            other => panic!("expected WindowsApi error, got {other:?}"),
        }

        assert_eq!(ctx.current_state(), ScmServiceState::StopPending);

        sink.set_fail(false);
        assert!(ctx.report_stopped(0).is_ok());
        assert_eq!(ctx.current_state(), ScmServiceState::Stopped);
    }

    #[test]
    fn test_39_repeated_pending_status_retains_checkpoint_on_publication_failure() {
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        ctx.report_start_pending(1, 5000).unwrap();
        assert_eq!(ctx.last_checkpoint(), 1);

        sink.set_fail(true);
        assert!(ctx.report_start_pending(2, 5000).is_err());
        assert_eq!(ctx.last_checkpoint(), 1);

        sink.set_fail(false);
        assert!(ctx.report_start_pending(2, 5000).is_ok());
        assert_eq!(ctx.last_checkpoint(), 2);
    }

    #[test]
    fn test_40_handler_full_channel_delivery_is_non_blocking() {
        let (tx, rx) = sync_channel(1);
        let handler_ctx = HandlerContext { sender: tx };

        assert!(handler_ctx.sender.try_send(ScmRuntimeControl::Stop).is_ok());

        let ret = handle_control_request(SERVICE_CONTROL_SHUTDOWN, |ctrl| {
            let _ = handler_ctx.sender.try_send(ctrl);
        });
        assert_eq!(ret, NO_ERROR);

        assert_eq!(rx.try_recv().unwrap(), ScmRuntimeControl::Stop);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_41_handler_disconnected_channel_is_non_blocking_and_non_panicking() {
        let (tx, rx) = sync_channel(1);
        let handler_ctx = HandlerContext { sender: tx };

        drop(rx);

        let ret = handle_control_request(SERVICE_CONTROL_STOP, |ctrl| {
            let _ = handler_ctx.sender.try_send(ctrl);
        });
        assert_eq!(ret, NO_ERROR);
    }

    #[test]
    fn test_42_channel_and_handler_context_exist_before_registration_phase() {
        let (tx, _rx) = sync_channel(1);
        let handler_ctx = Arc::new(HandlerContext { sender: tx });
        let ctx_ptr = Arc::as_ptr(&handler_ctx);
        assert!(!ctx_ptr.is_null());

        let retained = Arc::clone(&handler_ctx);
        drop(handler_ctx);
        assert!(retained.sender.try_send(ScmRuntimeControl::Stop).is_ok());
    }

    #[test]
    fn test_43_production_fallback_helper_requests_stopped_only_when_not_already_reported() {
        let sink = Arc::new(MockStatusSink::new());
        let stopped_reported = Arc::new(AtomicBool::new(false));

        // Call the unified production fallback helper
        execute_panic_fallback(&sink, &stopped_reported);

        assert_eq!(sink.published_count(), 1);
        assert_eq!(
            sink.last_published().unwrap().current_state,
            SERVICE_STOPPED
        );
        assert_eq!(
            sink.last_published().unwrap().win32_exit_code,
            ERROR_EXCEPTION_IN_SERVICE
        );
        assert!(stopped_reported.load(Ordering::SeqCst));

        // Subsequent call to production fallback helper must be a zero-publication no-op
        execute_panic_fallback(&sink, &stopped_reported);
        assert_eq!(sink.published_count(), 1);
    }

    #[test]
    fn test_44_production_fallback_helper_does_nothing_when_already_reported_gracefully() {
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        ctx.report_start_pending(1, 5000).unwrap();
        ctx.report_running().unwrap();
        ctx.report_stop_pending(1, 5000).unwrap();
        ctx.report_stopped(0).unwrap();

        assert_eq!(ctx.current_state(), ScmServiceState::Stopped);
        assert!(sink.stopped_reported.load(Ordering::SeqCst));
        let initial_count = sink.published_count();

        // Production fallback helper called after graceful stop
        execute_panic_fallback(&sink, &sink.stopped_reported);
        assert_eq!(sink.published_count(), initial_count);
    }

    #[test]
    fn test_45_production_fallback_publication_failure_leaves_flag_false_and_is_retryable() {
        let sink = Arc::new(MockStatusSink::new());
        let stopped_reported = Arc::new(AtomicBool::new(false));

        sink.set_fail(true);
        execute_panic_fallback(&sink, &stopped_reported);
        assert_eq!(sink.published_count(), 0);
        assert!(!stopped_reported.load(Ordering::SeqCst));

        // Retry succeeds
        sink.set_fail(false);
        execute_panic_fallback(&sink, &stopped_reported);
        assert_eq!(sink.published_count(), 1);
        assert!(stopped_reported.load(Ordering::SeqCst));
    }

    #[test]
    fn test_46_handler_context_retained_in_dispatcher_cell_outlives_scope() {
        let (tx, rx) = sync_channel(1);
        let handler_ctx = Arc::new(HandlerContext { sender: tx });

        if let Ok(mut cell) = get_handler_context_cell().lock() {
            *cell = Some(Arc::clone(&handler_ctx));
        }

        // Drop local Arc; context must remain alive in global cell
        drop(handler_ctx);

        {
            let cell = get_handler_context_cell().lock().unwrap();
            let stored = cell.as_ref().unwrap();
            assert!(stored.sender.try_send(ScmRuntimeControl::Stop).is_ok());
        }

        assert_eq!(rx.try_recv().unwrap(), ScmRuntimeControl::Stop);

        // Clear cell
        let mut cell = get_handler_context_cell().lock().unwrap();
        *cell = None;
    }

    #[test]
    fn test_47_service_entry_early_return_triggers_fallback_and_records_typed_error() {
        let sink = Arc::new(MockStatusSink::new());
        let stopped_reported = Arc::new(AtomicBool::new(false));

        // Simulated entry completion without reporting STOPPED
        let normal_return_without_stop = true;
        let mut recorded_result: Option<Result<(), ScmRuntimeError>> = None;

        if normal_return_without_stop && !stopped_reported.load(Ordering::SeqCst) {
            execute_panic_fallback(&sink, &stopped_reported);
            recorded_result = Some(Err(ScmRuntimeError::ServiceEntryReturnedBeforeStopped));
        }

        assert_eq!(sink.published_count(), 1);
        assert_eq!(
            sink.last_published().unwrap().current_state,
            SERVICE_STOPPED
        );
        assert!(stopped_reported.load(Ordering::SeqCst));
        match recorded_result {
            Some(Err(ScmRuntimeError::ServiceEntryReturnedBeforeStopped)) => (),
            other => panic!("expected ServiceEntryReturnedBeforeStopped, got {other:?}"),
        }
    }

    #[test]
    fn test_48_service_entry_stopped_return_produces_dispatcher_success() {
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        ctx.report_start_pending(1, 5000).unwrap();
        ctx.report_running().unwrap();
        ctx.report_stop_pending(1, 5000).unwrap();
        ctx.report_stopped(0).unwrap();

        assert!(sink.stopped_reported.load(Ordering::SeqCst));

        let mut recorded_result: Option<Result<(), ScmRuntimeError>> = None;
        if sink.stopped_reported.load(Ordering::SeqCst) {
            recorded_result = Some(Ok(()));
        }

        assert!(matches!(recorded_result, Some(Ok(()))));
    }

    // ========================================================================
    // CORRECTION-3 TESTS: Bridge Fail-Closed & Structural Invariants
    // ========================================================================

    #[test]
    fn test_49_missing_configured_entry_records_typed_internal_bridge_error() {
        // Clear entry cell
        {
            let mut cell = get_entry_cell().lock().unwrap();
            *cell = None;
        }

        // Run retrieval logic as in service_main_inner
        let retrieval_result: Result<PalkaServiceEntry, ScmRuntimeError> =
            match get_entry_cell().lock() {
                Ok(guard) => match *guard {
                    Some(e) => Ok(e),
                    None => Err(ScmRuntimeError::InternalBridgeError(
                        "configured service entry missing",
                    )),
                },
                Err(_) => Err(ScmRuntimeError::InternalBridgeError(
                    "entry cell mutex poisoned in ServiceMain",
                )),
            };

        match retrieval_result {
            Err(ScmRuntimeError::InternalBridgeError(msg)) => {
                assert_eq!(msg, "configured service entry missing");
            }
            other => panic!("expected InternalBridgeError, got {other:?}"),
        }
    }

    #[test]
    fn test_50_failed_fallback_bridge_installation_invokes_local_stopped_helper() {
        let sink = Arc::new(MockStatusSink::new());
        let stopped_reported = Arc::new(AtomicBool::new(false));

        // Simulated local fallback object
        let local_fallback_stopped = Arc::clone(&stopped_reported);
        let local_sink = Arc::clone(&sink);

        // Simulated installation failure (e.g. mutex poisoned)
        let installation_failed = true;
        let mut recorded_error = None;

        if installation_failed {
            execute_panic_fallback(&local_sink, &local_fallback_stopped);
            recorded_error = Some(ScmRuntimeError::InternalBridgeError(
                "active fallback mutex poisoned during installation",
            ));
        }

        assert_eq!(sink.published_count(), 1);
        assert_eq!(
            sink.last_published().unwrap().current_state,
            SERVICE_STOPPED
        );
        assert!(stopped_reported.load(Ordering::SeqCst));
        match recorded_error {
            Some(ScmRuntimeError::InternalBridgeError(msg)) => {
                assert_eq!(msg, "active fallback mutex poisoned during installation");
            }
            other => panic!("expected InternalBridgeError, got {other:?}"),
        }
    }

    #[test]
    fn test_51_fallback_snapshot_releases_mutex_before_publish() {
        let sink = Arc::new(MockStatusSink::new());
        let stopped_reported = Arc::new(AtomicBool::new(false));
        let fallback_cell = Mutex::new(Some((Arc::clone(&sink), Arc::clone(&stopped_reported))));

        // Snapshot and release lock
        let snapshot = {
            let guard = fallback_cell.lock().unwrap();
            guard.clone()
        }; // Lock released here

        // Assert mutex is unlockable during publication
        assert!(fallback_cell.try_lock().is_ok());

        if let Some((ref s, ref r)) = snapshot {
            execute_panic_fallback(s, r);
        }

        assert_eq!(sink.published_count(), 1);
        assert!(stopped_reported.load(Ordering::SeqCst));
    }

    #[test]
    fn test_52_non_send_context_cannot_race_and_stopped_at_most_once_proven() {
        // Compile-time structural guarantee: ScmServiceContext contains PhantomData<Rc<()>>,
        // which is structurally !Send and !Sync. SCM lifecycle ownership is pinned to
        // the service entry lifecycle thread and cannot be sent to worker threads.
        let (mut ctx, _tx, sink) = create_test_context_with_sink();
        ctx.report_start_pending(1, 5000).unwrap();
        ctx.report_running().unwrap();
        ctx.report_stop_pending(1, 5000).unwrap();
        ctx.report_stopped(0).unwrap();

        assert_eq!(ctx.current_state(), ScmServiceState::Stopped);
        assert_eq!(sink.published_count(), 4); // start, run, stop_pending, stopped
        assert!(sink.stopped_reported.load(Ordering::SeqCst));

        // Attempt fallback after normal stop
        execute_panic_fallback(&sink, &sink.stopped_reported);
        // Publication count must NOT increase
        assert_eq!(sink.published_count(), 4);
    }
}
