//! Shared gold-file test harness for integration tests.
//!
//! Fixture layout: `tests/fixtures/<suite>/<case>/`, where each case holds
//! whatever inputs the suite needs plus golden files holding expected output.
//! Suites arrive with their milestones (`generate` in Milestone 3); the
//! harness and discovery land here in Milestone 0.
//!
//! Set `BLESS=1` to (re)write golden files instead of comparing against them.
//! Always review `git diff` after blessing.

use std::path::{Path, PathBuf};

/// Repository-rooted fixtures directory: `<repo>/tests/fixtures`.
pub fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Sorted case directories for a suite: `<root>/<suite>/*/`.
///
/// Missing suite directories yield no cases, so suites can land incrementally.
pub fn cases(suite: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(fixtures_root().join(suite)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// Compare `actual` bytes against the golden file, or bless it with `BLESS=1`.
///
/// Unused until the first gold-file suite lands (Milestone 3).
#[allow(dead_code)]
#[track_caller]
pub fn assert_golden(actual: &[u8], golden_path: &Path) {
    if std::env::var_os("BLESS").is_some() {
        if let Some(parent) = golden_path.parent() {
            std::fs::create_dir_all(parent).expect("create golden parent dir");
        }
        std::fs::write(golden_path, actual).expect("bless golden file");
        return;
    }
    let expected = std::fs::read(golden_path).unwrap_or_else(|_| {
        panic!(
            "missing golden file: {} (run with BLESS=1 to create it, then review the diff)",
            golden_path.display()
        )
    });
    assert_eq!(
        actual,
        expected.as_slice(),
        "golden mismatch: {}",
        golden_path.display()
    );
}
