//! Windows DPAPI primitive for data protection and recovery.

use std::fmt;

/// Error returned when Windows DPAPI data protection or unprotection fails.
#[derive(Debug)]
pub enum DpapiError {
    /// Input data size exceeds maximum supported length.
    InputTooLarge { size: usize },
    /// A Windows API call failed.
    WindowsApi {
        function: &'static str,
        code: u32,
        message: String,
    },
}

impl fmt::Display for DpapiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { size } => {
                write!(
                    f,
                    "Input data size ({size} bytes) exceeds maximum supported DPAPI length ({} bytes)",
                    u32::MAX
                )
            }
            Self::WindowsApi {
                function,
                code,
                message,
            } => {
                write!(
                    f,
                    "Windows DPAPI call '{function}' failed with error code {code} (0x{code:08X}): {message}"
                )
            }
        }
    }
}

impl std::error::Error for DpapiError {}

#[cfg(windows)]
struct AutoCryptoBlob(windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB);

#[cfg(windows)]
impl Drop for AutoCryptoBlob {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(Some(
                    windows::Win32::Foundation::HLOCAL(self.0.pbData as _),
                ));
            }
            self.0.pbData = std::ptr::null_mut();
            self.0.cbData = 0;
        }
    }
}

#[cfg(windows)]
fn win32_error_code(err: &windows::core::Error) -> u32 {
    windows::Win32::Foundation::WIN32_ERROR::from_error(err)
        .map(|e| e.0)
        .unwrap_or_else(|| err.code().0 as u32)
}

/// Protects an arbitrary byte slice using Windows DPAPI under current user/service credentials.
///
/// Uses `CryptProtectData` with `CRYPTPROTECT_UI_FORBIDDEN`.
/// Does not use machine-wide scope (`CRYPTPROTECT_LOCAL_MACHINE`).
#[cfg(windows)]
pub fn protect_data(plaintext: &[u8]) -> Result<Vec<u8>, DpapiError> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };
    use windows::core::PCWSTR;

    let len = u32::try_from(plaintext.len()).map_err(|_| DpapiError::InputTooLarge {
        size: plaintext.len(),
    })?;

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: len,
        pbData: plaintext.as_ptr() as *mut u8,
    };

    let mut out_blob = AutoCryptoBlob(CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    });

    unsafe {
        CryptProtectData(
            &in_blob,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob.0,
        )
    }
    .map_err(|err| DpapiError::WindowsApi {
        function: "CryptProtectData",
        code: win32_error_code(&err),
        message: err.message(),
    })?;

    let result = if out_blob.0.cbData > 0 && !out_blob.0.pbData.is_null() {
        unsafe {
            std::slice::from_raw_parts(out_blob.0.pbData, out_blob.0.cbData as usize).to_vec()
        }
    } else {
        Vec::new()
    };

    Ok(result)
}

/// Recovers a protected byte slice using Windows DPAPI under current user/service credentials.
///
/// Uses `CryptUnprotectData` with `CRYPTPROTECT_UI_FORBIDDEN`.
#[cfg(windows)]
pub fn unprotect_data(ciphertext: &[u8]) -> Result<Vec<u8>, DpapiError> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let len = u32::try_from(ciphertext.len()).map_err(|_| DpapiError::InputTooLarge {
        size: ciphertext.len(),
    })?;

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: len,
        pbData: ciphertext.as_ptr() as *mut u8,
    };

    let mut out_blob = AutoCryptoBlob(CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    });

    unsafe {
        CryptUnprotectData(
            &in_blob,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob.0,
        )
    }
    .map_err(|err| DpapiError::WindowsApi {
        function: "CryptUnprotectData",
        code: win32_error_code(&err),
        message: err.message(),
    })?;

    let result = if out_blob.0.cbData > 0 && !out_blob.0.pbData.is_null() {
        unsafe {
            std::slice::from_raw_parts(out_blob.0.pbData, out_blob.0.cbData as usize).to_vec()
        }
    } else {
        Vec::new()
    };

    Ok(result)
}

#[cfg(not(windows))]
pub fn protect_data(plaintext: &[u8]) -> Result<Vec<u8>, DpapiError> {
    if plaintext.len() > u32::MAX as usize {
        return Err(DpapiError::InputTooLarge {
            size: plaintext.len(),
        });
    }
    Ok(plaintext.to_vec())
}

#[cfg(not(windows))]
pub fn unprotect_data(ciphertext: &[u8]) -> Result<Vec<u8>, DpapiError> {
    if ciphertext.len() > u32::MAX as usize {
        return Err(DpapiError::InputTooLarge {
            size: ciphertext.len(),
        });
    }
    Ok(ciphertext.to_vec())
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::*;

    #[test]
    fn protect_unprotect_known_bytes_round_trip() {
        let plaintext = b"palka-secret-payload-test-string";
        let protected = protect_data(plaintext).expect("protect_data should succeed");
        assert!(!protected.is_empty(), "Protected blob should not be empty");

        let recovered = unprotect_data(&protected).expect("unprotect_data should succeed");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn protect_unprotect_arbitrary_binary_payload() {
        let mut binary_data = Vec::new();
        for b in 0..=255u8 {
            binary_data.push(b);
        }
        // Non-UTF-8 sequence
        binary_data.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x01, 0x80, 0xC0]);

        let protected = protect_data(&binary_data).expect("protect_data should succeed");
        assert!(!protected.is_empty());

        let recovered = unprotect_data(&protected).expect("unprotect_data should succeed");
        assert_eq!(recovered, binary_data);
    }

    #[test]
    fn protect_unprotect_empty_slice() {
        let empty: &[u8] = b"";
        let protected = protect_data(empty).expect("protect_data on empty slice should succeed");
        assert!(!protected.is_empty(), "DPAPI blob header is non-empty");

        let recovered = unprotect_data(&protected).expect("unprotect_data should succeed");
        assert_eq!(recovered, empty);
    }

    #[test]
    fn corrupted_protected_blob_returns_controlled_error() {
        let invalid_blob = b"not-a-valid-dpapi-protected-ciphertext-blob";
        let res = unprotect_data(invalid_blob);
        match res {
            Err(DpapiError::WindowsApi { function, code, .. }) => {
                assert_eq!(function, "CryptUnprotectData");
                assert_ne!(code, 0, "Error code should not be 0");
            }
            other => panic!("expected DpapiError::WindowsApi, got: {other:?}"),
        }
    }

    #[test]
    fn random_garbage_bytes_fail_cleanly_without_panic() {
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let res = unprotect_data(&garbage);
        assert!(res.is_err(), "Unprotecting garbage data must return Err");
    }

    #[test]
    fn protected_blob_is_non_empty_and_distinct_from_plaintext() {
        let plaintext = b"distinctive-message-for-payload-test";
        let protected = protect_data(plaintext).expect("protect_data should succeed");
        assert!(!protected.is_empty());
        assert_ne!(protected.as_slice(), plaintext.as_slice());
    }
}
