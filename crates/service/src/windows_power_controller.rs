//! Windows Power Controller implementation adapting `windows_platform::power` to `PowerController`.
//!
//! Enforces:
//! - Strict compliance with docs/019 §5.3 mapping rules
//! - Preservation of exact Win32 error codes and variant names in PlatformError.reason
//! - Unmodified generic `PowerController` trait from `runtime.rs`
//! - Prevention of production readiness probe bypasses (Correction-3)

use crate::runtime::{PlatformError, PowerController};
use palka_windows_platform::power::{
    TestPowerPort, UnsupportedPowerPort, WindowsPowerController, WindowsPowerError,
    WindowsPowerPort,
};

#[cfg(windows)]
use palka_windows_platform::power_windows::WindowsPowerEngine;

impl From<WindowsPowerError> for PlatformError {
    fn from(err: WindowsPowerError) -> Self {
        match err {
            WindowsPowerError::UnsupportedPlatform => {
                Self::new("WindowsPowerError: UnsupportedPlatform")
            }
            WindowsPowerError::OpenProcessTokenFailure { win32_code } => Self::new(format!(
                "WindowsPowerError: OpenProcessTokenFailure (win32_code: {win32_code})"
            )),
            WindowsPowerError::LookupPrivilegeFailure { win32_code } => Self::new(format!(
                "WindowsPowerError: LookupPrivilegeFailure (win32_code: {win32_code})"
            )),
            WindowsPowerError::TokenPrivilegeQueryFailure { win32_code } => Self::new(format!(
                "WindowsPowerError: TokenPrivilegeQueryFailure (win32_code: {win32_code})"
            )),
            WindowsPowerError::PrivilegeNotAssigned => {
                Self::new("WindowsPowerError: PrivilegeNotAssigned")
            }
            WindowsPowerError::AdjustPrivilegeFailure { win32_code } => Self::new(format!(
                "WindowsPowerError: AdjustPrivilegeFailure (win32_code: {win32_code})"
            )),
            WindowsPowerError::PrivilegeRestoreFailure { win32_code } => Self::new(format!(
                "WindowsPowerError: PrivilegeRestoreFailure (win32_code: {win32_code})"
            )),
            WindowsPowerError::ShutdownRequestFailure { win32_code } => Self::new(format!(
                "WindowsPowerError: ShutdownRequestFailure (win32_code: {win32_code})"
            )),
            WindowsPowerError::ShutdownAlreadyInProgress { win32_code } => Self::new(format!(
                "WindowsPowerError: ShutdownAlreadyInProgress (win32_code: {win32_code})"
            )),
        }
    }
}

#[cfg(windows)]
pub type DefaultPowerPort = WindowsPowerEngine;

#[cfg(not(windows))]
pub type DefaultPowerPort = UnsupportedPowerPort;

/// Service-side adapter wrapping `WindowsPowerController` to implement `PowerController`.
pub struct WindowsPowerControllerAdapter<P: WindowsPowerPort = DefaultPowerPort> {
    inner: WindowsPowerController<P>,
}

impl<P: TestPowerPort> WindowsPowerControllerAdapter<P> {
    /// Infallible constructor without immediate readiness probe, restricted to test-seam ports.
    /// Real production ports (`WindowsPowerEngine`) cannot be constructed via this method.
    pub fn new(port: P) -> Self {
        Self {
            inner: WindowsPowerController::new(port),
        }
    }
}

impl<P: WindowsPowerPort> WindowsPowerControllerAdapter<P> {
    /// Fallible constructor executing a read-only readiness probe (docs/019 §4.3).
    pub fn with_readiness_probe(port: P) -> Result<Self, PlatformError> {
        let inner =
            WindowsPowerController::with_readiness_probe(port).map_err(PlatformError::from)?;
        Ok(Self { inner })
    }

    /// Accesses the underlying platform controller.
    pub fn inner(&self) -> &WindowsPowerController<P> {
        &self.inner
    }
}

#[cfg(windows)]
impl WindowsPowerControllerAdapter<WindowsPowerEngine> {
    /// Creates a production Windows power controller adapter, running a non-mutating
    /// platform readiness probe during construction.
    pub fn from_production() -> Result<Self, PlatformError> {
        let inner = WindowsPowerController::<WindowsPowerEngine>::from_production()
            .map_err(PlatformError::from)?;
        Ok(Self { inner })
    }
}

#[cfg(not(windows))]
impl WindowsPowerControllerAdapter<UnsupportedPowerPort> {
    /// Creates a production power controller adapter on non-Windows platforms, cleanly returning
    /// UnsupportedPlatform.
    pub fn from_production() -> Result<Self, PlatformError> {
        Err(PlatformError::from(WindowsPowerError::UnsupportedPlatform))
    }
}

impl WindowsPowerControllerAdapter<UnsupportedPowerPort> {
    /// Fallible constructor for unsupported platforms returning PlatformError(UnsupportedPlatform).
    pub fn from_unsupported() -> Result<Self, PlatformError> {
        Err(PlatformError::from(WindowsPowerError::UnsupportedPlatform))
    }
}

impl<P: WindowsPowerPort> PowerController for WindowsPowerControllerAdapter<P> {
    fn initiate_shutdown(&self) -> Result<(), PlatformError> {
        self.inner.initiate_shutdown().map_err(PlatformError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palka_windows_platform::power::{ERROR_ACCESS_DENIED, FakeWindowsPowerPort};

    #[test]
    fn test_service_power_controller_trait_delegation_success() {
        let fake = FakeWindowsPowerPort::default();
        let adapter = WindowsPowerControllerAdapter::new(fake.clone());

        let dyn_power: &dyn PowerController = &adapter;
        let res = dyn_power.initiate_shutdown();

        assert!(res.is_ok());
        assert_eq!(
            fake.shutdown_call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_service_power_controller_trait_delegation_failure() {
        let fake = FakeWindowsPowerPort::default();
        *fake.open_token_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED);
        let adapter = WindowsPowerControllerAdapter::new(fake);

        let dyn_power: &dyn PowerController = &adapter;
        let err = dyn_power.initiate_shutdown().unwrap_err();

        assert!(err.reason.contains("OpenProcessTokenFailure"));
        assert!(err.reason.contains("5"));
    }

    #[test]
    fn test_platform_error_mapping_preserves_native_codes_and_names() {
        let err_unsupported = PlatformError::from(WindowsPowerError::UnsupportedPlatform);
        assert_eq!(
            err_unsupported.reason,
            "WindowsPowerError: UnsupportedPlatform"
        );

        let err_not_assigned = PlatformError::from(WindowsPowerError::PrivilegeNotAssigned);
        assert_eq!(
            err_not_assigned.reason,
            "WindowsPowerError: PrivilegeNotAssigned"
        );

        let err_open =
            PlatformError::from(WindowsPowerError::OpenProcessTokenFailure { win32_code: 5 });
        assert!(err_open.reason.contains("OpenProcessTokenFailure"));
        assert!(err_open.reason.contains("5"));

        let err_lookup =
            PlatformError::from(WindowsPowerError::LookupPrivilegeFailure { win32_code: 1313 });
        assert!(err_lookup.reason.contains("LookupPrivilegeFailure"));
        assert!(err_lookup.reason.contains("1313"));

        let err_query =
            PlatformError::from(WindowsPowerError::TokenPrivilegeQueryFailure { win32_code: 122 });
        assert!(err_query.reason.contains("TokenPrivilegeQueryFailure"));
        assert!(err_query.reason.contains("122"));

        let err_adjust =
            PlatformError::from(WindowsPowerError::AdjustPrivilegeFailure { win32_code: 1300 });
        assert!(err_adjust.reason.contains("AdjustPrivilegeFailure"));
        assert!(err_adjust.reason.contains("1300"));

        let err_restore =
            PlatformError::from(WindowsPowerError::PrivilegeRestoreFailure { win32_code: 87 });
        assert!(err_restore.reason.contains("PrivilegeRestoreFailure"));
        assert!(err_restore.reason.contains("87"));

        let err_shutdown =
            PlatformError::from(WindowsPowerError::ShutdownRequestFailure { win32_code: 1116 });
        assert!(err_shutdown.reason.contains("ShutdownRequestFailure"));
        assert!(err_shutdown.reason.contains("1116"));

        let err_already =
            PlatformError::from(WindowsPowerError::ShutdownAlreadyInProgress { win32_code: 1115 });
        assert!(err_already.reason.contains("ShutdownAlreadyInProgress"));
        assert!(err_already.reason.contains("1115"));
    }

    #[test]
    fn test_service_adapter_with_readiness_probe_success() {
        let fake = FakeWindowsPowerPort::default();
        let res = WindowsPowerControllerAdapter::with_readiness_probe(fake);
        assert!(res.is_ok());
    }

    #[test]
    fn test_service_adapter_with_readiness_probe_failure() {
        let fake = FakeWindowsPowerPort::default();
        *fake.open_token_result.lock().unwrap() = Err(ERROR_ACCESS_DENIED);
        let res = WindowsPowerControllerAdapter::with_readiness_probe(fake);
        let err = res.err().expect("fails when unready");
        assert!(err.reason.contains("OpenProcessTokenFailure"));
        assert!(err.reason.contains("5"));
    }

    #[test]
    fn test_service_adapter_unsupported_platform_production_constructor() {
        let res = WindowsPowerControllerAdapter::<UnsupportedPowerPort>::from_unsupported();
        let err = res.err().expect("fails on unsupported platform");
        assert_eq!(err.reason, "WindowsPowerError: UnsupportedPlatform");
    }

    #[test]
    fn test_service_adapter_unsupported_platform_runtime_initiate_shutdown() {
        let adapter = WindowsPowerControllerAdapter::new(UnsupportedPowerPort);
        let dyn_power: &dyn PowerController = &adapter;
        let err = dyn_power.initiate_shutdown().unwrap_err();
        assert_eq!(err.reason, "WindowsPowerError: UnsupportedPlatform");
    }
}
