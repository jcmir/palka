//! Windows platform adapter crate for PALKA.

pub mod atomic_file;
pub mod dpapi;
pub mod protected_directory;
pub mod scm;
pub mod scm_provisioning;
pub mod scm_runtime;

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
