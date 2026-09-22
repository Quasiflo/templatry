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
            "extends-basic",
            "json-merge",
            "labels",
            "local-chain",
            "merge-formats",
            "multi-source-abstract-scope",
            "multi-source-basic",
            "multi-source-conflict",
            "multi-source-samename",
            "multi-source-shadowed",
            "multi-source-shared",
            "none-copy",
            "preserve-ignore",
            "replace-missing",
            "shared-arrays",
            "shared-conflict",
            "shared-destination",
            "strategies",
            "template-select",
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
async fn golden_shared_arrays() {
    // Contributors with different arrays union instead of conflicting.
    let (_temp, root) = setup("shared-arrays");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("shared-arrays").join("expected"));
}

#[tokio::test]
async fn golden_template_select() {
    let (_temp, root) = setup("template-select");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("template-select").join("expected"));
    common::assert_absent(&root, &source_case("template-select"));
}

#[tokio::test]
async fn golden_preserve_ignore() {
    // The pre-existing generated file carries hand-maintained ignored state:
    // one-shot generation must preserve it instead of overwriting.
    let (_temp, root) = setup("preserve-ignore");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("preserve-ignore").join("expected"));
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
async fn golden_multi_source_basic() {
    // Disjoint destinations across sources generate side by side.
    let (_temp, root) = setup("multi-source-basic");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("multi-source-basic").join("expected"));
}

#[tokio::test]
async fn golden_multi_source_shared() {
    // Same destination across sources unions in (source, template) order.
    let (_temp, root) = setup("multi-source-shared");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("multi-source-shared").join("expected"));
    let merged = std::fs::read_to_string(
        root.join(".config")
            .join("generated")
            .join("extensions.json"),
    )
    .expect("read merged");
    let alpha = merged.find("alpha-ext").expect("alpha first");
    let shared = merged.find("shared").expect("shared middle");
    let beta = merged.find("beta-ext").expect("beta last");
    assert!(alpha < shared && shared < beta, "{merged}");
}

#[tokio::test]
async fn multi_source_conflict_names_qualified_templates() {
    let (_temp, root) = setup("multi-source-conflict");
    let err = generate::run(&options(&root, |_| {}))
        .await
        .expect_err("conflicting leaves");
    let message = err.to_string();
    assert!(message.contains("conflicting values"), "{message}");
    assert!(message.contains("`editor.size`"), "{message}");
    // The offending template renders source-qualified through merge errors.
    assert!(message.contains("beta:beta-settings"), "{message}");
}

#[tokio::test]
async fn multi_source_samename_errors_when_both_active() {
    let (_temp, root) = setup("multi-source-samename");
    let err = generate::run(&options(&root, |_| {}))
        .await
        .expect_err("same name in both sources");
    let message = err.to_string();
    assert!(message.contains("template `app`"), "{message}");
    assert!(message.contains("enabled in more than one"), "{message}");
}

#[tokio::test]
async fn golden_multi_source_shadowed() {
    // Per-source `exclude_templates` deactivates one copy: no conflict, and
    // only the live source generates.
    let (_temp, root) = setup("multi-source-shadowed");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(
        &root,
        &source_case("multi-source-shadowed").join("expected"),
    );
}

#[tokio::test]
async fn golden_multi_source_abstract_scope() {
    // Same abstract name with different bodies per source: each concrete
    // template resolves against its own source only.
    let (_temp, root) = setup("multi-source-abstract-scope");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(
        &root,
        &source_case("multi-source-abstract-scope").join("expected"),
    );
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

#[tokio::test]
async fn golden_local_chain() {
    let (_temp, root) = setup("local-chain");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("local-chain").join("expected"));
}

#[tokio::test]
async fn golden_extends_basic() {
    let (_temp, root) = setup("extends-basic");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("extends-basic").join("expected"));
}

#[tokio::test]
async fn golden_merge_formats() {
    let (_temp, root) = setup("merge-formats");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("merge-formats").join("expected"));
}

#[tokio::test]
async fn none_strategy_copies_bytes_and_permissions() {
    // Strategy `none` is a straight copy: identical bytes plus the
    // template's permission bits, so shipped scripts stay executable.
    let (_temp, root) = setup("none-copy");
    generate::run(&options(&root, |_| {}))
        .await
        .expect("generate");
    common::assert_tree_matches(&root, &source_case("none-copy").join("expected"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = |path: PathBuf| {
            std::fs::metadata(&path)
                .unwrap_or_else(|_| panic!("stat {}", path.display()))
                .permissions()
                .mode()
                & 0o777
        };
        let template = root.join("templates").join("deploy.sh");
        let generated = root.join(".config").join("generated").join("deploy.sh");
        assert_eq!(
            bits(generated.clone()),
            bits(template),
            "permissions mirrored from the template"
        );
        assert_eq!(
            bits(generated) & 0o111,
            0o111,
            "copied script stays executable"
        );
    }
}
