//! Service state file store implementing atomic Windows persistence.

use crate::persistence::{
    PersistenceError, PersistentState, decode_state_json_bytes, encode_state_json_pretty,
};
use palka_windows_platform::{AtomicPublishError, atomic_publish_file};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Error returned by `StateFileStore` operations.
#[derive(Debug)]
pub enum StateStoreError {
    MissingCanonical(PathBuf),
    InvalidPath(String),
    Io(std::io::Error),
    Codec(PersistenceError),
    Publish(AtomicPublishError),
}

impl fmt::Display for StateStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCanonical(path) => {
                write!(f, "Canonical state file does not exist: {}", path.display())
            }
            Self::InvalidPath(msg) => write!(f, "Invalid state file path: {msg}"),
            Self::Io(err) => write!(f, "I/O error in state file store: {err}"),
            Self::Codec(err) => write!(f, "Codec error in state file store: {err}"),
            Self::Publish(err) => write!(f, "Atomic publish error: {err}"),
        }
    }
}

impl std::error::Error for StateStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Codec(err) => Some(err),
            Self::Publish(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StateStoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<PersistenceError> for StateStoreError {
    fn from(err: PersistenceError) -> Self {
        Self::Codec(err)
    }
}

impl From<AtomicPublishError> for StateStoreError {
    fn from(err: AtomicPublishError) -> Self {
        Self::Publish(err)
    }
}

/// Store responsible for loading and atomically persisting `state.json`.
#[derive(Debug, Clone)]
pub struct StateFileStore {
    canonical_path: PathBuf,
}

impl StateFileStore {
    /// Creates a new store for the given canonical state file path.
    pub fn new(canonical_path: impl Into<PathBuf>) -> Self {
        Self {
            canonical_path: canonical_path.into(),
        }
    }

    /// Returns a reference to the configured canonical file path.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// Loads and strictly validates the authoritative `PersistentState` from the canonical file.
    ///
    /// If the canonical file does not exist, returns `StateStoreError::MissingCanonical`.
    /// Does not fall back to temporary candidate files and never creates a default state.
    pub fn load(&self) -> Result<PersistentState, StateStoreError> {
        let bytes = match std::fs::read(&self.canonical_path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StateStoreError::MissingCanonical(
                    self.canonical_path.clone(),
                ));
            }
            Err(err) => return Err(StateStoreError::Io(err)),
        };

        let state = decode_state_json_bytes(&bytes)?;
        Ok(state)
    }

    /// Validates and atomically persists `PersistentState` to the canonical file.
    ///
    /// First performs strict validation and encoding in memory.
    /// If encoding succeeds, writes to a temporary candidate file in the same directory,
    /// flushes buffers to disk, and atomically replaces/publishes the target file.
    pub fn save(&self, state: &PersistentState) -> Result<(), StateStoreError> {
        // Step A: Validate and encode before touching filesystem
        let json_string = encode_state_json_pretty(state)?;
        let json_bytes = json_string.as_bytes();

        let parent = self.canonical_path.parent().ok_or_else(|| {
            StateStoreError::InvalidPath(format!(
                "Canonical path has no parent directory: {}",
                self.canonical_path.display()
            ))
        })?;

        // Step B: Create unique collision-resistant candidate temporary file in the same directory
        let (temp_path, mut temp_file) = create_unique_temp_file(parent, &self.canonical_path)?;

        // Step C & D: Write all bytes, flush buffers, and close file
        let write_result = (|| -> Result<(), std::io::Error> {
            temp_file.write_all(json_bytes)?;
            temp_file.sync_all()?;
            drop(temp_file);
            Ok(())
        })();

        if let Err(write_err) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(StateStoreError::Io(write_err));
        }

        // Step E: Atomic publication
        let publish_result = atomic_publish_file(&temp_path, &self.canonical_path);

        if let Err(pub_err) = publish_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(StateStoreError::Publish(pub_err));
        }

        Ok(())
    }
}

fn create_unique_temp_file(
    parent: &Path,
    canonical_path: &Path,
) -> Result<(PathBuf, File), StateStoreError> {
    let file_name = canonical_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    let pid = std::process::id();

    for _ in 0..1000 {
        let count = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate_name = format!(".{file_name}.{pid}.{nanos}.{count}.tmp");
        let candidate_path = parent.join(candidate_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate_path)
        {
            Ok(file) => return Ok((candidate_path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(StateStoreError::Io(err)),
        }
    }

    Err(StateStoreError::InvalidPath(
        "Failed to allocate a unique temporary candidate file name after 1000 attempts".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{InternetRetry, OutboxEntryId, TelegramOutboxEntry, TelegramPayload};
    use palka_core::{
        ActionExecutionState, ActionKind, ChatMessage, Deadline, DeliveryStatus,
        DesiredInternetState, Initiator, MessageId, MessageSender, ScheduledAction, TimerId,
        UtcDateTime, WarningThreshold,
    };
    use std::collections::HashSet;
    use std::fs;

    fn setup_test_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "palka_state_store_test_{name}_{}_{}",
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn sample_valid_state(duration_minutes: i64) -> PersistentState {
        let mut thresholds = HashSet::new();
        thresholds.insert(WarningThreshold::M60);

        PersistentState {
            desired_internet_state: DesiredInternetState::Blocked,
            active_actions: vec![ScheduledAction {
                id: TimerId([1; 16]),
                action_kind: ActionKind::BlockInternet,
                deadline: Deadline(UtcDateTime(1700000000000 + duration_minutes * 60_000)),
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
                        text: "Можно продлить?".to_string(),
                        timestamp: UtcDateTime(1700000000000),
                        delivery_status: DeliveryStatus::AcceptedByService,
                    },
                },
                attempt_count: 0,
                last_error: None,
            }],
        }
    }

    #[test]
    fn save_new_state_creates_canonical_and_load_round_trips() {
        let dir = setup_test_dir("save_new");
        let canonical_path = dir.join("state.json");
        let store = StateFileStore::new(&canonical_path);

        assert!(!canonical_path.exists());

        let state = sample_valid_state(10);
        store.save(&state).expect("save should succeed");

        assert!(canonical_path.exists());
        let loaded = store.load().expect("load should succeed");
        assert_eq!(state, loaded);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_second_state_atomically_replaces_canonical() {
        let dir = setup_test_dir("save_replace");
        let canonical_path = dir.join("state.json");
        let store = StateFileStore::new(&canonical_path);

        let state1 = sample_valid_state(10);
        store.save(&state1).unwrap();
        assert_eq!(store.load().unwrap(), state1);

        let mut state2 = sample_valid_state(20);
        state2.desired_internet_state = DesiredInternetState::Unrestricted;
        store.save(&state2).unwrap();

        let loaded2 = store.load().unwrap();
        assert_eq!(loaded2, state2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_canonical_returns_error_and_does_not_create_file() {
        let dir = setup_test_dir("missing_canonical");
        let canonical_path = dir.join("state.json");
        let store = StateFileStore::new(&canonical_path);

        assert!(!canonical_path.exists());

        let err = store.load().unwrap_err();
        match err {
            StateStoreError::MissingCanonical(p) => assert_eq!(p, canonical_path),
            other => panic!("expected MissingCanonical, got: {other:?}"),
        }

        assert!(!canonical_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_json_canonical_returns_error_without_defaulting() {
        let dir = setup_test_dir("invalid_json");
        let canonical_path = dir.join("state.json");
        fs::write(&canonical_path, "{ malformed json content }").unwrap();

        let store = StateFileStore::new(&canonical_path);
        let err = store.load().unwrap_err();
        match err {
            StateStoreError::Codec(PersistenceError::Json(_)) => {}
            other => panic!("expected Codec(Json), got: {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_utf8_canonical_returns_error() {
        let dir = setup_test_dir("invalid_utf8");
        let canonical_path = dir.join("state.json");
        fs::write(&canonical_path, [0xFF, 0xFE, 0x00, 0x01]).unwrap();

        let store = StateFileStore::new(&canonical_path);
        let err = store.load().unwrap_err();
        match err {
            StateStoreError::Codec(PersistenceError::Json(_)) => {}
            other => panic!("expected Codec error for invalid UTF-8 JSON, got: {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_temp_with_valid_canonical_loads_canonical() {
        let dir = setup_test_dir("stale_temp_valid_can");
        let canonical_path = dir.join("state.json");
        let stale_temp_path = dir.join(".state.json.stale.tmp");

        let state = sample_valid_state(10);
        let store = StateFileStore::new(&canonical_path);
        store.save(&state).unwrap();

        fs::write(&stale_temp_path, "stale temp content").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_temp_with_missing_canonical_returns_missing_canonical() {
        let dir = setup_test_dir("stale_temp_missing_can");
        let canonical_path = dir.join("state.json");
        let stale_temp_path = dir.join(".state.json.stale.tmp");

        fs::write(&stale_temp_path, "stale candidate data").unwrap();

        let store = StateFileStore::new(&canonical_path);
        let err = store.load().unwrap_err();
        match err {
            StateStoreError::MissingCanonical(p) => assert_eq!(p, canonical_path),
            other => panic!("expected MissingCanonical, got: {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_in_memory_state_on_save_leaves_canonical_unmodified() {
        let dir = setup_test_dir("invalid_save_completed");
        let canonical_path = dir.join("state.json");
        let store = StateFileStore::new(&canonical_path);

        let initial_state = sample_valid_state(10);
        store.save(&initial_state).unwrap();
        let initial_bytes = fs::read(&canonical_path).unwrap();

        let mut invalid_state = sample_valid_state(20);
        invalid_state.active_actions[0].execution_state = ActionExecutionState::Completed;

        let err = store.save(&invalid_state).unwrap_err();
        match err {
            StateStoreError::Codec(PersistenceError::Validation(msg)) => {
                assert!(msg.contains("Terminal state Completed is prohibited"))
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }

        let current_bytes = fs::read(&canonical_path).unwrap();
        assert_eq!(initial_bytes, current_bytes);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disallowed_outbox_chat_sender_leaves_canonical_unmodified() {
        let dir = setup_test_dir("disallowed_sender");
        let canonical_path = dir.join("state.json");
        let store = StateFileStore::new(&canonical_path);

        let initial_state = sample_valid_state(10);
        store.save(&initial_state).unwrap();
        let initial_bytes = fs::read(&canonical_path).unwrap();

        let mut invalid_state = sample_valid_state(20);
        if let TelegramPayload::Chat { ref mut message } = invalid_state.telegram_outbox[0].payload
        {
            message.sender = MessageSender::Parent;
        }

        let err = store.save(&invalid_state).unwrap_err();
        match err {
            StateStoreError::Codec(PersistenceError::Validation(msg)) => {
                assert!(msg.contains("Prohibited Parent sender in telegram_outbox"))
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }

        let current_bytes = fs::read(&canonical_path).unwrap();
        assert_eq!(initial_bytes, current_bytes);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn successful_save_leaves_no_temp_files() {
        let dir = setup_test_dir("no_temp_left");
        let canonical_path = dir.join("state.json");
        let store = StateFileStore::new(&canonical_path);

        let state = sample_valid_state(15);
        store.save(&state).unwrap();

        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        assert_eq!(entries, vec!["state.json"]);

        let _ = fs::remove_dir_all(&dir);
    }
}
