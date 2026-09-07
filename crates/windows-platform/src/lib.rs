//! Windows platform adapter crate for PALKA.

pub mod atomic_file;
pub mod dpapi;
pub mod id_source;
#[cfg(windows)]
mod id_source_windows;
pub mod internet_gate;
pub mod power;
#[cfg(windows)]
pub mod power_windows;
pub mod protected_directory;
pub mod scm;
pub mod scm_provisioning;
pub mod scm_runtime;
pub mod wfp;
pub mod wfp_windows;

pub use internet_gate::{
    FILTER_ALE_AUTH_CONNECT_V4_KEY, FILTER_ALE_AUTH_CONNECT_V6_KEY,
    FILTER_ALE_AUTH_RECV_ACCEPT_V4_KEY, FILTER_ALE_AUTH_RECV_ACCEPT_V6_KEY,
    FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4, FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
    PALKA_WFP_FILTER_WEIGHT, PALKA_WFP_PROVIDER_DATA, PALKA_WFP_PROVIDER_DESC,
    PALKA_WFP_PROVIDER_KEY, PALKA_WFP_PROVIDER_NAME, PALKA_WFP_SERVICE_NAME,
    PALKA_WFP_SUBLAYER_DESC, PALKA_WFP_SUBLAYER_KEY, PALKA_WFP_SUBLAYER_NAME,
    PALKA_WFP_SUBLAYER_WEIGHT, WindowsInternetGate, WindowsWfpError, canonical_retry_delay,
};
pub use wfp::{
    FakeWfpEnginePort, GUID, WfpActionType, WfpEnginePort, WfpFilterSnapshot, WfpFilterSpec,
    WfpProviderSnapshot, WfpProviderSpec, WfpSubLayerSnapshot, WfpSubLayerSpec,
};
pub use wfp_windows::WindowsWfpEngine;

pub use atomic_file::{AtomicPublishError, atomic_publish_file};
pub use dpapi::{DpapiError, protect_data, unprotect_data};
pub use protected_directory::{
    PROTECTED_DIRECTORY_SDDL, ProtectedDirectoryError, ensure_protected_directory,
};
pub use scm::{
    PALKA_SERVICE_ACCOUNT, PALKA_SERVICE_DESCRIPTION, PALKA_SERVICE_DISPLAY_NAME,
    PALKA_SERVICE_ERROR_CONTROL, PALKA_SERVICE_NAME, PALKA_SERVICE_RESET_PERIOD_SEC,
    PALKA_SERVICE_RESTART_DELAY_1_MS, PALKA_SERVICE_RESTART_DELAY_2_MS,
    PALKA_SERVICE_RESTART_DELAY_3_MS, PALKA_SERVICE_START_TYPE, PALKA_SERVICE_TYPE,
    ScmConfigMismatch, ScmConfigSnapshot, ScmQueryError, ScmRecoveryAction, ScmRecoveryActionType,
    query_palka_service_config,
};

pub use scm_provisioning::{
    ScmProvisionError, ScmProvisionOutcome, ScmProvisionPlan, ScmProvisionResult,
    classify_scm_mutation_error, plan_provisioning, provision_palka_service,
    validate_and_render_canonical_binary_path,
};

pub use scm_runtime::{
    CanonicalServiceStatus, DecodedControl, PalkaServiceEntry, ScmLifecycleStateMachine,
    ScmRuntimeControl, ScmRuntimeError, ScmServiceContext, ScmServiceState, decode_service_control,
    handle_control_request, run_palka_service_dispatcher,
};

pub use id_source::{WindowsIdGeneratorError, WindowsIdSequence};
pub use power::{
    AdjustPrivilegeResult, CANONICAL_SHUTDOWN_FORCE_APPS, CANONICAL_SHUTDOWN_MACHINE,
    CANONICAL_SHUTDOWN_MESSAGE, CANONICAL_SHUTDOWN_REASON, CANONICAL_SHUTDOWN_REBOOT,
    CANONICAL_SHUTDOWN_TIMEOUT, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_PARAMETER, ERROR_NOT_ALL_ASSIGNED, ERROR_NOT_ENOUGH_MEMORY,
    ERROR_PRIVILEGE_NOT_HELD, ERROR_SHUTDOWN_IN_PROGRESS, FakeWindowsPowerPort, Luid,
    LuidAndAttributes, PreviousPrivilegeState, RawTokenHandle, RestorePrivilegeResult,
    ShutdownCallParams, SizingResult, TestPowerPort, UnsupportedPowerPort, WindowsPowerController,
    WindowsPowerError, WindowsPowerPort, validate_restore_buffer,
};

#[cfg(windows)]
pub use power_windows::{
    AlignedBuffer, SafeHandle, WindowsPowerEngine, raw_win32_code_from_error,
    raw_win32_code_from_hresult,
};
