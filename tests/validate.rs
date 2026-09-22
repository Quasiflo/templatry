//! `validate` orchestration fixtures: `tests/fixtures/validate/<case>/`.
//!
//! Each case is a fake repository root validated via [`templatry::validate::run_in`].
//! The inventory test below locks the case list so new behavior always arrives
//! with a fixture.

// Shared harness: each suite uses a different subset of helpers.
#[allow(dead_code)]
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
            "project-both-invalid-source",
            "project-both-present",
            "project-empty",
            "project-mixed-forms",
            "project-multi-backprop",
            "project-multi-shadowed",
            "project-multi-valid",
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
async fn both_configs_present_validates_both() {
    // A template source that is also a templatry project: both halves must
    // pass for the repository to validate.
    validate::run_in(&fixture("project-both-present"), None)
        .await
        .expect("dual-role repository validates");
}

#[tokio::test]
async fn both_configs_present_fails_on_either_half() {
    // The project half is valid here, so the broken source half must still
    // fail the run (neither half is skipped).
    let err = validate::run_in(&fixture("project-both-invalid-source"), None)
        .await
        .expect_err("broken source half");
    assert!(err.to_string().contains("missing.json"), "{err:?}");
}

#[tokio::test]
async fn missing_configs_fails() {
    let err = validate::run_in(&fixture("project-empty"), None)
        .await
        .expect_err("no config");
    assert!(err.to_string().contains("neither"), "{err:?}");
}

#[tokio::test]
async fn mixed_source_forms_fail() {
    let err = validate::run_in(&fixture("project-mixed-forms"), None)
        .await
        .expect_err("singular plus plural");
    assert!(err.to_string().contains("mixes"), "{err:?}");
}

#[tokio::test]
async fn multi_source_project_passes() {
    validate::run_in(&fixture("project-multi-valid"), None)
        .await
        .expect("multi-source project validates");
}

#[tokio::test]
async fn multi_source_cross_backprop_fails() {
    let err = validate::run_in(&fixture("project-multi-backprop"), None)
        .await
        .expect_err("cross-source backprop");
    let message = err.to_string();
    assert!(message.contains("spanning sources"), "{message}");
    assert!(message.contains("a:a"), "{message}");
    assert!(message.contains("b:b"), "{message}");
}

#[tokio::test]
async fn multi_source_shadowed_passes() {
    // Same name defined twice but enabled once: valid (a warning goes to
    // stderr, which this harness does not capture).
    validate::run_in(&fixture("project-multi-shadowed"), None)
        .await
        .expect("shadowed duplicate validates");
}
