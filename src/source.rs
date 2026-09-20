//! Template source retrieval, caching, and integrity (Milestone 2).
//!
//! Fetchers for the four v1 source kinds (local directory, GitHub release,
//! generic URL archive, git checkout) plus the content-addressed cache land
//! here. The cache constants below are already final per `ROADMAP.md`.

/// Directory name for cached sources inside the platform cache dir.
pub const CACHE_DIR_NAME: &str = "templatry";

/// Per-entry sidecar filename: JSON with `last_used_unix` and `resolved_sha`.
pub const ENTRY_SIDECAR_FILENAME: &str = ".templatry-meta.json";

/// Flush the entire cache; the next run re-pulls all sources.
pub fn cache_clear() -> crate::Result<()> {
    Err(crate::Error::unimplemented("templatry cache clear"))
}
