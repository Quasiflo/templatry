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

/// Copy a fixture case to a fresh tempdir for generation tests.
///
/// Returns the tempdir (keep it alive) plus the case root. Fixture-local
/// relative paths (e.g. `path = "../templates"`) keep working after the copy.
pub fn setup_case(suite: &str, case: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join(case);
    copy_dir(&fixtures_root().join(suite).join(case), &root);
    (temp, root)
}

/// Recursive directory copy (files and directories).
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create case dir");
    let entries = std::fs::read_dir(from).expect("read fixture dir");
    for entry in entries {
        let entry = entry.expect("dir entry");
        let source = entry.path();
        let dest = to.join(entry.file_name());
        if source.is_dir() {
            copy_dir(&source, &dest);
        } else {
            std::fs::copy(&source, &dest).expect("copy fixture file");
        }
    }
}

/// Assert every file under `expected/` matches the same relative path under `root`.
#[track_caller]
pub fn assert_tree_matches(root: &Path, expected: &Path) {
    let mut compared = 0;
    assert_tree_matches_inner(root, expected, expected, &mut compared);
    assert!(compared > 0, "no golden files under {}", expected.display());
}

fn assert_tree_matches_inner(
    root: &Path,
    expected_root: &Path,
    current: &Path,
    compared: &mut usize,
) {
    let entries = std::fs::read_dir(current).expect("read expected dir");
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            assert_tree_matches_inner(root, expected_root, &path, compared);
            continue;
        }
        let relative = path
            .strip_prefix(expected_root)
            .expect("expected-relative path");
        let actual = std::fs::read(root.join(relative)).unwrap_or_else(|_| {
            panic!("missing generated file: {}", relative.display());
        });
        let wanted = std::fs::read(&path).expect("read golden file");
        assert_eq!(
            actual,
            wanted,
            "mismatch in generated file: {}",
            relative.display()
        );
        *compared += 1;
    }
}

/// Assert relative paths listed in `<case>/absent.txt` were not generated.
///
/// The list file is optional; blank lines and `#` comments are skipped.
#[track_caller]
pub fn assert_absent(root: &Path, case_source: &Path) {
    let list = case_source.join("absent.txt");
    if !list.is_file() {
        return;
    }
    let content = std::fs::read_to_string(&list).expect("read absent.txt");
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        assert!(!root.join(line).exists(), "file should be absent: {line}");
    }
}
