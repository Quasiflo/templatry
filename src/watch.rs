//! File watching and regeneration dispatch (Milestone 4).
//!
//! `watchexec` subscription, per-path regeneration, config-change restart,
//! debounce, and the self-trigger write guard land here, following the
//! `smartworkspace` pattern.
