//! PIN authentication and authoritative lockout engine for `palka-service`.
//!
//! Provides salted Argon2id PHC hashing, strictly-governed policy verification,
//! and deterministic monotonic anti-bruteforce lockout state tracking.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{Ident, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use palka_core::SensitivePinString;
use std::fmt;

/// Expected Argon2 memory cost: 64 MiB (65536 KiB).
pub const ARGON2_M_COST: u32 = 65536;
/// Expected Argon2 time cost: 3 iterations.
pub const ARGON2_T_COST: u32 = 3;
/// Expected Argon2 parallelism: 4 lanes.
pub const ARGON2_P_COST: u32 = 4;

/// Number of consecutive failed attempts before triggering lockout.
pub const FAILURES_PER_LOCKOUT: u32 = 3;
/// Lockout progression timeouts in seconds: 30s -> 60s -> 300s (capped).
pub const LOCKOUT_SCHEDULE_SECONDS: &[u64] = &[30, 60, 300];

/// Errors occurring during PIN hashing, verification, or policy enforcement.
#[derive(Debug, PartialEq, Eq)]
pub enum PinAuthError {
    /// PIN cannot be empty for provisioning or authentication.
    EmptyPin,
    /// Stored PHC string is malformed or cannot be parsed.
    MalformedHash(String),
    /// Stored PHC string violates normative security policy.
    PolicyViolation(String),
    /// Cryptographic calculation error.
    Crypto(String),
}

impl fmt::Display for PinAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPin => write!(f, "PIN cannot be empty"),
            Self::MalformedHash(msg) => write!(f, "Malformed PHC hash: {msg}"),
            Self::PolicyViolation(msg) => write!(f, "PIN security policy violation: {msg}"),
            Self::Crypto(msg) => write!(f, "Cryptographic failure in PIN auth: {msg}"),
        }
    }
}

impl std::error::Error for PinAuthError {}

/// Generates a salted Argon2id PHC hash for the provided PIN under normative security policy.
///
/// Uses OS CSPRNG for fresh salt generation and exact parameters:
/// `argon2id`, `v=19`, `m=65536`, `t=3`, `p=4`.
pub fn hash_pin(pin: &SensitivePinString) -> Result<String, PinAuthError> {
    if pin.as_str().is_empty() {
        return Err(PinAuthError::EmptyPin);
    }

    let salt = SaltString::generate(&mut OsRng);
    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, None)
        .map_err(|e| PinAuthError::Crypto(e.to_string()))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let password_hash = argon2
        .hash_password(pin.as_str().as_bytes(), &salt)
        .map_err(|e| PinAuthError::Crypto(e.to_string()))?;

    Ok(password_hash.to_string())
}

/// Verifies a candidate PIN against a stored Argon2id PHC string.
///
/// Performs strict security policy validation BEFORE executing Argon2 computations:
/// - Exact algorithm `argon2id`;
/// - Exact version `v=19` (0x13);
/// - Exact parameter configuration `m=65536,t=3,p=4`;
/// - Valid non-empty salt and hash components.
///
/// Returns:
/// - `Ok(true)` if the PIN matches;
/// - `Ok(false)` if the PIN is incorrect or candidate PIN is empty (after stored PHC policy validation);
/// - `Err(PinAuthError)` if stored PHC is malformed, policy-downgraded, or altered.
pub fn verify_pin(pin: &SensitivePinString, stored_phc: &str) -> Result<bool, PinAuthError> {
    let parsed_hash =
        PasswordHash::new(stored_phc).map_err(|e| PinAuthError::MalformedHash(e.to_string()))?;

    // 1. Validate Algorithm is exactly argon2id
    if parsed_hash.algorithm.as_str() != "argon2id" {
        return Err(PinAuthError::PolicyViolation(
            "Algorithm must be 'argon2id'".to_string(),
        ));
    }

    // 2. Validate Version is exactly v=19 (0x13)
    match parsed_hash.version {
        Some(v) if v == 19 => {}
        _ => {
            return Err(PinAuthError::PolicyViolation(
                "Argon2 version must be 'v=19'".to_string(),
            ));
        }
    }

    // 3. Validate Parameters: exactly m=65536, t=3, p=4
    let m_ident = Ident::new("m").map_err(|e| PinAuthError::Crypto(e.to_string()))?;
    let t_ident = Ident::new("t").map_err(|e| PinAuthError::Crypto(e.to_string()))?;
    let p_ident = Ident::new("p").map_err(|e| PinAuthError::Crypto(e.to_string()))?;

    let m = parsed_hash
        .params
        .get(m_ident)
        .ok_or_else(|| PinAuthError::PolicyViolation("Missing parameter 'm'".to_string()))?
        .decimal()
        .map_err(|e| PinAuthError::PolicyViolation(format!("Invalid parameter 'm': {e}")))?;

    let t = parsed_hash
        .params
        .get(t_ident)
        .ok_or_else(|| PinAuthError::PolicyViolation("Missing parameter 't'".to_string()))?
        .decimal()
        .map_err(|e| PinAuthError::PolicyViolation(format!("Invalid parameter 't': {e}")))?;

    let p = parsed_hash
        .params
        .get(p_ident)
        .ok_or_else(|| PinAuthError::PolicyViolation("Missing parameter 'p'".to_string()))?
        .decimal()
        .map_err(|e| PinAuthError::PolicyViolation(format!("Invalid parameter 'p': {e}")))?;

    // Check count of parameters: exactly 3 (m, t, p)
    let count = parsed_hash.params.iter().count();
    if count != 3 {
        return Err(PinAuthError::PolicyViolation(format!(
            "Unexpected extra parameters in PHC string (found {count})"
        )));
    }

    if m != ARGON2_M_COST {
        return Err(PinAuthError::PolicyViolation(format!(
            "Memory cost must be {ARGON2_M_COST}"
        )));
    }

    if t != ARGON2_T_COST {
        return Err(PinAuthError::PolicyViolation(format!(
            "Time cost must be {ARGON2_T_COST}"
        )));
    }

    if p != ARGON2_P_COST {
        return Err(PinAuthError::PolicyViolation(format!(
            "Parallelism cost must be {ARGON2_P_COST}"
        )));
    }

    // 4. Validate Salt and Hash are present
    if parsed_hash.salt.is_none() {
        return Err(PinAuthError::PolicyViolation(
            "Missing salt in PHC".to_string(),
        ));
    }

    if parsed_hash.hash.is_none() {
        return Err(PinAuthError::PolicyViolation(
            "Missing hash in PHC".to_string(),
        ));
    }

    // 5. If candidate PIN is empty, return false without performing expensive Argon2 verification
    if pin.as_str().is_empty() {
        return Ok(false);
    }

    // 6. Execute Argon2 verification
    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, None)
        .map_err(|e| PinAuthError::Crypto(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    match argon2.verify_password(pin.as_str().as_bytes(), &parsed_hash) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(err) => Err(PinAuthError::Crypto(err.to_string())),
    }
}

/// Result of checking whether a PIN authentication attempt is permitted.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LockoutCheckResult {
    /// Authentication attempt is permitted.
    Allowed,
    /// Authentication attempt is blocked by an active lockout.
    Locked { remaining_seconds: u64 },
}

/// Result of recording a failed authentication attempt.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FailureResult {
    /// Attempt failed; remaining attempts before lockout.
    Failed { remaining_attempts: u32 },
    /// Attempt triggered a new lockout.
    LockoutTriggered {
        lockout_seconds: u64,
        remaining_seconds: u64,
    },
    /// Attempt was made while already locked out.
    StillLocked { remaining_seconds: u64 },
}

/// Deterministic, monotonic in-memory anti-bruteforce lockout state machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinLockoutState {
    consecutive_failures: u32,
    lockout_level: usize,
    lockout_until: Option<u64>,
}

impl PinLockoutState {
    /// Creates a new initial lockout state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if an authentication attempt is permitted at the given monotonic timestamp.
    pub fn check_attempt(&self, now_seconds: u64) -> LockoutCheckResult {
        if let Some(until) = self.lockout_until {
            if now_seconds < until {
                return LockoutCheckResult::Locked {
                    remaining_seconds: until - now_seconds,
                };
            }
        }
        LockoutCheckResult::Allowed
    }

    /// Records a failed authentication attempt at the given monotonic timestamp.
    ///
    /// If an attempt is made during an active lockout, failure count and duration are NOT incremented.
    pub fn record_failure(&mut self, now_seconds: u64) -> FailureResult {
        if let Some(until) = self.lockout_until {
            if now_seconds < until {
                return FailureResult::StillLocked {
                    remaining_seconds: until - now_seconds,
                };
            }
            self.lockout_until = None;
        }

        self.consecutive_failures += 1;

        if self.consecutive_failures >= FAILURES_PER_LOCKOUT {
            let timeout = LOCKOUT_SCHEDULE_SECONDS
                [self.lockout_level.min(LOCKOUT_SCHEDULE_SECONDS.len() - 1)];
            self.lockout_until = Some(now_seconds + timeout);
            self.consecutive_failures = 0;

            if self.lockout_level < LOCKOUT_SCHEDULE_SECONDS.len() - 1 {
                self.lockout_level += 1;
            }

            FailureResult::LockoutTriggered {
                lockout_seconds: timeout,
                remaining_seconds: timeout,
            }
        } else {
            FailureResult::Failed {
                remaining_attempts: FAILURES_PER_LOCKOUT - self.consecutive_failures,
            }
        }
    }

    /// Records a successful authentication, resetting failure count, active lockout, and escalation level.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.lockout_level = 0;
        self.lockout_until = None;
    }

    /// Returns the number of consecutive failures since last reset or lockout.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Returns the current lockout escalation level index.
    pub fn lockout_level(&self) -> usize {
        self.lockout_level
    }

    /// Returns the monotonic timestamp until which lockout is active, if any.
    pub fn lockout_until(&self) -> Option<u64> {
        self.lockout_until
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_valid_phc() -> String {
        let pin = SensitivePinString::new("test-sample-pin".to_string());
        hash_pin(&pin).expect("sample hash generation")
    }

    #[test]
    fn hash_pin_produces_valid_normative_phc() {
        let pin = SensitivePinString::new("123456".to_string());
        let phc = hash_pin(&pin).expect("hashing should succeed");

        assert!(phc.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"));
        let parts: Vec<&str> = phc.split('$').collect();
        assert_eq!(parts.len(), 6);
        assert_eq!(parts[1], "argon2id");
        assert_eq!(parts[2], "v=19");
        assert_eq!(parts[3], "m=65536,t=3,p=4");
        assert!(!parts[4].is_empty(), "salt must be non-empty");
        assert!(!parts[5].is_empty(), "hash must be non-empty");
    }

    #[test]
    fn hashing_same_pin_twice_produces_different_hashes_due_to_random_salt() {
        let pin = SensitivePinString::new("secret-pin-99".to_string());
        let hash1 = hash_pin(&pin).unwrap();
        let hash2 = hash_pin(&pin).unwrap();

        assert_ne!(hash1, hash2, "Distinct salts must produce distinct hashes");
        assert!(verify_pin(&pin, &hash1).unwrap());
        assert!(verify_pin(&pin, &hash2).unwrap());
    }

    #[test]
    fn correct_pin_returns_true() {
        let pin = SensitivePinString::new("CorrectHorseBatteryStaple".to_string());
        let phc = hash_pin(&pin).unwrap();
        assert!(verify_pin(&pin, &phc).unwrap());
    }

    #[test]
    fn wrong_pin_returns_false() {
        let pin = SensitivePinString::new("original-correct-pin".to_string());
        let wrong_pin = SensitivePinString::new("wrong-attempt-pin".to_string());
        let phc = hash_pin(&pin).unwrap();
        assert!(!verify_pin(&wrong_pin, &phc).unwrap());
    }

    #[test]
    fn empty_provisioning_pin_is_rejected() {
        let empty_pin = SensitivePinString::new(String::new());
        let err = hash_pin(&empty_pin).unwrap_err();
        assert_eq!(err, PinAuthError::EmptyPin);
    }

    #[test]
    fn valid_phc_empty_candidate_pin_returns_false() {
        let pin = SensitivePinString::new("real-pin".to_string());
        let phc = hash_pin(&pin).unwrap();
        let empty_candidate = SensitivePinString::new(String::new());
        assert_eq!(verify_pin(&empty_candidate, &phc).unwrap(), false);
    }

    #[test]
    fn malformed_phc_empty_candidate_pin_returns_error() {
        let empty_candidate = SensitivePinString::new(String::new());
        let malformed = "not-even-a-phc";
        let res = verify_pin(&empty_candidate, malformed);
        assert!(
            res.is_err(),
            "Malformed PHC with empty candidate must return error, got: {res:?}"
        );
    }

    #[test]
    fn policy_invalid_phc_empty_candidate_pin_returns_policy_violation() {
        let empty_candidate = SensitivePinString::new(String::new());
        let valid = sample_valid_phc();

        // Policy violation: wrong m
        let bad_m = valid.replace("m=65536", "m=32768");
        let err_m = verify_pin(&empty_candidate, &bad_m).unwrap_err();
        assert!(
            matches!(err_m, PinAuthError::PolicyViolation(_)),
            "Policy invalid PHC with empty candidate must return PolicyViolation, got: {err_m:?}"
        );

        // Policy violation: wrong algorithm
        let bad_algo = valid.replace("$argon2id$", "$argon2i$");
        let err_algo = verify_pin(&empty_candidate, &bad_algo).unwrap_err();
        assert!(
            matches!(err_algo, PinAuthError::PolicyViolation(_)),
            "Wrong algorithm with empty candidate must return PolicyViolation, got: {err_algo:?}"
        );
    }

    #[test]
    fn malformed_stored_phc_returns_controlled_error() {
        let pin = SensitivePinString::new("pin".to_string());
        let malformed = &[
            "not-even-a-phc",
            "$argon2id$",
            "$argon2id$v=19",
            "$argon2id$garbage$salt$hash",
        ];

        for bad in malformed {
            let res = verify_pin(&pin, bad);
            assert!(
                res.is_err(),
                "Malformed PHC '{bad}' should return error, got: {res:?}"
            );
        }
    }

    #[test]
    fn wrong_algorithm_rejected_by_policy() {
        let pin = SensitivePinString::new("pin".to_string());
        let valid = sample_valid_phc();

        let phc_argon2i = valid.replace("$argon2id$", "$argon2i$");
        let phc_argon2d = valid.replace("$argon2id$", "$argon2d$");

        let err_i = verify_pin(&pin, &phc_argon2i).unwrap_err();
        assert!(matches!(err_i, PinAuthError::PolicyViolation(_)));

        let err_d = verify_pin(&pin, &phc_argon2d).unwrap_err();
        assert!(matches!(err_d, PinAuthError::PolicyViolation(_)));
    }

    #[test]
    fn wrong_version_rejected_by_policy() {
        let pin = SensitivePinString::new("pin".to_string());
        let valid = sample_valid_phc();

        let phc_v18 = valid.replace("$v=19$", "$v=18$");
        let err = verify_pin(&pin, &phc_v18).unwrap_err();
        assert!(matches!(err, PinAuthError::PolicyViolation(_)));
    }

    #[test]
    fn wrong_parameters_rejected_by_policy() {
        let pin = SensitivePinString::new("pin".to_string());
        let valid = sample_valid_phc();

        // Wrong m
        let bad_m = valid.replace("m=65536", "m=32768");
        assert!(matches!(
            verify_pin(&pin, &bad_m).unwrap_err(),
            PinAuthError::PolicyViolation(_)
        ));

        // Wrong t
        let bad_t = valid.replace("t=3", "t=2");
        assert!(matches!(
            verify_pin(&pin, &bad_t).unwrap_err(),
            PinAuthError::PolicyViolation(_)
        ));

        // Wrong p
        let bad_p = valid.replace("p=4", "p=2");
        assert!(matches!(
            verify_pin(&pin, &bad_p).unwrap_err(),
            PinAuthError::PolicyViolation(_)
        ));

        // Extra parameter
        let bad_extra = valid.replace("p=4", "p=4,extra=1");
        assert!(matches!(
            verify_pin(&pin, &bad_extra).unwrap_err(),
            PinAuthError::PolicyViolation(_)
        ));

        // Missing parameter
        let bad_missing = valid.replace(",p=4", "");
        assert!(matches!(
            verify_pin(&pin, &bad_missing).unwrap_err(),
            PinAuthError::PolicyViolation(_)
        ));
    }

    #[test]
    fn missing_hash_rejected_by_policy() {
        let pin = SensitivePinString::new("pin".to_string());
        let missing_hash = "$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHQ";
        assert!(verify_pin(&pin, missing_hash).is_err());
    }

    #[test]
    fn missing_salt_rejected_by_policy() {
        let pin = SensitivePinString::new("pin".to_string());
        let missing_salt = "$argon2id$v=19$m=65536,t=3,p=4$$c29tZWhhc2g";
        assert!(verify_pin(&pin, missing_salt).is_err());
    }

    #[test]
    fn malformed_stored_phc_does_not_mutate_lockout_state() {
        let mut state = PinLockoutState::new();
        state.record_failure(10);
        let snapshot_before = state.clone();

        let pin = SensitivePinString::new("candidate-pin".to_string());
        let res = verify_pin(&pin, "not-even-a-phc");
        assert!(res.is_err());

        assert_eq!(
            state, snapshot_before,
            "Configuration error from malformed PHC must not alter lockout state"
        );
    }

    #[test]
    fn huge_memory_parameter_rejected_before_argon2_computation() {
        let pin = SensitivePinString::new("pin".to_string());
        let valid = sample_valid_phc();

        // Attacker attempting DoS with 16GB memory requirement
        let huge_m = valid.replace("m=65536", "m=16777216");
        let err = verify_pin(&pin, &huge_m).unwrap_err();
        assert!(
            matches!(err, PinAuthError::PolicyViolation(_)),
            "Huge memory cost must be rejected by policy before execution"
        );
    }

    #[test]
    fn phc_result_does_not_contain_plaintext_pin() {
        let plaintext = "super-secret-unique-pin-12389";
        let pin = SensitivePinString::new(plaintext.to_string());
        let phc = hash_pin(&pin).unwrap();
        assert!(!phc.contains(plaintext));
    }

    // --- Lockout State Machine Tests ---

    #[test]
    fn lockout_progression_and_virtual_time_semantics() {
        let mut state = PinLockoutState::new();

        // Initial check: allowed
        assert_eq!(state.check_attempt(0), LockoutCheckResult::Allowed);

        // Failure 1: no lockout
        let res1 = state.record_failure(0);
        assert_eq!(
            res1,
            FailureResult::Failed {
                remaining_attempts: 2
            }
        );
        assert_eq!(state.check_attempt(0), LockoutCheckResult::Allowed);

        // Failure 2: no lockout
        let res2 = state.record_failure(1);
        assert_eq!(
            res2,
            FailureResult::Failed {
                remaining_attempts: 1
            }
        );
        assert_eq!(state.check_attempt(1), LockoutCheckResult::Allowed);

        // Failure 3: triggers 30s lockout (until t=32)
        let res3 = state.record_failure(2);
        assert_eq!(
            res3,
            FailureResult::LockoutTriggered {
                lockout_seconds: 30,
                remaining_seconds: 30
            }
        );

        // Attempt at +29s (t=31): locked out, 1s remaining
        assert_eq!(
            state.check_attempt(31),
            LockoutCheckResult::Locked {
                remaining_seconds: 1
            }
        );

        // Attempting to record failure during lockout does NOT increment counters or extend timeout
        let locked_attempt = state.record_failure(31);
        assert_eq!(
            locked_attempt,
            FailureResult::StillLocked {
                remaining_seconds: 1
            }
        );
        assert_eq!(state.consecutive_failures(), 0);
        assert_eq!(state.lockout_level(), 1);
        assert_eq!(state.lockout_until(), Some(32));

        // Exact expiry at t=32: permitted!
        assert_eq!(state.check_attempt(32), LockoutCheckResult::Allowed);

        // Next series of 3 failures (t=32, 33, 34)
        assert_eq!(
            state.record_failure(32),
            FailureResult::Failed {
                remaining_attempts: 2
            }
        );
        assert_eq!(
            state.record_failure(33),
            FailureResult::Failed {
                remaining_attempts: 1
            }
        );
        let res6 = state.record_failure(34);
        assert_eq!(
            res6,
            FailureResult::LockoutTriggered {
                lockout_seconds: 60,
                remaining_seconds: 60
            }
        );
        assert_eq!(state.lockout_until(), Some(94)); // 34 + 60

        // Third series of 3 failures after t=94 (t=95, 96, 97)
        assert_eq!(state.check_attempt(94), LockoutCheckResult::Allowed);
        assert_eq!(
            state.record_failure(95),
            FailureResult::Failed {
                remaining_attempts: 2
            }
        );
        assert_eq!(
            state.record_failure(96),
            FailureResult::Failed {
                remaining_attempts: 1
            }
        );
        let res9 = state.record_failure(97);
        assert_eq!(
            res9,
            FailureResult::LockoutTriggered {
                lockout_seconds: 300,
                remaining_seconds: 300
            }
        );
        assert_eq!(state.lockout_until(), Some(397)); // 97 + 300

        // Fourth series after t=397 remains capped at 300s
        assert_eq!(state.check_attempt(397), LockoutCheckResult::Allowed);
        assert_eq!(
            state.record_failure(398),
            FailureResult::Failed {
                remaining_attempts: 2
            }
        );
        assert_eq!(
            state.record_failure(399),
            FailureResult::Failed {
                remaining_attempts: 1
            }
        );
        let res12 = state.record_failure(400);
        assert_eq!(
            res12,
            FailureResult::LockoutTriggered {
                lockout_seconds: 300,
                remaining_seconds: 300
            }
        );
        assert_eq!(state.lockout_until(), Some(700));
    }

    #[test]
    fn success_resets_all_lockout_and_escalation_state() {
        let mut state = PinLockoutState::new();

        // 2 failures
        state.record_failure(0);
        state.record_failure(1);
        assert_eq!(state.consecutive_failures(), 2);

        // Success resets consecutive failures
        state.record_success();
        assert_eq!(state.consecutive_failures(), 0);
        assert_eq!(state.lockout_level(), 0);
        assert_eq!(state.lockout_until(), None);

        // Next 2 failures still do not lock out
        state.record_failure(10);
        state.record_failure(11);
        assert_eq!(state.check_attempt(11), LockoutCheckResult::Allowed);

        // Third failure triggers 30s (initial level)
        let res = state.record_failure(12);
        assert_eq!(
            res,
            FailureResult::LockoutTriggered {
                lockout_seconds: 30,
                remaining_seconds: 30
            }
        );

        // Success resets escalation level even after lockout triggered
        state.record_success();
        assert_eq!(state.lockout_level(), 0);
        assert_eq!(state.lockout_until(), None);
    }
}
