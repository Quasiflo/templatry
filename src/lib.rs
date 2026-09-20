//! Templatry library: template retrieval, merging, generation, watching, and validation.
//!
//! Module map (see `ROADMAP.md` for the full plan):
//!
//! - [`config`]: project and source configuration schemas (Milestone 1).
//! - [`source`]: template source retrieval, caching, and integrity (Milestone 2).
//! - [`merge`]: structured and text merge strategies (Milestone 3).
//! - [`generate`]: generation orchestration, check/dry-run diffing (Milestone 3).
//! - [`watch`]: file watching and regeneration dispatch (Milestone 4).
//! - [`backprop`]: two-way sync folding generated edits into overrides (Milestone 5).
//! - [`validate`]: project and source validation diagnostics (Milestone 1).

use std::path::Path;

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

    /// A config file that is not valid TOML.
    #[error("invalid TOML in {path}: {message}")]
    #[diagnostic(code(templatry::config::parse))]
    Parse {
        /// File that failed to parse.
        path: String,
        /// Parser message, with line/column when the parser reports a span.
        message: String,
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

    /// `generate --check` found differences between generated output and disk.
    ///
    /// The differing paths are printed during the check; the binary maps this
    /// to exit code 2 for CI consumption.
    #[error("generated output differs in {count} file(s)")]
    #[diagnostic(code(templatry::check::diff))]
    CheckDifferences {
        /// Number of differing files (already printed).
        count: usize,
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

/// Parse TOML config, mapping syntax failures to [`Error::Parse`] with line/column.
pub(crate) fn parse_toml<T>(path: &Path, content: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    toml::from_str(content).map_err(|err: toml::de::Error| {
        let message = match err.span() {
            Some(span) => {
                let (line, column) = line_column(content, span.start);
                format!("{} (line {line}, column {column})", err.message())
            }
            None => err.message().to_string(),
        };
        Error::Parse {
            path: path.display().to_string(),
            message,
        }
    })
}

/// 1-based line/column for a byte offset (offsets past the end clamp safely).
fn line_column(content: &str, offset: usize) -> (usize, usize) {
    let prefix = content.get(..offset).unwrap_or(content);
    let line = prefix.bytes().filter(|&byte| byte == b'\n').count() + 1;
    let column = prefix
        .rsplit('\n')
        .next()
        .map(|fragment| fragment.chars().count() + 1)
        .unwrap_or(1);
    (line, column)
}

/// Read a config file as text, mapping IO failures to diagnostics.
pub(crate) fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|err| invalid(path, format!("cannot read file: {err}")))
}

/// Read a file as bytes, mapping IO failures to diagnostics.
pub(crate) fn read_file_bytes(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|err| invalid(path, format!("cannot read file: {err}")))
}

pub mod backprop;
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

    #[test]
    fn line_column_counts_from_one() {
        let content = "ab\ncde\n\nf";
        assert_eq!(line_column(content, 0), (1, 1));
        assert_eq!(line_column(content, 2), (1, 3));
        assert_eq!(line_column(content, 3), (2, 1));
        assert_eq!(line_column(content, 7), (3, 1));
        assert_eq!(line_column(content, 8), (4, 1));
        // Past-the-end offsets clamp instead of panicking.
        assert_eq!(line_column(content, 100), (4, 2));
    }
}
