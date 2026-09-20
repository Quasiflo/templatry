//! Project (`templatry.toml`) and source (`templatry.source.toml`) configuration schemas.
//!
//! Full schemas land in Milestone 1; this module currently only locks the
//! well-known file locations decided in `ROADMAP.md`.

/// Project configuration path, relative to the project repository root.
pub const PROJECT_CONFIG_PATH: &str = ".config/templatry.toml";

/// Source configuration filename, relative to the source root (or `root` subtree).
pub const SOURCE_CONFIG_FILENAME: &str = "templatry.source.toml";
