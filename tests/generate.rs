//! Gold-file generation fixtures: `tests/fixtures/generate/<case>/`.
//!
//! Milestone 3 executes template-plus-override generation per case and
//! compares against golden output via [`common::assert_golden`]. Until then,
//! this test locks in fixture discovery so suites cannot silently go empty.

mod common;

#[test]
fn golden_generate_fixtures_are_discoverable() {
    for case in common::cases("generate") {
        assert!(
            case.is_dir(),
            "fixture case is not a dir: {}",
            case.display()
        );
    }
}
