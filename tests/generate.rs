//! Gold-file generation fixtures: `tests/fixtures/generate/<case>/`.
//!
//! Each case is copied to a tempdir, generated into, and compared against
//! `expected/` (plus `absent.txt` for files that must not exist). The
//! inventory test locks the case list so new behavior arrives with a fixture.

// Shared harness: each suite uses a different subset of helpers.
#[allow(dead_code)]
mod common;

use std::path::{Path, PathBuf};

use templatry::generate::{self, Options};

fn options(root: &Path, customize: impl FnOnce(&mut Options)) -> Options {
    let mut options = Options {
        config: Some(root.join(".config").join("templatry.toml")),
        ..Default::default()
    };
    customize(&mut options);
    options
}

fn setup(case: &str) -> (tempfile::TempDir, PathBuf) {
    common::setup_case("generate", case)
}

fn source_case(case: &str) -> PathBuf {
    common::fixtures_root().join("generate").join(case)
}

#[test]
fn all_generate_fixtures_are_known() {
    let names: Vec<String> = common::cases("generate")
        .iter()
        .map(|path| {
            path.file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        [
            "json-merge",
            "labels",
            "replace-missing",
            "shared-conflict",
            "shared-destination",
            "strategies",
        ]
    );
}

#[tokio::test]
async fn golden_json_merge() {
    let (_temp, root) = setup("json-merge");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("json-merge").join("expected"));
}

#[tokio::test]
async fn golden_strategies() {
    let (_temp, root) = setup("strategies");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("strategies").join("expected"));
}

#[tokio::test]
async fn golden_labels() {
    let (_temp, root) = setup("labels");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("labels").join("expected"));
    common::assert_absent(&root, &source_case("labels"));
}

#[tokio::test]
async fn golden_shared_destination() {
    let (_temp, root) = setup("shared-destination");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("shared-destination").join("expected"));
}

#[tokio::test]
async fn shared_conflict_errors() {
    let (_temp, root) = setup("shared-conflict");
    let err = generate::run(&options(&root, |_| {}))
        .await
        .expect_err("conflicting leaves");
    let message = err.to_string();
    assert!(message.contains("conflicting values"), "{message}");
    assert!(message.contains("`editor.size`"), "{message}");
}

#[tokio::test]
async fn replace_missing_override_errors() {
    let (_temp, root) = setup("replace-missing");
    let err = generate::run(&options(&root, |_| {}))
        .await
        .expect_err("replace without override");
    assert!(
        err.to_string().contains("no override file exists"),
        "{err:?}"
    );
}

#[tokio::test]
async fn check_flow() {
    let (_temp, root) = setup("json-merge");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    generate::run(&options(&root, |options| options.check = true))
        .await
        .expect("clean check");

    let generated = root.join(".config").join("generated").join("settings.json");
    std::fs::write(&generated, "{}\n").expect("dirty a generated file");
    let err = generate::run(&options(&root, |options| options.check = true))
        .await
        .expect_err("dirty check");
    assert!(
        matches!(err, templatry::Error::CheckDifferences { count: 1 }),
        "{err:?}"
    );
}

#[tokio::test]
async fn dry_run_writes_nothing() {
    let (_temp, root) = setup("json-merge");
    generate::run(&options(&root, |options| options.dry_run = true))
        .await
        .expect("dry run");
    assert!(!root.join(".config").join("generated").exists());
}
