//! Windows-native implementation of random seed acquisition for IdSource using CNG.

use crate::id_source::WindowsIdGeneratorError;
#[cfg(windows)]
use windows::Win32::Foundation::NTSTATUS;
#[cfg(windows)]
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

/// Checks the NTSTATUS returned by BCryptGenRandom according to canonical NT_SUCCESS semantics.
#[cfg(windows)]
#[inline]
pub(crate) fn check_bcrypt_status(status: NTSTATUS) -> Result<(), WindowsIdGeneratorError> {
    if status.is_err() {
        Err(WindowsIdGeneratorError::RandomSeedGenerationFailure { ntstatus: status.0 })
    } else {
        Ok(())
    }
}

/// Acquires a 16-byte random seed using Windows CNG BCryptGenRandom.
///
/// Uses BCRYPT_USE_SYSTEM_PREFERRED_RNG with a null algorithm handle.
/// Preserves raw NTSTATUS error semantics on failure.
#[cfg(windows)]
pub(crate) fn acquire_system_random_seed() -> Result<[u8; 16], WindowsIdGeneratorError> {
    let mut seed = [0u8; 16];

    let status = unsafe { BCryptGenRandom(None, &mut seed, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };

    check_bcrypt_status(status)?;

    Ok(seed)
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::*;

    #[test]
    fn test_check_bcrypt_status_success() {
        let ok_status = NTSTATUS(0);
        assert!(ok_status.is_ok());
        assert!(check_bcrypt_status(ok_status).is_ok());
    }

    #[test]
    fn test_check_bcrypt_status_failure_preserves_ntstatus() {
        let failure_code = -1073741811; // 0xC000000D STATUS_INVALID_PARAMETER
        let err_status = NTSTATUS(failure_code);
        assert!(err_status.is_err());

        let res = check_bcrypt_status(err_status);
        assert_eq!(
            res,
            Err(WindowsIdGeneratorError::RandomSeedGenerationFailure {
                ntstatus: failure_code
            })
        );
    }

    #[test]
    fn test_physical_bcrypt_gen_random_execution() {
        let _seed = acquire_system_random_seed()
            .expect("real BCryptGenRandom seed acquisition must succeed");
    }
}
