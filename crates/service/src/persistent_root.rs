//! Persistent root bootstrap and authoritative canonical path model for `palka-service`.
//!
//! Provides `%ProgramData%\Palka` persistent root resolution, canonical path accessors,
//! and platform-authoritative directory hardening via `palka-windows-platform`.

use palka_windows_platform::ProtectedDirectoryError;
use std::fmt;
use std::path::{Path, PathBuf};

/// Name of the PALKA persistent root directory under `ProgramData`.
const PALKA_DATA_DIR_NAME: &str = "Palka";
/// Name of the canonical configuration file.
const CONFIG_FILE_NAME: &str = "config.json";
/// Name of the canonical credentials file.
const CREDENTIALS_FILE_NAME: &str = "credentials.json";
/// Name of the canonical state file.
const STATE_FILE_NAME: &str = "state.json";

/// Errors occurring during persistent root bootstrap or path resolution.
#[derive(Debug)]
pub enum PersistentRootError {
    /// System `ProgramData` environment variable is not present.
    ProgramDataUnavailable,
    /// `ProgramData` path is empty, relative, or invalid.
    InvalidProgramDataPath(String),
    /// Platform primitive failure during directory creation or DACL hardening.
    Platform(ProtectedDirectoryError),
    /// Production bootstrap called on an unsupported platform.
    UnsupportedPlatform,
}

impl fmt::Display for PersistentRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProgramDataUnavailable => {
                write!(f, "ProgramData environment variable is unavailable")
            }
            Self::InvalidProgramDataPath(msg) => {
                write!(f, "Invalid ProgramData path: {msg}")
            }
            Self::Platform(err) => {
                write!(f, "Platform directory protection error: {err}")
            }
            Self::UnsupportedPlatform => {
                write!(
                    f,
                    "Persistent root bootstrap is unsupported on this platform"
                )
            }
        }
    }
}

impl std::error::Error for PersistentRootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Platform(err) => Some(err),
            _ => None,
        }
    }
}

impl From<ProtectedDirectoryError> for PersistentRootError {
    fn from(err: ProtectedDirectoryError) -> Self {
        Self::Platform(err)
    }
}

/// Authoritative canonical persistent paths for the PALKA service daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentPaths {
    root: PathBuf,
    config: PathBuf,
    credentials: PathBuf,
    state: PathBuf,
}

impl PersistentPaths {
    /// Constructs a `PersistentPaths` object from a verified `ProgramData` base path.
    fn from_program_data_base(program_data_base: &Path) -> Result<Self, PersistentRootError> {
        if program_data_base.as_os_str().is_empty() {
            return Err(PersistentRootError::InvalidProgramDataPath(
                "ProgramData base path cannot be empty".to_string(),
            ));
        }

        if !program_data_base.is_absolute() {
            return Err(PersistentRootError::InvalidProgramDataPath(
                "ProgramData base path must be absolute".to_string(),
            ));
        }

        let root = program_data_base.join(PALKA_DATA_DIR_NAME);
        let config = root.join(CONFIG_FILE_NAME);
        let credentials = root.join(CREDENTIALS_FILE_NAME);
        let state = root.join(STATE_FILE_NAME);

        Ok(Self {
            root,
            config,
            credentials,
            state,
        })
    }

    /// Read-only accessor for the persistent root directory (`%ProgramData%\Palka`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read-only accessor for the canonical configuration file (`%ProgramData%\Palka\config.json`).
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// Read-only accessor for the canonical credentials file (`%ProgramData%\Palka\credentials.json`).
    pub fn credentials(&self) -> &Path {
        &self.credentials
    }

    /// Read-only accessor for the canonical state file (`%ProgramData%\Palka\state.json`).
    pub fn state(&self) -> &Path {
        &self.state
    }
}

/// Bootstraps the authoritative persistent root directory under `%ProgramData%\Palka`.
///
/// Ensures the root directory exists and is secured with the normative DACL
/// via `palka_windows_platform::ensure_protected_directory`.
///
/// On non-Windows platforms in production, returns `Err(PersistentRootError::UnsupportedPlatform)`.
pub fn bootstrap_persistent_root() -> Result<PersistentPaths, PersistentRootError> {
    #[cfg(windows)]
    {
        let program_data =
            std::env::var_os("ProgramData").ok_or(PersistentRootError::ProgramDataUnavailable)?;

        let base_path = PathBuf::from(program_data);
        bootstrap_at_program_data_base(&base_path)
    }

    #[cfg(not(windows))]
    {
        Err(PersistentRootError::UnsupportedPlatform)
    }
}

/// Internal testable bootstrap helper that operates under a specified `ProgramData` base directory.
fn bootstrap_at_program_data_base(
    program_data_base: &Path,
) -> Result<PersistentPaths, PersistentRootError> {
    let paths = PersistentPaths::from_program_data_base(program_data_base)?;
    palka_windows_platform::ensure_protected_directory(paths.root())?;
    Ok(paths)
}

#[cfg(test)]
pub(crate) fn canonical_paths_for_test(
    program_data_base: &Path,
) -> Result<PersistentPaths, PersistentRootError> {
    PersistentPaths::from_program_data_base(program_data_base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TestTempDir {
        path: PathBuf,
    }

    impl TestTempDir {
        fn new(test_name: &str) -> Self {
            let unique_id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let temp_dir = std::env::temp_dir();
            let path = temp_dir.join(format!("palka_test_root_{test_name}_{pid}_{unique_id}"));
            fs::create_dir_all(&path).expect("failed to create test temp base dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let root = self.path.join(PALKA_DATA_DIR_NAME);
            let _ = fs::remove_dir(&root);
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn canonical_layout_construction_from_absolute_base() {
        let temp_dir = std::env::temp_dir();
        let base = temp_dir.join("test_program_data");
        let paths = PersistentPaths::from_program_data_base(&base).expect("valid base path");

        assert_eq!(paths.root(), base.join("Palka"));
        assert_eq!(paths.config(), base.join("Palka").join("config.json"));
        assert_eq!(
            paths.credentials(),
            base.join("Palka").join("credentials.json")
        );
        assert_eq!(paths.state(), base.join("Palka").join("state.json"));
    }

    #[test]
    fn filenames_exact() {
        assert_eq!(PALKA_DATA_DIR_NAME, "Palka");
        assert_eq!(CONFIG_FILE_NAME, "config.json");
        assert_eq!(CREDENTIALS_FILE_NAME, "credentials.json");
        assert_eq!(STATE_FILE_NAME, "state.json");

        let temp_dir = std::env::temp_dir();
        let paths = PersistentPaths::from_program_data_base(&temp_dir).unwrap();

        assert_eq!(
            paths.root().file_name().and_then(|s| s.to_str()),
            Some("Palka")
        );
        assert_eq!(
            paths.config().file_name().and_then(|s| s.to_str()),
            Some("config.json")
        );
        assert_eq!(
            paths.credentials().file_name().and_then(|s| s.to_str()),
            Some("credentials.json")
        );
        assert_eq!(
            paths.state().file_name().and_then(|s| s.to_str()),
            Some("state.json")
        );
    }

    #[test]
    fn distinct_canonical_paths() {
        let temp_dir = std::env::temp_dir();
        let paths = PersistentPaths::from_program_data_base(&temp_dir).unwrap();

        assert_ne!(paths.config(), paths.credentials());
        assert_ne!(paths.config(), paths.state());
        assert_ne!(paths.credentials(), paths.state());
    }

    #[test]
    fn every_canonical_file_parent_is_root() {
        let temp_dir = std::env::temp_dir();
        let paths = PersistentPaths::from_program_data_base(&temp_dir).unwrap();

        assert_eq!(paths.config().parent(), Some(paths.root()));
        assert_eq!(paths.credentials().parent(), Some(paths.root()));
        assert_eq!(paths.state().parent(), Some(paths.root()));
    }

    #[test]
    fn relative_program_data_is_rejected() {
        let rel_path = Path::new("relative/program_data");
        let res = PersistentPaths::from_program_data_base(rel_path);
        assert!(matches!(
            res,
            Err(PersistentRootError::InvalidProgramDataPath(_))
        ));

        let res_bootstrap = bootstrap_at_program_data_base(rel_path);
        assert!(matches!(
            res_bootstrap,
            Err(PersistentRootError::InvalidProgramDataPath(_))
        ));
    }

    #[test]
    fn empty_program_data_is_rejected() {
        let empty_path = Path::new("");
        let res = PersistentPaths::from_program_data_base(empty_path);
        assert!(matches!(
            res,
            Err(PersistentRootError::InvalidProgramDataPath(_))
        ));

        let res_bootstrap = bootstrap_at_program_data_base(empty_path);
        assert!(matches!(
            res_bootstrap,
            Err(PersistentRootError::InvalidProgramDataPath(_))
        ));
    }

    #[test]
    fn bootstrap_creates_root_directory_when_parent_exists() {
        let temp_env = TestTempDir::new("create_root");
        let paths =
            bootstrap_at_program_data_base(temp_env.path()).expect("bootstrap should succeed");

        assert!(paths.root().is_dir(), "Root must be created as a directory");
    }

    #[test]
    fn bootstrap_does_not_create_any_canonical_json_files() {
        let temp_env = TestTempDir::new("no_json_files");
        let paths =
            bootstrap_at_program_data_base(temp_env.path()).expect("bootstrap should succeed");

        assert!(
            !paths.config().exists(),
            "config.json must not be created by bootstrap"
        );
        assert!(
            !paths.credentials().exists(),
            "credentials.json must not be created by bootstrap"
        );
        assert!(
            !paths.state().exists(),
            "state.json must not be created by bootstrap"
        );
    }

    #[test]
    fn bootstrap_leaves_newly_created_root_with_zero_directory_entries() {
        let temp_env = TestTempDir::new("empty_root");
        let paths =
            bootstrap_at_program_data_base(temp_env.path()).expect("bootstrap should succeed");

        assert!(paths.root().is_dir());
        assert!(!paths.config().exists());
        assert!(!paths.credentials().exists());
        assert!(!paths.state().exists());

        match fs::read_dir(paths.root()) {
            Ok(entries) => {
                assert_eq!(
                    entries.count(),
                    0,
                    "Root directory must contain zero entries"
                );
            }
            Err(err) => {
                #[cfg(windows)]
                assert_eq!(
                    err.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "Read access denied confirms non-elevated user is blocked by protected DACL"
                );
                #[cfg(not(windows))]
                panic!("read_dir failed unexpectedly: {err}");
            }
        }
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let temp_env = TestTempDir::new("idempotent");
        let paths1 = bootstrap_at_program_data_base(temp_env.path()).expect("first bootstrap");
        let paths2 = bootstrap_at_program_data_base(temp_env.path()).expect("second bootstrap");

        assert_eq!(paths1, paths2);
        assert!(paths1.root().is_dir());
        assert!(!paths1.config().exists());
        assert!(!paths1.credentials().exists());
        assert!(!paths1.state().exists());
    }

    #[test]
    fn existing_root_as_normal_directory_succeeds_and_is_reused() {
        let temp_env = TestTempDir::new("existing_dir");
        let root_dir = temp_env.path().join(PALKA_DATA_DIR_NAME);
        fs::create_dir(&root_dir).expect("create existing dir");

        let paths =
            bootstrap_at_program_data_base(temp_env.path()).expect("bootstrap over existing dir");
        assert_eq!(paths.root(), root_dir);
        assert!(paths.root().is_dir());
    }

    #[test]
    fn existing_root_as_file_returns_controlled_error() {
        let temp_env = TestTempDir::new("root_as_file");
        let root_file = temp_env.path().join(PALKA_DATA_DIR_NAME);
        fs::write(&root_file, b"i am a file not a directory").expect("write root file");

        let res = bootstrap_at_program_data_base(temp_env.path());
        assert!(
            matches!(res, Err(PersistentRootError::Platform(_))),
            "Existing root as file must return Platform error: {res:?}"
        );
    }

    #[test]
    fn missing_program_data_parent_returns_controlled_error() {
        let temp_env = TestTempDir::new("missing_parent");
        let non_existent_base = temp_env.path().join("non_existent_folder");
        // We do NOT create non_existent_base directory

        let res = bootstrap_at_program_data_base(&non_existent_base);
        assert!(
            matches!(res, Err(PersistentRootError::Platform(_))),
            "Missing ProgramData parent must return Platform error: {res:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn bootstrap_path_supports_reentrant_platform_hardening() {
        let temp_env = TestTempDir::new("reentrant_hardening");
        let paths = bootstrap_at_program_data_base(temp_env.path()).expect("bootstrap");

        // Verify that the created root supports re-entrant calls to ensure_protected_directory
        assert!(
            palka_windows_platform::ensure_protected_directory(paths.root()).is_ok(),
            "Re-invoking protected directory primitive must succeed"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_production_bootstrap_returns_unsupported_platform() {
        let res = bootstrap_persistent_root();
        assert!(matches!(res, Err(PersistentRootError::UnsupportedPlatform)));
    }
}
