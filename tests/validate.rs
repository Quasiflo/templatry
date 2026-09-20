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
            "project-remote-github",
            "project-valid-local",
            "source-invalid",
            "source-valid",
        ]
    );
}

#[test]
fn valid_source_config_passes() {
    validate::run_in(&fixture("source-valid"), None).expect("valid source config");
}

#[test]
fn invalid_source_config_fails() {
    let err = validate::run_in(&fixture("source-invalid"), None).expect_err("mutex violation");
    assert!(err.to_string().contains("sets both"), "{err:?}");
}

#[test]
fn valid_local_project_passes() {
    validate::run_in(&fixture("project-valid-local"), None).expect("valid local project");
}

#[test]
fn both_configs_present_fails() {
    let err = validate::run_in(&fixture("project-both-present"), None).expect_err("ambiguity");
    assert!(err.to_string().contains("both"), "{err:?}");
}

#[test]
fn missing_configs_fails() {
    let err = validate::run_in(&fixture("project-empty"), None).expect_err("no config");
    assert!(err.to_string().contains("neither"), "{err:?}");
}

#[test]
fn remote_source_is_milestone_2() {
    let err = validate::run_in(&fixture("project-remote-github"), None).expect_err("remote fetch");
    assert!(
        matches!(err, templatry::Error::Unimplemented(_)),
        "expected unimplemented firewall, got {err:?}"
    );
    assert!(err.to_string().contains("Milestone 2"), "{err:?}");
}
