//! `source` fixtures: `tests/fixtures/source/<case>/`.
//!
//! Local-directory resolution runs end-to-end through the public [`templatry::source::resolve`]
//! (offline-safe: local sources never touch the cache or network). Remote
//! fetchers are covered by fixture-based unit tests inside `src/source.rs`
//! (release-response parsing, asset-glob selection, archive extraction) so the
//! suite stays offline.

mod common;

use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    common::fixtures_root().join("source").join(name)
}

#[test]
fn all_source_fixtures_are_known() {
    let names: Vec<String> = common::cases("source")
        .iter()
        .map(|path| {
            path.file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["local-basic"]);
}

#[tokio::test]
async fn local_source_resolves_end_to_end() {
    let root = fixture("local-basic");
    let project_file = root.join(".config").join("templatry.toml");
    let content = std::fs::read_to_string(&project_file).expect("read fixture project file");
    let project: templatry::config::ProjectConfig =
        toml::from_str(&content).expect("parse fixture project file");
    let project_dir = project_file.parent().expect("project dir").to_path_buf();

    let first = templatry::source::resolve(&project.source, &project_dir, true)
        .await
        .expect("local resolves offline");
    assert!(!first.from_cache);
    assert_eq!(
        first.root_dir,
        root.join("templates").canonicalize().expect("canonicalize")
    );
    assert_eq!(first.source_id.len(), 64);

    let second = templatry::source::resolve(&project.source, &project_dir, true)
        .await
        .expect("local resolves again");
    assert_eq!(first.source_id, second.source_id);
}
