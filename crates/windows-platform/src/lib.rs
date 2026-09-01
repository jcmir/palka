//! Windows platform adapter crate for PALKA.

pub mod atomic_file;

pub use atomic_file::{AtomicPublishError, atomic_publish_file};
