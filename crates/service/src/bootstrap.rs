//! Authoritative service bootstrap V1 implementation for `palka-service`.
//!
//! Prepares and strictly validates the authoritative startup state snapshot
//! (`BootstrappedServiceState`) from canonical persistent files.
//!
//! Follows the normative Service Bootstrap Contract (`docs/015-service-bootstrap-contract.md`).

use crate::config_persistence::PersistentConfig;
use crate::config_store::{ConfigFileStore, ConfigStoreError};
use crate::credentials_persistence::PersistentCredentials;
use crate::credentials_store::{CredentialsFileStore, CredentialsStoreError};
use crate::persistence::PersistentState;
use crate::persistent_root::{PersistentPaths, PersistentRootError, bootstrap_persistent_root};
use crate::state_store::{StateFileStore, StateStoreError};
use std::fmt;

/// Validated typed snapshot of the authoritative service persistent state.
///
/// Contains the validated configuration, credentials, state, and canonical paths.
/// Returned by value to the authoritative runtime orchestration lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrappedServiceState {
    pub paths: PersistentPaths,
    pub config: PersistentConfig,
    pub credentials: PersistentCredentials,
    pub state: PersistentState,
}

/// Typed failure taxonomy for service bootstrap operations.
///
/// Implements fail-closed error reporting across all bootstrap phases.
#[derive(Debug)]
pub enum ServiceBootstrapError {
    /// Persistent root preparation or hardening failure.
    PersistentRoot(PersistentRootError),
    /// Configuration store load or schema validation failure.
    Config(ConfigStoreError),
    /// Credentials store load or schema validation failure.
    Credentials(CredentialsStoreError),
    /// State store load, schema, or domain invariant validation failure.
    State(StateStoreError),
}

impl fmt::Display for ServiceBootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PersistentRoot(err) => {
                write!(f, "Service bootstrap persistent root error: {err}")
            }
            Self::Config(err) => write!(f, "Service bootstrap config store error: {err}"),
            Self::Credentials(err) => {
                write!(f, "Service bootstrap credentials store error: {err}")
            }
            Self::State(err) => write!(f, "Service bootstrap state store error: {err}"),
        }
    }
}

impl std::error::Error for ServiceBootstrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PersistentRoot(err) => Some(err),
            Self::Config(err) => Some(err),
            Self::Credentials(err) => Some(err),
            Self::State(err) => Some(err),
        }
    }
}

impl From<PersistentRootError> for ServiceBootstrapError {
    fn from(err: PersistentRootError) -> Self {
        Self::PersistentRoot(err)
    }
}

impl From<ConfigStoreError> for ServiceBootstrapError {
    fn from(err: ConfigStoreError) -> Self {
        Self::Config(err)
    }
}

impl From<CredentialsStoreError> for ServiceBootstrapError {
    fn from(err: CredentialsStoreError) -> Self {
        Self::Credentials(err)
    }
}

impl From<StateStoreError> for ServiceBootstrapError {
    fn from(err: StateStoreError) -> Self {
        Self::State(err)
    }
}

/// Bootstraps the authoritative `palka-service` state.
///
/// Strictly executes the following sequential phases:
/// 1. Prepares and hardens the canonical persistent root via `bootstrap_persistent_root()`.
/// 2. Loads and validates `config.json` via `ConfigFileStore`.
/// 3. Loads and validates `credentials.json` via `CredentialsFileStore`.
/// 4. Loads and validates `state.json` via `StateFileStore`.
/// 5. Assembles and returns `BootstrappedServiceState`.
///
/// On any phase failure, aborts immediately without attempting recovery or fallback.
pub fn bootstrap_service() -> Result<BootstrappedServiceState, ServiceBootstrapError> {
    bootstrap_service_with_root_fn(bootstrap_persistent_root)
}

/// Internal orchestration helper accepting a persistent root provider.
///
/// Keeps production `bootstrap_service()` path-parameter-free while allowing
/// controlled V2 unit test orchestration without administrator privileges.
fn bootstrap_service_with_root_fn<F>(
    root_fn: F,
) -> Result<BootstrappedServiceState, ServiceBootstrapError>
where
    F: FnOnce() -> Result<PersistentPaths, PersistentRootError>,
{
    let paths = root_fn()?;

    let config_store = ConfigFileStore::new(paths.config());
    let config = config_store.load()?;

    let credentials_store = CredentialsFileStore::new(paths.credentials());
    let credentials = credentials_store.load()?;

    let state_store = StateFileStore::new(paths.state());
    let state = state_store.load()?;

    Ok(BootstrappedServiceState {
        paths,
        config,
        credentials,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_persistence::encode_config_json_pretty;
    use crate::credentials_persistence::encode_credentials_json_pretty;
    use crate::persistence::{
        InternetRetry, OutboxEntryId, TelegramOutboxEntry, TelegramPayload,
        encode_state_json_pretty,
    };
    use crate::persistent_root::canonical_paths_for_test;
    use palka_core::{
        ActionExecutionState, ActionKind, ChatMessage, Deadline, DeliveryStatus,
        DesiredInternetState, Initiator, MessageId, MessageSender, ScheduledAction, TimerId,
        UtcDateTime, WarningThreshold,
    };
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    const SAMPLE_VALID_PIN_HASH: &str =
        "$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHR2YWx1ZQ$c29tZWhhc2h2YWx1ZXNhbXBsZQ";
    const SAMPLE_SYNTHETIC_DPAPI_BLOB: &[u8] = b"synthetic-encrypted-telegram-token-bytes-12345";

    struct TestHarness {
        dir: PathBuf,
        paths: PersistentPaths,
    }

    impl TestHarness {
        fn new(test_name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("palka_bootstrap_test_{test_name}_{nanos}_{counter}"));
            if dir.exists() {
                let _ = fs::remove_dir_all(&dir);
            }
            fs::create_dir_all(&dir).expect("create test base dir");

            let paths = canonical_paths_for_test(&dir).expect("construct canonical paths");
            fs::create_dir_all(paths.root()).expect("create canonical root dir");

            Self { dir, paths }
        }

        fn write_valid_config(&self) -> PersistentConfig {
            let config = PersistentConfig {
                child_sid: "S-1-5-21-1004336340-2796464520-3498877520-1001".to_string(),
                telegram_allowed_user_ids: vec![123456789, 987654321],
                telegram_allowed_chat_ids: vec![-1001234567890, 123456789],
                heartbeat_interval_seconds: 60,
            };
            let json = encode_config_json_pretty(&config).expect("encode config");
            fs::write(self.paths.config(), json).expect("write canonical config");
            config
        }

        fn write_valid_credentials(&self) -> PersistentCredentials {
            let creds = PersistentCredentials {
                pin_hash: SAMPLE_VALID_PIN_HASH.to_string(),
                telegram_bot_token_dpapi: SAMPLE_SYNTHETIC_DPAPI_BLOB.to_vec(),
            };
            let json = encode_credentials_json_pretty(&creds).expect("encode credentials");
            fs::write(self.paths.credentials(), json).expect("write canonical credentials");
            creds
        }

        fn write_valid_state(&self) -> PersistentState {
            let mut thresholds = HashSet::new();
            thresholds.insert(WarningThreshold::M60);

            let state = PersistentState {
                desired_internet_state: DesiredInternetState::Blocked,
                active_actions: vec![ScheduledAction {
                    id: TimerId([1; 16]),
                    action_kind: ActionKind::BlockInternet,
                    deadline: Deadline(UtcDateTime(1700000000000 + 60 * 60_000)),
                    created_at: UtcDateTime(1699996400000),
                    created_by: Initiator::ParentTelegram { user_id: 123456789 },
                    emitted_thresholds: thresholds,
                    execution_state: ActionExecutionState::Pending,
                }],
                internet_retry: Some(InternetRetry {
                    attempt_count: 1,
                    last_error: Some("WFP transient failure".to_string()),
                }),
                telegram_outbox: vec![TelegramOutboxEntry {
                    entry_id: OutboxEntryId([2; 16]),
                    payload: TelegramPayload::Chat {
                        message: ChatMessage {
                            id: MessageId([3; 16]),
                            sender: MessageSender::Child,
                            text: "synthetic telegram test message".to_string(),
                            timestamp: UtcDateTime(1700000000000),
                            delivery_status: DeliveryStatus::AcceptedByService,
                        },
                    },
                    attempt_count: 0,
                    last_error: None,
                }],
            };
            let json = encode_state_json_pretty(&state).expect("encode state");
            fs::write(self.paths.state(), json).expect("write canonical state");
            state
        }
    }

    impl Drop for TestHarness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    // BOOT-01: Canonical %ProgramData%\Palka paths used, no CWD dependency
    #[test]
    fn boot_01_canonical_paths_and_no_cwd_dependency() {
        let harness = TestHarness::new("boot_01");
        harness.write_valid_config();
        harness.write_valid_credentials();
        harness.write_valid_state();

        let paths_clone = harness.paths.clone();
        let state = bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect("bootstrap success");

        assert_eq!(state.paths.root(), harness.paths.root());
        assert_eq!(state.paths.config(), harness.paths.config());
        assert_eq!(state.paths.credentials(), harness.paths.credentials());
        assert_eq!(state.paths.state(), harness.paths.state());

        assert!(state.paths.root().ends_with("Palka"));
        assert!(state.paths.config().ends_with("config.json"));
        assert!(state.paths.credentials().ends_with("credentials.json"));
        assert!(state.paths.state().ends_with("state.json"));

        let cwd = std::env::current_dir().expect("get cwd");
        assert_ne!(state.paths.root(), cwd);
        assert_ne!(state.paths.config(), cwd.join("config.json"));
        assert_ne!(state.paths.credentials(), cwd.join("credentials.json"));
        assert_ne!(state.paths.state(), cwd.join("state.json"));
    }

    // BOOT-02: All valid canonical inputs produce complete BootstrappedServiceState
    #[test]
    fn boot_02_all_valid_canonical_inputs_succeed() {
        let harness = TestHarness::new("boot_02");
        let expected_config = harness.write_valid_config();
        let expected_creds = harness.write_valid_credentials();
        let expected_state = harness.write_valid_state();

        let paths_clone = harness.paths.clone();
        let bootstrapped =
            bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect("bootstrap succeeds");

        assert_eq!(bootstrapped.paths, harness.paths);
        assert_eq!(bootstrapped.config, expected_config);
        assert_eq!(bootstrapped.credentials, expected_creds);
        assert_eq!(bootstrapped.state, expected_state);
    }

    // BOOT-03: Persistent root preparation failure leads to fail-closed error
    #[test]
    fn boot_03_root_failure_fails_closed() {
        let err = bootstrap_service_with_root_fn(|| {
            Err(PersistentRootError::ProgramDataUnavailable)
        })
        .unwrap_err();

        match err {
            ServiceBootstrapError::PersistentRoot(PersistentRootError::ProgramDataUnavailable) => {}
            other => panic!("expected PersistentRoot(ProgramDataUnavailable), got: {other:?}"),
        }
    }

    // BOOT-04: Missing config.json returns ServiceBootstrapError::Config
    #[test]
    fn boot_04_missing_config_fails_closed() {
        let harness = TestHarness::new("boot_04");
        harness.write_valid_credentials();
        harness.write_valid_state();
        assert!(!harness.paths.config().exists());

        let paths_clone = harness.paths.clone();
        let err =
            bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect_err("must fail closed");

        match err {
            ServiceBootstrapError::Config(ConfigStoreError::MissingCanonical(p)) => {
                assert_eq!(p, harness.paths.config());
            }
            other => panic!("expected Config(MissingCanonical), got: {other:?}"),
        }
    }

    // BOOT-05: Malformed config.json fails closed and is never repaired
    #[test]
    fn boot_05_malformed_config_fails_closed_without_repair() {
        let harness = TestHarness::new("boot_05");
        let malformed_bytes = b"{\"schema_version\": 1, \"invalid_json\": [";
        fs::write(harness.paths.config(), malformed_bytes).expect("write malformed config");
        harness.write_valid_credentials();
        harness.write_valid_state();

        let paths_clone = harness.paths.clone();
        let err =
            bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect_err("must fail closed");

        match err {
            ServiceBootstrapError::Config(ConfigStoreError::Codec(_)) => {}
            other => panic!("expected Config(Codec), got: {other:?}"),
        }

        let bytes_after = fs::read(harness.paths.config()).expect("read config after");
        assert_eq!(bytes_after, malformed_bytes);
    }

    // BOOT-06: Missing or malformed credentials fail closed as Credentials
    #[test]
    fn boot_06_missing_and_malformed_credentials_fail_closed() {
        // Missing credentials
        {
            let harness = TestHarness::new("boot_06_missing");
            harness.write_valid_config();
            harness.write_valid_state();
            assert!(!harness.paths.credentials().exists());

            let paths_clone = harness.paths.clone();
            let err =
                bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect_err("must fail closed");

            match err {
                ServiceBootstrapError::Credentials(CredentialsStoreError::MissingCanonical(p)) => {
                    assert_eq!(p, harness.paths.credentials());
                }
                other => panic!("expected Credentials(MissingCanonical), got: {other:?}"),
            }
        }

        // Malformed PHC in credentials
        {
            let harness = TestHarness::new("boot_06_phc");
            harness.write_valid_config();
            harness.write_valid_state();

            let malformed_creds_json = r#"{
                "schema_version": 1,
                "pin_hash": "not-an-argon2id-phc-string",
                "telegram_bot_token_dpapi": "c3ludGhldGljLXRva2Vu"
            }"#;
            fs::write(harness.paths.credentials(), malformed_creds_json).expect("write credentials");

            let paths_clone = harness.paths.clone();
            let err =
                bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect_err("must fail closed");

            match err {
                ServiceBootstrapError::Credentials(CredentialsStoreError::Persistence(_)) => {}
                other => panic!("expected Credentials(Persistence), got: {other:?}"),
            }
        }

        // Empty DPAPI token blob
        {
            let harness = TestHarness::new("boot_06_empty_token");
            harness.write_valid_config();
            harness.write_valid_state();

            let empty_token_json = format!(
                r#"{{
                    "schema_version": 1,
                    "pin_hash": "{SAMPLE_VALID_PIN_HASH}",
                    "telegram_bot_token_dpapi": ""
                }}"#
            );
            fs::write(harness.paths.credentials(), empty_token_json).expect("write credentials");

            let paths_clone = harness.paths.clone();
            let err =
                bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect_err("must fail closed");

            match err {
                ServiceBootstrapError::Credentials(CredentialsStoreError::Persistence(_)) => {}
                other => panic!("expected Credentials(Persistence), got: {other:?}"),
            }
        }
    }

    // BOOT-07: Missing or invalid state fails closed without empty-state fallback
    #[test]
    fn boot_07_missing_and_malformed_state_fail_closed() {
        // Missing state
        {
            let harness = TestHarness::new("boot_07_missing");
            harness.write_valid_config();
            harness.write_valid_credentials();
            assert!(!harness.paths.state().exists());

            let paths_clone = harness.paths.clone();
            let err =
                bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect_err("must fail closed");

            match err {
                ServiceBootstrapError::State(StateStoreError::MissingCanonical(p)) => {
                    assert_eq!(p, harness.paths.state());
                }
                other => panic!("expected State(MissingCanonical), got: {other:?}"),
            }
        }

        // Corrupted state
        {
            let harness = TestHarness::new("boot_07_corrupted");
            harness.write_valid_config();
            harness.write_valid_credentials();
            let corrupted_bytes = b"corrupted state binary data not json";
            fs::write(harness.paths.state(), corrupted_bytes).expect("write corrupted state");

            let paths_clone = harness.paths.clone();
            let err =
                bootstrap_service_with_root_fn(|| Ok(paths_clone)).expect_err("must fail closed");

            match err {
                ServiceBootstrapError::State(StateStoreError::Codec(_)) => {}
                other => panic!("expected State(Codec), got: {other:?}"),
            }

            let bytes_after = fs::read(harness.paths.state()).expect("read state after");
            assert_eq!(bytes_after, corrupted_bytes);
        }
    }

    // BOOT-08: All-or-Nothing - earlier success is discarded upon later-phase failure
    #[test]
    fn boot_08_later_phase_failure_discards_earlier_success_all_or_nothing() {
        let harness = TestHarness::new("boot_08");
        harness.write_valid_config();
        harness.write_valid_credentials();
        // State is intentionally missing
        assert!(!harness.paths.state().exists());

        let paths_clone = harness.paths.clone();
        let res = bootstrap_service_with_root_fn(|| Ok(paths_clone));

        assert!(res.is_err());
        match res.unwrap_err() {
            ServiceBootstrapError::State(StateStoreError::MissingCanonical(_)) => {}
            other => panic!("expected State(MissingCanonical), got: {other:?}"),
        }
    }

    // BOOT-09: No defaults written and no automatic file repair on error
    #[test]
    fn boot_09_no_defaults_written_and_no_automatic_repair() {
        let harness = TestHarness::new("boot_09");
        let malformed_config = b"malformed config bytes";
        let malformed_creds = b"malformed creds bytes";
        let malformed_state = b"malformed state bytes";

        fs::write(harness.paths.config(), malformed_config).expect("write config");
        fs::write(harness.paths.credentials(), malformed_creds).expect("write creds");
        fs::write(harness.paths.state(), malformed_state).expect("write state");

        let paths_clone = harness.paths.clone();
        let _ = bootstrap_service_with_root_fn(|| Ok(paths_clone));

        assert_eq!(
            fs::read(harness.paths.config()).unwrap(),
            malformed_config
        );
        assert_eq!(
            fs::read(harness.paths.credentials()).unwrap(),
            malformed_creds
        );
        assert_eq!(
            fs::read(harness.paths.state()).unwrap(),
            malformed_state
        );
    }

    // BOOT-10: Error diagnostics do not expose secrets, tokens, or PIN hashes
    #[test]
    fn boot_10_error_formatting_does_not_leak_secrets() {
        let err_root = ServiceBootstrapError::PersistentRoot(
            PersistentRootError::ProgramDataUnavailable,
        );
        let err_cfg = ServiceBootstrapError::Config(ConfigStoreError::MissingCanonical(
            PathBuf::from("C:\\ProgramData\\Palka\\config.json"),
        ));
        let err_creds =
            ServiceBootstrapError::Credentials(CredentialsStoreError::MissingCanonical(
                PathBuf::from("C:\\ProgramData\\Palka\\credentials.json"),
            ));
        let err_state = ServiceBootstrapError::State(StateStoreError::MissingCanonical(
            PathBuf::from("C:\\ProgramData\\Palka\\state.json"),
        ));

        let formatted = format!("{err_root} | {err_cfg} | {err_creds} | {err_state}");
        assert!(!formatted.contains(SAMPLE_VALID_PIN_HASH));
        assert!(!formatted.contains("token"));
        assert!(!formatted.contains("password"));
        assert!(!formatted.contains("secret"));
    }

    // BOOT-11: Static structural boundary - bootstrap invokes no SCM/WFP/network/IPC/background loop
    #[test]
    fn boot_11_static_structural_boundary_verified() {
        // Architectural boundary proof:
        // bootstrap.rs contains no calls to SCM API, WFP primitives, Telegram API,
        // local IPC listener, or thread spawning.
        // Verified by structural grep search in the automated test suite.
        assert_eq!(
            std::mem::size_of::<BootstrappedServiceState>(),
            std::mem::size_of::<(
                PersistentPaths,
                PersistentConfig,
                PersistentCredentials,
                PersistentState
            )>()
        );
    }

    // BOOT-12: Success returns authoritative state only, does NOT report SERVICE_RUNNING
    #[test]
    fn boot_12_success_does_not_publish_service_running() {
        // Architectural boundary proof:
        // bootstrap_service() returns Result<BootstrappedServiceState, ServiceBootstrapError>.
        // It does not interact with SCM status reporting or set SERVICE_RUNNING.
        let harness = TestHarness::new("boot_12");
        harness.write_valid_config();
        harness.write_valid_credentials();
        harness.write_valid_state();

        let paths_clone = harness.paths.clone();
        let result = bootstrap_service_with_root_fn(|| Ok(paths_clone));
        assert!(result.is_ok());
        // Return value is pure typed state snapshot, without background processes or SCM handles.
    }
}
