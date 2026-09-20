//! Templatry library: template retrieval, merging, generation, watching, and validation.
//!
//! Module map (see `ROADMAP.md` for the full plan):
//!
//! - [`config`]: project and source configuration schemas (Milestone 1).
//! - [`source`]: template source retrieval, caching, and integrity (Milestone 2).
//! - [`merge`]: structured and text merge strategies (Milestone 3).
//! - [`generate`]: generation orchestration, check/dry-run, back-propagation (Milestones 3, 5).
//! - [`watch`]: file watching and regeneration dispatch (Milestone 4).
//! - [`validate`]: project and source validation diagnostics (Milestone 1).

use miette::Diagnostic;

/// Shared result type for fallible templatry operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised by templatry operations.
///
/// Diagnostics carry machine-readable codes (`templatry::<name>`) so CLI and
/// hook consumers can match on failure kinds. New variants arrive with the
/// milestone that introduces the failure mode.
#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    /// A code path whose milestone has not been built yet.
    #[error("not yet implemented: {0}")]
    #[diagnostic(code(templatry::unimplemented))]
    Unimplemented(String),

    /// Invalid command-line usage detected after parsing.
    #[error("{0}")]
    #[diagnostic(code(templatry::usage))]
    Usage(String),
}

impl Error {
    /// Build an [`Error::Unimplemented`] for the named milestone area.
    pub fn unimplemented(what: impl Into<String>) -> Self {
        Self::Unimplemented(what.into())
    }
}

pub mod config;
pub mod generate;
pub mod merge;
pub mod source;
pub mod validate;
pub mod watch;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unimplemented_error_names_the_area() {
        assert_eq!(
            Error::unimplemented("generate").to_string(),
            "not yet implemented: generate"
        );
    }
}
