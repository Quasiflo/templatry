//! `watch` fixtures: `tests/fixtures/watch/<case>/`.
//!
//! The live test below runs [`templatry::watch::run`] in a background task
//! against a tempdir copy: initial generate, override-triggered regeneration,
//! config-touch reload with resubscribe, then clean abort. Polling (not fixed
//! sleeps) absorbs filesystem notification latency.

// Shared harness: each suite uses a different subset of helpers.
#[allow(dead_code)]
mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use templatry::generate::Options;

fn options(root: &Path) -> Options {
    Options {
        config: Some(root.join(".config").join("templatry.toml")),
        watch: true,
        ..Default::default()
    }
}

/// Enable debug logs for failing-watch diagnosis (first caller wins).
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("templatry=debug")
        .with_target(false)
        .without_time()
        .try_init();
}

async fn poll_until(path: &Path, want: &str, label: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(content) = std::fs::read_to_string(path)
            && content == want
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {label}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait out the self-trigger guard window so the next hand-edit is never
/// mistaken for our own write echo.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(700)).await;
}

#[test]
fn all_watch_fixtures_are_known() {
    let names: Vec<String> = common::cases("watch")
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
        ["backprop", "backprop-ignore", "basic", "local-backprop"]
    );
}

#[tokio::test]
async fn watch_regenerates_reloads_and_stops_cleanly() {
    let (_temp, root) = common::setup_case("watch", "basic");
    let generated: PathBuf = root.join(".config").join("generated").join("app.json");
    let override_file: PathBuf = root.join(".config").join("app.json");
    let project_file: PathBuf = root.join(".config").join("templatry.toml");

    let task = tokio::spawn(async move {
        let options = options(&root);
        templatry::watch::run(&options).await
    });

    poll_until(
        &generated,
        "{\n  \"over\": true,\n  \"v\": 1\n}\n",
        "initial generate",
    )
    .await;
    settle().await;

    std::fs::write(&override_file, "{\"over\": false, \"extra\": 1}\n").expect("edit override");
    poll_until(
        &generated,
        "{\n  \"extra\": 1,\n  \"over\": false,\n  \"v\": 1\n}\n",
        "override-triggered regen",
    )
    .await;

    // Touch the project config: the watcher must reload, resubscribe, and
    // keep serving override changes afterwards.
    let config = std::fs::read_to_string(&project_file).expect("read config");
    std::fs::write(&project_file, format!("{config}\n# touched\n")).expect("touch config");
    settle().await;
    std::fs::write(&override_file, "{\"over\": false, \"extra\": 2}\n")
        .expect("edit override again");
    poll_until(
        &generated,
        "{\n  \"extra\": 2,\n  \"over\": false,\n  \"v\": 1\n}\n",
        "regen after config reload",
    )
    .await;

    task.abort();
    let outcome = task.await.expect_err("abort cancels the task");
    assert!(outcome.is_cancelled());
}

#[tokio::test]
async fn watch_backpropagates_generated_edits() {
    let (_temp, root) = common::setup_case("watch", "backprop");
    let generated: PathBuf = root.join(".config").join("generated").join("app.json");
    let override_file: PathBuf = root.join(".config").join("app.json");

    let task = tokio::spawn(async move {
        let options = options(&root);
        templatry::watch::run(&options).await
    });

    poll_until(
        &generated,
        "{\n  \"a\": 1,\n  \"b\": 2,\n  \"list\": [\n    1,\n    2\n  ]\n}\n",
        "initial generate",
    )
    .await;
    settle().await;

    // Add a key: folds into the override, replay matches the hand-edit.
    std::fs::write(
        &generated,
        "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3,\n  \"list\": [\n    1,\n    2\n  ]\n}\n",
    )
    .expect("hand-edit generated");
    poll_until(
        &override_file,
        "{\n  \"b\": 2,\n  \"c\": 3\n}\n",
        "override absorbs the added key",
    )
    .await;
    settle().await;

    // Delete a template key: the override records the deletion marker.
    std::fs::write(
        &generated,
        "{\n  \"b\": 2,\n  \"c\": 3,\n  \"list\": [\n    1,\n    2\n  ]\n}\n",
    )
    .expect("hand-edit generated");
    poll_until(
        &override_file,
        "{\n  \"a\": \"_TEMPLATRY_DELETE_\",\n  \"b\": 2,\n  \"c\": 3\n}\n",
        "override records the deletion",
    )
    .await;
    settle().await;

    // Break an array under union policy: loud mismatch, both files
    // untouched. The deletion phase above already proved this watcher loop
    // is responsive, so a settle window suffices here; snapshot writing
    // itself is covered by unit test.
    let broken = "{\n  \"b\": 2,\n  \"c\": 3,\n  \"list\": [\n    2\n  ]\n}\n";
    std::fs::write(&generated, broken).expect("hand-edit generated");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        std::fs::read_to_string(&override_file).expect("read override"),
        "{\n  \"a\": \"_TEMPLATRY_DELETE_\",\n  \"b\": 2,\n  \"c\": 3\n}\n"
    );
    assert_eq!(
        std::fs::read_to_string(&generated).expect("read generated"),
        broken
    );

    task.abort();
    let outcome = task.await.expect_err("abort cancels the task");
    assert!(outcome.is_cancelled());
}

#[tokio::test]
async fn watch_maintains_ignored_state() {
    let (_temp, root) = common::setup_case("watch", "backprop-ignore");
    let generated: PathBuf = root.join(".config").join("generated").join("app.json");
    let override_file: PathBuf = root.join(".config").join("app.json");

    let task = tokio::spawn(async move {
        let options = options(&root);
        templatry::watch::run(&options).await
    });

    poll_until(
        &generated,
        "{\n  \"keep\": 1,\n  \"locked\": {\n    \"x\": 1\n  },\n  \"val\": 1\n}\n",
        "initial generate",
    )
    .await;
    settle().await;

    // Ignored-key edit plus a real edit: only the real one folds, and the
    // generated file keeps the ignored value.
    let edited = "{\n  \"keep\": 2,\n  \"locked\": {\n    \"x\": 99\n  },\n  \"val\": 1\n}\n";
    std::fs::write(&generated, edited).expect("hand-edit generated");
    poll_until(
        &override_file,
        "{\n  \"keep\": 2\n}\n",
        "non-ignored edit folds, ignored edit does not",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(&generated).expect("read generated"),
        edited
    );
    settle().await;

    // Value change on a value-ignored key plus an addition: the addition
    // folds, the value change is left alone.
    let edited = "{\n  \"extra\": true,\n  \"keep\": 2,\n  \"locked\": {\n    \"x\": 99\n  },\n  \"val\": 42\n}\n";
    std::fs::write(&generated, edited).expect("hand-edit generated");
    poll_until(
        &override_file,
        "{\n  \"extra\": true,\n  \"keep\": 2\n}\n",
        "addition folds, value change ignored",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(&generated).expect("read generated"),
        edited
    );
    settle().await;

    // Deleting a value-ignored key propagates (existence syncs).
    let edited = "{\n  \"extra\": true,\n  \"keep\": 2,\n  \"locked\": {\n    \"x\": 99\n  }\n}\n";
    std::fs::write(&generated, edited).expect("hand-edit generated");
    poll_until(
        &override_file,
        "{\n  \"extra\": true,\n  \"keep\": 2,\n  \"val\": \"_TEMPLATRY_DELETE_\"\n}\n",
        "deletion propagates",
    )
    .await;

    task.abort();
    let outcome = task.await.expect_err("abort cancels the task");
    assert!(outcome.is_cancelled());
}

#[tokio::test]
async fn watch_local_layer_flows_and_folds_to_override() {
    init_logging();
    let (_temp, root) = common::setup_case("watch", "local-backprop");
    let generated: PathBuf = root.join(".config").join("generated").join("app.json");
    let override_file: PathBuf = root.join(".config").join("app.json");
    let local_file: PathBuf = root.join(".config").join("app.local.json");

    let task = tokio::spawn(async move {
        let options = options(&root);
        templatry::watch::run(&options).await
    });

    // No local file yet: template plus override only.
    poll_until(
        &generated,
        "{\n  \"a\": 1,\n  \"shared\": 20\n}\n",
        "initial generate",
    )
    .await;
    settle().await;

    // Creating the local file regenerates with the third layer on top.
    std::fs::write(&local_file, "{\"local\": true, \"shared\": 30}\n").expect("write local");
    poll_until(
        &generated,
        "{\n  \"a\": 1,\n  \"local\": true,\n  \"shared\": 30\n}\n",
        "local layer applies",
    )
    .await;
    settle().await;

    // Hand-edit a key the local layer does not pin: folds into the main
    // override while the local file stays byte-identical.
    let edited = "{\n  \"a\": 2,\n  \"local\": true,\n  \"shared\": 30\n}\n";
    std::fs::write(&generated, edited).expect("hand-edit generated");
    poll_until(
        &override_file,
        "{\n  \"a\": 2,\n  \"shared\": 20\n}\n",
        "edit folds to the main override",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(&generated).expect("read generated"),
        edited
    );
    assert_eq!(
        std::fs::read_to_string(&local_file).expect("read local"),
        "{\"local\": true, \"shared\": 30}\n"
    );
    settle().await;

    // Deleting the local file drops its layer on regen.
    std::fs::remove_file(&local_file).expect("delete local");
    poll_until(
        &generated,
        "{\n  \"a\": 2,\n  \"shared\": 20\n}\n",
        "local layer drops",
    )
    .await;

    task.abort();
    let outcome = task.await.expect_err("abort cancels the task");
    assert!(outcome.is_cancelled());
}
