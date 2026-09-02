//! Protected persistent directory DACL primitive for Windows.

use std::fmt;
use std::path::{Path, PathBuf};

/// Error returned when ensuring a protected directory fails.
#[derive(Debug)]
pub enum ProtectedDirectoryError {
    /// The specified path is invalid.
    InvalidPath(String),
    /// The parent directory does not exist.
    ParentMissing(PathBuf),
    /// The target exists but is not a directory.
    TargetExistsAndNotDirectory(PathBuf),
    /// A Windows API call failed.
    WindowsApi {
        function: &'static str,
        code: u32,
        message: String,
    },
    /// An I/O error occurred.
    Io(std::io::Error),
}

impl fmt::Display for ProtectedDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(msg) => write!(f, "Invalid path: {msg}"),
            Self::ParentMissing(path) => {
                write!(f, "Parent directory does not exist: {}", path.display())
            }
            Self::TargetExistsAndNotDirectory(path) => {
                write!(
                    f,
                    "Target exists but is not a directory: {}",
                    path.display()
                )
            }
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
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for ProtectedDirectoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ProtectedDirectoryError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// SDDL representation for protected directory DACL:
/// - `D:P`: DACL with inheritance protection (SE_DACL_PROTECTED)
/// - `(A;OICI;FA;;;SY)`: Allow SYSTEM Full Control, Object Inherit + Container Inherit
/// - `(A;OICI;FA;;;BA)`: Allow Builtin Administrators Full Control, Object Inherit + Container Inherit
pub const PROTECTED_DIRECTORY_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

#[cfg(windows)]
struct AutoSecurityDescriptor(windows::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl Drop for AutoSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(Some(
                    windows::Win32::Foundation::HLOCAL(self.0.0),
                ));
            }
        }
    }
}

/// Ensures that the specified directory exists and has a protected DACL.
///
/// Security contract:
/// - Inheritance from parent disabled/protected (D:P).
/// - SYSTEM: Full Control (OICI).
/// - Built-in Administrators: Full Control (OICI).
/// - Inherited to new child files and directories.
/// - Broad Allow ACEs (Users, Authenticated Users, Everyone) removed.
/// - No explicit Deny ACEs.
#[cfg(windows)]
pub fn ensure_protected_directory(path: &Path) -> Result<(), ProtectedDirectoryError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{
        CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    };
    use windows::core::PCWSTR;

    let path_os = path.as_os_str();
    if path_os.is_empty() {
        return Err(ProtectedDirectoryError::InvalidPath(
            "path is empty".to_string(),
        ));
    }

    let mut path_wide: Vec<u16> = path_os.encode_wide().collect();
    path_wide.push(0);
    let path_pcwstr = PCWSTR(path_wide.as_ptr());

    // Check if target already exists using Win32 API
    let attrs = unsafe { GetFileAttributesW(path_pcwstr) };

    if attrs != INVALID_FILE_ATTRIBUTES {
        // Target exists
        if (attrs & FILE_ATTRIBUTE_DIRECTORY.0) == 0 {
            return Err(ProtectedDirectoryError::TargetExistsAndNotDirectory(
                path.to_path_buf(),
            ));
        }

        // Target exists and is a directory -> apply protected DACL
        apply_protected_dacl_to_existing_directory(path_pcwstr)?;
        return Ok(());
    }

    // Target does not exist. Verify parent directory exists.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let mut parent_wide: Vec<u16> = parent.as_os_str().encode_wide().collect();
            parent_wide.push(0);
            let parent_attrs = unsafe { GetFileAttributesW(PCWSTR(parent_wide.as_ptr())) };
            if parent_attrs == INVALID_FILE_ATTRIBUTES
                || (parent_attrs & FILE_ATTRIBUTE_DIRECTORY.0) == 0
            {
                return Err(ProtectedDirectoryError::ParentMissing(parent.to_path_buf()));
            }
        }
    }

    // Convert SDDL string to security descriptor
    let mut sddl_wide: Vec<u16> = PROTECTED_DIRECTORY_SDDL.encode_utf16().collect();
    sddl_wide.push(0);

    let mut raw_sd = PSECURITY_DESCRIPTOR::default();
    let res = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut raw_sd,
            None,
        )
    };

    if let Err(err) = res {
        return Err(ProtectedDirectoryError::WindowsApi {
            function: "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            code: err.code().0 as u32,
            message: err.message(),
        });
    }

    let _sd_guard = AutoSecurityDescriptor(raw_sd);

    let sec_attr = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: raw_sd.0,
        bInheritHandle: windows::core::BOOL(0),
    };

    let created = unsafe { CreateDirectoryW(path_pcwstr, Some(&sec_attr)) };

    if let Err(err) = created {
        let code = err.code().0 as u32;
        if code == ERROR_ALREADY_EXISTS.0 {
            // Race condition: target was created concurrently.
            let cur_attrs = unsafe { GetFileAttributesW(path_pcwstr) };
            if cur_attrs != INVALID_FILE_ATTRIBUTES && (cur_attrs & FILE_ATTRIBUTE_DIRECTORY.0) != 0
            {
                apply_protected_dacl_to_existing_directory(path_pcwstr)?;
                return Ok(());
            } else {
                return Err(ProtectedDirectoryError::TargetExistsAndNotDirectory(
                    path.to_path_buf(),
                ));
            }
        }

        if code == ERROR_PATH_NOT_FOUND.0 || code == ERROR_FILE_NOT_FOUND.0 {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    return Err(ProtectedDirectoryError::ParentMissing(parent.to_path_buf()));
                }
            }
        }

        return Err(ProtectedDirectoryError::WindowsApi {
            function: "CreateDirectoryW",
            code,
            message: err.message(),
        });
    }

    Ok(())
}

#[cfg(windows)]
fn apply_protected_dacl_to_existing_directory(
    path_pcwstr: windows::core::PCWSTR,
) -> Result<(), ProtectedDirectoryError> {
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };
    use windows::core::PCWSTR;

    let mut sddl_wide: Vec<u16> = PROTECTED_DIRECTORY_SDDL.encode_utf16().collect();
    sddl_wide.push(0);

    let mut raw_sd = PSECURITY_DESCRIPTOR::default();
    let res = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut raw_sd,
            None,
        )
    };

    if let Err(err) = res {
        return Err(ProtectedDirectoryError::WindowsApi {
            function: "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            code: err.code().0 as u32,
            message: err.message(),
        });
    }

    let _sd_guard = AutoSecurityDescriptor(raw_sd);

    let mut dacl_present = windows::core::BOOL(0);
    let mut p_dacl = std::ptr::null_mut();
    let mut dacl_defaulted = windows::core::BOOL(0);

    let get_dacl_res = unsafe {
        GetSecurityDescriptorDacl(raw_sd, &mut dacl_present, &mut p_dacl, &mut dacl_defaulted)
    };

    if let Err(err) = get_dacl_res {
        return Err(ProtectedDirectoryError::WindowsApi {
            function: "GetSecurityDescriptorDacl",
            code: err.code().0 as u32,
            message: err.message(),
        });
    }

    let sec_info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    let set_res = unsafe {
        SetNamedSecurityInfoW(
            path_pcwstr,
            SE_FILE_OBJECT,
            sec_info,
            None,
            None,
            if dacl_present.as_bool() && !p_dacl.is_null() {
                Some(p_dacl)
            } else {
                None
            },
            None,
        )
    };

    if set_res != WIN32_ERROR(0) {
        let err = windows::core::Error::from(set_res);
        return Err(ProtectedDirectoryError::WindowsApi {
            function: "SetNamedSecurityInfoW",
            code: set_res.0,
            message: err.message(),
        });
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn ensure_protected_directory(path: &Path) -> Result<(), ProtectedDirectoryError> {
    if path.as_os_str().is_empty() {
        return Err(ProtectedDirectoryError::InvalidPath(
            "path is empty".to_string(),
        ));
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(ProtectedDirectoryError::ParentMissing(parent.to_path_buf()));
        }
    }

    if path.exists() {
        if !path.is_dir() {
            return Err(ProtectedDirectoryError::TargetExistsAndNotDirectory(
                path.to_path_buf(),
            ));
        }
        return Ok(());
    }

    std::fs::create_dir(path).map_err(ProtectedDirectoryError::Io)
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows::Win32::Foundation::{HLOCAL, LocalFree, WIN32_ERROR};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION,
        AclSizeInformation, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetAce,
        GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, INHERITED_ACE,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows::core::{PCWSTR, PWSTR};

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const FILE_ALL_ACCESS: u32 = 0x001F01FF;

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempTestDir {
        path: PathBuf,
    }

    impl TempTestDir {
        fn new(name_prefix: &str) -> Self {
            let count = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            let pid = std::process::id();
            let path =
                std::env::temp_dir().join(format!("palka_pdacl_{name_prefix}_{pid}_{count}"));
            let _ = fs::remove_dir_all(&path);
            let _ = fs::remove_file(&path);
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
            let _ = fs::remove_file(&self.path);
        }
    }

    struct SecurityDescriptorHolder(PSECURITY_DESCRIPTOR);

    impl Drop for SecurityDescriptorHolder {
        fn drop(&mut self) {
            if !self.0.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0.0)));
                }
            }
        }
    }

    struct LocalAllocString(PWSTR);

    impl Drop for LocalAllocString {
        fn drop(&mut self) {
            if !self.0.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0.0 as _)));
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    struct ParsedAce {
        ace_type: u8,
        ace_flags: u8,
        mask: u32,
        sid: String,
    }

    #[derive(Debug)]
    struct DirectorySecurityInfo {
        is_protected: bool,
        sddl: String,
        aces: Vec<ParsedAce>,
    }

    fn inspect_path_security(path: &Path) -> DirectorySecurityInfo {
        let path_os = path.as_os_str();
        let mut path_wide: Vec<u16> = path_os.encode_wide().collect();
        path_wide.push(0);

        let mut p_sd = PSECURITY_DESCRIPTOR::default();
        let mut p_dacl: *mut ACL = std::ptr::null_mut();

        let sec_info = DACL_SECURITY_INFORMATION
            | PROTECTED_DACL_SECURITY_INFORMATION
            | UNPROTECTED_DACL_SECURITY_INFORMATION;

        let res = unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(path_wide.as_ptr()),
                SE_FILE_OBJECT,
                sec_info,
                None,
                None,
                Some(&mut p_dacl),
                None,
                &mut p_sd,
            )
        };

        assert_eq!(
            res,
            WIN32_ERROR(0),
            "GetNamedSecurityInfoW failed with {}",
            res.0
        );

        let _sd_guard = SecurityDescriptorHolder(p_sd);

        // Check control flags
        let mut control = 0u16;
        let mut revision = 0u32;
        let ctl_ok = unsafe { GetSecurityDescriptorControl(p_sd, &mut control, &mut revision) };
        assert!(
            ctl_ok.is_ok(),
            "GetSecurityDescriptorControl failed: {:?}",
            ctl_ok
        );

        let is_protected = (control & (SE_DACL_PROTECTED.0 as u16)) != 0;

        // Get SDDL string
        let mut sddl_ptr = PWSTR::null();
        let mut sddl_len = 0u32;
        let sddl_ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                p_sd,
                SDDL_REVISION_1,
                sec_info,
                &mut sddl_ptr,
                Some(&mut sddl_len),
            )
        };
        assert!(
            sddl_ok.is_ok(),
            "ConvertSecurityDescriptorToStringSecurityDescriptorW failed"
        );
        let _sddl_guard = LocalAllocString(sddl_ptr);
        let sddl = unsafe { sddl_ptr.to_string().unwrap_or_default() };

        // Parse ACEs from DACL
        let mut aces = Vec::new();
        if !p_dacl.is_null() {
            let mut acl_size_info = ACL_SIZE_INFORMATION::default();
            let info_ok = unsafe {
                GetAclInformation(
                    p_dacl,
                    &mut acl_size_info as *mut _ as *mut _,
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            };
            assert!(info_ok.is_ok(), "GetAclInformation failed");

            for i in 0..acl_size_info.AceCount {
                let mut p_ace: *mut std::ffi::c_void = std::ptr::null_mut();
                let ace_ok = unsafe { GetAce(p_dacl, i, &mut p_ace) };
                assert!(ace_ok.is_ok(), "GetAce failed for index {i}");

                let header = unsafe { *(p_ace as *const ACE_HEADER) };
                let ace_type = header.AceType;
                let ace_flags = header.AceFlags;

                let (mask, sid_ptr) = if ace_type == ACCESS_ALLOWED_ACE_TYPE {
                    let allowed = unsafe { &*(p_ace as *const ACCESS_ALLOWED_ACE) };
                    (
                        allowed.Mask,
                        &allowed.SidStart as *const u32 as *const std::ffi::c_void,
                    )
                } else if ace_type == ACCESS_DENIED_ACE_TYPE {
                    let denied = unsafe { &*(p_ace as *const ACCESS_DENIED_ACE) };
                    (
                        denied.Mask,
                        &denied.SidStart as *const u32 as *const std::ffi::c_void,
                    )
                } else {
                    (0, std::ptr::null())
                };

                let sid = if !sid_ptr.is_null() {
                    let mut str_sid = PWSTR::null();
                    let sid_ok =
                        unsafe { ConvertSidToStringSidW(PSID(sid_ptr as _), &mut str_sid) };
                    if sid_ok.is_ok() {
                        let _str_guard = LocalAllocString(str_sid);
                        unsafe { str_sid.to_string().unwrap_or_default() }
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                aces.push(ParsedAce {
                    ace_type,
                    ace_flags,
                    mask,
                    sid,
                });
            }
        }

        DirectorySecurityInfo {
            is_protected,
            sddl,
            aces,
        }
    }

    fn get_path_owner_sid(path: &Path) -> String {
        let path_os = path.as_os_str();
        let mut path_wide: Vec<u16> = path_os.encode_wide().collect();
        path_wide.push(0);

        let mut p_sd = PSECURITY_DESCRIPTOR::default();
        let mut p_owner = PSID::default();

        let res = unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(path_wide.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&mut p_owner),
                None,
                None,
                None,
                &mut p_sd,
            )
        };

        assert_eq!(
            res,
            WIN32_ERROR(0),
            "GetNamedSecurityInfoW for owner failed"
        );
        let _sd_guard = SecurityDescriptorHolder(p_sd);

        let mut str_sid = PWSTR::null();
        let sid_ok = unsafe { ConvertSidToStringSidW(p_owner, &mut str_sid) };
        assert!(sid_ok.is_ok(), "ConvertSidToStringSidW for owner failed");
        let _str_guard = LocalAllocString(str_sid);
        unsafe { str_sid.to_string().unwrap_or_default() }
    }

    /// Helper to temporarily add a non-inheritable Allow ACE for the owner on `target`
    /// so standard (non-elevated) test processes can create child items for inheritance verification.
    /// Because flags are 0 (no OI/CI), the added ACE is NOT inherited by child files/directories.
    fn grant_non_inheritable_owner_access(path: &Path) {
        let owner_sid = get_path_owner_sid(path);
        // D:P with SY (OI|CI), BA (OI|CI), and owner without OI/CI flags (direct container-only)
        let sddl = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;FA;;;{owner_sid})");
        let mut sddl_wide: Vec<u16> = sddl.encode_utf16().collect();
        sddl_wide.push(0);

        let mut raw_sd = PSECURITY_DESCRIPTOR::default();
        let res = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl_wide.as_ptr()),
                SDDL_REVISION_1,
                &mut raw_sd,
                None,
            )
        };
        assert!(res.is_ok(), "ConvertStringSecurityDescriptor failed");
        let _sd_guard = AutoSecurityDescriptor(raw_sd);

        let mut dacl_present = windows::core::BOOL(0);
        let mut p_dacl = std::ptr::null_mut();
        let mut dacl_defaulted = windows::core::BOOL(0);
        let get_dacl_res = unsafe {
            GetSecurityDescriptorDacl(raw_sd, &mut dacl_present, &mut p_dacl, &mut dacl_defaulted)
        };
        assert!(get_dacl_res.is_ok());

        let path_os = path.as_os_str();
        let mut path_wide: Vec<u16> = path_os.encode_wide().collect();
        path_wide.push(0);

        let sec_info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        let set_res = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(path_wide.as_ptr()),
                SE_FILE_OBJECT,
                sec_info,
                None,
                None,
                Some(p_dacl),
                None,
            )
        };
        assert_eq!(set_res, WIN32_ERROR(0));
    }

    const SID_SYSTEM: &str = "S-1-5-18";
    const SID_BUILTIN_ADMINISTRATORS: &str = "S-1-5-32-544";
    const SID_EVERYONE: &str = "S-1-1-0";
    const SID_AUTHENTICATED_USERS: &str = "S-1-5-11";
    const SID_BUILTIN_USERS: &str = "S-1-5-32-545";

    #[test]
    fn test_missing_directory_created_securely() {
        let temp = TempTestDir::new("missing_dir");
        let target = temp.path().to_path_buf();
        assert!(!target.exists());

        let res = ensure_protected_directory(&target);
        assert!(res.is_ok(), "ensure_protected_directory failed: {res:?}");
        assert!(target.is_dir());

        let sec_info = inspect_path_security(&target);
        assert!(
            sec_info.is_protected,
            "Created directory DACL is not protected"
        );
        assert_eq!(sec_info.aces.len(), 2, "Expected exactly 2 ACEs");
    }

    #[test]
    fn test_existing_directory_is_hardened() {
        let temp = TempTestDir::new("existing_dir");
        let target = temp.path().to_path_buf();
        fs::create_dir(&target).expect("failed to create existing directory");

        let res = ensure_protected_directory(&target);
        assert!(res.is_ok(), "ensure_protected_directory failed: {res:?}");

        let sec_info = inspect_path_security(&target);
        assert!(
            sec_info.is_protected,
            "Existing hardened directory DACL is not protected"
        );
        assert_eq!(sec_info.aces.len(), 2, "Expected exactly 2 ACEs");
    }

    #[test]
    fn test_operation_is_idempotent() {
        let temp = TempTestDir::new("idempotent");
        let target = temp.path().to_path_buf();

        // 1st call: creates missing directory
        ensure_protected_directory(&target).expect("first ensure failed");
        let info1 = inspect_path_security(&target);
        assert!(info1.is_protected);
        assert_eq!(info1.aces.len(), 2);

        // 2nd call: hardens existing directory
        ensure_protected_directory(&target).expect("second ensure failed");
        let info2 = inspect_path_security(&target);
        assert!(info2.is_protected);
        assert_eq!(info2.aces.len(), 2);

        // 3rd call: redundant call on hardened directory
        ensure_protected_directory(&target).expect("third ensure failed");
        let info3 = inspect_path_security(&target);
        assert!(info3.is_protected);
        assert_eq!(info3.aces.len(), 2);

        // Semantics between 2nd and 3rd runs must match identically
        assert_eq!(info2.sddl, info3.sddl);
        assert_eq!(info2.aces.len(), info3.aces.len());
        for i in 0..info2.aces.len() {
            assert_eq!(info2.aces[i].sid, info3.aces[i].sid);
            assert_eq!(info2.aces[i].mask, info3.aces[i].mask);
            assert_eq!(info2.aces[i].ace_type, info3.aces[i].ace_type);
            assert_eq!(info2.aces[i].ace_flags, info3.aces[i].ace_flags);
        }
    }

    #[test]
    fn test_target_file_is_rejected_and_unchanged() {
        let temp = TempTestDir::new("target_file");
        let target = temp.path().to_path_buf();
        fs::write(&target, b"important payload").expect("failed to write file");

        let res = ensure_protected_directory(&target);
        match res {
            Err(ProtectedDirectoryError::TargetExistsAndNotDirectory(p)) => {
                assert_eq!(p, target);
            }
            other => panic!("Expected TargetExistsAndNotDirectory, got {other:?}"),
        }

        let content = fs::read(&target).expect("failed to read file");
        assert_eq!(content, b"important payload");
    }

    #[test]
    fn test_missing_parent_is_rejected_without_recursive_creation() {
        let temp = TempTestDir::new("missing_parent");
        let non_existent_parent = temp.path().join("sub_parent");
        let target = non_existent_parent.join("target");

        let res = ensure_protected_directory(&target);
        match res {
            Err(ProtectedDirectoryError::ParentMissing(p)) => {
                assert_eq!(p, non_existent_parent);
            }
            other => panic!("Expected ParentMissing, got {other:?}"),
        }

        assert!(!non_existent_parent.exists());
        assert!(!target.exists());
    }

    #[test]
    fn test_dacl_is_protected() {
        let temp = TempTestDir::new("dacl_prot");
        let target = temp.path().to_path_buf();
        ensure_protected_directory(&target).unwrap();

        let sec_info = inspect_path_security(&target);
        assert!(sec_info.is_protected);
        assert!(sec_info.sddl.contains("D:P"));
    }

    #[test]
    fn test_system_has_full_control() {
        let temp = TempTestDir::new("sys_fc");
        let target = temp.path().to_path_buf();
        ensure_protected_directory(&target).unwrap();

        let sec_info = inspect_path_security(&target);
        let sys_ace = sec_info
            .aces
            .iter()
            .find(|a| a.sid.eq_ignore_ascii_case(SID_SYSTEM))
            .expect("SYSTEM ACE not found in DACL");

        assert_eq!(
            sys_ace.ace_type, ACCESS_ALLOWED_ACE_TYPE,
            "SYSTEM ACE is not Allow"
        );
        // Full control matches FILE_ALL_ACCESS (0x001F01FF) or GENERIC_ALL
        assert_eq!(
            sys_ace.mask & FILE_ALL_ACCESS,
            FILE_ALL_ACCESS,
            "SYSTEM does not have Full Control: mask=0x{:08X}",
            sys_ace.mask
        );
        let expected_flags = (OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0) as u8;
        assert_eq!(
            sys_ace.ace_flags & expected_flags,
            expected_flags,
            "SYSTEM ACE missing OI/CI flags"
        );
    }

    #[test]
    fn test_administrators_have_full_control() {
        let temp = TempTestDir::new("admin_fc");
        let target = temp.path().to_path_buf();
        ensure_protected_directory(&target).unwrap();

        let sec_info = inspect_path_security(&target);
        let admin_ace = sec_info
            .aces
            .iter()
            .find(|a| a.sid.eq_ignore_ascii_case(SID_BUILTIN_ADMINISTRATORS))
            .expect("Administrators ACE not found in DACL");

        assert_eq!(
            admin_ace.ace_type, ACCESS_ALLOWED_ACE_TYPE,
            "Administrators ACE is not Allow"
        );
        assert_eq!(
            admin_ace.mask & FILE_ALL_ACCESS,
            FILE_ALL_ACCESS,
            "Administrators do not have Full Control: mask=0x{:08X}",
            admin_ace.mask
        );
        let expected_flags = (OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0) as u8;
        assert_eq!(
            admin_ace.ace_flags & expected_flags,
            expected_flags,
            "Administrators ACE missing OI/CI flags"
        );
    }

    #[test]
    fn test_no_broad_allow_for_everyone_users_auth_users() {
        let temp = TempTestDir::new("no_broad_allow");
        let target = temp.path().to_path_buf();

        // Create directory with default (broad) permissions first, then harden
        fs::create_dir(&target).unwrap();
        ensure_protected_directory(&target).unwrap();

        let sec_info = inspect_path_security(&target);

        for ace in &sec_info.aces {
            if ace.ace_type == ACCESS_ALLOWED_ACE_TYPE {
                assert!(
                    !ace.sid.eq_ignore_ascii_case(SID_EVERYONE),
                    "Found Allow ACE for Everyone ({SID_EVERYONE})"
                );
                assert!(
                    !ace.sid.eq_ignore_ascii_case(SID_BUILTIN_USERS),
                    "Found Allow ACE for BUILTIN\\Users ({SID_BUILTIN_USERS})"
                );
                assert!(
                    !ace.sid.eq_ignore_ascii_case(SID_AUTHENTICATED_USERS),
                    "Found Allow ACE for Authenticated Users ({SID_AUTHENTICATED_USERS})"
                );
            }
        }
    }

    #[test]
    fn test_no_explicit_deny_aces() {
        let temp = TempTestDir::new("no_deny");
        let target = temp.path().to_path_buf();
        ensure_protected_directory(&target).unwrap();

        let sec_info = inspect_path_security(&target);
        let has_deny = sec_info
            .aces
            .iter()
            .any(|a| a.ace_type == ACCESS_DENIED_ACE_TYPE);
        assert!(!has_deny, "Found explicit Deny ACE in DACL");
    }

    #[test]
    fn test_new_child_file_inherits_expected_acl() {
        let temp = TempTestDir::new("child_file_inh");
        let target = temp.path().to_path_buf();
        ensure_protected_directory(&target).unwrap();

        // Ensure current test process can create child in test dir without inheriting any extra ACE
        grant_non_inheritable_owner_access(&target);

        let child_file = target.join("child.txt");
        fs::write(&child_file, b"child content").unwrap();

        let sec_info = inspect_path_security(&child_file);

        let sys_ace = sec_info
            .aces
            .iter()
            .find(|a| a.sid.eq_ignore_ascii_case(SID_SYSTEM))
            .expect("Child file has no SYSTEM ACE");
        assert_eq!(sys_ace.ace_type, ACCESS_ALLOWED_ACE_TYPE);
        assert_eq!(sys_ace.mask & FILE_ALL_ACCESS, FILE_ALL_ACCESS);
        assert_eq!(
            sys_ace.ace_flags & INHERITED_ACE.0 as u8,
            INHERITED_ACE.0 as u8,
            "SYSTEM ACE on child file should be marked INHERITED_ACE"
        );

        let admin_ace = sec_info
            .aces
            .iter()
            .find(|a| a.sid.eq_ignore_ascii_case(SID_BUILTIN_ADMINISTRATORS))
            .expect("Child file has no Administrators ACE");
        assert_eq!(admin_ace.ace_type, ACCESS_ALLOWED_ACE_TYPE);
        assert_eq!(admin_ace.mask & FILE_ALL_ACCESS, FILE_ALL_ACCESS);
        assert_eq!(
            admin_ace.ace_flags & INHERITED_ACE.0 as u8,
            INHERITED_ACE.0 as u8,
            "Administrators ACE on child file should be marked INHERITED_ACE"
        );

        // Child file should NOT have broad user allows
        for ace in &sec_info.aces {
            if ace.ace_type == ACCESS_ALLOWED_ACE_TYPE {
                assert!(!ace.sid.eq_ignore_ascii_case(SID_EVERYONE));
                assert!(!ace.sid.eq_ignore_ascii_case(SID_BUILTIN_USERS));
                assert!(!ace.sid.eq_ignore_ascii_case(SID_AUTHENTICATED_USERS));
            }
        }
    }

    #[test]
    fn test_new_child_directory_inherits_expected_acl() {
        let temp = TempTestDir::new("child_dir_inh");
        let target = temp.path().to_path_buf();
        ensure_protected_directory(&target).unwrap();

        // Ensure current test process can create child in test dir without inheriting any extra ACE
        grant_non_inheritable_owner_access(&target);

        let child_dir = target.join("sub_dir");
        fs::create_dir(&child_dir).unwrap();

        let sec_info = inspect_path_security(&child_dir);

        let sys_ace = sec_info
            .aces
            .iter()
            .find(|a| a.sid.eq_ignore_ascii_case(SID_SYSTEM))
            .expect("Child directory has no SYSTEM ACE");
        assert_eq!(sys_ace.ace_type, ACCESS_ALLOWED_ACE_TYPE);
        assert_eq!(sys_ace.mask & FILE_ALL_ACCESS, FILE_ALL_ACCESS);
        assert_eq!(
            sys_ace.ace_flags & INHERITED_ACE.0 as u8,
            INHERITED_ACE.0 as u8
        );

        let admin_ace = sec_info
            .aces
            .iter()
            .find(|a| a.sid.eq_ignore_ascii_case(SID_BUILTIN_ADMINISTRATORS))
            .expect("Child directory has no Administrators ACE");
        assert_eq!(admin_ace.ace_type, ACCESS_ALLOWED_ACE_TYPE);
        assert_eq!(admin_ace.mask & FILE_ALL_ACCESS, FILE_ALL_ACCESS);
        assert_eq!(
            admin_ace.ace_flags & INHERITED_ACE.0 as u8,
            INHERITED_ACE.0 as u8
        );

        // Child directory should NOT have broad user allows
        for ace in &sec_info.aces {
            if ace.ace_type == ACCESS_ALLOWED_ACE_TYPE {
                assert!(!ace.sid.eq_ignore_ascii_case(SID_EVERYONE));
                assert!(!ace.sid.eq_ignore_ascii_case(SID_BUILTIN_USERS));
                assert!(!ace.sid.eq_ignore_ascii_case(SID_AUTHENTICATED_USERS));
            }
        }
    }
}
