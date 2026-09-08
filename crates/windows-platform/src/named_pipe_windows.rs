//! Windows native implementation of the Named Pipe transport using Win32 APIs.
//!
//! Enforces docs/021 normative requirements:
//! - Canonical pipe configuration (`\\.\pipe\palka_ipc_v1`, `PIPE_TYPE_BYTE`, `PIPE_READMODE_BYTE`, `MAX_PIPE_INSTANCES = 4`, `PIPE_REJECT_REMOTE_CLIENTS`)
//! - Strict Child SID validation (`ConvertStringSidToSidW` -> `ConvertSidToStringSidW`)
//! - Protected kernel DACL with exact child mask `0x00100083` (`(mask & 0x4) == 0`)
//! - Overlapped asynchronous I/O with completion events and `CancelIoEx`
//! - Exact 4-byte Little-Endian initial request prefix read (`u32::from_le_bytes`)
//! - Strict pre-authorization guard: `1..=65536`, no body read, no body allocation
//! - Prebuffer byte preservation (`[u8; 4]`)
//! - Client security context extraction (`TokenUser`, `TokenGroups`, PID, Session ID)
//! - `RevertToSelf` failure policy is `PROCESS_FATAL` (`abort()`)

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_NOT_FOUND,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, HLOCAL,
    LocalFree, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, RevertToSelf, TOKEN_GROUPS, TOKEN_QUERY,
    TOKEN_USER, TokenGroups, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeClientSessionId, ImpersonateNamedPipeClient, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, INFINITE, OpenThreadToken, WaitForMultipleObjects,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use crate::named_pipe::{
    CANONICAL_PIPE_NAME, CHILD_PIPE_DACL_ALLOWED_ACCESS_MASK, ClientSecurityContext,
    INITIAL_FRAME_PREFIX_BYTES, MAX_PIPE_INSTANCES, NamedPipeError, ValidatedSid,
    is_active_administrator_group, validate_initial_request_prefix,
};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
static SUPPRESS_ABORT_FOR_TESTING: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn set_suppress_abort_for_testing(suppress: bool) {
    SUPPRESS_ABORT_FOR_TESTING.store(suppress, Ordering::SeqCst);
}

/// Invoked when RevertToSelf fails after impersonation.
///
/// In production builds (`not(test)`), this is unconditionally process-fatal (`std::process::abort()`).
/// There is no runtime switch or suppression mechanism compiled into production artifacts.
#[inline(always)]
fn on_revert_to_self_failure(code: u32) -> ! {
    #[cfg(test)]
    {
        if SUPPRESS_ABORT_FOR_TESTING.load(Ordering::SeqCst) {
            panic!("PROCESS_FATAL: RevertToSelf failed with code {code}");
        }
    }
    let _ = code;
    std::process::abort()
}

/// Decodes a raw Win32 error code from a `windows::core::HRESULT`.
pub fn raw_win32_code_from_hresult(hr: u32) -> u32 {
    let unsigned = hr as u32;
    if (unsigned & 0xFFFF_0000) == 0x8007_0000 {
        unsigned & 0x0000_FFFF
    } else {
        unsigned
    }
}

/// Decodes a raw Win32 error code from a `windows::core::Error`.
pub fn raw_win32_code_from_error(err: &windows::core::Error) -> u32 {
    raw_win32_code_from_hresult(err.code().0 as u32)
}

/// Dynamically allocated backing buffer guaranteed to meet explicit native alignment requirements.
#[derive(Debug)]
pub struct AlignedNativeBuffer {
    ptr: *mut u8,
    layout: Layout,
}

unsafe impl Send for AlignedNativeBuffer {}
unsafe impl Sync for AlignedNativeBuffer {}

impl AlignedNativeBuffer {
    /// Allocates zeroed memory with at least the specified size and alignment.
    /// Fails closed if size is zero or if layout cannot be constructed.
    pub fn new(size: usize, min_align: usize) -> Result<Self, NamedPipeError> {
        if size == 0 {
            return Err(NamedPipeError::SecurityViolation(
                "Cannot allocate zero-sized native buffer for token query".to_string(),
            ));
        }
        let align = min_align.max(1);
        let layout = Layout::from_size_align(size, align).map_err(|_| {
            NamedPipeError::SecurityViolation(format!(
                "Invalid layout parameters for native buffer: size={size}, align={align}"
            ))
        })?;
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(NamedPipeError::WindowsApi {
                function: "alloc_zeroed",
                code: 14, // ERROR_OUTOFMEMORY
                message: "Out of memory allocating aligned native buffer".to_string(),
            });
        }
        Ok(Self { ptr, layout })
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    pub fn size(&self) -> usize {
        self.layout.size()
    }

    #[allow(dead_code)]
    pub fn align(&self) -> usize {
        self.layout.align()
    }

    pub fn is_aligned_for<T>(&self) -> bool {
        (self.ptr as usize) % std::mem::align_of::<T>() == 0
    }
}

impl Drop for AlignedNativeBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                dealloc(self.ptr, self.layout);
            }
        }
    }
}

/// Helper converting a string slice to wide null-terminated Vec<u16>.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Formats a raw Win32 error code into a diagnostic string.
pub fn raw_win32_error_string(code: u32) -> String {
    format!("Win32 error {code} (0x{code:08X})")
}

impl ValidatedSid {
    /// Validates a raw SID string through Win32 `ConvertStringSidToSidW` and canonicalizes it
    /// through `ConvertSidToStringSidW`.
    ///
    /// Fails closed on empty, malformed, or injected strings.
    pub fn parse(s: &str) -> Result<Self, NamedPipeError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(NamedPipeError::InvalidChildSid(
                "SID string is empty".to_string(),
            ));
        }

        let wide_sid = to_wide(trimmed);
        let mut psid = PSID::default();

        unsafe {
            if let Err(e) = ConvertStringSidToSidW(PCWSTR(wide_sid.as_ptr()), &mut psid) {
                let code = raw_win32_code_from_error(&e);
                return Err(NamedPipeError::InvalidChildSid(format!(
                    "ConvertStringSidToSidW failed for '{trimmed}' with code {code}"
                )));
            }
        }

        // RAII guard for the parsed PSID
        struct AutoPsid(PSID);
        impl Drop for AutoPsid {
            fn drop(&mut self) {
                if !self.0.0.is_null() {
                    unsafe {
                        let _ = LocalFree(Some(HLOCAL(self.0.0)));
                    }
                }
            }
        }
        let _psid_guard = AutoPsid(psid);

        let mut p_canonical = PWSTR::null();
        unsafe {
            if let Err(e) = ConvertSidToStringSidW(psid, &mut p_canonical) {
                let code = raw_win32_code_from_error(&e);
                return Err(NamedPipeError::InvalidChildSid(format!(
                    "ConvertSidToStringSidW failed with code {code}"
                )));
            }
        }

        struct AutoPwsz(PWSTR);
        impl Drop for AutoPwsz {
            fn drop(&mut self) {
                if !self.0.0.is_null() {
                    unsafe {
                        let _ = LocalFree(Some(HLOCAL(self.0.0 as *mut c_void)));
                    }
                }
            }
        }
        let _pwsz_guard = AutoPwsz(p_canonical);

        let canonical_str = unsafe {
            let mut len = 0;
            while *p_canonical.0.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p_canonical.0, len);
            String::from_utf16_lossy(slice)
        };

        Ok(ValidatedSid(canonical_str))
    }
}

/// RAII wrapper for an allocated security descriptor.
pub struct AutoSecurityDescriptor(pub PSECURITY_DESCRIPTOR);

impl Drop for AutoSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
}

/// RAII wrapper for an owned Win32 HANDLE.
#[derive(Debug)]
pub struct ScopedHandle(pub HANDLE);

unsafe impl Send for ScopedHandle {}
unsafe impl Sync for ScopedHandle {}

#[allow(dead_code)]
impl ScopedHandle {
    pub fn new(handle: HANDLE) -> Option<Self> {
        if handle.is_invalid() || handle.0.is_null() {
            None
        } else {
            Some(Self(handle))
        }
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for ScopedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// RAII wrapper for a connected server pipe instance handle.
///
/// Ensures `DisconnectNamedPipe` is called before closing the handle.
#[derive(Debug)]
pub struct SafePipeHandle(pub HANDLE);

unsafe impl Send for SafePipeHandle {}
unsafe impl Sync for SafePipeHandle {}

impl Drop for SafePipeHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = DisconnectNamedPipe(self.0);
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// RAII guard for `ImpersonateNamedPipeClient`.
///
/// Enforces `REVERT_TO_SELF_FAILURE_POLICY = PROCESS_FATAL`.
/// If `RevertToSelf()` fails on any path, the process terminates immediately (`std::process::abort()`).
pub struct ImpersonationGuard {
    active: bool,
}

impl ImpersonationGuard {
    pub fn new() -> Self {
        Self { active: true }
    }

    /// Explicitly reverts impersonation on the normal non-unwinding execution path.
    pub fn revert(&mut self) -> Result<(), NamedPipeError> {
        if self.active {
            let ret = unsafe { RevertToSelf() };
            if let Err(e) = ret {
                let err_code = raw_win32_code_from_error(&e);
                on_revert_to_self_failure(err_code);
            }
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for ImpersonationGuard {
    fn drop(&mut self) {
        if self.active {
            let ret = unsafe { RevertToSelf() };
            if let Err(e) = ret {
                let err_code = raw_win32_code_from_error(&e);
                on_revert_to_self_failure(err_code);
            }
        }
    }
}

/// Builds the canonical protected Security Descriptor for the PALKA Named Pipe.
///
/// SDDL template:
/// `D:P(D;;GA;;;AN)(D;;GA;;;NU)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x100083;;;{canonical_child_sid})`
pub fn create_pipe_security_descriptor(
    child_sid: &ValidatedSid,
) -> Result<AutoSecurityDescriptor, NamedPipeError> {
    let sddl = format!(
        "D:P(D;;GA;;;AN)(D;;GA;;;NU)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x{:x};;;{})",
        CHILD_PIPE_DACL_ALLOWED_ACCESS_MASK,
        child_sid.as_str()
    );

    let sddl_w = to_wide(&sddl);
    let mut p_sd = PSECURITY_DESCRIPTOR::default();
    let mut sd_size = 0u32;

    unsafe {
        if let Err(e) = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut p_sd,
            Some(&mut sd_size),
        ) {
            let code = raw_win32_code_from_error(&e);
            return Err(NamedPipeError::WindowsApi {
                function: "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    Ok(AutoSecurityDescriptor(p_sd))
}

/// Inspects an already-completed OVERLAPPED I/O operation (or an immediately successful one)
/// via `GetOverlappedResult(..., false)` to retrieve the authoritative transferred byte count.
///
/// Returns:
/// - `Ok(transferred)`: Authoritative bytes transferred.
/// - `Err(NamedPipeError::OperationCancelled)`: If operation aborted.
/// - `Err(NamedPipeError::Disconnected)`: If pipe broken or no data.
/// - `Err(NamedPipeError::WindowsApi)`: Any other Win32 error.
fn completed_overlapped_transfer(pipe: HANDLE, ov: &OVERLAPPED) -> Result<u32, NamedPipeError> {
    let mut transferred = 0u32;
    let gor = unsafe { GetOverlappedResult(pipe, ov, &mut transferred, false) };
    if let Err(_e) = gor {
        let code = unsafe { GetLastError().0 };
        if code == ERROR_OPERATION_ABORTED.0 {
            return Err(NamedPipeError::OperationCancelled);
        }
        if code == ERROR_BROKEN_PIPE.0 || code == ERROR_NO_DATA.0 {
            return Err(NamedPipeError::Disconnected);
        }
        return Err(NamedPipeError::WindowsApi {
            function: "GetOverlappedResult",
            code,
            message: raw_win32_error_string(code),
        });
    }
    Ok(transferred)
}

/// Waits for an overlapped I/O completion event or a mandatory cancellation event.
///
/// Distinguishes:
/// - `WAIT_OBJECT_0`: I/O event signaled; retrieves transfer outcome via `GetOverlappedResult`.
/// - `WAIT_OBJECT_0 + 1`: Cancel event signaled; cancels operation via `CancelIoEx`, waits for abort, returns `OperationCancelled`.
/// - `WAIT_TIMEOUT`: Returns `NamedPipeError::Timeout`.
/// - `WAIT_FAILED`: Queries Win32 `GetLastError()` and returns typed `WindowsApi` error.
/// - Other unexpected wait code: Returns typed `WindowsApi` error.
fn wait_overlapped_or_cancel(
    pipe: HANDLE,
    io_event: HANDLE,
    cancel_event: HANDLE,
    ov: &OVERLAPPED,
    timeout_ms: Option<u32>,
) -> Result<u32, NamedPipeError> {
    let handles = [io_event, cancel_event];
    let wait_ms = timeout_ms.unwrap_or(INFINITE);
    let wait = unsafe { WaitForMultipleObjects(&handles, false, wait_ms) };

    if wait == WAIT_OBJECT_0 {
        completed_overlapped_transfer(pipe, ov)
    } else if wait.0 == WAIT_OBJECT_0.0 + 1 {
        // Cancel event was signaled
        let cancel_res = unsafe { CancelIoEx(pipe, Some(ov)) };
        let cancel_err = if cancel_res.is_err() {
            unsafe { GetLastError() }
        } else {
            WIN32_ERROR(0)
        };

        // Ensure the OVERLAPPED operation has actually reached terminal completion
        // before returning and before its stack/buffer storage can disappear.
        let mut transferred = 0u32;
        let term_res = unsafe { GetOverlappedResult(pipe, ov, &mut transferred, true) };

        if term_res.is_ok() {
            // Outcome B: Operation completed normally despite cancellation race
            return Ok(transferred);
        }

        let term_err = unsafe { GetLastError() };
        if term_err == ERROR_OPERATION_ABORTED {
            // Outcome A: Terminal outcome is confirmed cancelled
            return Err(NamedPipeError::OperationCancelled);
        }

        if term_err == ERROR_BROKEN_PIPE || term_err == ERROR_NO_DATA {
            return Err(NamedPipeError::Disconnected);
        }

        // If CancelIoEx failed with something other than ERROR_NOT_FOUND, report CancelIoEx
        if cancel_res.is_err() && cancel_err != ERROR_NOT_FOUND {
            return Err(NamedPipeError::WindowsApi {
                function: "CancelIoEx",
                code: cancel_err.0,
                message: raw_win32_error_string(cancel_err.0),
            });
        }

        // Outcome C: GetOverlappedResult produced another Win32 error
        Err(NamedPipeError::WindowsApi {
            function: "GetOverlappedResult",
            code: term_err.0,
            message: raw_win32_error_string(term_err.0),
        })
    } else if wait == windows::Win32::Foundation::WAIT_TIMEOUT {
        Err(NamedPipeError::Timeout)
    } else if wait == windows::Win32::Foundation::WAIT_FAILED {
        let code = unsafe { GetLastError().0 };
        Err(NamedPipeError::WindowsApi {
            function: "WaitForMultipleObjects",
            code,
            message: raw_win32_error_string(code),
        })
    } else {
        Err(NamedPipeError::WindowsApi {
            function: "WaitForMultipleObjects",
            code: wait.0,
            message: format!("Unexpected wait result {}", wait.0),
        })
    }
}

/// Represents a single low-level server instance of the named pipe waiting or connected.
pub struct NamedPipeServerInstance {
    pipe: ScopedHandle,
    pipe_name: String,
    child_sid: ValidatedSid,
}

impl NamedPipeServerInstance {
    /// Creates a new server pipe instance with the specified name, validated child SID,
    /// and canonical protected DACL.
    ///
    /// Structurally enforces:
    /// - Non-null canonical security descriptor from `create_pipe_security_descriptor`
    /// - `PIPE_REJECT_REMOTE_CLIENTS` (no remote bypass in production)
    /// - `FILE_FLAG_OVERLAPPED`
    /// - Byte mode (`PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT`)
    pub fn create(
        pipe_name: &str,
        child_sid: &ValidatedSid,
        max_instances: u32,
    ) -> Result<Self, NamedPipeError> {
        let sd = create_pipe_security_descriptor(child_sid)?;
        let sa = windows::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0.0,
            bInheritHandle: BOOL(0),
        };

        let pipe_name_w = to_wide(pipe_name);
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(pipe_name_w.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_OVERLAPPED.0),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                max_instances,
                65536,
                65536,
                5000,
                Some(&sa as *const _),
            )
        };

        if handle.is_invalid() {
            let code = unsafe { GetLastError().0 };
            if code == ERROR_PIPE_BUSY.0 {
                return Err(NamedPipeError::CapacityExceeded);
            }
            return Err(NamedPipeError::WindowsApi {
                function: "CreateNamedPipeW",
                code,
                message: raw_win32_error_string(code),
            });
        }

        Ok(Self {
            pipe: ScopedHandle(handle),
            pipe_name: pipe_name.to_string(),
            child_sid: child_sid.clone(),
        })
    }

    /// Returns the raw pipe handle.
    pub fn handle(&self) -> HANDLE {
        self.pipe.0
    }

    /// Returns the pipe name.
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Returns the configured child SID.
    pub fn child_sid(&self) -> &ValidatedSid {
        &self.child_sid
    }

    /// Asynchronously waits for a client connection using an overlapped event.
    ///
    /// Requires a mandatory cancellation event handle.
    pub fn accept_connection(
        self,
        cancel_event: HANDLE,
    ) -> Result<NamedPipeConnection, NamedPipeError> {
        let pipe_handle = self.pipe.0;

        let connect_evt =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
                let code = raw_win32_code_from_error(&e);
                NamedPipeError::WindowsApi {
                    function: "CreateEventW",
                    code,
                    message: raw_win32_error_string(code),
                }
            })?;
        let _evt_guard = ScopedHandle(connect_evt);

        let mut ov = OVERLAPPED {
            hEvent: connect_evt,
            ..Default::default()
        };

        let connect_res = unsafe { ConnectNamedPipe(pipe_handle, Some(&mut ov)) };
        if connect_res.is_err() {
            let err = unsafe { GetLastError() };
            if err == ERROR_PIPE_CONNECTED {
                // Client connected between CreateNamedPipe and ConnectNamedPipe
            } else if err == ERROR_IO_PENDING {
                wait_overlapped_or_cancel(pipe_handle, connect_evt, cancel_event, &ov, None)?;
            } else {
                return Err(NamedPipeError::WindowsApi {
                    function: "ConnectNamedPipe",
                    code: err.0,
                    message: raw_win32_error_string(err.0),
                });
            }
        }

        // Physical client connection established
        let raw_h = self.pipe.0;
        std::mem::forget(self.pipe);
        let pipe_guard = SafePipeHandle(raw_h);

        // Step 2: Bounded exact 4-byte prefix read before impersonation
        let mut prebuffer = [0u8; INITIAL_FRAME_PREFIX_BYTES];
        read_initial_prefix_overlapped(pipe_guard.0, &mut prebuffer, cancel_event)?;

        // Step 3: Prefix validation
        let _declared_length = validate_initial_request_prefix(&prebuffer)?;

        // Step 4: Impersonation & client security context extraction
        let context = extract_client_security_context(pipe_guard.0)?;

        Ok(NamedPipeConnection {
            pipe: pipe_guard,
            context,
            prebuffer,
        })
    }
}

/// Reads the exact initial 4-byte request prefix with mandatory cancellation.
/// If client disconnects prematurely, reports typed PartialPrefixDisconnect with bytes read.
fn read_initial_prefix_overlapped(
    pipe: HANDLE,
    buf: &mut [u8; INITIAL_FRAME_PREFIX_BYTES],
    cancel_event: HANDLE,
) -> Result<(), NamedPipeError> {
    let mut total_read = 0;
    match read_overlapped_internal(pipe, buf, cancel_event, &mut total_read) {
        Ok(()) => Ok(()),
        Err(NamedPipeError::Disconnected) => Err(NamedPipeError::PartialPrefixDisconnect {
            read_bytes: total_read,
            target_bytes: INITIAL_FRAME_PREFIX_BYTES,
        }),
        Err(e) => Err(e),
    }
}

/// Reads exactly `buf.len()` bytes using cancellable overlapped I/O.
///
/// Requires a mandatory cancellation event handle. Accumulates bytes across partial reads.
/// If client disconnects, returns `NamedPipeError::Disconnected`.
pub fn read_exact_overlapped(
    pipe: HANDLE,
    buf: &mut [u8],
    cancel_event: HANDLE,
) -> Result<(), NamedPipeError> {
    let mut total_read = 0;
    read_overlapped_internal(pipe, buf, cancel_event, &mut total_read)
}

fn read_overlapped_internal(
    pipe: HANDLE,
    buf: &mut [u8],
    cancel_event: HANDLE,
    total_read: &mut usize,
) -> Result<(), NamedPipeError> {
    let target = buf.len();

    let read_evt = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
        let code = raw_win32_code_from_error(&e);
        NamedPipeError::WindowsApi {
            function: "CreateEventW",
            code,
            message: raw_win32_error_string(code),
        }
    })?;
    let _evt_guard = ScopedHandle(read_evt);

    while *total_read < target {
        let mut ov = OVERLAPPED {
            hEvent: read_evt,
            ..Default::default()
        };

        let slice = &mut buf[*total_read..];

        let ret = unsafe { ReadFile(pipe, Some(slice), None, Some(&mut ov)) };

        let transferred = if ret.is_ok() {
            // Immediate completion on asynchronous handle:
            // Must inspect GetOverlappedResult to obtain authoritative transfer count.
            completed_overlapped_transfer(pipe, &ov)?
        } else {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                wait_overlapped_or_cancel(pipe, read_evt, cancel_event, &ov, None)?
            } else if err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA {
                return Err(NamedPipeError::Disconnected);
            } else {
                return Err(NamedPipeError::WindowsApi {
                    function: "ReadFile",
                    code: err.0,
                    message: raw_win32_error_string(err.0),
                });
            }
        };

        if transferred == 0 {
            return Err(NamedPipeError::Disconnected);
        }
        *total_read += transferred as usize;
    }

    Ok(())
}

/// Writes all bytes in `buf` using cancellable overlapped I/O.
///
/// Requires a mandatory cancellation event handle.
pub fn write_all_overlapped(
    pipe: HANDLE,
    buf: &[u8],
    cancel_event: HANDLE,
) -> Result<(), NamedPipeError> {
    let mut total_written = 0;
    let target = buf.len();

    let write_evt = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
        let code = raw_win32_code_from_error(&e);
        NamedPipeError::WindowsApi {
            function: "CreateEventW",
            code,
            message: raw_win32_error_string(code),
        }
    })?;
    let _evt_guard = ScopedHandle(write_evt);

    while total_written < target {
        let mut ov = OVERLAPPED {
            hEvent: write_evt,
            ..Default::default()
        };

        let slice = &buf[total_written..];

        let ret = unsafe { WriteFile(pipe, Some(slice), None, Some(&mut ov)) };

        let transferred = if ret.is_ok() {
            // Immediate completion on asynchronous handle:
            // Must inspect GetOverlappedResult to obtain authoritative transfer count.
            completed_overlapped_transfer(pipe, &ov)?
        } else {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                wait_overlapped_or_cancel(pipe, write_evt, cancel_event, &ov, None)?
            } else if err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA {
                return Err(NamedPipeError::Disconnected);
            } else {
                return Err(NamedPipeError::WindowsApi {
                    function: "WriteFile",
                    code: err.0,
                    message: raw_win32_error_string(err.0),
                });
            }
        };

        if transferred == 0 && total_written < target {
            return Err(NamedPipeError::Disconnected);
        }
        total_written += transferred as usize;
    }

    Ok(())
}

/// Impersonates client, extracts objective OS facts from the access token and pipe, then reverts.
///
/// Fails closed on any API error. Does NOT silently default PID, Session ID, or Admin state.
pub fn extract_client_security_context(
    pipe: HANDLE,
) -> Result<ClientSecurityContext, NamedPipeError> {
    unsafe {
        if let Err(e) = ImpersonateNamedPipeClient(pipe) {
            let code = raw_win32_code_from_error(&e);
            return Err(NamedPipeError::WindowsApi {
                function: "ImpersonateNamedPipeClient",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    let mut guard = ImpersonationGuard::new();

    // Inspect thread token
    let mut token_handle = HANDLE::default();
    let thread_handle = unsafe { GetCurrentThread() };

    let open_res = unsafe { OpenThreadToken(thread_handle, TOKEN_QUERY, true, &mut token_handle) };

    if let Err(e) = open_res {
        let code = raw_win32_code_from_error(&e);
        guard.revert()?;
        return Err(NamedPipeError::WindowsApi {
            function: "OpenThreadToken",
            code,
            message: raw_win32_error_string(code),
        });
    }
    let _token_guard = ScopedHandle(token_handle);

    // Step 1: Two-phase extraction of User SID (TokenUser)
    let mut user_buf_len = 0u32;
    let user_sizing_res =
        unsafe { GetTokenInformation(token_handle, TokenUser, None, 0, &mut user_buf_len) };
    if let Err(e) = user_sizing_res {
        let code = raw_win32_code_from_error(&e);
        // Expected outcome for sizing call is ERROR_INSUFFICIENT_BUFFER (122)
        if code != 122 {
            guard.revert()?;
            return Err(NamedPipeError::WindowsApi {
                function: "GetTokenInformation(TokenUser, sizing)",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    let min_user_size = size_of::<TOKEN_USER>();
    if (user_buf_len as usize) < min_user_size {
        guard.revert()?;
        return Err(NamedPipeError::SecurityViolation(format!(
            "GetTokenInformation(TokenUser) returned insufficient buffer size {user_buf_len}, minimum required is {min_user_size}"
        )));
    }

    let mut user_buf =
        AlignedNativeBuffer::new(user_buf_len as usize, std::mem::align_of::<TOKEN_USER>())?;

    let user_info_res = unsafe {
        GetTokenInformation(
            token_handle,
            TokenUser,
            Some(user_buf.as_mut_ptr() as *mut c_void),
            user_buf_len,
            &mut user_buf_len,
        )
    };
    if let Err(e) = user_info_res {
        let code = raw_win32_code_from_error(&e);
        guard.revert()?;
        return Err(NamedPipeError::WindowsApi {
            function: "GetTokenInformation(TokenUser)",
            code,
            message: raw_win32_error_string(code),
        });
    }

    if !user_buf.is_aligned_for::<TOKEN_USER>() {
        guard.revert()?;
        return Err(NamedPipeError::SecurityViolation(
            "TokenUser buffer pointer violates TOKEN_USER alignment contract".to_string(),
        ));
    }

    let token_user_ptr = user_buf.as_ptr() as *const TOKEN_USER;
    let user_psid = unsafe { (*token_user_ptr).User.Sid };
    if user_psid.0.is_null()
        || !unsafe { windows::Win32::Security::IsValidSid(user_psid).as_bool() }
    {
        guard.revert()?;
        return Err(NamedPipeError::SecurityViolation(
            "TokenUser contains null or invalid SID pointer".to_string(),
        ));
    }

    let mut user_str_ptr = PWSTR::null();
    unsafe {
        if let Err(e) = ConvertSidToStringSidW(user_psid, &mut user_str_ptr) {
            let code = raw_win32_code_from_error(&e);
            guard.revert()?;
            return Err(NamedPipeError::WindowsApi {
                function: "ConvertSidToStringSidW(TokenUser)",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    let user_sid = unsafe {
        let mut len = 0;
        while *user_str_ptr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(user_str_ptr.0, len);
        let s = String::from_utf16_lossy(slice);
        let _ = LocalFree(Some(HLOCAL(user_str_ptr.0 as *mut c_void)));
        s
    };

    // Step 2: Two-phase extraction of Admin Group membership (TokenGroups)
    let mut groups_buf_len = 0u32;
    let groups_sizing_res =
        unsafe { GetTokenInformation(token_handle, TokenGroups, None, 0, &mut groups_buf_len) };
    if let Err(e) = groups_sizing_res {
        let code = raw_win32_code_from_error(&e);
        // Expected outcome for sizing call is ERROR_INSUFFICIENT_BUFFER (122)
        if code != 122 {
            guard.revert()?;
            return Err(NamedPipeError::WindowsApi {
                function: "GetTokenInformation(TokenGroups, sizing)",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    let min_groups_size = size_of::<TOKEN_GROUPS>();
    if (groups_buf_len as usize) < min_groups_size {
        guard.revert()?;
        return Err(NamedPipeError::SecurityViolation(format!(
            "GetTokenInformation(TokenGroups) returned insufficient buffer size {groups_buf_len}, minimum required is {min_groups_size}"
        )));
    }

    let mut groups_buf = AlignedNativeBuffer::new(
        groups_buf_len as usize,
        std::mem::align_of::<TOKEN_GROUPS>(),
    )?;

    let groups_info_res = unsafe {
        GetTokenInformation(
            token_handle,
            TokenGroups,
            Some(groups_buf.as_mut_ptr() as *mut c_void),
            groups_buf_len,
            &mut groups_buf_len,
        )
    };
    if let Err(e) = groups_info_res {
        let code = raw_win32_code_from_error(&e);
        guard.revert()?;
        return Err(NamedPipeError::WindowsApi {
            function: "GetTokenInformation(TokenGroups)",
            code,
            message: raw_win32_error_string(code),
        });
    }

    if !groups_buf.is_aligned_for::<TOKEN_GROUPS>() {
        guard.revert()?;
        return Err(NamedPipeError::SecurityViolation(
            "TokenGroups buffer pointer violates TOKEN_GROUPS alignment contract".to_string(),
        ));
    }

    let token_groups = groups_buf.as_ptr() as *const TOKEN_GROUPS;
    let count = unsafe { (*token_groups).GroupCount } as usize;

    let group_elem_size = size_of::<windows::Win32::Security::SID_AND_ATTRIBUTES>();
    let groups_offset = unsafe {
        let base = token_groups as usize;
        let field = std::ptr::addr_of!((*token_groups).Groups) as usize;
        field - base
    };

    let total_groups_bytes = count
        .checked_mul(group_elem_size)
        .and_then(|arr| groups_offset.checked_add(arr))
        .ok_or_else(|| {
            NamedPipeError::SecurityViolation(
                "Integer overflow calculating TOKEN_GROUPS buffer bounds".to_string(),
            )
        })?;

    if groups_buf.size() < total_groups_bytes {
        guard.revert()?;
        return Err(NamedPipeError::SecurityViolation(format!(
            "TOKEN_GROUPS GroupCount {count} requires {total_groups_bytes} bytes, but buffer size is only {}",
            groups_buf.size()
        )));
    }

    let groups_slice =
        unsafe { std::slice::from_raw_parts((*token_groups).Groups.as_ptr(), count) };

    let mut is_local_administrator = false;

    // Builtin Administrators SID is S-1-5-32-544
    let admin_sid_w = to_wide("S-1-5-32-544");
    let mut admin_psid = PSID::default();
    unsafe {
        if let Err(e) = ConvertStringSidToSidW(PCWSTR(admin_sid_w.as_ptr()), &mut admin_psid) {
            let code = raw_win32_code_from_error(&e);
            guard.revert()?;
            return Err(NamedPipeError::WindowsApi {
                function: "ConvertStringSidToSidW(BuiltinAdministrators)",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    for group in groups_slice {
        if group.Sid.0.is_null() {
            continue;
        }
        let is_valid = unsafe { windows::Win32::Security::IsValidSid(group.Sid).as_bool() };
        if is_valid {
            let is_equal =
                unsafe { windows::Win32::Security::EqualSid(group.Sid, admin_psid).is_ok() };
            if is_equal && is_active_administrator_group(group.Attributes) {
                is_local_administrator = true;
                break;
            }
        }
    }
    unsafe {
        let _ = LocalFree(Some(HLOCAL(admin_psid.0)));
    }

    // Get process ID from the pipe handle
    let mut client_process_id = 0u32;
    unsafe {
        if let Err(e) = GetNamedPipeClientProcessId(pipe, &mut client_process_id) {
            let code = raw_win32_code_from_error(&e);
            guard.revert()?;
            return Err(NamedPipeError::WindowsApi {
                function: "GetNamedPipeClientProcessId",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    // Get session ID from the pipe handle
    let mut client_session_id = 0u32;
    unsafe {
        if let Err(e) = GetNamedPipeClientSessionId(pipe, &mut client_session_id) {
            let code = raw_win32_code_from_error(&e);
            guard.revert()?;
            return Err(NamedPipeError::WindowsApi {
                function: "GetNamedPipeClientSessionId",
                code,
                message: raw_win32_error_string(code),
            });
        }
    }

    // Explicitly revert before returning
    guard.revert()?;

    Ok(ClientSecurityContext {
        user_sid,
        is_local_administrator,
        client_process_id,
        client_session_id,
    })
}

/// Connected PALKA IPC Named Pipe client connection.
pub struct NamedPipeConnection {
    pipe: SafePipeHandle,
    context: ClientSecurityContext,
    prebuffer: [u8; INITIAL_FRAME_PREFIX_BYTES],
}

impl NamedPipeConnection {
    /// Returns the verified OS security context of the connected client.
    pub fn context(&self) -> &ClientSecurityContext {
        &self.context
    }

    /// Returns the exact physical 4 bytes of the initial length prefix read prior to impersonation.
    pub fn prebuffer(&self) -> &[u8; INITIAL_FRAME_PREFIX_BYTES] {
        &self.prebuffer
    }

    /// Reads exactly `buf.len()` bytes from the client with mandatory cancellation support.
    pub fn read_exact(
        &mut self,
        buf: &mut [u8],
        cancel_event: HANDLE,
    ) -> Result<(), NamedPipeError> {
        read_exact_overlapped(self.pipe.0, buf, cancel_event)
    }

    /// Writes all bytes in `buf` to the client with mandatory cancellation support.
    pub fn write_all(&mut self, buf: &[u8], cancel_event: HANDLE) -> Result<(), NamedPipeError> {
        write_all_overlapped(self.pipe.0, buf, cancel_event)
    }

    /// Cancels all pending I/O operations on this connection handle via `CancelIoEx`.
    pub fn cancel(&self) -> Result<(), NamedPipeError> {
        unsafe {
            if let Err(e) = CancelIoEx(self.pipe.0, None) {
                let code = raw_win32_code_from_error(&e);
                // If there were no pending operations, CancelIoEx returns ERROR_NOT_FOUND (1168)
                if code != 1168 {
                    return Err(NamedPipeError::WindowsApi {
                        function: "CancelIoEx",
                        code,
                        message: raw_win32_error_string(code),
                    });
                }
            }
        }
        Ok(())
    }

    /// Disconnects the named pipe instance.
    pub fn disconnect(&mut self) -> Result<(), NamedPipeError> {
        unsafe {
            if let Err(e) = DisconnectNamedPipe(self.pipe.0) {
                let code = raw_win32_code_from_error(&e);
                return Err(NamedPipeError::WindowsApi {
                    function: "DisconnectNamedPipe",
                    code,
                    message: raw_win32_error_string(code),
                });
            }
        }
        Ok(())
    }

    /// Returns the raw pipe handle.
    pub fn handle(&self) -> HANDLE {
        self.pipe.0
    }
}

/// Server listener for PALKA IPC Named Pipe connections.
pub struct NamedPipeServer {
    pipe_name: String,
    child_sid: ValidatedSid,
    max_instances: u32,
    cancel_event: ScopedHandle,
}

impl NamedPipeServer {
    /// Binds to the canonical pipe name `\\.\pipe\palka_ipc_v1`.
    pub fn bind(child_sid: ValidatedSid) -> Result<Self, NamedPipeError> {
        let cancel_evt =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
                let code = raw_win32_code_from_error(&e);
                NamedPipeError::WindowsApi {
                    function: "CreateEventW",
                    code,
                    message: raw_win32_error_string(code),
                }
            })?;

        Ok(Self {
            pipe_name: CANONICAL_PIPE_NAME.to_string(),
            child_sid,
            max_instances: MAX_PIPE_INSTANCES,
            cancel_event: ScopedHandle(cancel_evt),
        })
    }

    /// Binds to a custom pipe name (available strictly for tests).
    #[cfg(test)]
    pub fn bind_custom(
        pipe_name: &str,
        child_sid: ValidatedSid,
        max_instances: u32,
    ) -> Result<Self, NamedPipeError> {
        let cancel_evt =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
                let code = raw_win32_code_from_error(&e);
                NamedPipeError::WindowsApi {
                    function: "CreateEventW",
                    code,
                    message: raw_win32_error_string(code),
                }
            })?;

        Ok(Self {
            pipe_name: pipe_name.to_string(),
            child_sid,
            max_instances,
            cancel_event: ScopedHandle(cancel_evt),
        })
    }

    /// Accepts an incoming client connection.
    ///
    /// Executes the full normative security handshake:
    /// `ConnectNamedPipe` $\to$ read 4-byte prefix $\to$ validate $1..=65536 \to$
    /// `ImpersonateNamedPipeClient` $\to$ extract `ClientSecurityContext` $\to$ `RevertToSelf`.
    pub fn accept(&self) -> Result<NamedPipeConnection, NamedPipeError> {
        let instance =
            NamedPipeServerInstance::create(&self.pipe_name, &self.child_sid, self.max_instances)?;
        instance.accept_connection(self.cancel_event.0)
    }

    /// Cancels any pending `accept()` operation.
    pub fn cancel(&self) -> Result<(), NamedPipeError> {
        unsafe {
            if let Err(e) = windows::Win32::System::Threading::SetEvent(self.cancel_event.0) {
                let code = raw_win32_code_from_error(&e);
                return Err(NamedPipeError::WindowsApi {
                    function: "SetEvent",
                    code,
                    message: raw_win32_error_string(code),
                });
            }
        }
        Ok(())
    }

    /// Resets the cancellation event.
    pub fn reset_cancel(&self) -> Result<(), NamedPipeError> {
        unsafe {
            if let Err(e) = windows::Win32::System::Threading::ResetEvent(self.cancel_event.0) {
                let code = raw_win32_code_from_error(&e);
                return Err(NamedPipeError::WindowsApi {
                    function: "ResetEvent",
                    code,
                    message: raw_win32_error_string(code),
                });
            }
        }
        Ok(())
    }

    /// Returns the raw cancellation event handle.
    pub fn cancel_event(&self) -> HANDLE {
        self.cancel_event.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Pipes::NAMED_PIPE_MODE;

    pub const PIPE_ACCEPT_REMOTE_CLIENTS: NAMED_PIPE_MODE = NAMED_PIPE_MODE(0);

    impl NamedPipeServerInstance {
        pub(crate) fn create_test_instance_with_options(
            pipe_name: &str,
            child_sid: Option<&ValidatedSid>,
            buffer_size: u32,
            max_instances: u32,
            reject_remote: bool,
        ) -> Result<Self, NamedPipeError> {
            let sd = match child_sid {
                Some(sid) => Some(create_pipe_security_descriptor(sid)?),
                None => None,
            };
            let sid = child_sid
                .cloned()
                .unwrap_or_else(|| ValidatedSid("S-1-5-18".to_string()));

            let sa = sd
                .as_ref()
                .map(|s| windows::Win32::Security::SECURITY_ATTRIBUTES {
                    nLength: size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: s.0.0,
                    bInheritHandle: BOOL(0),
                });

            let remote_flag = if reject_remote {
                PIPE_REJECT_REMOTE_CLIENTS
            } else {
                PIPE_ACCEPT_REMOTE_CLIENTS
            };

            let pipe_name_w = to_wide(pipe_name);
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(pipe_name_w.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_OVERLAPPED.0),
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | remote_flag,
                    max_instances,
                    buffer_size,
                    buffer_size,
                    5000,
                    sa.as_ref().map(|a| a as *const _),
                )
            };

            if handle.is_invalid() {
                let code = unsafe { GetLastError().0 };
                if code == ERROR_PIPE_BUSY.0 {
                    return Err(NamedPipeError::CapacityExceeded);
                }
                return Err(NamedPipeError::WindowsApi {
                    function: "CreateNamedPipeW",
                    code,
                    message: raw_win32_error_string(code),
                });
            }

            Ok(Self {
                pipe: ScopedHandle(handle),
                pipe_name: pipe_name.to_string(),
                child_sid: sid,
            })
        }
    }
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc::channel;
    use std::thread;
    use std::time::Duration;

    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION,
        AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_MODE, OPEN_EXISTING};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use crate::named_pipe::{
        CHILD_PIPE_CLIENT_DESIRED_ACCESS_MASK, SE_GROUP_ENABLED, SE_GROUP_ENABLED_BY_DEFAULT,
        SE_GROUP_USE_FOR_DENY_ONLY, is_active_administrator_group, revert_to_self_failure_is_fatal,
        validate_initial_request_prefix,
    };

    static PIPE_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_test_pipe_name(tag: &str) -> String {
        let count = PIPE_TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!(r"\\.\pipe\palka_slice2_test_{tag}_{pid}_{nanos}_{count}")
    }

    fn create_test_cancel_event() -> ScopedHandle {
        let ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .expect("CreateEventW for test cancel failed");
        ScopedHandle(ev)
    }

    /// Retrieves current process user SID as a ValidatedSid.
    fn current_user_sid() -> ValidatedSid {
        let mut token_h = HANDLE::default();
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_h)
                .expect("OpenProcessToken failed");
        }
        let _guard = ScopedHandle(token_h);

        let mut len = 0u32;
        let sizing_res = unsafe { GetTokenInformation(token_h, TokenUser, None, 0, &mut len) };
        if let Err(e) = sizing_res {
            let code = raw_win32_code_from_error(&e);
            if code != 122 {
                panic!("GetTokenInformation sizing failed: {code}");
            }
        }
        let mut buf = AlignedNativeBuffer::new(len as usize, std::mem::align_of::<TOKEN_USER>())
            .expect("Allocate aligned TokenUser buffer");
        unsafe {
            GetTokenInformation(
                token_h,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut c_void),
                len,
                &mut len,
            )
            .expect("GetTokenInformation failed");
        }
        assert!(buf.is_aligned_for::<TOKEN_USER>());
        let token_user = buf.as_ptr() as *const TOKEN_USER;
        let psid = unsafe { (*token_user).User.Sid };

        let mut str_ptr = PWSTR::null();
        unsafe {
            ConvertSidToStringSidW(psid, &mut str_ptr).expect("ConvertSidToStringSidW failed");
        }

        let sid_str = unsafe {
            let mut l = 0;
            while *str_ptr.0.add(l) != 0 {
                l += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(str_ptr.0, l));
            let _ = LocalFree(Some(HLOCAL(str_ptr.0 as *mut c_void)));
            s
        };

        ValidatedSid::parse(&sid_str).expect("Failed to parse current user SID")
    }

    /// Helper connecting an overlapped client to a named pipe.
    fn connect_client(pipe_name: &str) -> ScopedHandle {
        let pipe_w = to_wide(pipe_name);
        let h = unsafe {
            CreateFileW(
                PCWSTR(pipe_w.as_ptr()),
                CHILD_PIPE_CLIENT_DESIRED_ACCESS_MASK,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .expect("Client CreateFileW failed");
        ScopedHandle(h)
    }

    /// IPC-57: Physical Windows integration test for remote client rejection.
    ///
    /// Verifies that opening an ephemeral pipe created with `PIPE_REJECT_REMOTE_CLIENTS`
    /// via a UNC path (`\\127.0.0.1\pipe\...`) fails closed with ERROR_ACCESS_DENIED (code 5),
    /// while local connection via `\\.\pipe\...` succeeds.
    #[test]
    fn test_physical_pipe_remote_client_rejection() {
        let child_sid = current_user_sid();

        // 1. Control pipe: created with reject_remote = false (PIPE_ACCEPT_REMOTE_CLIENTS)
        let ctrl_pipe_name = make_test_pipe_name("remote_ctrl");
        let ctrl_instance = NamedPipeServerInstance::create_test_instance_with_options(
            &ctrl_pipe_name,
            Some(&child_sid),
            4096,
            1,
            false, // reject_remote = false
        )
        .expect("Control server instance creation failed");

        // Attempt remote UNC connection to control pipe
        let ctrl_unc = ctrl_pipe_name.replace(r"\\.\pipe\", r"\\127.0.0.1\pipe\");
        let ctrl_unc_w = to_wide(&ctrl_unc);
        let ctrl_res = unsafe {
            CreateFileW(
                PCWSTR(ctrl_unc_w.as_ptr()),
                CHILD_PIPE_CLIENT_DESIRED_ACCESS_MASK,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };

        let ctrl_client = ctrl_res.expect("Remote UNC path must be available for control pipe");
        drop(ScopedHandle(ctrl_client));
        drop(ctrl_instance);

        // 2. Test pipe: created with reject_remote = true (PIPE_REJECT_REMOTE_CLIENTS - production default)
        let test_pipe_name = make_test_pipe_name("remote_test");
        let test_instance = NamedPipeServerInstance::create(&test_pipe_name, &child_sid, 1)
            .expect("Test server instance creation failed");

        // Attempt the exact same remote UNC connection to test pipe
        let test_unc = test_pipe_name.replace(r"\\.\pipe\", r"\\127.0.0.1\pipe\");
        let test_unc_w = to_wide(&test_unc);
        let test_res = unsafe {
            CreateFileW(
                PCWSTR(test_unc_w.as_ptr()),
                CHILD_PIPE_CLIENT_DESIRED_ACCESS_MASK,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };

        assert!(
            test_res.is_err(),
            "Remote UNC client open must fail on pipe created with PIPE_REJECT_REMOTE_CLIENTS"
        );
        let test_err = test_res.err().unwrap();
        let win32_code = raw_win32_code_from_error(&test_err);
        assert_eq!(
            win32_code, 5,
            "Differential proof: control pipe succeeded, test pipe must specifically be rejected with ERROR_ACCESS_DENIED (5), got {win32_code}"
        );

        // 3. Local connection via \\.\pipe\... succeeds
        let local_client = connect_client(&test_pipe_name);
        assert!(
            !local_client.raw().is_invalid(),
            "Local client should connect successfully"
        );
        drop(local_client);
        drop(test_instance);

        println!(
            "IPC_57_DIFFERENTIAL_PROOF=PASS (control UNC succeeded, test UNC rejected with ERROR_ACCESS_DENIED 5)"
        );
    }

    /// IPC-58: Physical DACL inspection on ephemeral pipe
    ///
    /// Verifies protected DACL semantics, explicit Deny for Anonymous and Network Logon,
    /// exact Full Control (0x001F01FF) for SYSTEM and Builtin Administrators,
    /// and exact 0x00100083 for Child with strict exclusion of bit 0x4 and sensitive rights.
    #[test]
    fn test_physical_pipe_dacl_inspection() {
        let pipe_name = make_test_pipe_name("dacl_inspect");
        let child_sid = current_user_sid();

        let server_instance = NamedPipeServerInstance::create(&pipe_name, &child_sid, 1)
            .expect("Server instance creation failed");

        let handle = server_instance.handle();

        let mut p_sd = PSECURITY_DESCRIPTOR::default();
        let mut p_dacl: *mut ACL = std::ptr::null_mut();

        let res = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut p_dacl),
                None,
                Some(&mut p_sd),
            )
        };
        assert_eq!(res.0, 0, "GetSecurityInfo failed with code {}", res.0);
        let _sd_guard = AutoSecurityDescriptor(p_sd);

        // 1. Verify protected DACL
        let mut control = 0u16;
        let mut revision = 0u32;
        unsafe {
            GetSecurityDescriptorControl(p_sd, &mut control, &mut revision)
                .expect("GetSecurityDescriptorControl failed");
        }
        let is_protected = (control & (SE_DACL_PROTECTED.0 as u16)) != 0;
        assert!(
            is_protected,
            "Server Named Pipe DACL must be protected (SE_DACL_PROTECTED)"
        );

        // 2. Inspect individual ACEs in the DACL
        assert!(!p_dacl.is_null(), "DACL must be present");

        let mut acl_size_info = ACL_SIZE_INFORMATION::default();
        unsafe {
            GetAclInformation(
                p_dacl,
                &mut acl_size_info as *mut _ as *mut _,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
            .expect("GetAclInformation failed");
        }

        struct InspectedAce {
            ace_type: u8,
            mask: u32,
            sid: String,
        }

        let mut aces = Vec::new();
        for i in 0..acl_size_info.AceCount {
            let mut p_ace: *mut c_void = std::ptr::null_mut();
            unsafe {
                GetAce(p_dacl, i, &mut p_ace).expect("GetAce failed");
            }
            let header = unsafe { *(p_ace as *const ACE_HEADER) };
            let (mask, sid_ptr) = if header.AceType == 0 {
                let allowed = unsafe { &*(p_ace as *const ACCESS_ALLOWED_ACE) };
                (
                    allowed.Mask,
                    &allowed.SidStart as *const u32 as *const c_void,
                )
            } else if header.AceType == 1 {
                let denied = unsafe { &*(p_ace as *const ACCESS_DENIED_ACE) };
                (denied.Mask, &denied.SidStart as *const u32 as *const c_void)
            } else {
                continue;
            };

            let mut str_ptr = PWSTR::null();
            unsafe {
                ConvertSidToStringSidW(PSID(sid_ptr as _), &mut str_ptr)
                    .expect("ConvertSidToStringSidW failed");
            }
            let sid_str = unsafe {
                let mut l = 0;
                while *str_ptr.0.add(l) != 0 {
                    l += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(str_ptr.0, l));
                let _ = LocalFree(Some(HLOCAL(str_ptr.0 as *mut c_void)));
                s
            };

            aces.push(InspectedAce {
                ace_type: header.AceType,
                mask,
                sid: sid_str,
            });
        }

        // Canonical expectations:
        // ANONYMOUS (S-1-5-7): DENY (type 1)
        // NETWORK LOGON (S-1-5-2): DENY (type 1)
        // SYSTEM (S-1-5-18): ALLOW FULL (0x001F01FF)
        // ADMINISTRATORS (S-1-5-32-544): ALLOW FULL (0x001F01FF)
        // CHILD SID: ALLOW 0x00100083
        let mut anon_denied = false;
        let mut net_denied = false;
        let mut sys_ace: Option<&InspectedAce> = None;
        let mut admin_ace: Option<&InspectedAce> = None;
        let mut child_ace: Option<&InspectedAce> = None;

        for ace in &aces {
            if ace.sid == "S-1-5-7" && ace.ace_type == 1 {
                anon_denied = true;
            } else if ace.sid == "S-1-5-2" && ace.ace_type == 1 {
                net_denied = true;
            } else if ace.sid == "S-1-5-18" && ace.ace_type == 0 {
                sys_ace = Some(ace);
            } else if ace.sid == "S-1-5-32-544" && ace.ace_type == 0 {
                admin_ace = Some(ace);
            } else if ace.sid == child_sid.as_str() && ace.ace_type == 0 {
                child_ace = Some(ace);
            }
        }

        assert!(anon_denied, "Anonymous must have explicit Deny ACE");
        assert!(net_denied, "Network Logon must have explicit Deny ACE");

        // Exact Full Control verification for SYSTEM and Administrators
        let sys = sys_ace.expect("SYSTEM must have Allow ACE");
        assert_eq!(
            sys.mask, 0x001F01FF,
            "SYSTEM Allow ACE must grant Full Control (0x001F01FF), got 0x{:08X}",
            sys.mask
        );

        let admin = admin_ace.expect("Builtin Administrators must have Allow ACE");
        assert_eq!(
            admin.mask, 0x001F01FF,
            "Builtin Administrators Allow ACE must grant Full Control (0x001F01FF), got 0x{:08X}",
            admin.mask
        );

        // Exact Child Mask verification
        let child = child_ace.expect("Child SID must have an Allow ACE in DACL");
        assert_eq!(
            child.mask, CHILD_PIPE_DACL_ALLOWED_ACCESS_MASK,
            "Child mask must exactly match canonical CHILD_PIPE_DACL_ALLOWED_ACCESS_MASK (0x00100083)"
        );

        // Rigorous check that child does NOT have bit 0x00000004
        assert_eq!(
            child.mask & 0x00000004,
            0,
            "Child ACE MUST NOT grant FILE_CREATE_PIPE_INSTANCE / FILE_APPEND_DATA (bit 0x4)"
        );

        // Verify absence of sensitive administrative rights
        assert_eq!(child.mask & 0x00010000, 0, "Child MUST NOT have DELETE");
        assert_eq!(
            child.mask & 0x00020000,
            0,
            "Child MUST NOT have READ_CONTROL"
        );
        assert_eq!(child.mask & 0x00040000, 0, "Child MUST NOT have WRITE_DAC");
        assert_eq!(
            child.mask & 0x00080000,
            0,
            "Child MUST NOT have WRITE_OWNER"
        );
        assert_eq!(
            child.mask & 0x00000100,
            0,
            "Child MUST NOT have FILE_WRITE_ATTRIBUTES"
        );
    }

    /// IPC-59: Client security context extraction, PID/Session ID, and RevertToSelf safety.
    #[test]
    fn test_client_security_context_and_revert_safety() {
        let pipe_name = make_test_pipe_name("security_ctx");
        let cur_user = current_user_sid();

        let server = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name, cur_user.clone(), 1)
                .expect("Server bind failed"),
        );
        let server_clone = Arc::clone(&server);
        let cur_user_clone = cur_user.clone();

        let (tx, rx) = channel();

        let server_thread = thread::spawn(move || {
            let conn = server_clone.accept().expect("Server accept failed");
            let ctx = conn.context();

            // 1. Verify extracted SID matches connecting user SID
            assert_eq!(ctx.user_sid, cur_user_clone.as_str());

            // 2. Verify client PID matches our process ID
            assert_eq!(ctx.client_process_id, std::process::id());

            // 3. Verify client session ID is non-zero
            assert_eq!(ctx.client_session_id, unsafe {
                let mut sid = 0u32;
                let _ = GetNamedPipeClientSessionId(conn.handle(), &mut sid);
                sid
            });

            // 4. Verify server thread is NOT impersonating (reverted)
            let mut tok = HANDLE::default();
            let has_thread_tok =
                unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut tok).is_ok() };
            assert!(
                !has_thread_tok,
                "Server thread must be reverted to self, but OpenThreadToken succeeded"
            );

            tx.send(()).expect("Channel send failed");
        });

        thread::sleep(Duration::from_millis(50));
        let client = connect_client(&pipe_name);
        let cancel_ev = create_test_cancel_event();

        // Send valid 4-byte prefix
        write_all_overlapped(client.raw(), &[10, 0, 0, 0], cancel_ev.raw()).expect("Write prefix");

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Server thread did not finish within 5 seconds");
        server_thread.join().expect("Server thread joined");

        // 5. Verify process-fatal invariant
        assert!(revert_to_self_failure_is_fatal());

        // 6. Test seam: verify on_revert_to_self_failure panics with PROCESS_FATAL when testing switch is on
        set_suppress_abort_for_testing(true);
        let panic_result = std::panic::catch_unwind(|| {
            on_revert_to_self_failure(1234);
        });
        assert!(
            panic_result.is_err(),
            "on_revert_to_self_failure must panic under test seam"
        );
        set_suppress_abort_for_testing(false);
    }

    /// IPC-60: Physical Overlapped I/O primitives verification
    #[test]
    fn test_physical_overlapped_io_primitives() {
        let pipe_name = make_test_pipe_name("overlapped_primitives");
        let child_sid = current_user_sid();

        let server = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name, child_sid, 1).expect("Server bind failed"),
        );
        let server_clone = Arc::clone(&server);
        let pipe_name_clone = pipe_name.clone();

        let (tx, rx) = channel();

        let server_thread = thread::spawn(move || {
            let mut conn = server_clone.accept().expect("Server accept failed");
            let cancel_ev = create_test_cancel_event();

            let mut body = [0u8; 4];
            conn.read_exact(&mut body, cancel_ev.raw())
                .expect("Read body failed");
            assert_eq!(&body, b"ping");

            conn.write_all(b"pong", cancel_ev.raw())
                .expect("Write pong failed");
            tx.send(()).expect("Signal complete");
        });

        thread::sleep(Duration::from_millis(50));
        let client = connect_client(&pipe_name_clone);
        let cancel_ev = create_test_cancel_event();

        // Write 4-byte length prefix
        write_all_overlapped(client.raw(), &[4, 0, 0, 0], cancel_ev.raw()).expect("Write prefix");
        // Write 4-byte body
        write_all_overlapped(client.raw(), b"ping", cancel_ev.raw()).expect("Write ping");

        // Read response
        let mut resp = [0u8; 4];
        read_exact_overlapped(client.raw(), &mut resp, cancel_ev.raw()).expect("Read pong");
        assert_eq!(&resp, b"pong");

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Server did not complete within bounded time");
        server_thread.join().expect("Server thread joined");
    }

    /// IPC-61: Physical pending ConnectNamedPipe cancellation via CancelIoEx
    #[test]
    fn test_physical_connect_cancellation() {
        let pipe_name = make_test_pipe_name("connect_cancel");
        let server = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name, current_user_sid(), 1)
                .expect("Server bind failed"),
        );
        let server_clone = Arc::clone(&server);

        let (tx, rx) = channel();

        let accept_thread = thread::spawn(move || {
            let res = server_clone.accept();
            assert!(
                matches!(res, Err(NamedPipeError::OperationCancelled)),
                "accept() must return OperationCancelled when cancelled, got: {:?}",
                res.err()
            );
            tx.send(()).expect("Signal complete");
        });

        // Ensure ConnectNamedPipe has established an overlapped pending state
        thread::sleep(Duration::from_millis(100));

        // Signal cancellation
        server.cancel().expect("Cancel failed");

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Pending ConnectNamedPipe cancellation timed out");
        accept_thread.join().expect("Accept thread joined");
        server.reset_cancel().expect("Reset cancel failed");

        println!("PENDING_IO_ESTABLISHED=YES");
        println!("CANCELIOEX_REQUESTED=YES");
        println!("COMPLETION=ERROR_OPERATION_ABORTED");
        println!("RESULT=OperationCancelled");
    }

    /// IPC-62: Physical pending ReadFile cancellation via CancelIoEx
    #[test]
    fn test_physical_read_cancellation() {
        let pipe_name = make_test_pipe_name("read_cancel");
        let server = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name, current_user_sid(), 1)
                .expect("Server bind failed"),
        );
        let server_clone = Arc::clone(&server);
        let pipe_name_clone = pipe_name.clone();

        let cancel_event = Arc::new(create_test_cancel_event());
        let cancel_clone = Arc::clone(&cancel_event);

        let (tx, rx) = channel();

        let server_thread = thread::spawn(move || {
            let mut conn = server_clone.accept().expect("Accept failed");

            // Attempt to read 100 bytes from client, which will send nothing
            let mut buf = [0u8; 100];
            let res = conn.read_exact(&mut buf, cancel_clone.raw());
            assert!(
                matches!(res, Err(NamedPipeError::OperationCancelled)),
                "Pending read_exact must return OperationCancelled, got: {:?}",
                res.err()
            );
            tx.send(()).expect("Signal complete");
        });

        thread::sleep(Duration::from_millis(50));
        let client = connect_client(&pipe_name_clone);
        let dummy_ev = create_test_cancel_event();

        // Send valid 4-byte prefix so accept() completes
        write_all_overlapped(client.raw(), &[100, 0, 0, 0], dummy_ev.raw()).expect("Write prefix");

        // Wait for server to enter pending ReadFile
        thread::sleep(Duration::from_millis(100));

        // Trigger cancellation of pending ReadFile
        unsafe {
            windows::Win32::System::Threading::SetEvent(cancel_event.raw())
                .expect("SetEvent on cancel_event failed");
        }

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Pending ReadFile cancellation timed out");
        server_thread.join().expect("Server thread joined");

        println!("PENDING_IO_ESTABLISHED=YES");
        println!("CANCELIOEX_REQUESTED=YES");
        println!("COMPLETION=ERROR_OPERATION_ABORTED");
        println!("RESULT=OperationCancelled");
    }

    /// IPC-63: Physical pending WriteFile cancellation via CancelIoEx
    #[test]
    fn test_physical_write_cancellation() {
        let pipe_name = make_test_pipe_name("write_cancel");
        // Create server instance with genuine small 512-byte buffer
        let server_instance = NamedPipeServerInstance::create_test_instance_with_options(
            &pipe_name,
            Some(&current_user_sid()),
            512,
            1,
            true,
        )
        .expect("Server instance creation failed");

        let client = connect_client(&pipe_name);
        let cancel_event = Arc::new(create_test_cancel_event());
        let cancel_clone = Arc::clone(&cancel_event);

        let (tx, rx) = channel();

        // Write a large 1 MiB buffer to the pipe without the server draining.
        // The 512-byte pipe buffer will fill up and WriteFile will enter a deterministically pending OVERLAPPED state.
        let write_thread = thread::spawn(move || {
            let big_buf = vec![0x42u8; 1024 * 1024];
            let res = write_all_overlapped(client.raw(), &big_buf, cancel_clone.raw());
            assert!(
                matches!(res, Err(NamedPipeError::OperationCancelled)),
                "Pending write_all must return OperationCancelled, got: {:?}",
                res.err()
            );
            tx.send(()).expect("Signal complete");
        });

        // Allow WriteFile to fill the buffer and enter pending state
        thread::sleep(Duration::from_millis(100));

        // Signal cancellation
        unsafe {
            windows::Win32::System::Threading::SetEvent(cancel_event.raw())
                .expect("SetEvent on cancel_event failed");
        }

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Pending WriteFile cancellation timed out");
        write_thread.join().expect("Write thread joined");
        drop(server_instance);

        println!("PENDING_IO_ESTABLISHED=YES");
        println!("CANCELIOEX_REQUESTED=YES");
        println!("COMPLETION=ERROR_OPERATION_ABORTED");
        println!("RESULT=OperationCancelled");
    }

    /// IPC-50: Physical capacity limit = 4, reject 5th, no eviction
    #[test]
    fn test_physical_pipe_capacity_and_no_eviction() {
        // Part 1: Canonical child DACL rejection
        let child_pipe_name = make_test_pipe_name("child_capacity_deny");
        let child_sid = current_user_sid();
        let inst1 = NamedPipeServerInstance::create(&child_pipe_name, &child_sid, 4)
            .expect("First instance created");

        let inst2_res = NamedPipeServerInstance::create(&child_pipe_name, &child_sid, 4);
        assert!(
            matches!(inst2_res, Err(NamedPipeError::WindowsApi { code: 5, .. })),
            "Second instance creation under child identity must fail with ERROR_ACCESS_DENIED (code 5)"
        );
        drop(inst1);

        // Part 2: SERVER_INSTANCE_CAPACITY_TEST
        let pipe_name = make_test_pipe_name("capacity");
        let max_instances = 4;

        let mut server_instances = Vec::new();
        for _ in 0..max_instances {
            let inst = NamedPipeServerInstance::create_test_instance_with_options(
                &pipe_name,
                None,
                65536,
                max_instances,
                true,
            )
            .expect("Server instance 1..4 should create successfully");
            server_instances.push(inst);
        }

        let fifth_res = NamedPipeServerInstance::create_test_instance_with_options(
            &pipe_name,
            None,
            65536,
            max_instances,
            true,
        );
        assert!(
            matches!(fifth_res, Err(NamedPipeError::CapacityExceeded)),
            "5th server instance must fail closed with CapacityExceeded, got: {:?}",
            fifth_res.err()
        );

        // Part 3: CLIENT_CONNECTION_CAPACITY_TEST
        let mut client_handles = Vec::new();
        for _ in 0..max_instances {
            let client = connect_client(&pipe_name);
            client_handles.push(client);
        }

        let pipe_w = to_wide(&pipe_name);
        let fifth_client = unsafe {
            CreateFileW(
                PCWSTR(pipe_w.as_ptr()),
                CHILD_PIPE_CLIENT_DESIRED_ACCESS_MASK,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        assert!(
            fifth_client.is_err(),
            "5th client connection must fail when all 4 instances are occupied"
        );
        let fifth_err = raw_win32_code_from_error(&fifth_client.err().unwrap());
        assert_eq!(
            fifth_err, 231,
            "5th client must receive ERROR_PIPE_BUSY (231), got {fifth_err}"
        );

        // Part 4: No eviction verification
        for inst in &server_instances {
            assert!(
                !inst.handle().is_invalid(),
                "Original server instance handle must remain valid (no eviction)"
            );
        }
        for client in &client_handles {
            assert!(
                !client.raw().is_invalid(),
                "Original client handle must remain valid (no eviction)"
            );
        }
    }

    /// IPC-64: Static verification that SetNamedPipeHandleState is not used for cancellation
    #[test]
    fn test_static_no_set_named_pipe_handle_state() {
        let windows_src = include_str!("named_pipe_windows.rs");
        let agnostic_src = include_str!("named_pipe.rs");

        // Inspect all production code before `mod tests`
        let prod_windows_src = match windows_src.find("mod tests") {
            Some(idx) => &windows_src[..idx],
            None => windows_src,
        };
        let prod_agnostic_src = match agnostic_src.find("mod tests") {
            Some(idx) => &agnostic_src[..idx],
            None => agnostic_src,
        };

        assert!(
            !prod_windows_src.contains("SetNamedPipeHandleState"),
            "named_pipe_windows.rs production code MUST NOT use SetNamedPipeHandleState"
        );
        assert!(
            !prod_agnostic_src.contains("SetNamedPipeHandleState"),
            "named_pipe.rs MUST NOT use SetNamedPipeHandleState"
        );
    }

    /// Direct prefix validation test
    #[test]
    fn test_initial_request_prefix_bounds_direct() {
        // N = 0 => ZeroLengthInitialRequest
        let p0 = 0u32.to_le_bytes();
        assert!(matches!(
            validate_initial_request_prefix(&p0),
            Err(NamedPipeError::ZeroLengthInitialRequest)
        ));

        // N = 1 => Ok(1)
        let p1 = 1u32.to_le_bytes();
        assert_eq!(validate_initial_request_prefix(&p1).unwrap(), 1);

        // N = 65536 => Ok(65536)
        let p64 = 65536u32.to_le_bytes();
        assert_eq!(validate_initial_request_prefix(&p64).unwrap(), 65536);

        // N = 65537 => FrameTooLarge
        let p_large = 65537u32.to_le_bytes();
        assert!(matches!(
            validate_initial_request_prefix(&p_large),
            Err(NamedPipeError::FrameTooLarge {
                length: 65537,
                max: 65536
            })
        ));
    }

    /// TokenGroups local administrator semantics test
    #[test]
    fn test_token_groups_local_administrator_semantics() {
        // attrs = SE_GROUP_ENABLED => true
        assert!(is_active_administrator_group(SE_GROUP_ENABLED));
        // attrs = SE_GROUP_ENABLED | SE_GROUP_ENABLED_BY_DEFAULT => true
        assert!(is_active_administrator_group(
            SE_GROUP_ENABLED | SE_GROUP_ENABLED_BY_DEFAULT
        ));

        // attrs = SE_GROUP_ENABLED_BY_DEFAULT only => false
        assert!(!is_active_administrator_group(SE_GROUP_ENABLED_BY_DEFAULT));

        // attrs = 0 => false
        assert!(!is_active_administrator_group(0));

        // attrs = SE_GROUP_USE_FOR_DENY_ONLY => false
        assert!(!is_active_administrator_group(SE_GROUP_USE_FOR_DENY_ONLY));

        // attrs = SE_GROUP_ENABLED | SE_GROUP_USE_FOR_DENY_ONLY => false
        assert!(!is_active_administrator_group(
            SE_GROUP_ENABLED | SE_GROUP_USE_FOR_DENY_ONLY
        ));
    }

    /// Fragmented prefix reconstruction and disconnect handling
    #[test]
    fn test_fragmented_prefix_and_partial_disconnect() {
        let pipe_name = make_test_pipe_name("frag_prefix");
        let server = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name, current_user_sid(), 1)
                .expect("Server bind failed"),
        );
        let server_clone = Arc::clone(&server);
        let pipe_name_clone = pipe_name.clone();

        let (tx, rx) = channel();

        let server_thread = thread::spawn(move || {
            let mut conn = server_clone.accept().expect("Accept failed");
            assert_eq!(conn.prebuffer(), &[4, 0, 0, 0]);

            let dummy_ev = create_test_cancel_event();
            let mut body = [0u8; 4];
            conn.read_exact(&mut body, dummy_ev.raw())
                .expect("Read body");
            assert_eq!(&body, b"ping");

            tx.send(()).expect("Signal complete");
        });

        thread::sleep(Duration::from_millis(50));
        let client = connect_client(&pipe_name_clone);
        let cancel_ev = create_test_cancel_event();

        // Write 4-byte prefix in 2 chunks: 1 byte + 3 bytes
        write_all_overlapped(client.raw(), &[4], cancel_ev.raw()).expect("Write byte 1");
        thread::sleep(Duration::from_millis(20));
        write_all_overlapped(client.raw(), &[0, 0, 0], cancel_ev.raw()).expect("Write bytes 2..4");
        // Write body
        write_all_overlapped(client.raw(), b"ping", cancel_ev.raw()).expect("Write ping");

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Server did not reconstruct fragmented prefix in time");
        server_thread.join().expect("Server thread joined");

        // Subtest: Disconnect after partial prefix (2 bytes) fails closed
        let pipe_name_partial = make_test_pipe_name("partial_disconnect");
        let server_p = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name_partial, current_user_sid(), 1)
                .expect("Server bind failed"),
        );
        let server_p_clone = Arc::clone(&server_p);

        let (tx2, rx2) = channel();

        let p_thread = thread::spawn(move || {
            let res = server_p_clone.accept();
            assert!(
                matches!(
                    res,
                    Err(NamedPipeError::PartialPrefixDisconnect {
                        read_bytes: 2,
                        target_bytes: 4
                    })
                ),
                "Accept must fail with PartialPrefixDisconnect, got: {:?}",
                res.err()
            );
            tx2.send(()).expect("Signal complete");
        });

        thread::sleep(Duration::from_millis(50));
        let client_p = connect_client(&pipe_name_partial);
        let cancel_ev2 = create_test_cancel_event();
        write_all_overlapped(client_p.raw(), &[4, 0], cancel_ev2.raw()).expect("Write 2 bytes");
        drop(client_p);

        rx2.recv_timeout(Duration::from_secs(5))
            .expect("Partial prefix disconnect test timed out");
        p_thread.join().expect("Partial thread joined");
    }

    /// IPC-57 & SID tests: valid, canonical, empty, malformed, injection rejection
    #[test]
    fn test_sid_validation_and_rejection() {
        let system_sid = ValidatedSid::parse("S-1-5-18").expect("S-1-5-18 must be valid");
        assert_eq!(system_sid.as_str(), "S-1-5-18");

        let admin_sid = ValidatedSid::parse("S-1-5-32-544").expect("S-1-5-32-544 must be valid");
        assert_eq!(admin_sid.as_str(), "S-1-5-32-544");

        let cur_sid = current_user_sid();
        let parsed_again =
            ValidatedSid::parse(cur_sid.as_str()).expect("Current user SID must round-trip");
        assert_eq!(cur_sid, parsed_again);

        // Fail-closed cases:
        assert!(matches!(
            ValidatedSid::parse(""),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
        assert!(matches!(
            ValidatedSid::parse("   "),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
        assert!(matches!(
            ValidatedSid::parse("invalid"),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
        assert!(matches!(
            ValidatedSid::parse("S-1-"),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
        assert!(matches!(
            ValidatedSid::parse("S-1-xyz"),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
        assert!(matches!(
            ValidatedSid::parse("S-1-999999999999999999999999999999999999999999"),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
        assert!(matches!(
            ValidatedSid::parse("S-1-5-18)(A;;GA;;;WD"),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
        assert!(matches!(
            ValidatedSid::parse("S-1-5-18;calc.exe"),
            Err(NamedPipeError::InvalidChildSid(_))
        ));
    }

    /// Item 9: Generic body disconnect maps to NamedPipeError::Disconnected,
    /// while initial prefix disconnect maps to NamedPipeError::PartialPrefixDisconnect.
    #[test]
    fn test_body_disconnect_returns_generic_disconnected() {
        let pipe_name = make_test_pipe_name("body_disconnect");
        let server = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name, current_user_sid(), 1)
                .expect("Server bind failed"),
        );
        let server_clone = Arc::clone(&server);
        let pipe_name_clone = pipe_name.clone();

        let (tx, rx) = channel();

        let server_thread = thread::spawn(move || {
            let mut conn = server_clone.accept().expect("Accept handshake succeeded");
            assert_eq!(conn.prebuffer(), &[4, 0, 0, 0]);

            let cancel_ev = create_test_cancel_event();
            let mut body = [0u8; 4];
            // Client will disconnect without sending body
            let res = conn.read_exact(&mut body, cancel_ev.raw());
            assert!(
                matches!(res, Err(NamedPipeError::Disconnected)),
                "Body read disconnect must return Disconnected, got: {:?}",
                res.err()
            );

            tx.send(()).expect("Signal complete");
        });

        thread::sleep(Duration::from_millis(50));
        let client = connect_client(&pipe_name_clone);
        let cancel_ev = create_test_cancel_event();

        // Send valid 4-byte prefix so accept() completes
        write_all_overlapped(client.raw(), &[4, 0, 0, 0], cancel_ev.raw()).expect("Write prefix");

        // Wait for server to finish accept() and enter body read
        thread::sleep(Duration::from_millis(100));

        // Client closes connection without sending body
        drop(client);

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Body disconnect test timed out");
        server_thread.join().expect("Server thread joined");
    }

    /// Item 10: Zero-byte successful write decision logic fails closed and does not spin.
    #[test]
    fn test_zero_byte_write_progress_guarantee() {
        let pipe_name = make_test_pipe_name("zero_write");
        let server_instance = NamedPipeServerInstance::create(&pipe_name, &current_user_sid(), 1)
            .expect("Server instance creation failed");

        let client = connect_client(&pipe_name);
        drop(server_instance); // Close server end immediately

        let cancel_ev = create_test_cancel_event();
        let buf = [0x42u8; 128];
        let res = write_all_overlapped(client.raw(), &buf, cancel_ev.raw());
        assert!(
            matches!(res, Err(NamedPipeError::Disconnected)),
            "Write to closed pipe must fail closed with Disconnected, got: {:?}",
            res.err()
        );
    }
    /// Correction 3: Aligned native buffer verification for TOKEN_USER and TOKEN_GROUPS
    #[test]
    fn test_aligned_native_buffer_token_user_and_groups() {
        use std::mem::{align_of, size_of};

        // Alignment of TOKEN_USER
        let user_align = align_of::<TOKEN_USER>();
        let user_size = size_of::<TOKEN_USER>() + 64;
        let user_buf = AlignedNativeBuffer::new(user_size, user_align)
            .expect("Allocate aligned TOKEN_USER buffer");
        assert_eq!(user_buf.size(), user_size, "Size preservation");
        assert!(
            user_buf.is_aligned_for::<TOKEN_USER>(),
            "Buffer pointer must satisfy align_of::<TOKEN_USER>() ({user_align})"
        );
        assert_eq!(
            (user_buf.as_ptr() as usize) % user_align,
            0,
            "Pointer modulo alignment must be 0"
        );

        // Alignment of TOKEN_GROUPS
        let groups_align = align_of::<TOKEN_GROUPS>();
        let groups_size = size_of::<TOKEN_GROUPS>() + 256;
        let groups_buf = AlignedNativeBuffer::new(groups_size, groups_align)
            .expect("Allocate aligned TOKEN_GROUPS buffer");
        assert_eq!(groups_buf.size(), groups_size, "Size preservation");
        assert!(
            groups_buf.is_aligned_for::<TOKEN_GROUPS>(),
            "Buffer pointer must satisfy align_of::<TOKEN_GROUPS>() ({groups_align})"
        );
        assert_eq!(
            (groups_buf.as_ptr() as usize) % groups_align,
            0,
            "Pointer modulo alignment must be 0"
        );

        // Zero-size allocation fails closed
        let zero_res = AlignedNativeBuffer::new(0, 8);
        assert!(
            zero_res.is_err(),
            "Zero-sized native buffer allocation must fail closed"
        );

        // RAII lifetime test: multiple allocations and drops
        for _ in 0..100 {
            let b = AlignedNativeBuffer::new(1024, 64).expect("Allocation in loop");
            assert_eq!(b.size(), 1024);
            assert_eq!((b.as_ptr() as usize) % 64, 0);
            drop(b);
        }
    }

    /// Correction 3: Win32 error code normalization test
    #[test]
    fn test_raw_win32_code_normalization() {
        // Direct Win32 codes
        assert_eq!(raw_win32_code_from_hresult(5), 5); // ERROR_ACCESS_DENIED
        assert_eq!(raw_win32_code_from_hresult(122), 122); // ERROR_INSUFFICIENT_BUFFER
        assert_eq!(raw_win32_code_from_hresult(995), 995); // ERROR_OPERATION_ABORTED
        assert_eq!(raw_win32_code_from_hresult(1168), 1168); // ERROR_NOT_FOUND
        assert_eq!(raw_win32_code_from_hresult(231), 231); // ERROR_PIPE_BUSY

        // HRESULT_FROM_WIN32 mappings (0x8007XXXX -> XXXX)
        assert_eq!(raw_win32_code_from_hresult(0x8007_0005), 5);
        assert_eq!(raw_win32_code_from_hresult(0x8007_007A), 122);
        assert_eq!(raw_win32_code_from_hresult(0x8007_03E3), 995);
        assert_eq!(raw_win32_code_from_hresult(0x8007_0490), 1168);
        assert_eq!(raw_win32_code_from_hresult(0x8007_00E7), 231);

        // Non-HRESULT_FROM_WIN32 codes preserved unchanged
        assert_eq!(raw_win32_code_from_hresult(0x8000_4005), 0x8000_4005); // E_FAIL
    }

    /// Correction 3: Static production-surface audit
    #[test]
    fn test_static_production_surface_audit() {
        let windows_src = include_str!("named_pipe_windows.rs");
        let agnostic_src = include_str!("named_pipe.rs");
        let lib_src = include_str!("lib.rs");

        // Inspect all production code before `mod tests`
        let prod_windows_src = match windows_src.find("mod tests") {
            Some(idx) => &windows_src[..idx],
            None => windows_src,
        };
        let prod_agnostic_src = match agnostic_src.find("mod tests") {
            Some(idx) => &agnostic_src[..idx],
            None => agnostic_src,
        };

        // 1. No Option<&AutoSecurityDescriptor> in production creation path
        assert!(
            !prod_windows_src.contains("Option<&AutoSecurityDescriptor>"),
            "named_pipe_windows.rs production code MUST NOT contain Option<&AutoSecurityDescriptor>"
        );

        // 2. No reject_remote: bool parameter in production creation path
        assert!(
            !prod_windows_src.contains("reject_remote: bool"),
            "named_pipe_windows.rs production code MUST NOT contain reject_remote: bool"
        );

        // 3. No PIPE_ACCEPT_REMOTE_CLIENTS in production code
        assert!(
            !prod_windows_src.contains("PIPE_ACCEPT_REMOTE_CLIENTS"),
            "named_pipe_windows.rs production code MUST NOT reference PIPE_ACCEPT_REMOTE_CLIENTS"
        );

        // 4. No Vec<u8> typed token dereferences in production code
        assert!(
            !prod_windows_src.contains("vec![0u8;"),
            "named_pipe_windows.rs production code MUST NOT allocate token buffers as Vec<u8>"
        );

        // 5. named_pipe_windows remains private in lib.rs
        assert!(
            lib_src.contains("#[cfg(windows)]\nmod named_pipe_windows;")
                || lib_src.contains("#[cfg(windows)]\r\nmod named_pipe_windows;")
                || lib_src.contains("mod named_pipe_windows;"),
            "lib.rs must declare named_pipe_windows as a private module"
        );
        assert!(
            !lib_src.contains("pub mod named_pipe_windows;"),
            "lib.rs MUST NOT declare named_pipe_windows as pub mod"
        );

        // 6. IPC-64: No SetNamedPipeHandleState
        assert!(
            !prod_windows_src.contains("SetNamedPipeHandleState"),
            "named_pipe_windows.rs production code MUST NOT use SetNamedPipeHandleState"
        );
        assert!(
            !prod_agnostic_src.contains("SetNamedPipeHandleState"),
            "named_pipe.rs MUST NOT use SetNamedPipeHandleState"
        );
    }

    /// Correction 4: Static audit verifying asynchronous ReadFile and WriteFile never supply
    /// non-NULL lpNumberOfBytesRead / lpNumberOfBytesWritten pointers and always obtain transfer count
    /// via GetOverlappedResult.
    #[test]
    fn test_static_overlapped_byte_count_audit() {
        let windows_src = include_str!("named_pipe_windows.rs");

        // Inspect all production code before `mod tests`
        let prod_windows_src = match windows_src.find("mod tests") {
            Some(idx) => &windows_src[..idx],
            None => windows_src,
        };

        // 1. No ReadFile passing Some(&mut bytes_read)
        assert!(
            !prod_windows_src.contains("Some(&mut bytes_read)"),
            "Production code MUST NOT pass Some(&mut bytes_read) to ReadFile"
        );

        // 2. No WriteFile passing Some(&mut bytes_written)
        assert!(
            !prod_windows_src.contains("Some(&mut bytes_written)"),
            "Production code MUST NOT pass Some(&mut bytes_written) to WriteFile"
        );

        // 3. Verify ReadFile call passes None for lpNumberOfBytesRead
        assert!(
            prod_windows_src.contains("ReadFile(pipe, Some(slice), None, Some(&mut ov))"),
            "Production ReadFile call MUST pass None for lpNumberOfBytesRead"
        );

        // 4. Verify WriteFile call passes None for lpNumberOfBytesWritten
        assert!(
            prod_windows_src.contains("WriteFile(pipe, Some(slice), None, Some(&mut ov))"),
            "Production WriteFile call MUST pass None for lpNumberOfBytesWritten"
        );

        // 5. Verify immediate success branches use completed_overlapped_transfer
        assert!(
            prod_windows_src.contains("completed_overlapped_transfer(pipe, &ov)?"),
            "Immediate success branches MUST call completed_overlapped_transfer"
        );
    }

    /// Correction 4: Physical integration test for preloaded read and immediate write data paths
    #[test]
    fn test_physical_immediate_completion_and_preload_transfer() {
        let pipe_name = make_test_pipe_name("immediate_io");
        let server = Arc::new(
            NamedPipeServer::bind_custom(&pipe_name, current_user_sid(), 1)
                .expect("Server bind failed"),
        );
        let server_clone = Arc::clone(&server);
        let pipe_name_clone = pipe_name.clone();

        let (tx, rx) = channel();

        let server_thread = thread::spawn(move || {
            let mut conn = server_clone.accept().expect("Accept handshake succeeded");

            // Client preloaded 32 bytes before server starts reading body:
            let cancel_ev = create_test_cancel_event();
            let mut body = [0u8; 32];
            conn.read_exact(&mut body, cancel_ev.raw())
                .expect("Read preloaded body");

            let expected_body: Vec<u8> = (0..32).map(|i| (i * 3 + 7) as u8).collect();
            assert_eq!(&body[..], &expected_body[..], "Preloaded body byte match");

            // Immediate write: server writes 32 bytes back into empty pipe buffer
            let resp: Vec<u8> = (0..32).map(|i| (0xFF - i) as u8).collect();
            conn.write_all(&resp, cancel_ev.raw())
                .expect("Write response");

            tx.send(()).expect("Signal complete");
        });

        thread::sleep(Duration::from_millis(50));
        let client = connect_client(&pipe_name_clone);
        let cancel_ev = create_test_cancel_event();

        // 1. Send valid prefix (4 bytes) + preload 32 body bytes in a single buffer
        let mut initial_data = Vec::new();
        initial_data.extend_from_slice(&(32u32.to_le_bytes()));
        let payload: Vec<u8> = (0..32).map(|i| (i * 3 + 7) as u8).collect();
        initial_data.extend_from_slice(&payload);

        write_all_overlapped(client.raw(), &initial_data, cancel_ev.raw()).expect("Preload write");

        // 2. Client reads response from server
        let mut client_resp = [0u8; 32];
        read_exact_overlapped(client.raw(), &mut client_resp, cancel_ev.raw())
            .expect("Client read response");

        let expected_resp: Vec<u8> = (0..32).map(|i| (0xFF - i) as u8).collect();
        assert_eq!(
            &client_resp[..],
            &expected_resp[..],
            "Client response byte match"
        );

        rx.recv_timeout(Duration::from_secs(5))
            .expect("Test completed");
        server_thread.join().expect("Server thread joined");
    }
}
