//! Production ID sequence implementation for PALKA.

use std::fmt;
use std::sync::Mutex;

/// Error type for Windows ID generation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsIdGeneratorError {
    UnsupportedPlatform,
    RandomSeedGenerationFailure { ntstatus: i32 },
}

impl fmt::Display for WindowsIdGeneratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "WindowsIdGeneratorError: UnsupportedPlatform"),
            Self::RandomSeedGenerationFailure { ntstatus } => {
                write!(
                    f,
                    "WindowsIdGeneratorError: RandomSeedGenerationFailure (ntstatus: {ntstatus})"
                )
            }
        }
    }
}

impl std::error::Error for WindowsIdGeneratorError {}

/// Local 128-bit monotonically increasing sequence seeded with cryptographic entropy.
pub struct WindowsIdSequence {
    state: Mutex<u128>,
}

impl WindowsIdSequence {
    /// Constructs a production sequence by acquiring entropy from the OS.
    ///
    /// On Windows, acquires 16 bytes via BCryptGenRandom.
    /// On non-Windows, returns `Err(WindowsIdGeneratorError::UnsupportedPlatform)`.
    pub fn from_production() -> Result<Self, WindowsIdGeneratorError> {
        #[cfg(windows)]
        {
            let seed = crate::id_source_windows::acquire_system_random_seed()?;
            Ok(Self::from_seed_internal(seed))
        }
        #[cfg(not(windows))]
        {
            Err(WindowsIdGeneratorError::UnsupportedPlatform)
        }
    }

    /// Internal deterministic constructor for unit tests within this crate.
    /// Not exposed as a public production API.
    #[inline]
    pub(crate) fn from_seed_internal(seed: [u8; 16]) -> Self {
        Self {
            state: Mutex::new(u128::from_be_bytes(seed)),
        }
    }

    /// Returns the next 128-bit identifier and increments internal state.
    ///
    /// The first value returned is the initial seed.
    /// Arithmetic overflow wraps around via `wrapping_add(1)`.
    /// Mutex poison errors are recovered via `PoisonError::into_inner`.
    pub fn next_128(&self) -> [u8; 16] {
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let current = *guard;
        *guard = current.wrapping_add(1);
        current.to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_seed_first_value_and_increment() {
        let seed = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        let seq = WindowsIdSequence::from_seed_internal(seed);

        let val1 = seq.next_128();
        assert_eq!(val1, seed);

        let val2 = seq.next_128();
        let expected_val2 = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x11,
        ];
        assert_eq!(val2, expected_val2);

        let val3 = seq.next_128();
        let expected_val3 = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x12,
        ];
        assert_eq!(val3, expected_val3);
    }

    #[test]
    fn test_big_endian_representation() {
        let val: u128 = 0x0123456789abcdef0123456789abcdef;
        let seed = val.to_be_bytes();
        let seq = WindowsIdSequence::from_seed_internal(seed);
        assert_eq!(seq.next_128(), val.to_be_bytes());
        assert_eq!(seq.next_128(), (val + 1).to_be_bytes());
    }

    #[test]
    fn test_u128_max_wrapping_no_panic() {
        let seed = u128::MAX.to_be_bytes();
        let seq = WindowsIdSequence::from_seed_internal(seed);
        let first = seq.next_128();
        assert_eq!(first, u128::MAX.to_be_bytes());
        let wrapped = seq.next_128();
        assert_eq!(wrapped, 0u128.to_be_bytes());
        let next = seq.next_128();
        assert_eq!(next, 1u128.to_be_bytes());
    }

    #[test]
    fn test_mutex_poison_recovery() {
        use std::sync::Arc;
        use std::thread;

        let seed = 42u128.to_be_bytes();
        let seq = Arc::new(WindowsIdSequence::from_seed_internal(seed));
        let seq_clone = Arc::clone(&seq);

        let handle = thread::spawn(move || {
            let _guard = seq_clone.state.lock().unwrap();
            panic!("deliberate panic to poison mutex");
        });
        let _ = handle.join();

        assert!(seq.state.is_poisoned());
        let val = seq.next_128();
        assert_eq!(val, 42u128.to_be_bytes());
        let next_val = seq.next_128();
        assert_eq!(next_val, 43u128.to_be_bytes());
    }

    #[test]
    fn test_error_display() {
        let err1 = WindowsIdGeneratorError::UnsupportedPlatform;
        assert_eq!(
            err1.to_string(),
            "WindowsIdGeneratorError: UnsupportedPlatform"
        );

        let err2 = WindowsIdGeneratorError::RandomSeedGenerationFailure {
            ntstatus: -1073741811,
        };
        assert_eq!(
            err2.to_string(),
            "WindowsIdGeneratorError: RandomSeedGenerationFailure (ntstatus: -1073741811)"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_non_windows_unsupported() {
        let res = WindowsIdSequence::from_production();
        assert_eq!(
            res.err(),
            Some(WindowsIdGeneratorError::UnsupportedPlatform)
        );
    }

    #[test]
    fn test_all_zero_seed_is_valid_domain_value() {
        let seq = WindowsIdSequence::from_seed_internal([0u8; 16]);
        assert_eq!(seq.next_128(), [0u8; 16]);
        assert_eq!(seq.next_128(), 1u128.to_be_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn test_real_windows_cng_production_construction() {
        let seq = WindowsIdSequence::from_production()
            .expect("real Windows production constructor must succeed");
        let id1 = seq.next_128();
        let id2 = seq.next_128();
        assert_eq!(
            u128::from_be_bytes(id2),
            u128::from_be_bytes(id1).wrapping_add(1)
        );
    }
}
