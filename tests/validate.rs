//! `validate` orchestration fixtures: `tests/fixtures/validate/<case>/`.
//!
//! Each case is a fake repository root validated via [`templatry::validate::run_in`].
//! The inventory test below locks the case list so new behavior always arrives
//! with a fixture.

mod common;

use std::path::PathBuf;

use templatry::validate;

fn fixture(name: &str) -> PathBuf {
    common::fixtures_root().join("validate").join(name)
}

#[test]
fn all_validate_fixtures_are_known() {
    let names: Vec<String> = common::cases("validate")
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
            "project-both-present",
            "project-empty",
            "project-valid-local",
            "source-invalid",
            "source-valid",
        ]
    );
}

#[tokio::test]
async fn valid_source_config_passes() {
    validate::run_in(&fixture("source-valid"), None)
        .await
        .expect("valid source config");
}

#[tokio::test]
async fn invalid_source_config_fails() {
    let err = validate::run_in(&fixture("source-invalid"), None)
        .await
        .expect_err("mutex violation");
    assert!(err.to_string().contains("sets both"), "{err:?}");
}

#[tokio::test]
async fn valid_local_project_passes() {
    validate::run_in(&fixture("project-valid-local"), None)
        .await
        .expect("valid local project");
}

#[tokio::test]
async fn both_configs_present_fails() {
    let err = validate::run_in(&fixture("project-both-present"), None)
        .await
        .expect_err("ambiguity");
    assert!(err.to_string().contains("both"), "{err:?}");
}

#[tokio::test]
async fn missing_configs_fails() {
    let err = validate::run_in(&fixture("project-empty"), None)
        .await
        .expect_err("no config");
    assert!(err.to_string().contains("neither"), "{err:?}");
}
