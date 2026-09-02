//! Service configuration file store implementing atomic Windows persistence.

use crate::config_persistence::{
    ConfigPersistenceError, PersistentConfig, decode_config_json_bytes, encode_config_json_pretty,
};
use palka_windows_platform::{AtomicPublishError, atomic_publish_file};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Error returned by `ConfigFileStore` operations.
#[derive(Debug)]
pub enum ConfigStoreError {
    MissingCanonical(PathBuf),
    InvalidPath(String),
    Io(std::io::Error),
    Codec(ConfigPersistenceError),
    Publish(AtomicPublishError),
}

impl fmt::Display for ConfigStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCanonical(path) => {
                write!(
                    f,
                    "Canonical config file does not exist: {}",
                    path.display()
                )
            }
            Self::InvalidPath(msg) => write!(f, "Invalid config file path: {msg}"),
            Self::Io(err) => write!(f, "I/O error in config file store: {err}"),
            Self::Codec(err) => write!(f, "Codec error in config file store: {err}"),
            Self::Publish(err) => write!(f, "Atomic publish error: {err}"),
        }
    }
}

impl std::error::Error for ConfigStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Codec(err) => Some(err),
            Self::Publish(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigStoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<ConfigPersistenceError> for ConfigStoreError {
    fn from(err: ConfigPersistenceError) -> Self {
        Self::Codec(err)
    }
}

impl From<AtomicPublishError> for ConfigStoreError {
    fn from(err: AtomicPublishError) -> Self {
        Self::Publish(err)
    }
}

/// Store responsible for loading and atomically persisting `config.json`.
#[derive(Debug, Clone)]
pub struct ConfigFileStore {
    canonical_path: PathBuf,
}

impl ConfigFileStore {
    /// Creates a new store for the given canonical config file path.
    pub fn new(canonical_path: impl Into<PathBuf>) -> Self {
        Self {
            canonical_path: canonical_path.into(),
        }
    }

    /// Returns a reference to the configured canonical file path.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// Loads and strictly validates the authoritative `PersistentConfig` from the canonical file.
    ///
    /// If the canonical file does not exist, returns `ConfigStoreError::MissingCanonical`.
    /// Does not fall back to temporary candidate files and never creates a default config.
    pub fn load(&self) -> Result<PersistentConfig, ConfigStoreError> {
        let bytes = match std::fs::read(&self.canonical_path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigStoreError::MissingCanonical(
                    self.canonical_path.clone(),
                ));
            }
            Err(err) => return Err(ConfigStoreError::Io(err)),
        };

        let config = decode_config_json_bytes(&bytes)?;
        Ok(config)
    }

    /// Validates and atomically persists `PersistentConfig` to the canonical file.
    ///
    /// First performs strict validation and encoding in memory.
    /// If encoding succeeds, writes to a temporary candidate file in the same directory,
    /// flushes buffers to disk, and atomically replaces/publishes the target file.
    pub fn save(&self, config: &PersistentConfig) -> Result<(), ConfigStoreError> {
        // Step A: Validate and encode before touching filesystem
        let json_string = encode_config_json_pretty(config)?;
        let json_bytes = json_string.as_bytes();

        let parent = self.canonical_path.parent().ok_or_else(|| {
            ConfigStoreError::InvalidPath(format!(
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
            return Err(ConfigStoreError::Io(write_err));
        }

        // Step E: Atomic publication
        let publish_result = atomic_publish_file(&temp_path, &self.canonical_path);

        if let Err(pub_err) = publish_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(ConfigStoreError::Publish(pub_err));
        }

        Ok(())
    }
}

fn create_unique_temp_file(
    parent: &Path,
    canonical_path: &Path,
) -> Result<(PathBuf, File), ConfigStoreError> {
    let file_name = canonical_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.json");
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
            Err(err) => return Err(ConfigStoreError::Io(err)),
        }
    }

    Err(ConfigStoreError::InvalidPath(
        "Failed to allocate a unique temporary candidate file name after 1000 attempts".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "palka_config_store_test_{name}_{}_{}",
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn sample_valid_config() -> PersistentConfig {
        PersistentConfig {
            child_sid: "S-1-5-21-1004336340-2796464520-3498877520-1001".to_string(),
            telegram_allowed_user_ids: vec![123456789, 987654321],
            telegram_allowed_chat_ids: vec![-1001234567890, 123456789],
            heartbeat_interval_seconds: 60,
        }
    }

    #[test]
    fn missing_canonical_returns_error() {
        let dir = setup_test_dir("missing_canonical");
        let canonical_path = dir.join("config.json");
        let store = ConfigFileStore::new(&canonical_path);

        assert!(!canonical_path.exists());

        let err = store.load().unwrap_err();
        match err {
            ConfigStoreError::MissingCanonical(p) => assert_eq!(p, canonical_path),
            other => panic!("expected MissingCanonical, got: {other:?}"),
        }

        assert!(!canonical_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_new_config_creates_canonical_and_round_trips() {
        let dir = setup_test_dir("save_new");
        let canonical_path = dir.join("config.json");
        let store = ConfigFileStore::new(&canonical_path);

        assert!(!canonical_path.exists());

        let config = sample_valid_config();
        store.save(&config).expect("save should succeed");

        assert!(canonical_path.exists());
        let loaded = store.load().expect("load should succeed");
        assert_eq!(config, loaded);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_replaces_existing_config_atomically() {
        let dir = setup_test_dir("save_replace");
        let canonical_path = dir.join("config.json");
        let store = ConfigFileStore::new(&canonical_path);

        let config1 = sample_valid_config();
        store.save(&config1).unwrap();
        assert_eq!(store.load().unwrap(), config1);

        let mut config2 = sample_valid_config();
        config2.child_sid = "S-1-5-21-999999".to_string();
        config2.heartbeat_interval_seconds = 120;
        store.save(&config2).unwrap();

        let loaded2 = store.load().unwrap();
        assert_eq!(loaded2, config2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_save_leaves_existing_canonical_byte_identical() {
        let dir = setup_test_dir("invalid_save");
        let canonical_path = dir.join("config.json");
        let store = ConfigFileStore::new(&canonical_path);

        let initial_config = sample_valid_config();
        store.save(&initial_config).unwrap();
        let initial_bytes = fs::read(&canonical_path).unwrap();

        let invalid_config = PersistentConfig {
            child_sid: "".to_string(),
            telegram_allowed_user_ids: vec![],
            telegram_allowed_chat_ids: vec![],
            heartbeat_interval_seconds: 60,
        };

        let err = store.save(&invalid_config).unwrap_err();
        match err {
            ConfigStoreError::Codec(ConfigPersistenceError::Validation(msg)) => {
                assert!(msg.contains("child_sid cannot be empty"));
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }

        let current_bytes = fs::read(&canonical_path).unwrap();
        assert_eq!(initial_bytes, current_bytes);

        // Verify no temporary files were created
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["config.json"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_temp_is_ignored_by_load() {
        let dir = setup_test_dir("stale_temp_ignored");
        let canonical_path = dir.join("config.json");
        let stale_temp_path = dir.join(".config.json.stale.tmp");

        let config = sample_valid_config();
        let store = ConfigFileStore::new(&canonical_path);
        store.save(&config).unwrap();

        fs::write(&stale_temp_path, "stale temp data").unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, config);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_canonical_is_not_recovered_from_valid_temp() {
        let dir = setup_test_dir("malformed_can_no_recovery");
        let canonical_path = dir.join("config.json");
        let stale_temp_path = dir.join(".config.json.valid.tmp");

        let valid_config = sample_valid_config();
        let valid_json = encode_config_json_pretty(&valid_config).unwrap();

        fs::write(&canonical_path, "{ broken json }").unwrap();
        fs::write(&stale_temp_path, valid_json).unwrap();

        let store = ConfigFileStore::new(&canonical_path);
        let err = store.load().unwrap_err();
        match err {
            ConfigStoreError::Codec(ConfigPersistenceError::Json(_)) => {}
            other => panic!("expected Codec(Json) error, got: {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_file_is_same_directory() {
        let dir = setup_test_dir("same_dir_temp");
        let canonical_path = dir.join("config.json");
        let parent = canonical_path.parent().unwrap();

        let (temp_path, file) = create_unique_temp_file(parent, &canonical_path).unwrap();
        drop(file);

        assert_eq!(temp_path.parent().unwrap(), parent);
        assert!(
            temp_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".config.json.")
        );
        assert!(
            temp_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".tmp")
        );

        let _ = fs::remove_file(&temp_path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn successful_save_leaves_no_own_temp_candidate() {
        let dir = setup_test_dir("no_own_temp_left");
        let canonical_path = dir.join("config.json");
        let store = ConfigFileStore::new(&canonical_path);

        let config = sample_valid_config();
        store.save(&config).unwrap();

        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        assert_eq!(entries, vec!["config.json"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_file_path_as_directory_or_invalid_parent_returns_controlled_error() {
        let dir = setup_test_dir("invalid_target");
        let store = ConfigFileStore::new(&dir);

        let config = sample_valid_config();
        let err = store.save(&config).unwrap_err();
        match err {
            ConfigStoreError::Publish(_) | ConfigStoreError::Io(_) => {}
            other => {
                panic!("expected Publish or Io error when target is directory, got: {other:?}")
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
