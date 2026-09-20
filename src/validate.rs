//! Project and source validation diagnostics (Milestone 1).
//!
//! `templatry validate` auto-detects source versus project context, resolves
//! remote sources for project validation, and reports file/table/key-level
//! diagnostics with one suggested fix each.

use std::path::Path;

/// Validate the configuration in scope, auto-detecting source vs project context.
pub fn run(_config: Option<&Path>) -> crate::Result<()> {
    Err(crate::Error::unimplemented("templatry validate"))
}
