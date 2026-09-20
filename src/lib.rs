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

use std::path::Path;

use miette::{Diagnostic, NamedSource, SourceSpan};

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

    /// A config file that is not valid TOML.
    #[error("invalid TOML in {path}: {message}")]
    #[diagnostic(code(templatry::config::parse))]
    Parse {
        /// File that failed to parse.
        path: String,
        /// Parser message.
        message: String,
        /// File contents for span rendering.
        #[source_code]
        src: NamedSource<String>,
        /// Offending span, when the parser reports one.
        #[label("here")]
        span: Option<SourceSpan>,
    },

    /// A config file that parses but violates schema or consistency rules.
    #[error("invalid configuration in {path}: {message}")]
    #[diagnostic(code(templatry::config::invalid))]
    Invalid {
        /// File (or directory) at fault.
        path: String,
        /// What is wrong, with one suggested fix.
        message: String,
    },
}

impl Error {
    /// Build an [`Error::Unimplemented`] for the named milestone area.
    pub fn unimplemented(what: impl Into<String>) -> Self {
        Self::Unimplemented(what.into())
    }
}

/// Build an [`Error::Invalid`] naming the file (or directory) at fault.
pub(crate) fn invalid(path: &Path, message: impl Into<String>) -> Error {
    Error::Invalid {
        path: path.display().to_string(),
        message: message.into(),
    }
}

/// Parse TOML config, mapping syntax failures to [`Error::Parse`] with spans.
pub(crate) fn parse_toml<T>(path: &Path, content: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    toml::from_str(content).map_err(|err: toml::de::Error| {
        let span = err
            .span()
            .map(|range| SourceSpan::new(range.start.into(), range.len()));
        Error::Parse {
            path: path.display().to_string(),
            message: err.message().to_string(),
            src: NamedSource::new(path.display().to_string(), content.to_string()),
            span,
        }
    })
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
