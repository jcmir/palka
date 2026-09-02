//! Atomic file store for service credentials (`credentials.json`).

use crate::credentials_persistence::{
    CredentialsPersistenceError, PersistentCredentials, decode_credentials_json_bytes,
    encode_credentials_json_pretty,
};
use palka_windows_platform::{AtomicPublishError, atomic_publish_file};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Errors occurring during credentials file store operations.
#[derive(Debug)]
pub enum CredentialsStoreError {
    /// Canonical file does not exist.
    MissingCanonical(PathBuf),
    /// The specified path is invalid.
    InvalidPath(String),
    /// I/O error while reading or preparing temporary candidate file.
    Io(io::Error),
    /// Serialization or schema validation failed.
    Persistence(CredentialsPersistenceError),
    /// Atomic file publishing failed.
    Publish(AtomicPublishError),
}

impl fmt::Display for CredentialsStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCanonical(path) => {
                write!(
                    f,
                    "Canonical credentials file does not exist: {}",
                    path.display()
                )
            }
            Self::InvalidPath(msg) => write!(f, "Invalid credentials file path: {msg}"),
            Self::Io(err) => write!(f, "I/O error in credentials file store: {err}"),
            Self::Persistence(err) => write!(f, "Credentials persistence error: {err}"),
            Self::Publish(err) => write!(f, "Atomic publish error for credentials: {err}"),
        }
    }
}

impl std::error::Error for CredentialsStoreError {}

impl From<io::Error> for CredentialsStoreError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<CredentialsPersistenceError> for CredentialsStoreError {
    fn from(err: CredentialsPersistenceError) -> Self {
        Self::Persistence(err)
    }
}

impl From<AtomicPublishError> for CredentialsStoreError {
    fn from(err: AtomicPublishError) -> Self {
        Self::Publish(err)
    }
}

/// Manages atomic persistence of `credentials.json`.
#[derive(Clone, Debug)]
pub struct CredentialsFileStore {
    canonical_path: PathBuf,
}

impl CredentialsFileStore {
    /// Creates a new `CredentialsFileStore` for the given canonical credentials path.
    pub fn new(canonical_path: impl Into<PathBuf>) -> Self {
        Self {
            canonical_path: canonical_path.into(),
        }
    }

    /// Returns the canonical path to `credentials.json`.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// Loads and strictly validates credentials from the canonical path.
    ///
    /// Returns `MissingCanonical` if the canonical file does not exist.
    /// Does not attempt recovery from `.tmp` files and never creates defaults.
    pub fn load(&self) -> Result<PersistentCredentials, CredentialsStoreError> {
        let raw_bytes = match fs::read(&self.canonical_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(CredentialsStoreError::MissingCanonical(
                    self.canonical_path.clone(),
                ));
            }
            Err(err) => return Err(CredentialsStoreError::Io(err)),
        };

        let creds = decode_credentials_json_bytes(&raw_bytes)?;
        Ok(creds)
    }

    /// Atomically persists credentials into the canonical path.
    ///
    /// Validates and encodes credentials in memory before modifying the filesystem.
    /// If validation fails, existing canonical files remain byte-identical and untouched.
    pub fn save(&self, creds: &PersistentCredentials) -> Result<(), CredentialsStoreError> {
        // 1. Validate and encode completely in memory
        let payload = encode_credentials_json_pretty(creds)?;

        // 2. Prepare temp candidate file path in the same directory
        let parent = self.canonical_path.parent().ok_or_else(|| {
            CredentialsStoreError::InvalidPath(
                "Canonical credentials file has no parent directory".to_string(),
            )
        })?;

        let file_name = self.canonical_path.file_name().ok_or_else(|| {
            CredentialsStoreError::InvalidPath(
                "Canonical credentials file has no file name".to_string(),
            )
        })?;

        let (temp_path, mut temp_file) = create_temp_file(parent, file_name)?;

        // 3. Write, flush, and close temp file
        let write_result = (|| -> Result<(), io::Error> {
            temp_file.write_all(payload.as_bytes())?;
            temp_file.sync_all()?;
            drop(temp_file);
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(CredentialsStoreError::Io(err));
        }

        // 4. Atomically publish
        if let Err(err) = atomic_publish_file(&temp_path, &self.canonical_path) {
            let _ = fs::remove_file(&temp_path);
            return Err(CredentialsStoreError::Publish(err));
        }

        Ok(())
    }
}

fn create_temp_file(
    parent: &Path,
    file_name: &std::ffi::OsStr,
) -> Result<(PathBuf, fs::File), CredentialsStoreError> {
    let pid = std::process::id();
    let name_str = file_name.to_string_lossy();

    for _ in 0..100 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let temp_name = format!(".{name_str}.{pid}.{nanos}.{counter}.tmp");
        let temp_path = parent.join(temp_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(CredentialsStoreError::Io(err)),
        }
    }

    Err(CredentialsStoreError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Failed to allocate a unique temporary credentials file name after multiple attempts",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_VALID_PIN_HASH: &str =
        "$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHR2YWx1ZQ$c29tZWhhc2h2YWx1ZXNhbXBsZQ";
    const SAMPLE_DPAPI_BLOB: &[u8] = b"synthetic-dpapi-protected-token-blob-bytes-12345";

    fn sample_valid_credentials() -> PersistentCredentials {
        PersistentCredentials {
            pin_hash: SAMPLE_VALID_PIN_HASH.to_string(),
            telegram_bot_token_dpapi: SAMPLE_DPAPI_BLOB.to_vec(),
        }
    }

    fn unique_test_dir(test_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("palka_creds_test_{test_name}_{nanos}_{counter}"));
        fs::create_dir_all(&dir).expect("failed to create unique test dir");
        dir
    }

    #[test]
    fn missing_canonical_returns_error_and_creates_nothing() {
        let dir = unique_test_dir("missing_canonical");
        let canonical = dir.join("credentials.json");
        let store = CredentialsFileStore::new(&canonical);

        let err = store.load().unwrap_err();
        match err {
            CredentialsStoreError::MissingCanonical(p) => assert_eq!(p, canonical),
            other => panic!("expected MissingCanonical, got: {other:?}"),
        }
        assert!(!canonical.exists(), "load must not create canonical file");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_new_credentials_creates_canonical_and_round_trips() {
        let dir = unique_test_dir("save_new");
        let canonical = dir.join("credentials.json");
        let store = CredentialsFileStore::new(&canonical);

        let creds = sample_valid_credentials();
        store.save(&creds).expect("save should succeed");
        assert!(canonical.exists());

        let loaded = store.load().expect("load should succeed");
        assert_eq!(loaded, creds);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_save_atomically_replaces_canonical() {
        let dir = unique_test_dir("atomic_replace");
        let canonical = dir.join("credentials.json");
        let store = CredentialsFileStore::new(&canonical);

        let creds1 = sample_valid_credentials();
        store.save(&creds1).expect("initial save should succeed");

        let creds2 = PersistentCredentials {
            pin_hash: "$argon2id$v=19$m=65536,t=3,p=4$bmV3c2FsdHZhbHVl$bmV3aGFzaHZhbHVl"
                .to_string(),
            telegram_bot_token_dpapi: b"new-synthetic-dpapi-blob-999".to_vec(),
        };
        store.save(&creds2).expect("second save should succeed");

        let loaded = store
            .load()
            .expect("load should return updated credentials");
        assert_eq!(loaded, creds2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_temp_ignored_by_load() {
        let dir = unique_test_dir("stale_temp");
        let canonical = dir.join("credentials.json");
        let store = CredentialsFileStore::new(&canonical);

        let creds = sample_valid_credentials();
        store.save(&creds).expect("save should succeed");

        // Create a stale temp file
        let stale_temp = dir.join(".credentials.json.9999.12345.1.tmp");
        fs::write(&stale_temp, b"stale temp garbage").unwrap();

        let loaded = store.load().expect("load should read canonical file");
        assert_eq!(loaded, creds);

        let _ = fs::remove_file(&stale_temp);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_canonical_is_not_recovered_from_valid_temp() {
        let dir = unique_test_dir("no_temp_recovery");
        let canonical = dir.join("credentials.json");
        let store = CredentialsFileStore::new(&canonical);

        // Write corrupt canonical file
        fs::write(&canonical, b"corrupt credentials data").unwrap();

        // Write valid temp file
        let valid_creds = sample_valid_credentials();
        let valid_json = encode_credentials_json_pretty(&valid_creds).unwrap();
        let temp_file = dir.join(".credentials.json.1111.22222.1.tmp");
        fs::write(&temp_file, valid_json.as_bytes()).unwrap();

        let err = store.load().unwrap_err();
        assert!(matches!(err, CredentialsStoreError::Persistence(_)));

        let _ = fs::remove_file(&temp_file);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_in_memory_save_leaves_canonical_byte_identical() {
        let dir = unique_test_dir("invalid_save_leaves_canonical_unmodified");
        let canonical = dir.join("credentials.json");
        let store = CredentialsFileStore::new(&canonical);

        let valid_creds = sample_valid_credentials();
        store.save(&valid_creds).expect("save valid should succeed");

        let original_bytes = fs::read(&canonical).expect("read original canonical");

        // Attempt saving invalid in-memory credentials (empty telegram_bot_token_dpapi)
        let invalid_creds = PersistentCredentials {
            pin_hash: SAMPLE_VALID_PIN_HASH.to_string(),
            telegram_bot_token_dpapi: Vec::new(),
        };
        let err = store.save(&invalid_creds).unwrap_err();
        assert!(matches!(err, CredentialsStoreError::Persistence(_)));

        let after_bytes = fs::read(&canonical).expect("read canonical after failed save");
        assert_eq!(
            original_bytes, after_bytes,
            "canonical file must remain byte-identical"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn successful_save_leaves_no_own_temp_candidate() {
        let dir = unique_test_dir("no_temp_candidates_left");
        let canonical = dir.join("credentials.json");
        let store = CredentialsFileStore::new(&canonical);

        let creds = sample_valid_credentials();
        store.save(&creds).expect("save should succeed");

        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();

        assert_eq!(entries, vec!["credentials.json"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_candidate_is_in_same_directory() {
        let dir = unique_test_dir("temp_file_in_same_dir");
        let parent = dir.as_path();
        let file_name = std::ffi::OsStr::new("credentials.json");

        let (temp_path, file) = create_temp_file(parent, file_name).unwrap();
        drop(file);

        assert_eq!(temp_path.parent().unwrap(), parent);
        assert!(
            temp_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".credentials.json.")
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
    fn invalid_target_parent_conditions_return_controlled_error() {
        let dir = unique_test_dir("invalid_target_parent");
        // Target path is a directory itself
        let store = CredentialsFileStore::new(&dir);
        let creds = sample_valid_credentials();

        let err = store.save(&creds).unwrap_err();
        match err {
            CredentialsStoreError::Publish(_) | CredentialsStoreError::Io(_) => {}
            other => {
                panic!("expected Publish or Io error when target is directory, got: {other:?}")
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
