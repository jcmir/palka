//! Windows Service Control Manager (SCM) service provisioning and configuration convergence for PALKA.
//!
//! Responsibilities:
//! - Validate canonical executable path for `palka-service.exe`.
//! - Create `PalkaService` if absent using minimal necessary SCM rights.
//! - Converge existing `PalkaService` configuration to the canonical contract.
//! - Configure description, delayed auto-start, failure actions, and failure flag via `ChangeServiceConfig2W`.
//! - Perform mandatory query-back via the read-only SCM query adapter to verify convergence.
//! - Zero writes if the service is already canonical.
//! - Fail closed on unsupported platforms, unrepairable identity drift, or query-back mismatches.

use std::fmt;
use std::path::Path;

use crate::scm::{
    PALKA_SERVICE_ACCOUNT, PALKA_SERVICE_DESCRIPTION, PALKA_SERVICE_DISPLAY_NAME,
    PALKA_SERVICE_NAME, PALKA_SERVICE_RESET_PERIOD_SEC, PALKA_SERVICE_RESTART_DELAY_1_MS,
    PALKA_SERVICE_RESTART_DELAY_2_MS, PALKA_SERVICE_RESTART_DELAY_3_MS, ScmConfigMismatch,
    ScmConfigSnapshot, ScmQueryError, query_palka_service_config,
};

/// High-level outcome of the provisioning operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmProvisionOutcome {
    /// Service was already in the canonical configuration; zero mutations performed.
    AlreadyCanonical,
    /// Service was absent and was created with canonical configuration.
    Created,
    /// Service was present with drift and was converged to canonical configuration.
    Updated,
}

impl fmt::Display for ScmProvisionOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyCanonical => write!(f, "already canonical (no changes)"),
            Self::Created => write!(f, "service created"),
            Self::Updated => write!(f, "service configuration converged"),
        }
    }
}

/// Detailed result of a successful provisioning operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmProvisionResult {
    /// Action performed during provisioning.
    pub outcome: ScmProvisionOutcome,
    /// Authoritative post-provisioning snapshot verified by query-back.
    pub final_snapshot: ScmConfigSnapshot,
    /// Whether `fFailureActionsOnNonCrashFailures` was modified from `true` to `false`.
    pub failure_actions_flag_changed: bool,
    /// Whether OS restart is required before V4 behavioral verification of the failure actions flag.
    pub requires_os_restart_before_recovery_behavior_verification: bool,
}

/// Typed error model for SCM provisioning operations.
#[derive(Debug)]
pub enum ScmProvisionError {
    /// Platform is not supported (only Windows is supported).
    UnsupportedPlatform,
    /// Binary path failed canonical validation.
    InvalidBinaryPath { detail: String },
    /// SCM query failed during pre-provisioning inspection or query-back.
    Query { source: ScmQueryError },
    /// Win32 API call failed.
    WindowsApi {
        function: &'static str,
        code: u32,
        message: String,
    },
    /// Existing service identity does not match canonical name and cannot be safely repaired.
    UnrepairableServiceIdentity { expected: String, actual: String },
    /// Display name conflict (e.g. ERROR_DUPLICATE_SERVICE_NAME).
    DisplayNameConflict {
        display_name: String,
        code: u32,
        message: String,
    },
    /// Query-back validation failed: deviations remain after configuration.
    QueryBackMismatch { mismatches: Vec<ScmConfigMismatch> },
    /// Concurrent modification race detected in SCM.
    ConcurrentScmMutation { detail: String },
}

impl fmt::Display for ScmProvisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "SCM provisioning is only supported on Windows")
            }
            Self::InvalidBinaryPath { detail } => {
                write!(f, "invalid executable path for SCM provisioning: {detail}")
            }
            Self::Query { source } => {
                write!(f, "SCM query failed: {source}")
            }
            Self::WindowsApi {
                function,
                code,
                message,
            } => {
                write!(
                    f,
                    "Windows SCM API '{function}' failed with code {code} ({code:#x}): {message}"
                )
            }
            Self::UnrepairableServiceIdentity { expected, actual } => {
                write!(
                    f,
                    "service identity cannot be repaired: expected '{expected}', actual '{actual}'"
                )
            }
            Self::DisplayNameConflict {
                display_name,
                code,
                message,
            } => {
                write!(
                    f,
                    "display name conflict for '{display_name}' (code {code}): {message}"
                )
            }
            Self::QueryBackMismatch { mismatches } => {
                write!(
                    f,
                    "query-back validation failed with {} mismatch(es): {:?}",
                    mismatches.len(),
                    mismatches
                )
            }
            Self::ConcurrentScmMutation { detail } => {
                write!(f, "concurrent SCM mutation detected: {detail}")
            }
        }
    }
}

impl std::error::Error for ScmProvisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Query { source } => Some(source),
            _ => None,
        }
    }
}

/// Pure planning result comparing an existing state snapshot against the canonical contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmProvisionPlan {
    /// Service matches the canonical contract; zero mutations needed.
    AlreadyCanonical,
    /// Service is not installed; creation is needed.
    Create,
    /// Service exists but has repairable drift; configuration update is needed.
    Update {
        failure_actions_flag_changed: bool,
        requires_os_restart: bool,
    },
    /// Existing service identity does not match canonical name and cannot be repaired.
    UnrepairableIdentity,
}

/// Pure helper to plan provisioning actions without touching SCM.
pub fn plan_provisioning(
    query_result: Result<&ScmConfigSnapshot, &ScmQueryError>,
    canonical_binary_path: &str,
) -> Result<ScmProvisionPlan, ScmProvisionError> {
    match query_result {
        Ok(snapshot) => {
            if snapshot.service_name != PALKA_SERVICE_NAME {
                return Ok(ScmProvisionPlan::UnrepairableIdentity);
            }
            let mismatches = snapshot.validate_canonical(canonical_binary_path);
            if mismatches.is_empty() {
                Ok(ScmProvisionPlan::AlreadyCanonical)
            } else {
                let flag_changed = snapshot.failure_actions_on_non_crash_failures;
                Ok(ScmProvisionPlan::Update {
                    failure_actions_flag_changed: flag_changed,
                    requires_os_restart: flag_changed,
                })
            }
        }
        Err(ScmQueryError::ServiceNotInstalled { .. }) => Ok(ScmProvisionPlan::Create),
        Err(e) => Err(ScmProvisionError::Query {
            source: (*e).clone(),
        }),
    }
}

/// Validates that an executable path is canonical and returns its SCM representation.
///
/// Rules:
/// - Must not be empty.
/// - Must not contain NUL bytes.
/// - Must not be pre-quoted.
/// - Must not use script/shell wrappers (`cmd.exe`, `powershell.exe`, `.bat`, `.cmd`).
/// - Must be absolute (drive root or UNC).
/// - Must identify `palka-service.exe` (case-insensitive).
/// - If path contains spaces, surrounds it with exactly one pair of double quotes.
pub fn validate_and_render_canonical_binary_path(
    executable_path: &Path,
) -> Result<String, ScmProvisionError> {
    let path_os = executable_path.as_os_str();
    if path_os.is_empty() {
        return Err(ScmProvisionError::InvalidBinaryPath {
            detail: "executable path must not be empty".to_string(),
        });
    }

    let path_str = path_os
        .to_str()
        .ok_or_else(|| ScmProvisionError::InvalidBinaryPath {
            detail: "executable path must be valid Unicode".to_string(),
        })?;

    if path_str.contains('\0') {
        return Err(ScmProvisionError::InvalidBinaryPath {
            detail: "executable path must not contain NUL bytes".to_string(),
        });
    }

    if path_str.starts_with('"') || path_str.ends_with('"') || path_str.contains('"') {
        return Err(ScmProvisionError::InvalidBinaryPath {
            detail: "executable path must not be pre-quoted".to_string(),
        });
    }

    let lower = path_str.to_lowercase();
    if lower.contains("cmd.exe")
        || lower.contains("powershell.exe")
        || lower.ends_with(".bat")
        || lower.ends_with(".cmd")
    {
        return Err(ScmProvisionError::InvalidBinaryPath {
            detail: "shell or script wrappers are strictly prohibited".to_string(),
        });
    }

    let is_absolute = if cfg!(windows) {
        executable_path.is_absolute()
            || (path_str.len() >= 3
                && path_str.as_bytes()[1] == b':'
                && (path_str.as_bytes()[2] == b'\\' || path_str.as_bytes()[2] == b'/'))
            || path_str.starts_with(r"\\")
    } else {
        executable_path.is_absolute()
            || (path_str.len() >= 3
                && path_str.as_bytes()[1] == b':'
                && (path_str.as_bytes()[2] == b'\\' || path_str.as_bytes()[2] == b'/'))
            || path_str.starts_with(r"\\")
    };

    if !is_absolute {
        return Err(ScmProvisionError::InvalidBinaryPath {
            detail: "executable path must be absolute".to_string(),
        });
    }

    let file_name = executable_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ScmProvisionError::InvalidBinaryPath {
            detail: "executable path must have a valid file name".to_string(),
        })?;

    if !file_name.eq_ignore_ascii_case("palka-service.exe") {
        return Err(ScmProvisionError::InvalidBinaryPath {
            detail: format!("executable file name must be 'palka-service.exe', got '{file_name}'"),
        });
    }

    if path_str.contains(' ') {
        Ok(format!("\"{path_str}\""))
    } else {
        Ok(path_str.to_string())
    }
}

/// Provisions or converges the `PalkaService` Windows Service.
///
/// This administrative API ensures that `PalkaService` is installed and its
/// configuration strictly matches the PALKA service lifecycle contract.
///
/// It is idempotent: if the service is already canonical, zero SCM mutations are performed.
///
/// It does NOT start or stop the service.
pub fn provision_palka_service(
    executable_path: &Path,
) -> Result<ScmProvisionResult, ScmProvisionError> {
    #[cfg(windows)]
    {
        provision_palka_service_windows(executable_path)
    }
    #[cfg(not(windows))]
    {
        let _ = executable_path;
        Err(ScmProvisionError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
struct ScHandle(windows::Win32::System::Services::SC_HANDLE);

#[cfg(windows)]
impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is a valid open SC_HANDLE that has not been closed yet.
            unsafe {
                let _ = windows::Win32::System::Services::CloseServiceHandle(self.0);
            }
        }
    }
}

/// Classifies a Win32 SCM mutation error into a typed `ScmProvisionError`.
///
/// Specifically maps `ERROR_DUPLICATE_SERVICE_NAME` to `DisplayNameConflict`
/// while leaving all other errors as `WindowsApi`.
pub fn classify_scm_mutation_error(
    function: &'static str,
    code: u32,
    message: String,
    display_name: &str,
) -> ScmProvisionError {
    #[cfg(windows)]
    let is_dup = code == windows::Win32::Foundation::ERROR_DUPLICATE_SERVICE_NAME.0;
    #[cfg(not(windows))]
    let is_dup = code == 1078; // ERROR_DUPLICATE_SERVICE_NAME Win32 code

    if is_dup {
        ScmProvisionError::DisplayNameConflict {
            display_name: display_name.to_string(),
            code,
            message,
        }
    } else {
        ScmProvisionError::WindowsApi {
            function,
            code,
            message,
        }
    }
}

#[cfg(windows)]
fn win32_error_code(err: &windows::core::Error) -> u32 {
    windows::Win32::Foundation::WIN32_ERROR::from_error(err)
        .map(|code| code.0)
        .unwrap_or_else(|| err.code().0 as u32)
}

#[cfg(windows)]
fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
unsafe fn apply_optional_config(svc: &ScHandle) -> Result<(), ScmProvisionError> {
    use windows::Win32::System::Services::*;
    use windows::core::{BOOL, PWSTR};

    // 1. Description
    let mut desc_wide = to_wide_null(PALKA_SERVICE_DESCRIPTION);
    let desc_struct = SERVICE_DESCRIPTIONW {
        lpDescription: PWSTR(desc_wide.as_mut_ptr()),
    };
    // SAFETY: `desc_struct` and `desc_wide` remain valid for the duration of the Win32 call.
    unsafe {
        ChangeServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some(&desc_struct as *const _ as *const core::ffi::c_void),
        )
    }
    .map_err(|e| ScmProvisionError::WindowsApi {
        function: "ChangeServiceConfig2W(SERVICE_CONFIG_DESCRIPTION)",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    // 2. Delayed auto start (FALSE)
    let delayed_info = SERVICE_DELAYED_AUTO_START_INFO {
        fDelayedAutostart: BOOL::from(false),
    };
    // SAFETY: `delayed_info` is valid for the duration of the Win32 call.
    unsafe {
        ChangeServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
            Some(&delayed_info as *const _ as *const core::ffi::c_void),
        )
    }
    .map_err(|e| ScmProvisionError::WindowsApi {
        function: "ChangeServiceConfig2W(SERVICE_CONFIG_DELAYED_AUTO_START_INFO)",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    // 3. Failure actions (Restart 5s, 15s, 60s; reset 86400s; no reboot; no run command)
    let mut actions = [
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: PALKA_SERVICE_RESTART_DELAY_1_MS,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: PALKA_SERVICE_RESTART_DELAY_2_MS,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: PALKA_SERVICE_RESTART_DELAY_3_MS,
        },
    ];
    let fail_actions = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: PALKA_SERVICE_RESET_PERIOD_SEC,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: `fail_actions` and `actions` remain valid for the duration of the Win32 call.
    unsafe {
        ChangeServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&fail_actions as *const _ as *const core::ffi::c_void),
        )
    }
    .map_err(|e| ScmProvisionError::WindowsApi {
        function: "ChangeServiceConfig2W(SERVICE_CONFIG_FAILURE_ACTIONS)",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    // 4. Failure actions flag (FALSE)
    let flag_info = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: BOOL::from(false),
    };
    // SAFETY: `flag_info` is valid for the duration of the Win32 call.
    unsafe {
        ChangeServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            Some(&flag_info as *const _ as *const core::ffi::c_void),
        )
    }
    .map_err(|e| ScmProvisionError::WindowsApi {
        function: "ChangeServiceConfig2W(SERVICE_CONFIG_FAILURE_ACTIONS_FLAG)",
        code: win32_error_code(&e),
        message: e.message().to_string(),
    })?;

    Ok(())
}

#[cfg(windows)]
fn converge_existing_service(
    scm: &ScHandle,
    canonical_binary_path: &str,
    existing_snapshot: &ScmConfigSnapshot,
) -> Result<ScmProvisionResult, ScmProvisionError> {
    use windows::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
    use windows::Win32::System::Services::*;
    use windows::core::PCWSTR;

    if existing_snapshot.service_name != PALKA_SERVICE_NAME {
        return Err(ScmProvisionError::UnrepairableServiceIdentity {
            expected: PALKA_SERVICE_NAME.to_string(),
            actual: existing_snapshot.service_name.clone(),
        });
    }

    let service_name_wide = to_wide_null(PALKA_SERVICE_NAME);
    // SAFETY: Opening service with SERVICE_CHANGE_CONFIG access only.
    let svc_raw = unsafe {
        OpenServiceW(
            scm.0,
            PCWSTR(service_name_wide.as_ptr()),
            SERVICE_CHANGE_CONFIG,
        )
    }
    .map_err(|e| {
        let code = win32_error_code(&e);
        if code == ERROR_SERVICE_DOES_NOT_EXIST.0 {
            ScmProvisionError::ConcurrentScmMutation {
                detail: "service removed before update could be applied".to_string(),
            }
        } else {
            ScmProvisionError::WindowsApi {
                function: "OpenServiceW",
                code,
                message: e.message().to_string(),
            }
        }
    })?;
    let svc = ScHandle(svc_raw);

    // 1. Base configuration convergence via ChangeServiceConfigW
    let binary_path_wide = to_wide_null(canonical_binary_path);
    let empty_multi_sz: [u16; 2] = [0, 0];
    let account_wide = to_wide_null(PALKA_SERVICE_ACCOUNT);
    // NULL in ChangeServiceConfigW means preserve existing password.
    // An explicit empty UTF-16 string (`[0]`) is intentional to prevent retention of a legacy credential when switching to LocalSystem.
    let empty_password_wide: [u16; 1] = [0];
    let display_name_wide = to_wide_null(PALKA_SERVICE_DISPLAY_NAME);

    // SAFETY: `svc.0` is valid, string slices are null-terminated and live for call duration.
    // `empty_password_wide` remains alive throughout the call to explicitly clear any legacy password.
    unsafe {
        ChangeServiceConfigW(
            svc.0,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(binary_path_wide.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR(empty_multi_sz.as_ptr()),
            PCWSTR(account_wide.as_ptr()),
            PCWSTR(empty_password_wide.as_ptr()),
            PCWSTR(display_name_wide.as_ptr()),
        )
    }
    .map_err(|e| {
        let code = win32_error_code(&e);
        classify_scm_mutation_error(
            "ChangeServiceConfigW",
            code,
            e.message().to_string(),
            PALKA_SERVICE_DISPLAY_NAME,
        )
    })?;

    // 2. Optional configuration convergence via ChangeServiceConfig2W
    // SAFETY: Updates description, delayed autostart, recovery actions, and failure flag.
    unsafe { apply_optional_config(&svc)? };

    // Explicitly drop service handle before query-back
    drop(svc);

    let failure_actions_flag_changed = existing_snapshot.failure_actions_on_non_crash_failures;
    let requires_os_restart_before_recovery_behavior_verification = failure_actions_flag_changed;

    // 3. Mandatory Query-back verification
    let final_snapshot =
        query_palka_service_config().map_err(|source| ScmProvisionError::Query { source })?;
    let mismatches = final_snapshot.validate_canonical(canonical_binary_path);
    if !mismatches.is_empty() {
        return Err(ScmProvisionError::QueryBackMismatch { mismatches });
    }

    Ok(ScmProvisionResult {
        outcome: ScmProvisionOutcome::Updated,
        final_snapshot,
        failure_actions_flag_changed,
        requires_os_restart_before_recovery_behavior_verification,
    })
}

#[cfg(windows)]
fn create_service_and_converge(
    scm_create: &ScHandle,
    scm_connect: &ScHandle,
    canonical_binary_path: &str,
) -> Result<ScmProvisionResult, ScmProvisionError> {
    use windows::Win32::Foundation::ERROR_SERVICE_EXISTS;
    use windows::Win32::System::Services::*;
    use windows::core::PCWSTR;

    let service_name_wide = to_wide_null(PALKA_SERVICE_NAME);
    let display_name_wide = to_wide_null(PALKA_SERVICE_DISPLAY_NAME);
    let binary_path_wide = to_wide_null(canonical_binary_path);

    // SAFETY: Creating PalkaService with SERVICE_CHANGE_CONFIG access and canonical settings.
    let create_res = unsafe {
        CreateServiceW(
            scm_create.0,
            PCWSTR(service_name_wide.as_ptr()),
            PCWSTR(display_name_wide.as_ptr()),
            SERVICE_CHANGE_CONFIG,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(binary_path_wide.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    };

    let svc = match create_res {
        Ok(raw_handle) => ScHandle(raw_handle),
        Err(e) => {
            let code = win32_error_code(&e);
            if code == ERROR_SERVICE_EXISTS.0 {
                // Race: service was created concurrently.
                // Re-query existing canonical service and plan accordingly.
                let existing_snapshot = query_palka_service_config()
                    .map_err(|source| ScmProvisionError::Query { source })?;

                match plan_provisioning(Ok(&existing_snapshot), canonical_binary_path)? {
                    ScmProvisionPlan::AlreadyCanonical => {
                        // Already canonical: ZERO subsequent SCM writes!
                        return Ok(ScmProvisionResult {
                            outcome: ScmProvisionOutcome::AlreadyCanonical,
                            final_snapshot: existing_snapshot,
                            failure_actions_flag_changed: false,
                            requires_os_restart_before_recovery_behavior_verification: false,
                        });
                    }
                    ScmProvisionPlan::UnrepairableIdentity => {
                        return Err(ScmProvisionError::UnrepairableServiceIdentity {
                            expected: PALKA_SERVICE_NAME.to_string(),
                            actual: existing_snapshot.service_name,
                        });
                    }
                    ScmProvisionPlan::Update { .. } => {
                        return converge_existing_service(
                            scm_connect,
                            canonical_binary_path,
                            &existing_snapshot,
                        );
                    }
                    ScmProvisionPlan::Create => {
                        return Err(ScmProvisionError::ConcurrentScmMutation {
                            detail:
                                "service disappeared immediately after ERROR_SERVICE_EXISTS race"
                                    .to_string(),
                        });
                    }
                }
            } else {
                return Err(classify_scm_mutation_error(
                    "CreateServiceW",
                    code,
                    e.message().to_string(),
                    PALKA_SERVICE_DISPLAY_NAME,
                ));
            }
        }
    };

    // Configure optional canonical fields
    // SAFETY: Handle was created with SERVICE_CHANGE_CONFIG access.
    unsafe { apply_optional_config(&svc)? };

    // Explicitly drop service handle before query-back
    drop(svc);

    // Mandatory Query-back verification
    let final_snapshot =
        query_palka_service_config().map_err(|source| ScmProvisionError::Query { source })?;
    let mismatches = final_snapshot.validate_canonical(canonical_binary_path);
    if !mismatches.is_empty() {
        return Err(ScmProvisionError::QueryBackMismatch { mismatches });
    }

    Ok(ScmProvisionResult {
        outcome: ScmProvisionOutcome::Created,
        final_snapshot,
        failure_actions_flag_changed: false,
        requires_os_restart_before_recovery_behavior_verification: false,
    })
}

#[cfg(windows)]
fn provision_palka_service_windows(
    executable_path: &Path,
) -> Result<ScmProvisionResult, ScmProvisionError> {
    use windows::Win32::System::Services::{
        OpenSCManagerW, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    };
    use windows::core::PCWSTR;

    let canonical_binary_path = validate_and_render_canonical_binary_path(executable_path)?;

    // 1. Authoritative initial query
    match query_palka_service_config() {
        Ok(existing_snapshot) => {
            let mismatches = existing_snapshot.validate_canonical(&canonical_binary_path);
            if mismatches.is_empty() {
                // CASE 1: ALREADY CANONICAL - Zero SCM writes
                return Ok(ScmProvisionResult {
                    outcome: ScmProvisionOutcome::AlreadyCanonical,
                    final_snapshot: existing_snapshot,
                    failure_actions_flag_changed: false,
                    requires_os_restart_before_recovery_behavior_verification: false,
                });
            }

            // CASE 3: EXISTING SERVICE CONVERGENCE
            // SAFETY: Connect to SCM with SC_MANAGER_CONNECT only.
            let scm_raw =
                unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
                    .map_err(|e| ScmProvisionError::WindowsApi {
                        function: "OpenSCManagerW",
                        code: win32_error_code(&e),
                        message: e.message().to_string(),
                    })?;
            let scm = ScHandle(scm_raw);

            converge_existing_service(&scm, &canonical_binary_path, &existing_snapshot)
        }
        Err(ScmQueryError::ServiceNotInstalled { .. }) => {
            // CASE 2: SERVICE ABSENT
            // SAFETY: Connect to SCM with minimal create rights.
            let scm_create_raw = unsafe {
                OpenSCManagerW(
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
                )
            }
            .map_err(|e| ScmProvisionError::WindowsApi {
                function: "OpenSCManagerW",
                code: win32_error_code(&e),
                message: e.message().to_string(),
            })?;
            let scm_create = ScHandle(scm_create_raw);

            create_service_and_converge(&scm_create, &scm_create, &canonical_binary_path)
        }
        Err(e) => Err(ScmProvisionError::Query { source: e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scm::{PALKA_SERVICE_ERROR_CONTROL, PALKA_SERVICE_START_TYPE, PALKA_SERVICE_TYPE};
    use std::path::PathBuf;

    const CANONICAL_BIN: &str = r"C:\PALKA\palka-service.exe";
    const CANONICAL_SPACED_BIN: &str = r"C:\Program Files\PALKA\palka-service.exe";

    fn make_canonical_snapshot(binary_path: &str) -> ScmConfigSnapshot {
        ScmConfigSnapshot {
            service_name: PALKA_SERVICE_NAME.to_string(),
            display_name: PALKA_SERVICE_DISPLAY_NAME.to_string(),
            description: PALKA_SERVICE_DESCRIPTION.to_string(),
            service_type: PALKA_SERVICE_TYPE,
            account: PALKA_SERVICE_ACCOUNT.to_string(),
            start_type: PALKA_SERVICE_START_TYPE,
            delayed_auto_start: false,
            error_control: PALKA_SERVICE_ERROR_CONTROL,
            dependencies: vec![],
            binary_path: binary_path.to_string(),
            failure_reset_period_sec: PALKA_SERVICE_RESET_PERIOD_SEC,
            recovery_actions: vec![
                crate::scm::ScmRecoveryAction {
                    action_type: crate::scm::ScmRecoveryActionType::Restart,
                    delay_ms: PALKA_SERVICE_RESTART_DELAY_1_MS,
                },
                crate::scm::ScmRecoveryAction {
                    action_type: crate::scm::ScmRecoveryActionType::Restart,
                    delay_ms: PALKA_SERVICE_RESTART_DELAY_2_MS,
                },
                crate::scm::ScmRecoveryAction {
                    action_type: crate::scm::ScmRecoveryActionType::Restart,
                    delay_ms: PALKA_SERVICE_RESTART_DELAY_3_MS,
                },
            ],
            failure_actions_on_non_crash_failures: false,
        }
    }

    #[test]
    fn test_01_valid_absolute_path_without_spaces() {
        let path = PathBuf::from(CANONICAL_BIN);
        let rendered = validate_and_render_canonical_binary_path(&path).unwrap();
        assert_eq!(rendered, CANONICAL_BIN);
    }

    #[test]
    fn test_02_valid_absolute_path_with_spaces_exact_quoting() {
        let path = PathBuf::from(CANONICAL_SPACED_BIN);
        let rendered = validate_and_render_canonical_binary_path(&path).unwrap();
        assert_eq!(rendered, format!("\"{CANONICAL_SPACED_BIN}\""));
    }

    #[test]
    fn test_03_unicode_path() {
        let path = PathBuf::from(r"C:\ПапкаРодительскогоКонтроля\palka-service.exe");
        let rendered = validate_and_render_canonical_binary_path(&path).unwrap();
        assert_eq!(rendered, r"C:\ПапкаРодительскогоКонтроля\palka-service.exe");
    }

    #[test]
    fn test_04_empty_path_rejected() {
        let path = PathBuf::from("");
        let err = validate_and_render_canonical_binary_path(&path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));
    }

    #[test]
    fn test_05_relative_path_rejected() {
        let path = PathBuf::from(r"bin\palka-service.exe");
        let err = validate_and_render_canonical_binary_path(&path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));
    }

    #[test]
    fn test_06_embedded_nul_rejected() {
        let path = PathBuf::from("C:\\Palka\\palka-service\0.exe");
        let err = validate_and_render_canonical_binary_path(&path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));
    }

    #[test]
    fn test_07_wrong_executable_filename_rejected() {
        let path = PathBuf::from(r"C:\PALKA\other-service.exe");
        let err = validate_and_render_canonical_binary_path(&path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));
    }

    #[test]
    fn test_08_pre_quoted_input_rejected() {
        let path = PathBuf::from(format!("\"{CANONICAL_SPACED_BIN}\""));
        let err = validate_and_render_canonical_binary_path(&path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));
    }

    #[test]
    fn test_09_command_shell_style_input_rejected() {
        let cmd_path = PathBuf::from(r"C:\Windows\System32\cmd.exe /c palka-service.exe");
        let err = validate_and_render_canonical_binary_path(&cmd_path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));

        let ps_path = PathBuf::from(r"C:\Windows\System32\powershell.exe -File script.ps1");
        let err = validate_and_render_canonical_binary_path(&ps_path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));

        let bat_path = PathBuf::from(r"C:\PALKA\run.bat");
        let err = validate_and_render_canonical_binary_path(&bat_path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));

        let cmd_file_path = PathBuf::from(r"C:\PALKA\run.cmd");
        let err = validate_and_render_canonical_binary_path(&cmd_file_path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));
    }

    #[test]
    fn test_10_arguments_after_exe_rejected() {
        let path = PathBuf::from(r"C:\PALKA\palka-service.exe --daemon");
        let err = validate_and_render_canonical_binary_path(&path).unwrap_err();
        assert!(matches!(err, ScmProvisionError::InvalidBinaryPath { .. }));
    }

    #[test]
    fn test_11_canonical_snapshot_plans_already_canonical_zero_write() {
        let snapshot = make_canonical_snapshot(CANONICAL_BIN);
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(plan, ScmProvisionPlan::AlreadyCanonical);
    }

    #[test]
    fn test_12_display_name_drift_is_repairable() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.display_name = "Wrong Display Name".to_string();
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: false,
                requires_os_restart: false,
            }
        );
    }

    #[test]
    fn test_13_description_drift_is_repairable() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.description = "Outdated description".to_string();
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: false,
                requires_os_restart: false,
            }
        );
    }

    #[test]
    fn test_14_binary_path_drift_is_repairable() {
        let snapshot = make_canonical_snapshot(r"C:\OldPalka\palka-service.exe");
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: false,
                requires_os_restart: false,
            }
        );
    }

    #[test]
    fn test_15_dependencies_drift_is_repairable() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.dependencies = vec!["Tcpip".to_string(), "Dnscache".to_string()];
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: false,
                requires_os_restart: false,
            }
        );
    }

    #[test]
    fn test_16_account_drift_is_repairable() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.account = r"NT AUTHORITY\NetworkService".to_string();
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: false,
                requires_os_restart: false,
            }
        );
    }

    #[test]
    fn test_17_recovery_drift_is_repairable() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.failure_reset_period_sec = 3600;
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: false,
                requires_os_restart: false,
            }
        );
    }

    #[test]
    fn test_18_failure_flag_true_marks_restart_before_behavior_verification() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.failure_actions_on_non_crash_failures = true;
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: true,
                requires_os_restart: true,
            }
        );
    }

    #[test]
    fn test_19_exact_service_name_identity_drift_is_unrepairable() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.service_name = "WrongService".to_string();
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(plan, ScmProvisionPlan::UnrepairableIdentity);
    }

    #[test]
    fn test_20_multiple_repairable_mismatches_remain_convergence_capable() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.display_name = "Wrong Display".to_string();
        snapshot.description = "Wrong Description".to_string();
        snapshot.delayed_auto_start = true;
        snapshot.account = "LocalService".to_string();
        snapshot.dependencies = vec!["RpcSs".to_string()];
        snapshot.failure_actions_on_non_crash_failures = true;
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: true,
                requires_os_restart: true,
            }
        );
    }

    #[test]
    fn test_21_service_absent_plans_create() {
        let err = ScmQueryError::ServiceNotInstalled {
            service_name: PALKA_SERVICE_NAME.to_string(),
        };
        let plan = plan_provisioning(Err(&err), CANONICAL_BIN).unwrap();
        assert_eq!(plan, ScmProvisionPlan::Create);
    }

    #[test]
    fn test_22_non_windows_unsupported_platform_check() {
        #[cfg(not(windows))]
        {
            let path = PathBuf::from(CANONICAL_BIN);
            let res = provision_palka_service(&path);
            assert!(matches!(res, Err(ScmProvisionError::UnsupportedPlatform)));
        }
    }

    #[test]
    fn test_23_error_service_exists_race_canonical_plans_already_canonical() {
        let snapshot = make_canonical_snapshot(CANONICAL_BIN);
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(plan, ScmProvisionPlan::AlreadyCanonical);
    }

    #[test]
    fn test_24_error_service_exists_race_repairable_plans_update() {
        let mut snapshot = make_canonical_snapshot(CANONICAL_BIN);
        snapshot.account = "NT AUTHORITY\\NetworkService".to_string();
        let plan = plan_provisioning(Ok(&snapshot), CANONICAL_BIN).unwrap();
        assert_eq!(
            plan,
            ScmProvisionPlan::Update {
                failure_actions_flag_changed: false,
                requires_os_restart: false,
            }
        );
    }

    #[test]
    fn test_25_classify_scm_mutation_error_duplicate_name() {
        let err = classify_scm_mutation_error(
            "ChangeServiceConfigW",
            1078, // ERROR_DUPLICATE_SERVICE_NAME
            "The display name is already in use".to_string(),
            PALKA_SERVICE_DISPLAY_NAME,
        );
        match err {
            ScmProvisionError::DisplayNameConflict {
                display_name,
                code,
                message,
            } => {
                assert_eq!(display_name, PALKA_SERVICE_DISPLAY_NAME);
                assert_eq!(code, 1078);
                assert!(message.contains("already in use"));
            }
            other => panic!("expected DisplayNameConflict, got {other:?}"),
        }
    }

    #[test]
    fn test_26_classify_scm_mutation_error_other_code() {
        let err = classify_scm_mutation_error(
            "ChangeServiceConfigW",
            5, // ERROR_ACCESS_DENIED
            "Access is denied".to_string(),
            PALKA_SERVICE_DISPLAY_NAME,
        );
        match err {
            ScmProvisionError::WindowsApi {
                function,
                code,
                message,
            } => {
                assert_eq!(function, "ChangeServiceConfigW");
                assert_eq!(code, 5);
                assert!(message.contains("Access is denied"));
            }
            other => panic!("expected WindowsApi, got {other:?}"),
        }
    }
}
