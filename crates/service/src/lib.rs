//! Service daemon library for PALKA.

pub mod bootstrap;
pub mod config_persistence;
pub mod config_store;
pub mod credentials_persistence;
pub mod credentials_store;
pub mod persistence;
pub mod persistent_root;
pub mod pin_auth;
pub mod runtime;
pub mod state_store;

pub use bootstrap::{BootstrappedServiceState, ServiceBootstrapError, bootstrap_service};

pub use config_persistence::{
    CONFIG_SCHEMA_VERSION_V1, ConfigPersistenceError, PersistentConfig, decode_config_json,
    decode_config_json_bytes, encode_config_json, encode_config_json_pretty,
};
pub use config_store::{ConfigFileStore, ConfigStoreError};
pub use credentials_persistence::{
    CREDENTIALS_SCHEMA_VERSION_V1, CredentialsPersistenceError, PersistentCredentials,
    decode_credentials_json, decode_credentials_json_bytes, encode_credentials_json,
    encode_credentials_json_pretty,
};
pub use credentials_store::{CredentialsFileStore, CredentialsStoreError};
pub use persistence::{
    InternetRetry, OutboxEntryId, PersistenceError, PersistentState, STATE_SCHEMA_VERSION_V1,
    TelegramOutboxEntry, TelegramPayload, decode_state_json, decode_state_json_bytes,
    encode_state_json, encode_state_json_pretty,
};
pub use persistent_root::{PersistentPaths, PersistentRootError, bootstrap_persistent_root};
pub use pin_auth::{
    ARGON2_M_COST, ARGON2_P_COST, ARGON2_T_COST, FAILURES_PER_LOCKOUT, FailureResult,
    LOCKOUT_SCHEDULE_SECONDS, LockoutCheckResult, PinAuthError, PinLockoutState, hash_pin,
    verify_pin,
};
pub use runtime::{
    IdSource, InternetGate, InternetRetryPolicy, PlatformError, PowerController, RuntimeClock,
    RuntimeConstructionError, RuntimeHandle, SchedulerError, ServiceRuntime, ServiceRuntimeError,
    StartupReadiness, StartupRecoveryError, SystemClock, TeardownError, WorkerError,
    remaining_seconds_from_delta_ms,
};
pub use state_store::{StateFileStore, StateStoreError};
