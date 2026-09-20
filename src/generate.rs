//! Generation orchestration, check/dry-run diffing, and back-propagation.
//!
//! One-shot generation and `--check` land in Milestone 3; full two-way sync
//! with the in-memory equivalence safety check lands in Milestone 5.

use std::path::PathBuf;

/// Options for [`run`], mirroring `templatry generate` flags.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Keep watching and regenerate on change (Milestone 4).
    pub watch: bool,
    /// Diff against disk without writing; exit 2 on difference.
    pub check: bool,
    /// Print planned writes without touching disk.
    pub dry_run: bool,
    /// Project config other than [`.config/templatry.toml`](crate::config::PROJECT_CONFIG_PATH).
    pub config: Option<PathBuf>,
    /// Never touch the network; fail on cache miss.
    pub offline: bool,
}

/// Conflict snapshot directory name inside the system temp dir.
///
/// On back-propagation safety-check mismatch, the conflicting generated
/// content is recoverable here at `<stem>.<timestamp>.conflict.<ext>`.
pub const CONFLICT_SNAPSHOT_DIR_NAME: &str = "templatry-conflicts";

/// Generate configuration files for the current project.
pub fn run(_options: &Options) -> crate::Result<()> {
    Err(crate::Error::unimplemented("templatry generate"))
}
