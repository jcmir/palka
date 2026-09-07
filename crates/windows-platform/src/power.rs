//! Windows power management domain abstractions and testable port interface.
//!
//! Enforces docs/019 normative contract requirements:
//! - Canonical shutdown parameters (PWR-01)
//! - Rigorous 9-variant error taxonomy (PWR-02..PWR-11)
//! - Two-phase GetTokenInformation protocol (PWR-04, PWR-05)
//! - PreviousState capture and restoration on failure (PWR-07..PWR-10)
//! - Asynchronous REQUEST_ACCEPTED semantics (PWR-12)
//! - UnsupportedPlatform real runtime behavior (PWR-13)
//! - RAII SafeHandle token lifecycle (PWR-14)
//! - Read-only constructor readiness probe (docs/019 §4.3)

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

pub const SE_SHUTDOWN_NAME: &str = "SeShutdownPrivilege";
pub const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;

// Win32 error code constants (canonical values)
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;
pub const ERROR_SHUTDOWN_IN_PROGRESS: u32 = 1115;

// Win32 access masks
pub const TOKEN_QUERY: u32 = 0x0008;
pub const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;

// Canonical shutdown arguments for PALKA V1 (docs/019 §3.1)
pub const CANONICAL_SHUTDOWN_MACHINE: Option<&str> = None;
pub const CANONICAL_SHUTDOWN_MESSAGE: Option<&str> = None;
pub const CANONICAL_SHUTDOWN_TIMEOUT: u32 = 0;
pub const CANONICAL_SHUTDOWN_FORCE_APPS: bool = true;
pub const CANONICAL_SHUTDOWN_REBOOT: bool = false;
pub const CANONICAL_SHUTDOWN_REASON: u32 = 0x8000_0000; // SHTDN_REASON_FLAG_PLANNED

/// Normative 9-variant error taxonomy for Windows power operations (docs/019 §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsPowerError {
    /// Operation attempted on non-Windows platform (PWR-13)
    UnsupportedPlatform,

    /// Failure calling OpenProcessToken (PWR-02)
    OpenProcessTokenFailure { win32_code: u32 },

    /// Failure calling LookupPrivilegeValueW (PWR-03)
    LookupPrivilegeFailure { win32_code: u32 },

    /// Failure calling or querying GetTokenInformation (PWR-04, PWR-05)
    TokenPrivilegeQueryFailure { win32_code: u32 },

    /// SeShutdownPrivilege not assigned to token (PWR-06, PWR-07)
    PrivilegeNotAssigned,

    /// Failure calling AdjustTokenPrivileges to enable privilege (PWR-08)
    AdjustPrivilegeFailure { win32_code: u32 },

    /// Failure calling AdjustTokenPrivileges during restore after shutdown failure (PWR-10)
    PrivilegeRestoreFailure { win32_code: u32 },

    /// Failure calling InitiateSystemShutdownExW (PWR-09)
    ShutdownRequestFailure { win32_code: u32 },

    /// System shutdown already in progress (code 1115) (PWR-11)
    ShutdownAlreadyInProgress { win32_code: u32 },
}

impl std::fmt::Display for WindowsPowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "power operations unsupported on this platform"),
            Self::OpenProcessTokenFailure { win32_code } => {
                write!(f, "OpenProcessToken failed with code {win32_code}")
            }
            Self::LookupPrivilegeFailure { win32_code } => {
                write!(f, "LookupPrivilegeValueW failed with code {win32_code}")
            }
            Self::TokenPrivilegeQueryFailure { win32_code } => {
                write!(f, "GetTokenInformation failed with code {win32_code}")
            }
            Self::PrivilegeNotAssigned => {
                write!(f, "SeShutdownPrivilege is not assigned to process token")
            }
            Self::AdjustPrivilegeFailure { win32_code } => {
                write!(
                    f,
                    "AdjustTokenPrivileges failed to enable privilege with code {win32_code}"
                )
            }
            Self::PrivilegeRestoreFailure { win32_code } => {
                write!(
                    f,
                    "AdjustTokenPrivileges failed to restore privilege with code {win32_code}"
                )
            }
            Self::ShutdownRequestFailure { win32_code } => {
                write!(f, "InitiateSystemShutdownExW failed with code {win32_code}")
            }
            Self::ShutdownAlreadyInProgress { win32_code } => {
                write!(f, "system shutdown already in progress (code {win32_code})")
            }
        }
    }
}

impl std::error::Error for WindowsPowerError {}

/// Opaque wrapper for a token handle passed to platform ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawTokenHandle(pub u64);

/// Platform-neutral LUID representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Luid {
    pub low_part: u32,
    pub high_part: i32,
}

/// Platform-neutral LUID and attributes structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct LuidAndAttributes {
    pub luid: Luid,
    pub attributes: u32,
}

/// Parameters for calling InitiateSystemShutdownExW.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownCallParams {
    pub machine_name: Option<String>,
    pub message: Option<String>,
    pub timeout: u32,
    pub force_apps_closed: bool,
    pub reboot_after_shutdown: bool,
    pub reason: u32,
}

/// Sizing result returned by first call to GetTokenInformation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizingResult {
    pub return_length: u32,
    pub win32_last_error: u32,
}

/// Result returned from AdjustTokenPrivileges enable call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjustPrivilegeResult {
    pub previous_state: Vec<u8>,
    pub return_length: u32,
    pub win32_last_error: u32,
}

/// Result returned from AdjustTokenPrivileges restore call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePrivilegeResult {
    pub win32_last_error: u32,
}

/// Marker trait identifying test-seam ports.
///
/// Infallible / no-probe construction is strictly limited to ports implementing this trait.
/// Real production ports (`WindowsPowerEngine`) explicitly do NOT implement `TestPowerPort`,
/// enforcing at compile-time that production controllers can only be obtained via fallible
/// readiness-probing constructors.
pub trait TestPowerPort: WindowsPowerPort {}

/// Internal behavioral seam for Windows power operations.
pub trait WindowsPowerPort: Send + Sync {
    /// Native or test RAII token ownership type satisfying PWR-14.
    type Token: Send + 'static;

    /// Returns true if this port supports Windows power operations.
    /// Returns false on unsupported platforms (PWR-13).
    fn is_supported(&self) -> bool {
        true
    }

    /// Opens the current process token. Returns owned Self::Token or raw Win32 error code.
    fn open_process_token(&self, desired_access: u32) -> Result<Self::Token, u32>;

    /// Looks up the LUID for the named privilege.
    fn lookup_privilege_value(&self, name: &str) -> Result<Luid, u32>;

    /// First call to GetTokenInformation(TokenPrivileges) for buffer sizing.
    fn get_token_privileges_sizing(&self, token: &Self::Token) -> Result<SizingResult, u32>;

    /// Second call to GetTokenInformation(TokenPrivileges) to retrieve data.
    fn get_token_privileges_data(&self, token: &Self::Token, buffer: &mut [u8])
    -> Result<u32, u32>;

    /// Calls AdjustTokenPrivileges to enable SeShutdownPrivilege and captures PreviousState.
    fn adjust_privilege_enable(
        &self,
        token: &Self::Token,
        shutdown_luid: Luid,
    ) -> Result<AdjustPrivilegeResult, u32>;

    /// Initiates shutdown via InitiateSystemShutdownExW.
    fn request_shutdown(&self, params: &ShutdownCallParams) -> Result<(), u32>;

    /// Restores PreviousState via AdjustTokenPrivileges.
    fn restore_privilege(
        &self,
        token: &Self::Token,
        previous_state_bytes: &[u8],
    ) -> Result<RestorePrivilegeResult, u32>;
}

/// Platform-neutral orchestration controller for Windows power operations.
#[derive(Debug)]
pub struct WindowsPowerController<P: WindowsPowerPort> {
    port: P,
}

impl<P: TestPowerPort> WindowsPowerController<P> {
    /// Infallible constructor without immediate readiness probe, strictly restricted to test-seam ports.
    /// Real production ports (`WindowsPowerEngine`) cannot be constructed via this method.
    pub fn new(port: P) -> Self {
        Self { port }
    }
}

impl<P: WindowsPowerPort> WindowsPowerController<P> {
    /// Fallible constructor executing a read-only readiness probe (docs/019 §4.3).
    /// Proves SeShutdownPrivilege is assigned without enabling it and without requesting shutdown.
    pub fn with_readiness_probe(port: P) -> Result<Self, WindowsPowerError> {
        let controller = Self { port };
        controller.probe_readiness()?;
        Ok(controller)
    }

    pub fn port(&self) -> &P {
        &self.port
    }

    /// Read-only readiness probe (PWR-02 .. PWR-06).
    /// Does NOT enable SeShutdownPrivilege and does NOT request shutdown.
    pub fn probe_readiness(&self) -> Result<(), WindowsPowerError> {
        if !self.port.is_supported() {
            return Err(WindowsPowerError::UnsupportedPlatform);
        }

        // 1. Open current process token with TOKEN_QUERY
        let token = self
            .port
            .open_process_token(TOKEN_QUERY)
            .map_err(|win32_code| WindowsPowerError::OpenProcessTokenFailure { win32_code })?;

        // 2. Lookup LUID for SeShutdownPrivilege
        let shutdown_luid = self
            .port
            .lookup_privilege_value(SE_SHUTDOWN_NAME)
            .map_err(|win32_code| WindowsPowerError::LookupPrivilegeFailure { win32_code })?;

        // 3. Two-phase GetTokenInformation protocol
        // Phase 1: sizing call
        let sizing_res = self
            .port
            .get_token_privileges_sizing(&token)
            .map_err(|win32_code| WindowsPowerError::TokenPrivilegeQueryFailure { win32_code })?;

        if sizing_res.win32_last_error != ERROR_INSUFFICIENT_BUFFER {
            return Err(WindowsPowerError::TokenPrivilegeQueryFailure {
                win32_code: sizing_res.win32_last_error,
            });
        }

        let allocated_capacity = sizing_res.return_length;
        if allocated_capacity == 0 {
            return Err(WindowsPowerError::TokenPrivilegeQueryFailure {
                win32_code: ERROR_INVALID_PARAMETER,
            });
        }

        // Phase 2: allocate aligned buffer and retrieve data
        let mut buffer = vec![0u8; allocated_capacity as usize];
        let actual_return_length = self
            .port
            .get_token_privileges_data(&token, &mut buffer)
            .map_err(|win32_code| WindowsPowerError::TokenPrivilegeQueryFailure { win32_code })?;

        // Rigorous validation of returned token privileges buffer (Point 4)
        parse_and_verify_token_privileges(
            &buffer,
            actual_return_length,
            allocated_capacity,
            shutdown_luid,
        )
        // Note: token (SafeHandle in production) drops here, executing CloseHandle exactly once.
    }

    /// Orchestrated initiate_shutdown implementation.
    pub fn initiate_shutdown(&self) -> Result<(), WindowsPowerError> {
        // Probe readiness first (read-only verification that privilege is assigned)
        self.probe_readiness()?;

        // Open current process token with TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY
        let token = self
            .port
            .open_process_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)
            .map_err(|win32_code| WindowsPowerError::OpenProcessTokenFailure { win32_code })?;

        let shutdown_luid = self
            .port
            .lookup_privilege_value(SE_SHUTDOWN_NAME)
            .map_err(|win32_code| WindowsPowerError::LookupPrivilegeFailure { win32_code })?;

        // Enable privilege and capture PreviousState
        let adjust_res = self
            .port
            .adjust_privilege_enable(&token, shutdown_luid)
            .map_err(|win32_code| WindowsPowerError::AdjustPrivilegeFailure { win32_code })?;

        if adjust_res.win32_last_error == ERROR_NOT_ALL_ASSIGNED {
            return Err(WindowsPowerError::PrivilegeNotAssigned);
        }
        if adjust_res.win32_last_error != 0 {
            return Err(WindowsPowerError::AdjustPrivilegeFailure {
                win32_code: adjust_res.win32_last_error,
            });
        }

        // Validate and capture exact PreviousState bytes (Point 5)
        let previous_state = parse_and_validate_previous_state(
            &adjust_res.previous_state,
            adjust_res.return_length,
            adjust_res.previous_state.len() as u32,
        )?;

        // Request system shutdown with canonical arguments
        let params = ShutdownCallParams {
            machine_name: CANONICAL_SHUTDOWN_MACHINE.map(str::to_string),
            message: CANONICAL_SHUTDOWN_MESSAGE.map(str::to_string),
            timeout: CANONICAL_SHUTDOWN_TIMEOUT,
            force_apps_closed: CANONICAL_SHUTDOWN_FORCE_APPS,
            reboot_after_shutdown: CANONICAL_SHUTDOWN_REBOOT,
            reason: CANONICAL_SHUTDOWN_REASON,
        };

        match self.port.request_shutdown(&params) {
            Ok(()) => {
                // Accepted asynchronous shutdown (REQUEST_ACCEPTED)
                // No mandatory privilege restoration after success in PALKA V1 (PWR-12)
                Ok(())
            }
            Err(raw_code) => {
                // Map native shutdown error (PWR-11)
                let shutdown_error = if raw_code == ERROR_SHUTDOWN_IN_PROGRESS {
                    WindowsPowerError::ShutdownAlreadyInProgress {
                        win32_code: ERROR_SHUTDOWN_IN_PROGRESS,
                    }
                } else {
                    WindowsPowerError::ShutdownRequestFailure {
                        win32_code: raw_code,
                    }
                };

                // Validate PreviousState at untrusted boundary before restore (Point 6)
                if let Err(e) = validate_previous_state_before_restore(&previous_state) {
                    return Err(e);
                }

                // Compensation: attempt to restore PreviousState
                let restore_result = self
                    .port
                    .restore_privilege(&token, previous_state.raw_bytes());

                match restore_result {
                    Ok(res) => {
                        // Point 7: ERROR_NOT_ALL_ASSIGNED during restore is a failure!
                        if res.win32_last_error == ERROR_NOT_ALL_ASSIGNED {
                            return Err(WindowsPowerError::PrivilegeRestoreFailure {
                                win32_code: ERROR_NOT_ALL_ASSIGNED,
                            });
                        }
                        if res.win32_last_error != 0 {
                            return Err(WindowsPowerError::PrivilegeRestoreFailure {
                                win32_code: res.win32_last_error,
                            });
                        }
                        // Restoration succeeded: return the ORIGINAL shutdown error (PWR-09)
                        Err(shutdown_error)
                    }
                    Err(win32_code) => {
                        // Restoration failed: return PrivilegeRestoreFailure (priority over shutdown error) (PWR-10)
                        Err(WindowsPowerError::PrivilegeRestoreFailure { win32_code })
                    }
                }
            }
        }
        // Note: token (SafeHandle in production) drops here, executing CloseHandle exactly once.
    }
}

/// Validates and parses TOKEN_PRIVILEGES buffer in strict compliance with Point 4:
/// 1. second call succeeds (precondition)
/// 2. validate actual_return_length >= size required to read PrivilegeCount
/// 3. validate actual_return_length <= allocated_capacity
/// 4. only then read PrivilegeCount
/// 5. if count == 0 -> PrivilegeNotAssigned without Privileges[0] access
/// 6. checked u32 -> usize conversion
/// 7. checked ANYSIZE_ARRAY size calculation
/// 8. validate required payload <= actual_return_length
/// 9. validate required payload <= allocated_capacity
/// 10. only then dereference dynamic privilege entries
pub fn parse_and_verify_token_privileges(
    buffer: &[u8],
    actual_return_length: u32,
    allocated_capacity: u32,
    target_luid: Luid,
) -> Result<(), WindowsPowerError> {
    let header_size = std::mem::size_of::<u32>();

    // 2. Validate actual_return_length >= size required to read PrivilegeCount
    if (actual_return_length as usize) < header_size {
        return Err(WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }

    // 3. Validate actual_return_length <= allocated_capacity
    if actual_return_length > allocated_capacity {
        return Err(WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }

    if buffer.len() < actual_return_length as usize {
        return Err(WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }

    // 4. Only then read PrivilegeCount
    let count = u32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);

    // 5. If count == 0 -> PrivilegeNotAssigned without dereferencing Privileges[0]
    if count == 0 {
        return Err(WindowsPowerError::PrivilegeNotAssigned);
    }

    // 6. Checked u32 -> usize conversion
    let count_usize =
        usize::try_from(count).map_err(|_| WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        })?;

    // 7. Checked ANYSIZE_ARRAY size calculation
    let entry_size = std::mem::size_of::<LuidAndAttributes>();
    let entries_bytes = count_usize.checked_mul(entry_size).ok_or(
        WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        },
    )?;

    let required_payload = header_size.checked_add(entries_bytes).ok_or(
        WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        },
    )?;

    // 8. Validate required payload <= actual_return_length
    if required_payload > actual_return_length as usize {
        return Err(WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }

    // 9. Validate required payload <= allocated_capacity
    if required_payload > allocated_capacity as usize {
        return Err(WindowsPowerError::TokenPrivilegeQueryFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }

    // 10. Only then safely dereference dynamic privilege entries
    let mut offset = header_size;
    let mut found = false;

    for _ in 0..count_usize {
        let low_part = u32::from_ne_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]);
        let high_part = i32::from_ne_bytes([
            buffer[offset + 4],
            buffer[offset + 5],
            buffer[offset + 6],
            buffer[offset + 7],
        ]);

        if low_part == target_luid.low_part && high_part == target_luid.high_part {
            found = true;
            break;
        }

        offset += entry_size;
    }

    if found {
        Ok(())
    } else {
        Err(WindowsPowerError::PrivilegeNotAssigned)
    }
}

/// Owned container for validated, exact PreviousState bytes (Point 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousPrivilegeState {
    raw_bytes: Vec<u8>,
}

impl PreviousPrivilegeState {
    pub fn new(raw_bytes: Vec<u8>) -> Self {
        Self { raw_bytes }
    }

    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }
}

/// Validates and captures exact valid PreviousState bytes (Point 5).
pub fn parse_and_validate_previous_state(
    bytes: &[u8],
    return_length: u32,
    allocated_capacity: u32,
) -> Result<PreviousPrivilegeState, WindowsPowerError> {
    let header_size = std::mem::size_of::<u32>();

    if (return_length as usize) < header_size || return_length > allocated_capacity {
        return Err(WindowsPowerError::AdjustPrivilegeFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }
    if bytes.len() < return_length as usize {
        return Err(WindowsPowerError::AdjustPrivilegeFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }

    let count = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let required_payload = if count == 0 {
        header_size
    } else {
        let count_usize =
            usize::try_from(count).map_err(|_| WindowsPowerError::AdjustPrivilegeFailure {
                win32_code: ERROR_INVALID_PARAMETER,
            })?;
        let entry_size = std::mem::size_of::<LuidAndAttributes>();
        let entries_bytes = count_usize.checked_mul(entry_size).ok_or(
            WindowsPowerError::AdjustPrivilegeFailure {
                win32_code: ERROR_INVALID_PARAMETER,
            },
        )?;
        header_size
            .checked_add(entries_bytes)
            .ok_or(WindowsPowerError::AdjustPrivilegeFailure {
                win32_code: ERROR_INVALID_PARAMETER,
            })?
    };

    if required_payload > return_length as usize || required_payload > allocated_capacity as usize {
        return Err(WindowsPowerError::AdjustPrivilegeFailure {
            win32_code: ERROR_INVALID_PARAMETER,
        });
    }

    // Capture ONLY valid PreviousState bytes
    let captured = bytes[..required_payload].to_vec();
    Ok(PreviousPrivilegeState::new(captured))
}

/// Validates previous state bytes at the native boundary before casting to `*const TOKEN_PRIVILEGES`.
///
/// Enforces:
/// 1. bytes.len() >= size needed to read PrivilegeCount (header check)
/// 2. read PrivilegeCount only after header proof
/// 3. checked u32 -> usize conversion
/// 4. checked: offset_of Privileges + count * size_of::<LUID_AND_ATTRIBUTES>() without overflow
/// 5. required_payload <= bytes.len()
/// Returns raw Win32 error code (ERROR_INVALID_PARAMETER = 87) on failure.
pub fn validate_restore_buffer(bytes: &[u8]) -> Result<usize, u32> {
    let header_size = std::mem::size_of::<u32>();
    if bytes.len() < header_size {
        return Err(ERROR_INVALID_PARAMETER);
    }

    let count = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let count_usize = usize::try_from(count).map_err(|_| ERROR_INVALID_PARAMETER)?;

    let entry_size = std::mem::size_of::<LuidAndAttributes>();
    let entries_bytes = count_usize
        .checked_mul(entry_size)
        .ok_or(ERROR_INVALID_PARAMETER)?;
    let required_len = header_size
        .checked_add(entries_bytes)
        .ok_or(ERROR_INVALID_PARAMETER)?;

    if bytes.len() < required_len {
        return Err(ERROR_INVALID_PARAMETER);
    }

    Ok(required_len)
}

/// Validates PreviousPrivilegeState at untrusted boundary before passing to Win32 (Point 6).
pub fn validate_previous_state_before_restore(
    state: &PreviousPrivilegeState,
) -> Result<usize, WindowsPowerError> {
    validate_restore_buffer(state.raw_bytes())
        .map_err(|win32_code| WindowsPowerError::PrivilegeRestoreFailure { win32_code })
}

/// Production port implementation on unsupported platforms (!windows).
#[derive(Debug, Default, Clone, Copy)]
pub struct UnsupportedPowerPort;

#[derive(Debug)]
pub struct UnsupportedToken;

impl TestPowerPort for UnsupportedPowerPort {}

impl WindowsPowerPort for UnsupportedPowerPort {
    type Token = UnsupportedToken;

    fn is_supported(&self) -> bool {
        false
    }

    fn open_process_token(&self, _desired_access: u32) -> Result<UnsupportedToken, u32> {
        Err(1)
    }

    fn lookup_privilege_value(&self, _name: &str) -> Result<Luid, u32> {
        Err(1)
    }

    fn get_token_privileges_sizing(&self, _token: &UnsupportedToken) -> Result<SizingResult, u32> {
        Err(1)
    }

    fn get_token_privileges_data(
        &self,
        _token: &UnsupportedToken,
        _buffer: &mut [u8],
    ) -> Result<u32, u32> {
        Err(1)
    }

    fn adjust_privilege_enable(
        &self,
        _token: &UnsupportedToken,
        _shutdown_luid: Luid,
    ) -> Result<AdjustPrivilegeResult, u32> {
        Err(1)
    }

    fn request_shutdown(&self, _params: &ShutdownCallParams) -> Result<(), u32> {
        Err(1)
    }

    fn restore_privilege(
        &self,
        _token: &UnsupportedToken,
        _previous_state_bytes: &[u8],
    ) -> Result<RestorePrivilegeResult, u32> {
        Err(1)
    }
}

impl WindowsPowerController<UnsupportedPowerPort> {
    /// Production constructor on unsupported platforms cleanly returns UnsupportedPlatform (Section 5).
    pub fn from_production() -> Result<Self, WindowsPowerError> {
        Err(WindowsPowerError::UnsupportedPlatform)
    }
}

/// RAII handle wrapper for FakeWindowsPowerPort tokens.
#[derive(Debug, Clone)]
pub struct FakeTokenHandle {
    pub handle: RawTokenHandle,
    pub closed_handles: Arc<Mutex<Vec<RawTokenHandle>>>,
}

impl Drop for FakeTokenHandle {
    fn drop(&mut self) {
        self.closed_handles.lock().unwrap().push(self.handle);
    }
}

/// Configurable test fake implementation for WindowsPowerPort.
#[derive(Debug, Clone)]
pub struct FakeWindowsPowerPort {
    pub open_token_result: Arc<Mutex<Result<RawTokenHandle, u32>>>,
    pub lookup_privilege_result: Arc<Mutex<Result<Luid, u32>>>,
    pub sizing_result: Arc<Mutex<Result<SizingResult, u32>>>,
    pub token_data_result: Arc<Mutex<Result<(Vec<u8>, u32), u32>>>,
    pub adjust_enable_result: Arc<Mutex<Result<AdjustPrivilegeResult, u32>>>,
    pub request_shutdown_result: Arc<Mutex<Result<(), u32>>>,
    pub restore_privilege_result: Arc<Mutex<Result<RestorePrivilegeResult, u32>>>,
    pub closed_handles: Arc<Mutex<Vec<RawTokenHandle>>>,
    pub last_shutdown_params: Arc<Mutex<Option<ShutdownCallParams>>>,
    pub restore_call_count: Arc<AtomicU32>,
    pub last_restored_bytes: Arc<Mutex<Option<Vec<u8>>>>,
    pub adjust_call_count: Arc<AtomicU32>,
    pub shutdown_call_count: Arc<AtomicU32>,
}

impl Default for FakeWindowsPowerPort {
    fn default() -> Self {
        let default_luid = Luid {
            low_part: 0x1234,
            high_part: 0,
        };

        // Create a default valid TOKEN_PRIVILEGES buffer with 1 entry containing default_luid
        let mut default_bytes = Vec::new();
        default_bytes.extend_from_slice(&1u32.to_ne_bytes()); // count = 1
        default_bytes.extend_from_slice(&default_luid.low_part.to_ne_bytes());
        default_bytes.extend_from_slice(&default_luid.high_part.to_ne_bytes());
        default_bytes.extend_from_slice(&SE_PRIVILEGE_ENABLED.to_ne_bytes());

        let len = default_bytes.len() as u32;

        Self {
            open_token_result: Arc::new(Mutex::new(Ok(RawTokenHandle(42)))),
            lookup_privilege_result: Arc::new(Mutex::new(Ok(default_luid))),
            sizing_result: Arc::new(Mutex::new(Ok(SizingResult {
                return_length: len,
                win32_last_error: ERROR_INSUFFICIENT_BUFFER,
            }))),
            token_data_result: Arc::new(Mutex::new(Ok((default_bytes.clone(), len)))),
            adjust_enable_result: Arc::new(Mutex::new(Ok(AdjustPrivilegeResult {
                previous_state: default_bytes,
                return_length: len,
                win32_last_error: 0,
            }))),
            request_shutdown_result: Arc::new(Mutex::new(Ok(()))),
            restore_privilege_result: Arc::new(Mutex::new(Ok(RestorePrivilegeResult {
                win32_last_error: 0,
            }))),
            closed_handles: Arc::new(Mutex::new(Vec::new())),
            last_shutdown_params: Arc::new(Mutex::new(None)),
            restore_call_count: Arc::new(AtomicU32::new(0)),
            last_restored_bytes: Arc::new(Mutex::new(None)),
            adjust_call_count: Arc::new(AtomicU32::new(0)),
            shutdown_call_count: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl TestPowerPort for FakeWindowsPowerPort {}

impl WindowsPowerPort for FakeWindowsPowerPort {
    type Token = FakeTokenHandle;

    fn open_process_token(&self, _desired_access: u32) -> Result<FakeTokenHandle, u32> {
        let res = self.open_token_result.lock().unwrap().clone();
        match res {
            Ok(raw) => Ok(FakeTokenHandle {
                handle: raw,
                closed_handles: Arc::clone(&self.closed_handles),
            }),
            Err(err) => Err(err),
        }
    }

    fn lookup_privilege_value(&self, _name: &str) -> Result<Luid, u32> {
        self.lookup_privilege_result.lock().unwrap().clone()
    }

    fn get_token_privileges_sizing(&self, _token: &FakeTokenHandle) -> Result<SizingResult, u32> {
        self.sizing_result.lock().unwrap().clone()
    }

    fn get_token_privileges_data(
        &self,
        _token: &FakeTokenHandle,
        buffer: &mut [u8],
    ) -> Result<u32, u32> {
        let guard = self.token_data_result.lock().unwrap();
        match &*guard {
            Ok((data, ret_len)) => {
                let to_copy = data.len().min(buffer.len());
                buffer[..to_copy].copy_from_slice(&data[..to_copy]);
                Ok(*ret_len)
            }
            Err(e) => Err(*e),
        }
    }

    fn adjust_privilege_enable(
        &self,
        _token: &FakeTokenHandle,
        _shutdown_luid: Luid,
    ) -> Result<AdjustPrivilegeResult, u32> {
        self.adjust_call_count.fetch_add(1, Ordering::SeqCst);
        self.adjust_enable_result.lock().unwrap().clone()
    }

    fn request_shutdown(&self, params: &ShutdownCallParams) -> Result<(), u32> {
        self.shutdown_call_count.fetch_add(1, Ordering::SeqCst);
        *self.last_shutdown_params.lock().unwrap() = Some(params.clone());
        self.request_shutdown_result.lock().unwrap().clone()
    }

    fn restore_privilege(
        &self,
        _token: &FakeTokenHandle,
        previous_state_bytes: &[u8],
    ) -> Result<RestorePrivilegeResult, u32> {
        self.restore_call_count.fetch_add(1, Ordering::SeqCst);
        *self.last_restored_bytes.lock().unwrap() = Some(previous_state_bytes.to_vec());
        self.restore_privilege_result.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PWR-01: Initiate shutdown passes exact canonical arguments to port
    #[test]
    fn test_pwr_01_initiate_shutdown_passes_exact_canonical_arguments() {
        let fake = FakeWindowsPowerPort::default();
        let controller = WindowsPowerController::new(fake.clone());

        let res = controller.initiate_shutdown();
        assert_eq!(res, Ok(()));

        let recorded_params = fake
            .last_shutdown_params
            .lock()
            .unwrap()
            .clone()
            .expect("shutdown params recorded");

        assert_eq!(recorded_params.machine_name, None);
        assert_eq!(recorded_params.message, None);
        assert_eq!(recorded_params.timeout, 0);
        assert_eq!(recorded_params.force_apps_closed, true);
        assert_eq!(recorded_params.reboot_after_shutdown, false);
        assert_eq!(recorded_params.reason, 0x8000_0000);
    }

    // PWR-02: Native OpenProcessToken failure maps to OpenProcessTokenFailure with raw code
    #[test]
    fn test_pwr_02_open_process_token_failure_maps_raw_win32_code() {
        let fake = FakeWindowsPowerPort::default();
        *fake.open_token_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED); // raw Win32 code 5
        let controller = WindowsPowerController::new(fake);

        let res = controller.probe_readiness();
        assert_eq!(
            res,
            Err(WindowsPowerError::OpenProcessTokenFailure {
                win32_code: ERROR_ACCESS_DENIED
            })
        );
    }

    // PWR-03: Native LookupPrivilegeValueW failure maps to LookupPrivilegeFailure with raw code
    #[test]
    fn test_pwr_03_lookup_privilege_failure_maps_raw_win32_code() {
        let fake = FakeWindowsPowerPort::default();
        *fake.lookup_privilege_result.lock().unwrap() = Err(ERROR_PRIVILEGE_NOT_HELD); // raw 1314
        let controller = WindowsPowerController::new(fake);

        let res = controller.probe_readiness();
        assert_eq!(
            res,
            Err(WindowsPowerError::LookupPrivilegeFailure {
                win32_code: ERROR_PRIVILEGE_NOT_HELD
            })
        );
    }

    // PWR-04: First GetTokenInformation sizing call handles ERROR_INSUFFICIENT_BUFFER
    // and unexpected sizing failure maps to TokenPrivilegeQueryFailure with raw code
    #[test]
    fn test_pwr_04_token_privilege_query_sizing_call_semantics() {
        let fake = FakeWindowsPowerPort::default();
        // Unexpected error on sizing call (e.g. 5 instead of 122)
        *fake.sizing_result.lock().unwrap() = Ok(SizingResult {
            return_length: 0,
            win32_last_error: ERROR_ACCESS_DENIED,
        });
        let controller = WindowsPowerController::new(fake);

        let res = controller.probe_readiness();
        assert_eq!(
            res,
            Err(WindowsPowerError::TokenPrivilegeQueryFailure {
                win32_code: ERROR_ACCESS_DENIED
            })
        );
    }

    // PWR-05: Second GetTokenInformation call failure maps to TokenPrivilegeQueryFailure with raw code
    #[test]
    fn test_pwr_05_token_privilege_query_data_call_failure() {
        let fake = FakeWindowsPowerPort::default();
        *fake.token_data_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED);
        let controller = WindowsPowerController::new(fake);

        let res = controller.probe_readiness();
        assert_eq!(
            res,
            Err(WindowsPowerError::TokenPrivilegeQueryFailure {
                win32_code: ERROR_ACCESS_DENIED
            })
        );
    }

    // PWR-06: Privilege not found in token privileges array maps to PrivilegeNotAssigned
    #[test]
    fn test_pwr_06_privilege_not_assigned_when_luid_absent() {
        let fake = FakeWindowsPowerPort::default();
        // Return a valid buffer with 1 entry, but different LUID
        let other_luid = Luid {
            low_part: 0x9999,
            high_part: 0,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_ne_bytes());
        bytes.extend_from_slice(&other_luid.low_part.to_ne_bytes());
        bytes.extend_from_slice(&other_luid.high_part.to_ne_bytes());
        bytes.extend_from_slice(&SE_PRIVILEGE_ENABLED.to_ne_bytes());
        let len = bytes.len() as u32;

        *fake.token_data_result.lock().unwrap() = Ok((bytes, len));
        let controller = WindowsPowerController::new(fake);

        let res = controller.probe_readiness();
        assert_eq!(res, Err(WindowsPowerError::PrivilegeNotAssigned));
    }

    // Explicit test for Point 4 (zero count):
    #[test]
    fn test_token_privileges_with_zero_count_returns_privilege_not_assigned() {
        let fake_zero = FakeWindowsPowerPort::default();
        *fake_zero.token_data_result.lock().unwrap() = Ok((0u32.to_ne_bytes().to_vec(), 4));
        *fake_zero.sizing_result.lock().unwrap() = Ok(SizingResult {
            return_length: 4,
            win32_last_error: ERROR_INSUFFICIENT_BUFFER,
        });
        let controller_zero = WindowsPowerController::new(fake_zero);
        assert_eq!(
            controller_zero.probe_readiness(),
            Err(WindowsPowerError::PrivilegeNotAssigned)
        );
    }

    // Explicit test for Point 4 (truncated payload):
    #[test]
    fn test_token_privileges_with_truncated_payload_fails_closed() {
        let fake_trunc = FakeWindowsPowerPort::default();
        // claims 2 entries (requiring 4 + 2 * 12 = 28 bytes) but provides only 16 bytes
        let mut trunc_bytes = Vec::new();
        trunc_bytes.extend_from_slice(&2u32.to_ne_bytes());
        trunc_bytes.extend_from_slice(&[0u8; 12]);
        *fake_trunc.token_data_result.lock().unwrap() = Ok((trunc_bytes, 16));
        *fake_trunc.sizing_result.lock().unwrap() = Ok(SizingResult {
            return_length: 16,
            win32_last_error: ERROR_INSUFFICIENT_BUFFER,
        });
        let controller_trunc = WindowsPowerController::new(fake_trunc);
        assert_eq!(
            controller_trunc.probe_readiness(),
            Err(WindowsPowerError::TokenPrivilegeQueryFailure {
                win32_code: ERROR_INVALID_PARAMETER
            })
        );
    }

    // PWR-07: AdjustTokenPrivileges returns ERROR_NOT_ALL_ASSIGNED maps to PrivilegeNotAssigned
    #[test]
    fn test_pwr_07_adjust_privilege_error_not_all_assigned_maps_to_privilege_not_assigned() {
        let fake = FakeWindowsPowerPort::default();
        *fake.adjust_enable_result.lock().unwrap() = Ok(AdjustPrivilegeResult {
            previous_state: Vec::new(),
            return_length: 0,
            win32_last_error: ERROR_NOT_ALL_ASSIGNED, // 1300
        });
        let controller = WindowsPowerController::new(fake);

        let res = controller.initiate_shutdown();
        assert_eq!(res, Err(WindowsPowerError::PrivilegeNotAssigned));
    }

    // PWR-08: AdjustTokenPrivileges native failure maps to AdjustPrivilegeFailure with raw code
    #[test]
    fn test_pwr_08_adjust_privilege_native_failure_maps_raw_code() {
        let fake = FakeWindowsPowerPort::default();
        *fake.adjust_enable_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED); // raw code 5
        let controller = WindowsPowerController::new(fake);

        let res = controller.initiate_shutdown();
        assert_eq!(
            res,
            Err(WindowsPowerError::AdjustPrivilegeFailure {
                win32_code: ERROR_ACCESS_DENIED
            })
        );
    }

    // PWR-09: Shutdown failure + successful restore returns original ShutdownRequestFailure
    #[test]
    fn test_pwr_09_shutdown_failure_with_successful_restore_returns_shutdown_error() {
        let fake = FakeWindowsPowerPort::default();
        *fake.request_shutdown_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED); // shutdown fails with 5
        *fake.restore_privilege_result.lock().unwrap() = Ok(RestorePrivilegeResult {
            win32_last_error: 0, // restore succeeds
        });
        let controller = WindowsPowerController::new(fake.clone());

        let res = controller.initiate_shutdown();
        assert_eq!(
            res,
            Err(WindowsPowerError::ShutdownRequestFailure {
                win32_code: ERROR_ACCESS_DENIED
            })
        );
        assert_eq!(fake.restore_call_count.load(Ordering::SeqCst), 1);
        assert!(fake.last_restored_bytes.lock().unwrap().is_some());
    }

    // PWR-10: Shutdown failure + restore failure returns PrivilegeRestoreFailure (priority)
    #[test]
    fn test_pwr_10_shutdown_failure_with_restore_failure_prioritizes_restore_error() {
        let fake = FakeWindowsPowerPort::default();
        *fake.request_shutdown_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED);
        *fake.restore_privilege_result.lock().unwrap() = Err(ERROR_NOT_ENOUGH_MEMORY); // restore fails with 8
        let controller = WindowsPowerController::new(fake);

        let res = controller.initiate_shutdown();
        assert_eq!(
            res,
            Err(WindowsPowerError::PrivilegeRestoreFailure {
                win32_code: ERROR_NOT_ENOUGH_MEMORY
            })
        );
    }

    // Explicit test for Point 7: restore API success + ERROR_NOT_ALL_ASSIGNED -> PrivilegeRestoreFailure { win32_code: 1300 }
    #[test]
    fn test_restore_api_success_with_error_not_all_assigned_fails_closed() {
        let fake = FakeWindowsPowerPort::default();
        *fake.request_shutdown_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED);
        *fake.restore_privilege_result.lock().unwrap() = Ok(RestorePrivilegeResult {
            win32_last_error: ERROR_NOT_ALL_ASSIGNED, // 1300
        });
        let controller = WindowsPowerController::new(fake);

        let res = controller.initiate_shutdown();
        assert_eq!(
            res,
            Err(WindowsPowerError::PrivilegeRestoreFailure {
                win32_code: ERROR_NOT_ALL_ASSIGNED
            })
        );
    }

    // PWR-11: Shutdown already in progress (1115) maps to ShutdownAlreadyInProgress { win32_code: 1115 }
    #[test]
    fn test_pwr_11_shutdown_already_in_progress_1115_maps_raw_code() {
        let fake = FakeWindowsPowerPort::default();
        *fake.request_shutdown_result.lock().unwrap() = Err(ERROR_SHUTDOWN_IN_PROGRESS); // raw 1115
        let controller = WindowsPowerController::new(fake);

        let res = controller.initiate_shutdown();
        assert_eq!(
            res,
            Err(WindowsPowerError::ShutdownAlreadyInProgress {
                win32_code: ERROR_SHUTDOWN_IN_PROGRESS
            })
        );
    }

    // PWR-12: Successful accepted request returns Ok(()) and does not restore
    #[test]
    fn test_pwr_12_request_accepted_returns_ok_and_does_not_restore() {
        let fake = FakeWindowsPowerPort::default();
        let controller = WindowsPowerController::new(fake.clone());

        let res = controller.initiate_shutdown();
        assert_eq!(res, Ok(()));
        assert_eq!(fake.restore_call_count.load(Ordering::SeqCst), 0);
    }

    // PWR-13: Real unsupported platform path naturally returns UnsupportedPlatform (Sections 4, 5, 6)
    #[test]
    fn test_pwr_13_unsupported_platform_real_runtime_path() {
        let controller = WindowsPowerController::new(UnsupportedPowerPort);
        assert_eq!(
            controller.probe_readiness(),
            Err(WindowsPowerError::UnsupportedPlatform)
        );
        assert_eq!(
            controller.initiate_shutdown(),
            Err(WindowsPowerError::UnsupportedPlatform)
        );
        assert_eq!(
            WindowsPowerController::<UnsupportedPowerPort>::from_production().err(),
            Some(WindowsPowerError::UnsupportedPlatform)
        );
    }

    // PWR-14: RAII SafeHandle token lifecycle - token handles are closed on all paths
    #[test]
    fn test_pwr_14_token_handles_closed_via_raii_on_all_paths() {
        // Path A: successful probe_readiness closes token
        let fake = FakeWindowsPowerPort::default();
        let controller = WindowsPowerController::new(fake.clone());
        controller.probe_readiness().expect("probe succeeds");
        assert_eq!(fake.closed_handles.lock().unwrap().len(), 1);

        // Path B: failed probe_readiness on lookup error closes token
        let fake_err = FakeWindowsPowerPort::default();
        *fake_err.lookup_privilege_result.lock().unwrap() = Err(ERROR_PRIVILEGE_NOT_HELD);
        let controller_err = WindowsPowerController::new(fake_err.clone());
        let _ = controller_err.probe_readiness();
        assert_eq!(fake_err.closed_handles.lock().unwrap().len(), 1);

        // Path C: successful initiate_shutdown closes both tokens (1 probe + 1 shutdown)
        let fake_sd = FakeWindowsPowerPort::default();
        let controller_sd = WindowsPowerController::new(fake_sd.clone());
        controller_sd
            .initiate_shutdown()
            .expect("shutdown succeeds");
        assert_eq!(fake_sd.closed_handles.lock().unwrap().len(), 2);

        // Path D: shutdown failure + restore closes tokens
        let fake_fail = FakeWindowsPowerPort::default();
        *fake_fail.request_shutdown_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED);
        let controller_fail = WindowsPowerController::new(fake_fail.clone());
        let _ = controller_fail.initiate_shutdown();
        assert_eq!(fake_fail.closed_handles.lock().unwrap().len(), 2);
    }

    // Section 5 tests: Native boundary restore buffer validation
    #[test]
    fn test_restore_truncated_header_rejected_before_native_call() {
        let truncated = [1u8, 2u8]; // len 2 < 4
        let res = validate_restore_buffer(&truncated);
        assert_eq!(res, Err(ERROR_INVALID_PARAMETER));
    }

    #[test]
    fn test_restore_count_payload_overflow_rejected_before_native_call() {
        let mut bytes = vec![0u8; 16];
        bytes[0..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        let res = validate_restore_buffer(&bytes);
        assert_eq!(res, Err(ERROR_INVALID_PARAMETER));
    }

    #[test]
    fn test_restore_count_exceeds_buffer_rejected_before_native_call() {
        let mut bytes = vec![0u8; 16];
        bytes[0..4].copy_from_slice(&2u32.to_ne_bytes()); // count 2 requires 4 + 2 * 12 = 28 bytes
        let res = validate_restore_buffer(&bytes);
        assert_eq!(res, Err(ERROR_INVALID_PARAMETER));
    }

    #[test]
    fn test_restore_valid_exact_previous_state_accepted_by_validation() {
        let mut bytes = vec![0u8; 16];
        bytes[0..4].copy_from_slice(&1u32.to_ne_bytes()); // count 1 requires 4 + 1 * 12 = 16 bytes
        let res = validate_restore_buffer(&bytes);
        assert_eq!(res, Ok(16));
    }

    // Section 7: Constructor readiness probe tests (docs/019 §4.3)
    #[test]
    fn test_construction_readiness_success_does_not_enable_or_shutdown() {
        let fake = FakeWindowsPowerPort::default();
        let controller = WindowsPowerController::with_readiness_probe(fake.clone())
            .expect("readiness probe succeeds");
        assert_eq!(
            controller.port().adjust_call_count.load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            controller.port().shutdown_call_count.load(Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn test_construction_open_process_token_failure() {
        let fake = FakeWindowsPowerPort::default();
        *fake.open_token_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED);
        let res = WindowsPowerController::with_readiness_probe(fake);
        assert_eq!(
            res.err(),
            Some(WindowsPowerError::OpenProcessTokenFailure {
                win32_code: ERROR_ACCESS_DENIED
            })
        );
    }

    #[test]
    fn test_construction_lookup_failure() {
        let fake = FakeWindowsPowerPort::default();
        *fake.lookup_privilege_result.lock().unwrap() = Err(ERROR_PRIVILEGE_NOT_HELD);
        let res = WindowsPowerController::with_readiness_probe(fake);
        assert_eq!(
            res.err(),
            Some(WindowsPowerError::LookupPrivilegeFailure {
                win32_code: ERROR_PRIVILEGE_NOT_HELD
            })
        );
    }

    #[test]
    fn test_construction_privilege_not_assigned() {
        let fake = FakeWindowsPowerPort::default();
        *fake.token_data_result.lock().unwrap() = Ok((0u32.to_ne_bytes().to_vec(), 4));
        let res = WindowsPowerController::with_readiness_probe(fake);
        assert_eq!(res.err(), Some(WindowsPowerError::PrivilegeNotAssigned));
    }

    // Verification of exact PreviousState capture and untrusted restore validation (Points 5 & 6)
    #[test]
    fn test_previous_state_exact_payload_and_validation() {
        let mut buffer = vec![0u8; 100]; // allocated capacity 100
        let luid = Luid {
            low_part: 10,
            high_part: 0,
        };
        buffer[..4].copy_from_slice(&1u32.to_ne_bytes()); // count = 1
        buffer[4..8].copy_from_slice(&luid.low_part.to_ne_bytes());
        buffer[8..12].copy_from_slice(&luid.high_part.to_ne_bytes());
        buffer[12..16].copy_from_slice(&0u32.to_ne_bytes());

        // Return length is 16
        let state = parse_and_validate_previous_state(&buffer, 16, 100).expect("valid parse");
        // Captured bytes must be EXACTLY 16 bytes, not the 100 allocated bytes!
        assert_eq!(state.raw_bytes().len(), 16);

        // Untrusted boundary validation before restore
        let valid_len =
            validate_previous_state_before_restore(&state).expect("valid before restore");
        assert_eq!(valid_len, 16);

        // Malformed state (header claims 5 entries but only 16 bytes supplied)
        let mut bad_bytes = state.raw_bytes().to_vec();
        bad_bytes[..4].copy_from_slice(&5u32.to_ne_bytes());
        let bad_state = PreviousPrivilegeState::new(bad_bytes);
        assert_eq!(
            validate_previous_state_before_restore(&bad_state),
            Err(WindowsPowerError::PrivilegeRestoreFailure {
                win32_code: ERROR_INVALID_PARAMETER
            })
        );
    }
}
