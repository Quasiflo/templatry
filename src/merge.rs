//! Structured and text merge strategies (Milestone 3).
//!
//! Deep merge with `union`/`replace` array policies, the `_TEMPLATRY_DELETE_`
//! value marker, `append_top`/`append_bottom`/`replace` text strategies, and
//! shared-destination additive combination all land here.

/// Override value marker deleting the key from merged output (Milestone 3).
///
/// All current and future markers share the `_TEMPLATRY_` prefix plus trailing
/// underscore convention.
pub const DELETE_MARKER: &str = "_TEMPLATRY_DELETE_";
