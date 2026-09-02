//! Windows Service Control Manager (SCM) read-only query adapter and canonical configuration validator.
//!
//! This module provides:
//! - Canonical Windows Service identity and configuration constants for PALKA V1.
//! - Typed platform-neutral snapshot of installed Windows Service configuration (`ScmConfigSnapshot`).
//! - Read-only query function (`query_palka_service_config`) querying real Windows SCM via Win32 API.
//! - Pure canonical validator (`ScmConfigSnapshot::validate_canonical`) comparing a snapshot with PALKA SCM contract.
//! - Fail-closed error handling on non-Windows platforms.

use std::fmt;

/// Canonical Windows service key name in SCM registry.
pub const PALKA_SERVICE_NAME: &str = "PalkaService";

/// Canonical human-readable display name in Windows Services MMC.
pub const PALKA_SERVICE_DISPLAY_NAME: &str = "PALKA Service";

/// Canonical service description.
pub const PALKA_SERVICE_DESCRIPTION: &str = "PALKA parental control enforcement service";

/// Canonical service execution account.
pub const PALKA_SERVICE_ACCOUNT: &str = "LocalSystem";

/// Canonical service type: `SERVICE_WIN32_OWN_PROCESS` (0x00000010).
pub const PALKA_SERVICE_TYPE: u32 = 0x0000_0010;

/// Canonical start type: `SERVICE_AUTO_START` (0x00000002).
pub const PALKA_SERVICE_START_TYPE: u32 = 0x0000_0002;

/// Canonical error control: `SERVICE_ERROR_NORMAL` (0x00000001).
pub const PALKA_SERVICE_ERROR_CONTROL: u32 = 0x0000_0001;

/// Canonical reset period for failure actions count in seconds (24 hours).
pub const PALKA_SERVICE_RESET_PERIOD_SEC: u32 = 86_400;

/// Canonical restart delay for failure 1 in milliseconds (5 seconds).
pub const PALKA_SERVICE_RESTART_DELAY_1_MS: u32 = 5_000;

/// Canonical restart delay for failure 2 in milliseconds (15 seconds).
pub const PALKA_SERVICE_RESTART_DELAY_2_MS: u32 = 15_000;

/// Canonical restart delay for failure 3+ in milliseconds (60 seconds).
pub const PALKA_SERVICE_RESTART_DELAY_3_MS: u32 = 60_000;

/// Typed representation of an SCM recovery action type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmRecoveryActionType {
    Restart,
    Reboot,
    RunCommand,
    None,
    Unknown(u32),
}

impl fmt::Display for ScmRecoveryActionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Restart => write!(f, "Restart"),
            Self::Reboot => write!(f, "Reboot"),
            Self::RunCommand => write!(f, "RunCommand"),
            Self::None => write!(f, "None"),
            Self::Unknown(raw) => write!(f, "Unknown({raw})"),
        }
    }
}

/// Typed representation of an SCM recovery action and its delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmRecoveryAction {
    pub action_type: ScmRecoveryActionType,
    pub delay_ms: u32,
}

impl fmt::Display for ScmRecoveryAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(delay={}ms)", self.action_type, self.delay_ms)
    }
}

/// Owned, platform-neutral snapshot of an installed service's configuration in Windows SCM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmConfigSnapshot {
    pub service_name: String,
    pub display_name: String,
    pub description: String,
    pub service_type: u32,
    pub account: String,
    pub start_type: u32,
    pub error_control: u32,
    pub dependencies: Vec<String>,
    pub binary_path: String,
    pub delayed_auto_start: bool,
    pub failure_reset_period_sec: u32,
    pub recovery_actions: Vec<ScmRecoveryAction>,
    pub failure_actions_on_non_crash_failures: bool,
}

/// Mismatches identified when validating an SCM snapshot against the canonical contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScmConfigMismatch {
    ServiceName {
        expected: String,
        actual: String,
    },
    DisplayName {
        expected: String,
        actual: String,
    },
    Description {
        expected: String,
        actual: String,
    },
    ServiceType {
        expected: u32,
        actual: u32,
    },
    Account {
        expected: String,
        actual: String,
    },
    StartType {
        expected: u32,
        actual: u32,
    },
    DelayedAutoStart {
        expected: bool,
        actual: bool,
    },
    ErrorControl {
        expected: u32,
        actual: u32,
    },
    Dependencies {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    BinaryPath {
        expected: String,
        actual: String,
    },
    FailureResetPeriod {
        expected: u32,
        actual: u32,
    },
    RecoveryActionsCount {
        expected: usize,
        actual: usize,
    },
    RecoveryActionMismatch {
        index: usize,
        expected: ScmRecoveryAction,
        actual: ScmRecoveryAction,
    },
    ProhibitedRecoveryAction {
        index: usize,
        action: ScmRecoveryAction,
    },
    FailureActionsOnNonCrashFailures {
        expected: bool,
        actual: bool,
    },
}

impl fmt::Display for ScmConfigMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServiceName { expected, actual } => {
                write!(f, "ServiceName mismatch: expected '{expected}', got '{actual}'")
            }
            Self::DisplayName { expected, actual } => {
                write!(f, "DisplayName mismatch: expected '{expected}', got '{actual}'")
            }
            Self::Description { expected, actual } => {
                write!(f, "Description mismatch: expected '{expected}', got '{actual}'")
            }
            Self::ServiceType { expected, actual } => {
                write!(f, "ServiceType mismatch: expected {expected:#x}, got {actual:#x}")
            }
            Self::Account { expected, actual } => {
                write!(f, "Account mismatch: expected '{expected}', got '{actual}'")
            }
            Self::StartType { expected, actual } => {
                write!(f, "StartType mismatch: expected {expected:#x}, got {actual:#x}")
            }
            Self::DelayedAutoStart { expected, actual } => {
                write!(f, "DelayedAutoStart mismatch: expected {expected}, got {actual}")
            }
            Self::ErrorControl { expected, actual } => {
                write!(f, "ErrorControl mismatch: expected {expected:#x}, got {actual:#x}")
            }
            Self::Dependencies { expected, actual } => {
                write!(f, "Dependencies mismatch: expected {expected:?}, got {actual:?}")
            }
            Self::BinaryPath { expected, actual } => {
                write!(f, "BinaryPath mismatch: expected '{expected}', got '{actual}'")
            }
            Self::FailureResetPeriod { expected, actual } => {
                write!(f, "FailureResetPeriod mismatch: expected {expected}s, got {actual}s")
            }
            Self::RecoveryActionsCount { expected, actual } => {
                write!(f, "RecoveryActionsCount mismatch: expected {expected}, got {actual}")
            }
            Self::RecoveryActionMismatch { index, expected, actual } => {
                write!(f, "RecoveryAction[{index}] mismatch: expected {expected}, got {actual}")
            }
            Self::ProhibitedRecoveryAction { index, action } => {
                write!(f, "ProhibitedRecoveryAction at index {index}: {action}")
            }
            Self::FailureActionsOnNonCrashFailures { expected, actual } => {
                write!(
                    f,
                    "FailureActionsOnNonCrashFailures mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl ScmConfigSnapshot {
    /// Pure canonical validator comparing this snapshot with canonical PALKA SCM profile.
    ///
    /// Checks:
    /// - Identity: service key name, display name, description.
    /// - Service type and account: `SERVICE_WIN32_OWN_PROCESS` and `LocalSystem`.
    /// - Startup: `SERVICE_AUTO_START`, delayed autostart false, `SERVICE_ERROR_NORMAL`, empty dependencies.
    /// - Binary path: exactly equal to the provided expected representation.
    /// - Recovery policy: 86400s reset period, exactly 3 restart actions (5s, 15s, 60s),
    ///   no `Reboot` or `RunCommand` actions, and `failure_actions_on_non_crash_failures == false`.
    pub fn validate_canonical(&self, expected_binary_path: &str) -> Vec<ScmConfigMismatch> {
        let mut mismatches = Vec::new();

        // 1. Identity checks
        if self.service_name != PALKA_SERVICE_NAME {
            mismatches.push(ScmConfigMismatch::ServiceName {
                expected: PALKA_SERVICE_NAME.to_string(),
                actual: self.service_name.clone(),
            });
        }
        if self.display_name != PALKA_SERVICE_DISPLAY_NAME {
            mismatches.push(ScmConfigMismatch::DisplayName {
                expected: PALKA_SERVICE_DISPLAY_NAME.to_string(),
                actual: self.display_name.clone(),
            });
        }
        if self.description != PALKA_SERVICE_DESCRIPTION {
            mismatches.push(ScmConfigMismatch::Description {
                expected: PALKA_SERVICE_DESCRIPTION.to_string(),
                actual: self.description.clone(),
            });
        }

        // 2. Type and Account checks
        if self.service_type != PALKA_SERVICE_TYPE {
            mismatches.push(ScmConfigMismatch::ServiceType {
                expected: PALKA_SERVICE_TYPE,
                actual: self.service_type,
            });
        }
        if self.account != PALKA_SERVICE_ACCOUNT {
            mismatches.push(ScmConfigMismatch::Account {
                expected: PALKA_SERVICE_ACCOUNT.to_string(),
                actual: self.account.clone(),
            });
        }

        // 3. Startup and Dependency checks
        if self.start_type != PALKA_SERVICE_START_TYPE {
            mismatches.push(ScmConfigMismatch::StartType {
                expected: PALKA_SERVICE_START_TYPE,
                actual: self.start_type,
            });
        }
        if self.delayed_auto_start {
            mismatches.push(ScmConfigMismatch::DelayedAutoStart {
                expected: false,
                actual: self.delayed_auto_start,
            });
        }
        if self.error_control != PALKA_SERVICE_ERROR_CONTROL {
            mismatches.push(ScmConfigMismatch::ErrorControl {
                expected: PALKA_SERVICE_ERROR_CONTROL,
                actual: self.error_control,
            });
        }
        if !self.dependencies.is_empty() {
            mismatches.push(ScmConfigMismatch::Dependencies {
                expected: Vec::new(),
                actual: self.dependencies.clone(),
            });
        }

        // 4. Binary path check
        if self.binary_path != expected_binary_path {
            mismatches.push(ScmConfigMismatch::BinaryPath {
                expected: expected_binary_path.to_string(),
                actual: self.binary_path.clone(),
            });
        }

        // 5. Recovery reset period check
        if self.failure_reset_period_sec != PALKA_SERVICE_RESET_PERIOD_SEC {
            mismatches.push(ScmConfigMismatch::FailureResetPeriod {
                expected: PALKA_SERVICE_RESET_PERIOD_SEC,
                actual: self.failure_reset_period_sec,
            });
        }

        // 6. Recovery actions check
        let expected_actions = [
            ScmRecoveryAction {
                action_type: ScmRecoveryActionType::Restart,
                delay_ms: PALKA_SERVICE_RESTART_DELAY_1_MS,
            },
            ScmRecoveryAction {
                action_type: ScmRecoveryActionType::Restart,
                delay_ms: PALKA_SERVICE_RESTART_DELAY_2_MS,
            },
            ScmRecoveryAction {
                action_type: ScmRecoveryActionType::Restart,
                delay_ms: PALKA_SERVICE_RESTART_DELAY_3_MS,
            },
        ];

        // Check for any prohibited actions (Reboot, RunCommand)
        for (i, action) in self.recovery_actions.iter().enumerate() {
            if matches!(
                action.action_type,
                ScmRecoveryActionType::Reboot | ScmRecoveryActionType::RunCommand
            ) {
                mismatches.push(ScmConfigMismatch::ProhibitedRecoveryAction {
                    index: i,
                    action: action.clone(),
                });
            }
        }

        // Compare all overlapping action positions regardless of length
        for (i, (expected, actual)) in
            expected_actions.iter().zip(self.recovery_actions.iter()).enumerate()
        {
            if expected != actual {
                mismatches.push(ScmConfigMismatch::RecoveryActionMismatch {
                    index: i,
                    expected: expected.clone(),
                    actual: actual.clone(),
                });
            }
        }

        // Check action count separately
        if self.recovery_actions.len() != expected_actions.len() {
            mismatches.push(ScmConfigMismatch::RecoveryActionsCount {
                expected: expected_actions.len(),
                actual: self.recovery_actions.len(),
            });
        }

        // 7. Failure flag check
        if self.failure_actions_on_non_crash_failures {
            mismatches.push(ScmConfigMismatch::FailureActionsOnNonCrashFailures {
                expected: false,
                actual: self.failure_actions_on_non_crash_failures,
            });
        }

        mismatches
    }
}

/// Errors returned by the SCM query adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScmQueryError {
    /// SCM operations are not supported on this platform (Fail-Closed, SCM-20).
    UnsupportedPlatform,
    /// The target service is not installed in the Windows SCM database.
    ServiceNotInstalled { service_name: String },
    /// A Windows API call failed.
    WindowsApi {
        function: &'static str,
        code: u32,
        message: String,
    },
    /// A Windows API returned malformed or uninterpretable data.
    MalformedResponse {
        function: &'static str,
        detail: String,
    },
}

impl fmt::Display for ScmQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "SCM operations are only supported on Windows"),
            Self::ServiceNotInstalled { service_name } => {
                write!(f, "service '{service_name}' is not installed")
            }
            Self::WindowsApi { function, code, message } => {
                write!(f, "Windows API '{function}' failed with code {code} ({code:#x}): {message}")
            }
            Self::MalformedResponse { function, detail } => {
                write!(f, "malformed response from Windows API '{function}': {detail}")
            }
        }
    }
}

impl std::error::Error for ScmQueryError {}

#[cfg(windows)]
struct ScHandle(windows::Win32::System::Services::SC_HANDLE);

#[cfg(windows)]
impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is an active, valid SC_HANDLE opened via OpenSCManagerW or OpenServiceW.
            // Closing it with CloseServiceHandle satisfies Win32 SCM resource management rules.
            unsafe {
                let _ = windows::Win32::System::Services::CloseServiceHandle(self.0);
            }
        }
    }
}

/// Reusable aligned memory buffer for Win32 query structures.
///
/// Backed by a `Vec<u64>` to guarantee 8-byte alignment, satisfying all Win32 SCM configuration structs.
#[cfg(windows)]
struct AlignedBuffer {
    data: Vec<u64>,
    byte_len: usize,
}

#[cfg(windows)]
impl AlignedBuffer {
    fn new(byte_len: usize) -> Self {
        let u64_count = byte_len
            .checked_add(7)
            .map(|v| v / 8)
            .unwrap_or(byte_len);
        Self {
            data: vec![0u64; u64_count],
            byte_len,
        }
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `self.data` holds `data.len() * 8` bytes which is >= `self.byte_len`.
        // The slice is fully valid for mutable byte-level access up to `self.byte_len`.
        unsafe {
            std::slice::from_raw_parts_mut(self.data.as_mut_ptr() as *mut u8, self.byte_len)
        }
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `self.data` holds `data.len() * 8` bytes which is >= `self.byte_len`.
        // The slice is fully valid for read-only access up to `self.byte_len`.
        unsafe {
            std::slice::from_raw_parts(self.data.as_ptr() as *const u8, self.byte_len)
        }
    }

    fn as_struct<T>(&self) -> Result<&T, ScmQueryError> {
        let req_size = std::mem::size_of::<T>();
        if self.byte_len < req_size {
            return Err(ScmQueryError::MalformedResponse {
                function: std::any::type_name::<T>(),
                detail: format!(
                    "buffer size {} is less than required struct size {}",
                    self.byte_len, req_size
                ),
            });
        }
        let ptr = self.data.as_ptr() as *const u8;
        let align = std::mem::align_of::<T>();
        if (ptr as usize) % align != 0 {
            return Err(ScmQueryError::MalformedResponse {
                function: std::any::type_name::<T>(),
                detail: format!("buffer address {ptr:p} is not aligned for struct (align {align})"),
            });
        }
        // SAFETY: `ptr` is aligned to align_of::<u64>() (8 bytes) which is >= align_of::<T>()
        // for all Win32 service configuration structs, and `self.byte_len >= size_of::<T>()`.
        // Memory has been populated by Win32 API.
        unsafe { Ok(&*(ptr as *const T)) }
    }
}

#[cfg(windows)]
fn win32_error_code(err: &windows::core::Error) -> u32 {
    windows::Win32::Foundation::WIN32_ERROR::from_error(err)
        .map(|e| e.0)
        .unwrap_or_else(|| err.code().0 as u32)
}

/// Safely decodes a null-terminated UTF-16 string from within caller-owned query buffer boundaries.
#[cfg(windows)]
fn decode_utf16_z_within_buffer(
    ptr: windows::core::PWSTR,
    buffer: &[u8],
    field_name: &'static str,
    allow_null: bool,
) -> Result<String, ScmQueryError> {
    if ptr.is_null() {
        if allow_null {
            return Ok(String::new());
        } else {
            return Err(ScmQueryError::MalformedResponse {
                function: field_name,
                detail: "unexpected null pointer for non-optional string field".to_string(),
            });
        }
    }

    let p_addr = ptr.0 as usize;
    let b_start = buffer.as_ptr() as usize;
    let b_end = b_start.checked_add(buffer.len()).ok_or_else(|| {
        ScmQueryError::MalformedResponse {
            function: field_name,
            detail: "buffer address arithmetic overflow".to_string(),
        }
    })?;

    if p_addr < b_start || p_addr >= b_end || (p_addr % 2) != 0 {
        return Err(ScmQueryError::MalformedResponse {
            function: field_name,
            detail: "string pointer is outside buffer or not 2-byte aligned".to_string(),
        });
    }

    let max_u16_chars = (b_end - p_addr) / 2;
    // SAFETY: `ptr.0` resides within `buffer`, is 2-byte aligned, and `max_u16_chars`
    // strictly limits access to the remaining bytes of `buffer`.
    let u16_slice = unsafe { std::slice::from_raw_parts(ptr.0, max_u16_chars) };

    let mut nul_pos = None;
    for (idx, &ch) in u16_slice.iter().enumerate() {
        if ch == 0 {
            nul_pos = Some(idx);
            break;
        }
    }

    let nul_idx = nul_pos.ok_or_else(|| ScmQueryError::MalformedResponse {
        function: field_name,
        detail: "string is not null-terminated within buffer bounds".to_string(),
    })?;

    String::from_utf16(&u16_slice[..nul_idx]).map_err(|e| ScmQueryError::MalformedResponse {
        function: field_name,
        detail: format!("invalid UTF-16: {e}"),
    })
}

/// Safely parses a MULTI_SZ string array within caller-owned query buffer boundaries.
#[cfg(windows)]
fn parse_multi_sz_within_buffer(
    ptr: windows::core::PWSTR,
    buffer: &[u8],
) -> Result<Vec<String>, ScmQueryError> {
    if ptr.is_null() {
        return Ok(Vec::new());
    }
    let p_addr = ptr.0 as usize;
    let b_start = buffer.as_ptr() as usize;
    let b_end = b_start.checked_add(buffer.len()).ok_or_else(|| {
        ScmQueryError::MalformedResponse {
            function: "QueryServiceConfigW(lpDependencies)",
            detail: "buffer address arithmetic overflow".to_string(),
        }
    })?;

    if p_addr < b_start || p_addr >= b_end || (p_addr % 2) != 0 {
        return Err(ScmQueryError::MalformedResponse {
            function: "QueryServiceConfigW(lpDependencies)",
            detail: "dependencies pointer is outside buffer or not 2-byte aligned".to_string(),
        });
    }

    let max_u16_chars = (b_end - p_addr) / 2;
    // SAFETY: `ptr.0` resides within `buffer`, is 2-byte aligned, and `max_u16_chars`
    // strictly limits access to the remaining bytes of `buffer`.
    let u16_slice = unsafe { std::slice::from_raw_parts(ptr.0, max_u16_chars) };

    let mut result = Vec::new();
    let mut current = Vec::new();

    let mut i = 0;
    while i < u16_slice.len() {
        let ch = u16_slice[i];
        if ch == 0 {
            if current.is_empty() {
                // Double null terminator indicates end of MULTI_SZ list
                return Ok(result);
            } else {
                let s = String::from_utf16(&current).map_err(|e| {
                    ScmQueryError::MalformedResponse {
                        function: "QueryServiceConfigW(lpDependencies)",
                        detail: format!("invalid UTF-16 in dependencies: {e}"),
                    }
                })?;
                result.push(s);
                current.clear();
            }
        } else {
            current.push(ch);
        }
        i += 1;
    }

    Err(ScmQueryError::MalformedResponse {
        function: "QueryServiceConfigW(lpDependencies)",
        detail: "dependencies MULTI_SZ missing terminal double null".to_string(),
    })
}

/// Reads the installed configuration of PALKA Windows Service from Windows SCM.
///
/// On Windows, this function:
/// 1. Connects to the local SCM database with minimal read-only rights (`SC_MANAGER_CONNECT`).
/// 2. Opens the service directly by canonical service key name (`PALKA_SERVICE_NAME = "PalkaService"`).
/// 3. Queries basic configuration via `QueryServiceConfigW` (type, account, start type, error control, binary path, dependencies).
/// 4. Queries the actual display name and retrieves the actual case-preserved service key name via `GetServiceKeyNameW`.
/// 5. Queries extended configuration via `QueryServiceConfig2W` (description, delayed autostart, failure actions, failure actions flag).
/// 6. Returns a fully owned, platform-neutral `ScmConfigSnapshot`.
///
/// On platforms other than Windows, returns `Err(ScmQueryError::UnsupportedPlatform)` (SCM-20 Fail-Closed).
pub fn query_palka_service_config() -> Result<ScmConfigSnapshot, ScmQueryError> {
    #[cfg(not(windows))]
    {
        Err(ScmQueryError::UnsupportedPlatform)
    }

    #[cfg(windows)]
    {
        query_palka_service_config_windows()
    }
}

#[cfg(windows)]
fn query_palka_service_config_windows() -> Result<ScmConfigSnapshot, ScmQueryError> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SERVICE_DOES_NOT_EXIST};
    use windows::Win32::System::Services::*;

    // 1. Open local SCM manager with minimal SC_MANAGER_CONNECT access
    // SAFETY: Local SCM connection with null machine/database names. Desired access is SC_MANAGER_CONNECT only.
    let scm_raw = unsafe {
        OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT,
        )
    }
    .map_err(|e| ScmQueryError::WindowsApi {
        function: "OpenSCManagerW",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;
    let scm = ScHandle(scm_raw);

    // 2. Open service directly by canonical service key name ("PalkaService")
    let service_name_wide: Vec<u16> = PALKA_SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `scm.0` is a valid open SC_HANDLE. `service_name_wide` is null-terminated UTF-16.
    // Desired access is SERVICE_QUERY_CONFIG (read-only).
    let svc_raw = unsafe {
        OpenServiceW(
            scm.0,
            PCWSTR(service_name_wide.as_ptr()),
            SERVICE_QUERY_CONFIG,
        )
    }
    .map_err(|e| {
        if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult()
            || win32_error_code(&e) == ERROR_SERVICE_DOES_NOT_EXIST.0
        {
            ScmQueryError::ServiceNotInstalled {
                service_name: PALKA_SERVICE_NAME.to_string(),
            }
        } else {
            ScmQueryError::WindowsApi {
                function: "OpenServiceW",
                code: win32_error_code(&e),
                message: e.message().to_string(),
            }
        }
    })?;
    let svc = ScHandle(svc_raw);

    // 3. Query basic configuration (QueryServiceConfigW)
    let mut config_bytes_needed = 0u32;
    // SAFETY: Initial sizing call with None buffer to retrieve required buffer size in bytes.
    let config_size_res = unsafe {
        QueryServiceConfigW(svc.0, None, 0, &mut config_bytes_needed)
    };
    if let Err(ref e) = config_size_res {
        if e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
            && win32_error_code(e) != ERROR_INSUFFICIENT_BUFFER.0
        {
            return Err(ScmQueryError::WindowsApi {
                function: "QueryServiceConfigW",
                code: win32_error_code(e),
                message: e.message().to_string(),
            });
        }
    }

    let mut config_buf = AlignedBuffer::new(config_bytes_needed as usize);
    // SAFETY: `config_buf` owns at least `config_bytes_needed` bytes and is 8-byte aligned.
    unsafe {
        QueryServiceConfigW(
            svc.0,
            Some(config_buf.as_bytes_mut().as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW),
            config_bytes_needed,
            &mut config_bytes_needed,
        )
    }
    .map_err(|e| ScmQueryError::WindowsApi {
        function: "QueryServiceConfigW",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    let qsc = config_buf.as_struct::<QUERY_SERVICE_CONFIGW>()?;

    let display_name = decode_utf16_z_within_buffer(
        qsc.lpDisplayName,
        config_buf.as_bytes(),
        "QueryServiceConfigW(lpDisplayName)",
        false,
    )?;

    let service_type = qsc.dwServiceType.0;
    let start_type = qsc.dwStartType.0;
    let error_control = qsc.dwErrorControl.0;

    let account = decode_utf16_z_within_buffer(
        qsc.lpServiceStartName,
        config_buf.as_bytes(),
        "QueryServiceConfigW(lpServiceStartName)",
        true,
    )?;

    let binary_path = decode_utf16_z_within_buffer(
        qsc.lpBinaryPathName,
        config_buf.as_bytes(),
        "QueryServiceConfigW(lpBinaryPathName)",
        false,
    )?;

    let dependencies = parse_multi_sz_within_buffer(
        qsc.lpDependencies,
        config_buf.as_bytes(),
    )?;

    // 4. Exact case-preserved service key name proof via GetServiceKeyNameW
    // using ACTUAL display name returned by SCM
    let display_name_wide: Vec<u16> = display_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut cch_needed = 0u32;
    // SAFETY: Initial sizing call to GetServiceKeyNameW with None buffer.
    let size_res = unsafe {
        GetServiceKeyNameW(
            scm.0,
            PCWSTR(display_name_wide.as_ptr()),
            None,
            &mut cch_needed,
        )
    };

    if let Err(ref e) = size_res {
        if e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
            && win32_error_code(e) != ERROR_INSUFFICIENT_BUFFER.0
        {
            return Err(ScmQueryError::WindowsApi {
                function: "GetServiceKeyNameW",
                code: win32_error_code(e),
                message: e.message().to_string(),
            });
        }
    }

    let buf_len = (cch_needed as usize).checked_add(1).ok_or_else(|| {
        ScmQueryError::MalformedResponse {
            function: "GetServiceKeyNameW",
            detail: "buffer size arithmetic overflow".to_string(),
        }
    })?;
    let mut key_name_buf = vec![0u16; buf_len];
    let mut cch_actual = key_name_buf.len() as u32;

    // SAFETY: `key_name_buf` has length `cch_needed + 1` u16 characters.
    unsafe {
        GetServiceKeyNameW(
            scm.0,
            PCWSTR(display_name_wide.as_ptr()),
            Some(PWSTR(key_name_buf.as_mut_ptr())),
            &mut cch_actual,
        )
    }
    .map_err(|e| ScmQueryError::WindowsApi {
        function: "GetServiceKeyNameW",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    if (cch_actual as usize) > key_name_buf.len() {
        return Err(ScmQueryError::MalformedResponse {
            function: "GetServiceKeyNameW",
            detail: "cch_actual exceeds allocated buffer size".to_string(),
        });
    }

    let actual_service_key_name =
        String::from_utf16(&key_name_buf[..cch_actual as usize]).map_err(|e| {
            ScmQueryError::MalformedResponse {
                function: "GetServiceKeyNameW",
                detail: format!("invalid UTF-16 in service key name: {e}"),
            }
        })?;

    // 5. Query description (QueryServiceConfig2W / SERVICE_CONFIG_DESCRIPTION)
    let mut desc_bytes_needed = 0u32;
    // SAFETY: Initial sizing call with None buffer.
    let desc_size_res = unsafe {
        QueryServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_DESCRIPTION,
            None,
            &mut desc_bytes_needed,
        )
    };
    if let Err(ref e) = desc_size_res {
        if e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
            && win32_error_code(e) != ERROR_INSUFFICIENT_BUFFER.0
        {
            return Err(ScmQueryError::WindowsApi {
                function: "QueryServiceConfig2W(DESCRIPTION)",
                code: win32_error_code(e),
                message: e.message().to_string(),
            });
        }
    }

    let mut desc_buf = AlignedBuffer::new(desc_bytes_needed as usize);
    // SAFETY: `desc_buf` owns at least `desc_bytes_needed` bytes and is 8-byte aligned.
    unsafe {
        QueryServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some(desc_buf.as_bytes_mut()),
            &mut desc_bytes_needed,
        )
    }
    .map_err(|e| ScmQueryError::WindowsApi {
        function: "QueryServiceConfig2W(DESCRIPTION)",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    let desc_struct = desc_buf.as_struct::<SERVICE_DESCRIPTIONW>()?;
    let description = decode_utf16_z_within_buffer(
        desc_struct.lpDescription,
        desc_buf.as_bytes(),
        "QueryServiceConfig2W(DESCRIPTION)",
        true,
    )?;

    // 6. Query delayed autostart (QueryServiceConfig2W / SERVICE_CONFIG_DELAYED_AUTO_START_INFO)
    let mut delayed_bytes_needed = 0u32;
    let req_delayed_size = std::mem::size_of::<SERVICE_DELAYED_AUTO_START_INFO>();
    let mut delayed_buf = AlignedBuffer::new(req_delayed_size);
    // SAFETY: Passing aligned buffer sized for SERVICE_DELAYED_AUTO_START_INFO.
    let delayed_res = unsafe {
        QueryServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
            Some(delayed_buf.as_bytes_mut()),
            &mut delayed_bytes_needed,
        )
    };

    let delayed_auto_start = match delayed_res {
        Ok(()) => {
            let s = delayed_buf.as_struct::<SERVICE_DELAYED_AUTO_START_INFO>()?;
            s.fDelayedAutostart.as_bool()
        }
        Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult()
            || win32_error_code(&e) == ERROR_INSUFFICIENT_BUFFER.0 =>
        {
            let mut dynamic_buf = AlignedBuffer::new(delayed_bytes_needed as usize);
            // SAFETY: Re-querying with dynamically sized buffer.
            unsafe {
                QueryServiceConfig2W(
                    svc.0,
                    SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                    Some(dynamic_buf.as_bytes_mut()),
                    &mut delayed_bytes_needed,
                )
            }
            .map_err(|err| ScmQueryError::WindowsApi {
                function: "QueryServiceConfig2W(DELAYED_AUTO_START_INFO)",
                code: win32_error_code(&err),
                message: err.message().to_string(),
            })?;
            let s = dynamic_buf.as_struct::<SERVICE_DELAYED_AUTO_START_INFO>()?;
            s.fDelayedAutostart.as_bool()
        }
        Err(e) => {
            return Err(ScmQueryError::WindowsApi {
                function: "QueryServiceConfig2W(DELAYED_AUTO_START_INFO)",
                code: win32_error_code(&e),
                message: e.message().to_string(),
            });
        }
    };

    // 7. Query failure actions (QueryServiceConfig2W / SERVICE_CONFIG_FAILURE_ACTIONS)
    let mut fail_bytes_needed = 0u32;
    // SAFETY: Initial sizing call with None buffer.
    let fail_size_res = unsafe {
        QueryServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            None,
            &mut fail_bytes_needed,
        )
    };
    if let Err(ref e) = fail_size_res {
        if e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
            && win32_error_code(e) != ERROR_INSUFFICIENT_BUFFER.0
        {
            return Err(ScmQueryError::WindowsApi {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS)",
                code: win32_error_code(e),
                message: e.message().to_string(),
            });
        }
    }

    let mut fail_buf = AlignedBuffer::new(fail_bytes_needed as usize);
    // SAFETY: `fail_buf` owns at least `fail_bytes_needed` bytes and is 8-byte aligned.
    unsafe {
        QueryServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(fail_buf.as_bytes_mut()),
            &mut fail_bytes_needed,
        )
    }
    .map_err(|e| ScmQueryError::WindowsApi {
        function: "QueryServiceConfig2W(FAILURE_ACTIONS)",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    let fail_struct = fail_buf.as_struct::<SERVICE_FAILURE_ACTIONSW>()?;
    let failure_reset_period_sec = fail_struct.dwResetPeriod;

    let mut recovery_actions = Vec::new();
    if fail_struct.cActions > 0 {
        if fail_struct.lpsaActions.is_null() {
            return Err(ScmQueryError::MalformedResponse {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS)",
                detail: "cActions > 0 but lpsaActions is null".to_string(),
            });
        }

        let action_count = fail_struct.cActions as usize;
        let action_size = std::mem::size_of::<SC_ACTION>();
        let total_action_bytes = action_count
            .checked_mul(action_size)
            .ok_or_else(|| ScmQueryError::MalformedResponse {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS)",
                detail: "action array size arithmetic overflow".to_string(),
            })?;

        let actions_ptr = fail_struct.lpsaActions as usize;
        let buf_start = fail_buf.as_bytes().as_ptr() as usize;
        let buf_end = buf_start.checked_add(fail_buf.as_bytes().len()).ok_or_else(|| {
            ScmQueryError::MalformedResponse {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS)",
                detail: "buffer address arithmetic overflow".to_string(),
            }
        })?;

        let actions_end = actions_ptr.checked_add(total_action_bytes).ok_or_else(|| {
            ScmQueryError::MalformedResponse {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS)",
                detail: "action array end address arithmetic overflow".to_string(),
            }
        })?;

        if actions_ptr < buf_start
            || actions_end > buf_end
            || (actions_ptr % std::mem::align_of::<SC_ACTION>()) != 0
        {
            return Err(ScmQueryError::MalformedResponse {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS)",
                detail: "lpsaActions array is outside buffer bounds or unaligned".to_string(),
            });
        }

        // SAFETY: `actions_ptr` resides completely within `fail_buf` bounds,
        // has length `action_count` elements, and is aligned to align_of::<SC_ACTION>().
        let actions_slice = unsafe {
            std::slice::from_raw_parts(fail_struct.lpsaActions, action_count)
        };

        for action in actions_slice {
            let action_type = match action.Type {
                SC_ACTION_RESTART => ScmRecoveryActionType::Restart,
                SC_ACTION_REBOOT => ScmRecoveryActionType::Reboot,
                SC_ACTION_RUN_COMMAND => ScmRecoveryActionType::RunCommand,
                SC_ACTION_NONE => ScmRecoveryActionType::None,
                other => ScmRecoveryActionType::Unknown(other.0 as u32),
            };
            recovery_actions.push(ScmRecoveryAction {
                action_type,
                delay_ms: action.Delay,
            });
        }
    }

    // 8. Query failure actions flag (QueryServiceConfig2W / SERVICE_CONFIG_FAILURE_ACTIONS_FLAG)
    let mut flag_bytes_needed = 0u32;
    let req_flag_size = std::mem::size_of::<SERVICE_FAILURE_ACTIONS_FLAG>();
    let mut flag_buf = AlignedBuffer::new(req_flag_size);
    // SAFETY: Passing aligned buffer sized for SERVICE_FAILURE_ACTIONS_FLAG.
    let flag_res = unsafe {
        QueryServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            Some(flag_buf.as_bytes_mut()),
            &mut flag_bytes_needed,
        )
    };

    let failure_actions_on_non_crash_failures = match flag_res {
        Ok(()) => {
            let s = flag_buf.as_struct::<SERVICE_FAILURE_ACTIONS_FLAG>()?;
            s.fFailureActionsOnNonCrashFailures.as_bool()
        }
        Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult()
            || win32_error_code(&e) == ERROR_INSUFFICIENT_BUFFER.0 =>
        {
            let mut dynamic_buf = AlignedBuffer::new(flag_bytes_needed as usize);
            // SAFETY: Re-querying with dynamically sized buffer.
            unsafe {
                QueryServiceConfig2W(
                    svc.0,
                    SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
                    Some(dynamic_buf.as_bytes_mut()),
                    &mut flag_bytes_needed,
                )
            }
            .map_err(|err| ScmQueryError::WindowsApi {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS_FLAG)",
                code: win32_error_code(&err),
                message: err.message().to_string(),
            })?;
            let s = dynamic_buf.as_struct::<SERVICE_FAILURE_ACTIONS_FLAG>()?;
            s.fFailureActionsOnNonCrashFailures.as_bool()
        }
        Err(e) => {
            return Err(ScmQueryError::WindowsApi {
                function: "QueryServiceConfig2W(FAILURE_ACTIONS_FLAG)",
                code: win32_error_code(&e),
                message: e.message().to_string(),
            });
        }
    };

    Ok(ScmConfigSnapshot {
        service_name: actual_service_key_name,
        display_name,
        description,
        service_type,
        account,
        start_type,
        error_control,
        dependencies,
        binary_path,
        delayed_auto_start,
        failure_reset_period_sec,
        recovery_actions,
        failure_actions_on_non_crash_failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_test_snapshot(binary_path: &str) -> ScmConfigSnapshot {
        ScmConfigSnapshot {
            service_name: PALKA_SERVICE_NAME.to_string(),
            display_name: PALKA_SERVICE_DISPLAY_NAME.to_string(),
            description: PALKA_SERVICE_DESCRIPTION.to_string(),
            service_type: PALKA_SERVICE_TYPE,
            account: PALKA_SERVICE_ACCOUNT.to_string(),
            start_type: PALKA_SERVICE_START_TYPE,
            error_control: PALKA_SERVICE_ERROR_CONTROL,
            dependencies: Vec::new(),
            binary_path: binary_path.to_string(),
            delayed_auto_start: false,
            failure_reset_period_sec: PALKA_SERVICE_RESET_PERIOD_SEC,
            recovery_actions: vec![
                ScmRecoveryAction {
                    action_type: ScmRecoveryActionType::Restart,
                    delay_ms: PALKA_SERVICE_RESTART_DELAY_1_MS,
                },
                ScmRecoveryAction {
                    action_type: ScmRecoveryActionType::Restart,
                    delay_ms: PALKA_SERVICE_RESTART_DELAY_2_MS,
                },
                ScmRecoveryAction {
                    action_type: ScmRecoveryActionType::Restart,
                    delay_ms: PALKA_SERVICE_RESTART_DELAY_3_MS,
                },
            ],
            failure_actions_on_non_crash_failures: false,
        }
    }

    const TEST_CANONICAL_PATH: &str = r#""C:\Program Files\PALKA\palka-service.exe""#;

    // 1. exact canonical synthetic snapshot => no mismatches
    #[test]
    fn test_01_canonical_snapshot_has_no_mismatches() {
        let snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert!(
            mismatches.is_empty(),
            "expected no mismatches, got {mismatches:?}"
        );
    }

    // 2. wrong service name => identity mismatch
    #[test]
    fn test_02_wrong_service_name_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.service_name = "WrongService".to_string();
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::ServiceName {
                expected: PALKA_SERVICE_NAME.to_string(),
                actual: "WrongService".to_string(),
            }]
        );
    }

    // 3. wrong display name => mismatch
    #[test]
    fn test_03_wrong_display_name_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.display_name = "Other Display Name".to_string();
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::DisplayName {
                expected: PALKA_SERVICE_DISPLAY_NAME.to_string(),
                actual: "Other Display Name".to_string(),
            }]
        );
    }

    // 4. wrong description => mismatch
    #[test]
    fn test_04_wrong_description_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.description = "Incorrect description".to_string();
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::Description {
                expected: PALKA_SERVICE_DESCRIPTION.to_string(),
                actual: "Incorrect description".to_string(),
            }]
        );
    }

    // 5. wrong service type => mismatch
    #[test]
    fn test_05_wrong_service_type_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.service_type = 0x0000_0020; // SERVICE_WIN32_SHARE_PROCESS
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::ServiceType {
                expected: PALKA_SERVICE_TYPE,
                actual: 0x0000_0020,
            }]
        );
    }

    // 6. wrong account => mismatch
    #[test]
    fn test_06_wrong_account_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.account = "NT AUTHORITY\\NetworkService".to_string();
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::Account {
                expected: PALKA_SERVICE_ACCOUNT.to_string(),
                actual: "NT AUTHORITY\\NetworkService".to_string(),
            }]
        );
    }

    // 7. manual/different start type => mismatch
    #[test]
    fn test_07_wrong_start_type_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.start_type = 0x0000_0003; // SERVICE_DEMAND_START (manual)
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::StartType {
                expected: PALKA_SERVICE_START_TYPE,
                actual: 0x0000_0003,
            }]
        );
    }

    // 8. delayed auto-start true => mismatch
    #[test]
    fn test_08_delayed_auto_start_true_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.delayed_auto_start = true;
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::DelayedAutoStart {
                expected: false,
                actual: true,
            }]
        );
    }

    // 9. wrong error control => mismatch
    #[test]
    fn test_09_wrong_error_control_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.error_control = 0x0000_0000; // SERVICE_ERROR_IGNORE
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::ErrorControl {
                expected: PALKA_SERVICE_ERROR_CONTROL,
                actual: 0x0000_0000,
            }]
        );
    }

    // 10. non-empty dependencies => mismatch
    #[test]
    fn test_10_non_empty_dependencies_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.dependencies = vec!["Tcpip".to_string(), "RpcSs".to_string()];
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::Dependencies {
                expected: Vec::new(),
                actual: vec!["Tcpip".to_string(), "RpcSs".to_string()],
            }]
        );
    }

    // 11. wrong binary path => mismatch
    #[test]
    fn test_11_wrong_binary_path_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.binary_path = r#""C:\Tampered\palka-service.exe""#.to_string();
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::BinaryPath {
                expected: TEST_CANONICAL_PATH.to_string(),
                actual: r#""C:\Tampered\palka-service.exe""#.to_string(),
            }]
        );
    }

    // 12. wrong reset period => mismatch
    #[test]
    fn test_12_wrong_reset_period_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.failure_reset_period_sec = 3600;
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::FailureResetPeriod {
                expected: PALKA_SERVICE_RESET_PERIOD_SEC,
                actual: 3600,
            }]
        );
    }

    // 13. recovery order incorrect => mismatch
    #[test]
    fn test_13_recovery_order_incorrect_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        // Swap delays: 15s first, 5s second
        snapshot.recovery_actions[0].delay_ms = 15_000;
        snapshot.recovery_actions[1].delay_ms = 5_000;
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![
                ScmConfigMismatch::RecoveryActionMismatch {
                    index: 0,
                    expected: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 5_000,
                    },
                    actual: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 15_000,
                    },
                },
                ScmConfigMismatch::RecoveryActionMismatch {
                    index: 1,
                    expected: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 15_000,
                    },
                    actual: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 5_000,
                    },
                },
            ]
        );
    }

    // 14. wrong restart delay => mismatch
    #[test]
    fn test_14_wrong_restart_delay_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.recovery_actions[2].delay_ms = 120_000;
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::RecoveryActionMismatch {
                index: 2,
                expected: ScmRecoveryAction {
                    action_type: ScmRecoveryActionType::Restart,
                    delay_ms: 60_000,
                },
                actual: ScmRecoveryAction {
                    action_type: ScmRecoveryActionType::Restart,
                    delay_ms: 120_000,
                },
            }]
        );
    }

    // 15. extra fourth configured action => mismatch
    #[test]
    fn test_15_extra_fourth_configured_action_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.recovery_actions.push(ScmRecoveryAction {
            action_type: ScmRecoveryActionType::Restart,
            delay_ms: 60_000,
        });
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::RecoveryActionsCount {
                expected: 3,
                actual: 4,
            }]
        );
    }

    // 16. Reboot action => mismatch
    #[test]
    fn test_16_reboot_action_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.recovery_actions[2] = ScmRecoveryAction {
            action_type: ScmRecoveryActionType::Reboot,
            delay_ms: 60_000,
        };
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![
                ScmConfigMismatch::ProhibitedRecoveryAction {
                    index: 2,
                    action: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Reboot,
                        delay_ms: 60_000,
                    },
                },
                ScmConfigMismatch::RecoveryActionMismatch {
                    index: 2,
                    expected: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 60_000,
                    },
                    actual: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Reboot,
                        delay_ms: 60_000,
                    },
                },
            ]
        );
    }

    // 17. RunCommand action => mismatch
    #[test]
    fn test_17_run_command_action_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.recovery_actions[1] = ScmRecoveryAction {
            action_type: ScmRecoveryActionType::RunCommand,
            delay_ms: 15_000,
        };
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![
                ScmConfigMismatch::ProhibitedRecoveryAction {
                    index: 1,
                    action: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::RunCommand,
                        delay_ms: 15_000,
                    },
                },
                ScmConfigMismatch::RecoveryActionMismatch {
                    index: 1,
                    expected: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 15_000,
                    },
                    actual: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::RunCommand,
                        delay_ms: 15_000,
                    },
                },
            ]
        );
    }

    // 18. failure_actions_on_non_crash_failures = true => mismatch
    #[test]
    fn test_18_failure_flag_true_reports_mismatch() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.failure_actions_on_non_crash_failures = true;
        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![ScmConfigMismatch::FailureActionsOnNonCrashFailures {
                expected: false,
                actual: true,
            }]
        );
    }

    // 19. multiple simultaneous deviations => validator returns all multiple deviations
    #[test]
    fn test_19_multiple_simultaneous_deviations_reported() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.service_name = "WrongName".to_string();
        snapshot.delayed_auto_start = true;
        snapshot.failure_reset_period_sec = 0;
        snapshot.failure_actions_on_non_crash_failures = true;

        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(mismatches.len(), 4);
        assert!(mismatches.contains(&ScmConfigMismatch::ServiceName {
            expected: PALKA_SERVICE_NAME.to_string(),
            actual: "WrongName".to_string(),
        }));
        assert!(mismatches.contains(&ScmConfigMismatch::DelayedAutoStart {
            expected: false,
            actual: true,
        }));
        assert!(mismatches.contains(&ScmConfigMismatch::FailureResetPeriod {
            expected: PALKA_SERVICE_RESET_PERIOD_SEC,
            actual: 0,
        }));
        assert!(
            mismatches.contains(&ScmConfigMismatch::FailureActionsOnNonCrashFailures {
                expected: false,
                actual: true,
            })
        );
    }

    // 20. non-Windows query API => UnsupportedPlatform when cfg(not(windows))
    #[cfg(not(windows))]
    #[test]
    fn test_20_non_windows_query_returns_unsupported_platform() {
        assert_eq!(
            query_palka_service_config(),
            Err(ScmQueryError::UnsupportedPlatform)
        );
    }

    // 21. recovery count AND element mismatch reported simultaneously
    #[test]
    fn test_21_recovery_count_and_element_mismatch_reported() {
        let mut snapshot = canonical_test_snapshot(TEST_CANONICAL_PATH);
        snapshot.recovery_actions = vec![
            ScmRecoveryAction {
                action_type: ScmRecoveryActionType::Restart,
                delay_ms: 15_000,
            },
            ScmRecoveryAction {
                action_type: ScmRecoveryActionType::Restart,
                delay_ms: 15_000,
            },
        ];

        let mismatches = snapshot.validate_canonical(TEST_CANONICAL_PATH);
        assert_eq!(
            mismatches,
            vec![
                ScmConfigMismatch::RecoveryActionMismatch {
                    index: 0,
                    expected: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 5_000,
                    },
                    actual: ScmRecoveryAction {
                        action_type: ScmRecoveryActionType::Restart,
                        delay_ms: 15_000,
                    },
                },
                ScmConfigMismatch::RecoveryActionsCount {
                    expected: 3,
                    actual: 2,
                },
            ]
        );
    }
}
