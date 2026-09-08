//! Windows Named Pipe low-level transport implementation for PALKA IPC V1.
//!
//! Enforces normative requirements of docs/021:
//! - Canonical pipe configuration (`\\.\pipe\palka_ipc_v1`, `PIPE_TYPE_BYTE`, `PIPE_READMODE_BYTE`, `MAX_PIPE_INSTANCES = 4`, `PIPE_REJECT_REMOTE_CLIENTS`)
//! - Strict Child SID validation (`ConvertStringSidToSidW` -> `ConvertSidToStringSidW`)
//! - Protected kernel DACL with exact child mask `0x00100083` (`(mask & 0x4) == 0`)
//! - Overlapped asynchronous I/O with completion events and `CancelIoEx`
//! - Exact 4-byte Little-Endian initial request prefix read (`u32::from_le_bytes`)
//! - Strict pre-authorization guard: `1..=65536`, no body read, no body allocation
//! - Prebuffer byte preservation (`[u8; 4]`)
//! - Client security context extraction (`TokenUser`, `TokenGroups`, PID, Session ID)
//! - `RevertToSelf` failure policy is `PROCESS_FATAL` (`abort()`)

use std::fmt;
use std::io;

/// Canonical named pipe name for PALKA IPC V1.
pub const CANONICAL_PIPE_NAME: &str = r"\\.\pipe\palka_ipc_v1";

/// Maximum concurrent pipe instances supported by the canonical server.
pub const MAX_PIPE_INSTANCES: u32 = 4;

/// Canonical child ACE access mask granted in the server DACL:
/// `FILE_READ_DATA (0x1) | FILE_WRITE_DATA (0x2) | FILE_READ_ATTRIBUTES (0x80) | SYNCHRONIZE (0x100000)`.
/// Excludes `FILE_CREATE_PIPE_INSTANCE / FILE_APPEND_DATA (0x4)`, `WRITE_DAC`, `WRITE_OWNER`, `DELETE`, `GENERIC_WRITE`.
pub const CHILD_PIPE_DACL_ALLOWED_ACCESS_MASK: u32 = 0x00100083;

/// Minimal data-transfer access mask required by the child client CreateFileW:
/// `FILE_READ_DATA (0x1) | FILE_WRITE_DATA (0x2)`.
pub const CHILD_PIPE_CLIENT_DESIRED_ACCESS_MASK: u32 = 0x00000003;

/// Number of bytes in the fixed initial frame length prefix.
pub const INITIAL_FRAME_PREFIX_BYTES: usize = 4;

/// Minimum allowed declared length for the first client request frame.
pub const INITIAL_REQUEST_LENGTH_MIN: u32 = 1;

/// Maximum allowed declared length for the first client request frame (64 KiB).
pub const INITIAL_REQUEST_LENGTH_MAX: u32 = 65536;

/// Canonical maximum initial client request bytes limit.
pub const MAX_INITIAL_CLIENT_REQUEST_BYTES: u32 = 65536;

/// RevertToSelf failure policy identity.
pub const REVERT_TO_SELF_FAILURE_POLICY: &str = "PROCESS_FATAL";

/// Token group attribute flag: group is enabled for access checks.
pub const SE_GROUP_ENABLED: u32 = 0x00000004;

/// Token group attribute flag: group was enabled by default.
pub const SE_GROUP_ENABLED_BY_DEFAULT: u32 = 0x00000002;

/// Token group attribute flag: group is used exclusively for deny ACEs.
/// It cannot be used to satisfy allow checks (e.g. in non-elevated split tokens).
pub const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x00000010;

/// Returns whether RevertToSelf failure is unconditionally process-fatal under the canonical contract policy.
#[inline]
pub const fn revert_to_self_failure_is_fatal() -> bool {
    true
}

/// Decodes and validates the initial 4-byte frame length prefix according to normative rules:
/// - Little-Endian 32-bit unsigned integer (`u32::from_le_bytes`)
/// - N == 0 => `NamedPipeError::ZeroLengthInitialRequest`
/// - N > 65536 => `NamedPipeError::FrameTooLarge { length: N, max: 65536 }`
/// - 1 <= N <= 65536 => Ok(N)
#[inline]
pub fn validate_initial_request_prefix(
    prefix: &[u8; INITIAL_FRAME_PREFIX_BYTES],
) -> Result<u32, NamedPipeError> {
    let n = u32::from_le_bytes(*prefix);
    if n == 0 {
        Err(NamedPipeError::ZeroLengthInitialRequest)
    } else if n > INITIAL_REQUEST_LENGTH_MAX {
        Err(NamedPipeError::FrameTooLarge {
            length: n,
            max: INITIAL_REQUEST_LENGTH_MAX,
        })
    } else {
        Ok(n)
    }
}

/// Determines whether a token group entry for BUILTIN\Administrators represents an active
/// local administrator.
///
/// Under the Windows access token model and docs/021 contract:
/// - Active membership requires `SE_GROUP_ENABLED (0x4)`.
/// - `SE_GROUP_ENABLED_BY_DEFAULT (0x2)` by itself is NOT sufficient.
/// - A group marked with `SE_GROUP_USE_FOR_DENY_ONLY (0x10)` (such as in a non-elevated split token)
///   is explicitly NOT an active local administrator, because it cannot satisfy allow ACEs.
#[inline]
pub fn is_active_administrator_group(attrs: u32) -> bool {
    let is_enabled = (attrs & SE_GROUP_ENABLED) != 0;
    let is_deny_only = (attrs & SE_GROUP_USE_FOR_DENY_ONLY) != 0;
    is_enabled && !is_deny_only
}

/// Validated Windows Security Identifier (SID).
///
/// Guaranteed to be validated and canonicalized through Win32 API (`ConvertStringSidToSidW` $\to$ `ConvertSidToStringSidW`).
/// Can be safely interpolated into SDDL templates without injection hazard.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValidatedSid(pub(crate) String);

impl ValidatedSid {
    /// Returns the canonical SID string.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrows the inner string representation.
    #[inline]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for ValidatedSid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Type alias for the configured child SID.
pub type ConfiguredChildSid = ValidatedSid;

/// Raw OS facts extracted from the authenticated client connection.
///
/// Contains strictly low-level operating system facts.
/// Authorization decisions, role mappings, and PIN policies belong strictly to `palka-service` (Slice 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSecurityContext {
    /// String representation of the connecting client user SID (`TokenUser`).
    pub user_sid: String,
    /// Whether the client token contains the Builtin Administrators group in an enabled state.
    pub is_local_administrator: bool,
    /// Client process identifier from `GetNamedPipeClientProcessId`.
    pub client_process_id: u32,
    /// Client session identifier from `GetNamedPipeClientSessionId`.
    pub client_session_id: u32,
}

/// Errors originating from the Windows Named Pipe transport layer.
#[derive(Debug)]
pub enum NamedPipeError {
    /// The specified Child SID is invalid, empty, or failed Win32 validation.
    InvalidChildSid(String),
    /// A Win32 system call failed.
    WindowsApi {
        function: &'static str,
        code: u32,
        message: String,
    },
    /// Initial client request length prefix declared 0 bytes.
    ZeroLengthInitialRequest,
    /// Initial client request length prefix exceeded the 64 KiB limit before body read or allocation.
    FrameTooLarge { length: u32, max: u32 },
    /// Client disconnected or EOF reached before accumulating the full 4-byte length prefix.
    PartialPrefixDisconnect {
        read_bytes: usize,
        target_bytes: usize,
    },
    /// The named pipe connection was closed or broken.
    Disconnected,
    /// The pending I/O operation was cancelled (`ERROR_OPERATION_ABORTED`).
    OperationCancelled,
    /// Maximum named pipe instances reached (`ERROR_PIPE_BUSY`).
    CapacityExceeded,
    /// Operation timed out.
    Timeout,
    /// Underlying standard I/O error.
    Io(io::Error),
    /// RevertToSelf failed with error code.
    RevertToSelfFailed(u32),
    /// Security violation or internally inconsistent token structure.
    SecurityViolation(String),
    /// Platform is not supported.
    UnsupportedPlatform,
}

impl fmt::Display for NamedPipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChildSid(msg) => write!(f, "Invalid child SID: {msg}"),
            Self::WindowsApi {
                function,
                code,
                message,
            } => {
                write!(
                    f,
                    "Windows API call '{function}' failed with error code {code} (0x{code:08X}): {message}"
                )
            }
            Self::ZeroLengthInitialRequest => {
                write!(f, "Initial client request declared 0-byte payload length")
            }
            Self::FrameTooLarge { length, max } => {
                write!(
                    f,
                    "Initial client request declared length {length} exceeds maximum {max}"
                )
            }
            Self::PartialPrefixDisconnect {
                read_bytes,
                target_bytes,
            } => {
                write!(
                    f,
                    "Connection closed after receiving {read_bytes} of {target_bytes} initial prefix bytes"
                )
            }
            Self::Disconnected => write!(f, "Named pipe connection was closed or broken"),
            Self::OperationCancelled => write!(f, "Named pipe operation was cancelled"),
            Self::CapacityExceeded => {
                write!(f, "Maximum named pipe server instance capacity reached")
            }
            Self::Timeout => write!(f, "Named pipe operation timed out"),
            Self::Io(err) => write!(f, "Named pipe I/O error: {err}"),
            Self::RevertToSelfFailed(code) => {
                write!(f, "RevertToSelf failed with error code {code}")
            }
            Self::SecurityViolation(msg) => write!(f, "Security violation: {msg}"),
            Self::UnsupportedPlatform => {
                write!(f, "Named pipe transport is only supported on Windows")
            }
        }
    }
}

impl std::error::Error for NamedPipeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for NamedPipeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(windows)]
pub use crate::named_pipe_windows::{
    NamedPipeConnection, NamedPipeServer, NamedPipeServerInstance, raw_win32_code_from_error,
    raw_win32_code_from_hresult, raw_win32_error_string,
};

#[cfg(not(windows))]
impl ValidatedSid {
    pub fn parse(s: &str) -> Result<Self, NamedPipeError> {
        if s.is_empty() {
            return Err(NamedPipeError::InvalidChildSid(
                "SID string is empty".to_string(),
            ));
        }
        Err(NamedPipeError::UnsupportedPlatform)
    }
}

#[cfg(not(windows))]
pub struct NamedPipeConnection;

#[cfg(not(windows))]
pub struct NamedPipeServer;

#[cfg(not(windows))]
pub struct NamedPipeServerInstance;
