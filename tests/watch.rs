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
    assert_eq!(names, ["basic"]);
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
