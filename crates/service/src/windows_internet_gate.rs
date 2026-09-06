//! Production service-side binding for Windows InternetGate and RetryPolicy.

use std::time::Duration;

use palka_core::InternetState;
use palka_windows_platform::internet_gate::{
    WindowsInternetGate, WindowsWfpError, canonical_retry_delay,
};
use palka_windows_platform::wfp::WfpEnginePort;

use crate::runtime::{InternetGate, InternetRetryPolicy, PlatformError};

impl From<WindowsWfpError> for PlatformError {
    fn from(err: WindowsWfpError) -> Self {
        Self {
            reason: err.to_string(),
        }
    }
}

impl<E: WfpEnginePort + Send + Sync + 'static> InternetGate for WindowsInternetGate<E> {
    fn current_state(&self, child_sid: &str) -> Result<InternetState, PlatformError> {
        self.current_state(child_sid).map_err(PlatformError::from)
    }

    fn block_internet(&self, child_sid: &str) -> Result<(), PlatformError> {
        self.block_internet(child_sid).map_err(PlatformError::from)
    }

    fn unblock_internet(&self, child_sid: &str) -> Result<(), PlatformError> {
        self.unblock_internet(child_sid)
            .map_err(PlatformError::from)
    }
}

/// Concrete zero-state production Internet retry policy bound to canonical delays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowsInternetRetryPolicy;

impl InternetRetryPolicy for WindowsInternetRetryPolicy {
    fn delay_for_attempt(&self, attempt_count: u32) -> Duration {
        canonical_retry_delay(attempt_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palka_windows_platform::wfp::FakeWfpEnginePort;

    const TEST_SID: &str = "S-1-5-21-111111111-222222222-333333333-1001";

    #[test]
    fn test_service_internet_gate_trait_delegates_to_platform_gate() {
        let fake = FakeWfpEnginePort::new();
        let gate = WindowsInternetGate::new(fake);

        // Exercise through &dyn InternetGate trait boundary
        let dyn_gate: &dyn InternetGate = &gate;

        assert_eq!(
            dyn_gate.current_state(TEST_SID).unwrap(),
            InternetState::Unrestricted
        );

        dyn_gate.block_internet(TEST_SID).unwrap();
        assert_eq!(
            dyn_gate.current_state(TEST_SID).unwrap(),
            InternetState::Blocked
        );

        dyn_gate.unblock_internet(TEST_SID).unwrap();
        assert_eq!(
            dyn_gate.current_state(TEST_SID).unwrap(),
            InternetState::Unrestricted
        );
    }

    #[test]
    fn test_windows_wfp_error_to_platform_error_mapping() {
        let wfp_err = WindowsWfpError::FilterAddFailure {
            win32_code: 0x80320009,
        };
        let platform_err: PlatformError = wfp_err.into();

        // Native code must remain observable in the diagnostic reason
        assert!(platform_err.reason.contains("80320009"));
        assert!(platform_err.reason.contains("Failed to add WFP filter"));
    }

    #[test]
    fn test_windows_internet_retry_policy_trait_sequence() {
        let policy = WindowsInternetRetryPolicy;
        let dyn_policy: &dyn InternetRetryPolicy = &policy;

        assert_eq!(dyn_policy.delay_for_attempt(0), Duration::from_secs(1));
        assert_eq!(dyn_policy.delay_for_attempt(1), Duration::from_secs(1));
        assert_eq!(dyn_policy.delay_for_attempt(2), Duration::from_secs(2));
        assert_eq!(dyn_policy.delay_for_attempt(3), Duration::from_secs(4));
        assert_eq!(dyn_policy.delay_for_attempt(4), Duration::from_secs(8));
        assert_eq!(dyn_policy.delay_for_attempt(5), Duration::from_secs(16));
        assert_eq!(dyn_policy.delay_for_attempt(6), Duration::from_secs(32));
        assert_eq!(dyn_policy.delay_for_attempt(7), Duration::from_secs(60));
        assert_eq!(dyn_policy.delay_for_attempt(100), Duration::from_secs(60));
    }
}
