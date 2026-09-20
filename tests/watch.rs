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

async fn poll_snapshot(dir: &Path) -> PathBuf {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let files: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect();
            if let Some(first) = files.into_iter().next() {
                return first;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a conflict snapshot"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
    assert_eq!(names, ["backprop", "basic"]);
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

    // Conflict snapshots land in our own TMPDIR so the test can observe them.
    // (Only this suite snapshots, so process-global TMPDIR is safe.)
    let snaps = root.join("snapshots");
    unsafe {
        std::env::set_var("TMPDIR", &snaps);
    }

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

    // Break an array under union policy: loud mismatch, snapshot saved,
    // both files untouched.
    let broken = "{\n  \"b\": 2,\n  \"c\": 3,\n  \"list\": [\n    2\n  ]\n}\n";
    std::fs::write(&generated, broken).expect("hand-edit generated");
    let snapshot = poll_snapshot(&snaps.join("templatry-conflicts")).await;
    assert_eq!(
        std::fs::read(&snapshot).expect("read snapshot"),
        broken.as_bytes()
    );
    settle().await;
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
