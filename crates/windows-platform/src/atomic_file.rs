//! Atomic file publication primitive for Windows.

use std::fmt;
use std::path::{Path, PathBuf};

/// Error returned when atomic file publication fails.
#[derive(Debug)]
pub enum AtomicPublishError {
    ParentMismatch {
        temp_parent: Option<PathBuf>,
        target_parent: Option<PathBuf>,
    },
    InvalidPath(String),
    WindowsApi {
        function: &'static str,
        code: u32,
        message: String,
    },
    Io(std::io::Error),
}

impl fmt::Display for AtomicPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentMismatch {
                temp_parent,
                target_parent,
            } => {
                write!(
                    f,
                    "Temporary and target files must be in the same directory: temp_parent={temp_parent:?}, target_parent={target_parent:?}"
                )
            }
            Self::InvalidPath(msg) => write!(f, "Invalid path: {msg}"),
            Self::WindowsApi {
                function,
                code,
                message,
            } => {
                write!(
                    f,
                    "Windows API call '{function}' failed with error code {code} (0x{code:08X}): {message}"
                )
            }
            Self::Io(err) => write!(f, "I/O error during atomic publish: {err}"),
        }
    }
}

impl std::error::Error for AtomicPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AtomicPublishError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Atomically publishes `temp_path` to `target_path` on Windows.
///
/// Both `temp_path` and `target_path` must reside in the same parent directory.
/// If `target_path` already exists, `ReplaceFileW` with zero replace flags is used.
/// If `target_path` does not exist, `MoveFileExW` with `MOVEFILE_WRITE_THROUGH` is used.
#[cfg(windows)]
pub fn atomic_publish_file(temp_path: &Path, target_path: &Path) -> Result<(), AtomicPublishError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACE_FILE_FLAGS, ReplaceFileW,
    };
    use windows::core::PCWSTR;

    let temp_parent = temp_path.parent().map(|p| p.to_path_buf());
    let target_parent = target_path.parent().map(|p| p.to_path_buf());

    if temp_parent != target_parent {
        return Err(AtomicPublishError::ParentMismatch {
            temp_parent,
            target_parent,
        });
    }

    let mut temp_wide: Vec<u16> = temp_path.as_os_str().encode_wide().collect();
    temp_wide.push(0);

    let mut target_wide: Vec<u16> = target_path.as_os_str().encode_wide().collect();
    target_wide.push(0);

    let temp_pcwstr = PCWSTR(temp_wide.as_ptr());
    let target_pcwstr = PCWSTR(target_wide.as_ptr());

    if target_path.exists() {
        // Target exists -> atomically replace target with temp using zero flags
        let res = unsafe {
            ReplaceFileW(
                target_pcwstr,
                temp_pcwstr,
                PCWSTR::null(),
                REPLACE_FILE_FLAGS(0),
                None,
                None,
            )
        };

        if let Err(err) = res {
            return Err(AtomicPublishError::WindowsApi {
                function: "ReplaceFileW",
                code: err.code().0 as u32,
                message: err.message(),
            });
        }
    } else {
        // Target does not exist -> initial atomic move with write-through
        let res = unsafe { MoveFileExW(temp_pcwstr, target_pcwstr, MOVEFILE_WRITE_THROUGH) };

        if let Err(err) = res {
            return Err(AtomicPublishError::WindowsApi {
                function: "MoveFileExW",
                code: err.code().0 as u32,
                message: err.message(),
            });
        }
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn atomic_publish_file(temp_path: &Path, target_path: &Path) -> Result<(), AtomicPublishError> {
    let temp_parent = temp_path.parent().map(|p| p.to_path_buf());
    let target_parent = target_path.parent().map(|p| p.to_path_buf());

    if temp_parent != target_parent {
        return Err(AtomicPublishError::ParentMismatch {
            temp_parent,
            target_parent,
        });
    }

    std::fs::rename(temp_path, target_path).map_err(AtomicPublishError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;

    fn setup_test_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("palka_atomic_test_{name}_{}", std::process::id()));
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn initial_publication_creates_target_and_removes_temp() {
        let dir = setup_test_dir("initial_pub");
        let temp_path = dir.join(".state.json.tmp");
        let target_path = dir.join("state.json");

        {
            let mut file = File::create(&temp_path).unwrap();
            file.write_all(b"initial state content").unwrap();
            file.sync_all().unwrap();
        }

        assert!(temp_path.exists());
        assert!(!target_path.exists());

        atomic_publish_file(&temp_path, &target_path).expect("publish should succeed");

        assert!(!temp_path.exists());
        assert!(target_path.exists());
        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            "initial state content"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacement_updates_existing_target_and_removes_temp() {
        let dir = setup_test_dir("replacement");
        let temp_path = dir.join(".state.json.tmp");
        let target_path = dir.join("state.json");

        // Existing target
        fs::write(&target_path, "old target content").unwrap();

        // New temp candidate
        {
            let mut file = File::create(&temp_path).unwrap();
            file.write_all(b"new state content").unwrap();
            file.sync_all().unwrap();
        }

        assert!(temp_path.exists());
        assert!(target_path.exists());

        atomic_publish_file(&temp_path, &target_path).expect("replacement should succeed");

        assert!(!temp_path.exists());
        assert!(target_path.exists());
        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            "new state content"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacement_failure_preserves_existing_target() {
        let dir = setup_test_dir("replacement_fail");
        let temp_path = dir.join(".nonexistent.tmp");
        let target_path = dir.join("state.json");

        fs::write(&target_path, "OLD canonical content").unwrap();

        assert!(!temp_path.exists());
        assert!(target_path.exists());

        let res = atomic_publish_file(&temp_path, &target_path);
        assert!(res.is_err());

        assert!(target_path.exists());
        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            "OLD canonical content"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_parent_directories_rejected() {
        let dir1 = setup_test_dir("parent_diff_1");
        let dir2 = setup_test_dir("parent_diff_2");

        let temp_path = dir1.join(".state.json.tmp");
        let target_path = dir2.join("state.json");

        fs::write(&temp_path, "temp content").unwrap();

        let err = atomic_publish_file(&temp_path, &target_path).unwrap_err();
        match err {
            AtomicPublishError::ParentMismatch { .. } => {}
            other => panic!("expected ParentMismatch error, got: {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir1);
        let _ = fs::remove_dir_all(&dir2);
    }
}
