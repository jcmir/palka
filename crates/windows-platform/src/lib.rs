//! Windows platform adapter crate for PALKA.

pub mod atomic_file;
pub mod protected_directory;

pub use atomic_file::{AtomicPublishError, atomic_publish_file};
pub use protected_directory::{
    PROTECTED_DIRECTORY_SDDL, ProtectedDirectoryError, ensure_protected_directory,
};
