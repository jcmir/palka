//! Service daemon entry point for PALKA.
//!
//! Minimal composition root delegating to the Windows SCM service dispatcher.

use palka_service::service_integration::palka_service_entry;
use palka_windows_platform::scm_runtime::run_palka_service_dispatcher;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_palka_service_dispatcher(palka_service_entry)?;
    Ok(())
}
