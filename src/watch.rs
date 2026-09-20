//! File watching and regeneration dispatch (Milestone 4).
//!
//! `watchexec` subscription, per-path regeneration, config-change restart,
//! debounce, and the self-trigger write guard, following the `smartworkspace`
//! pattern. Override changes regenerate only their destination group;
//! project/source config changes trigger a full reload plus resubscribe.
//! Generated files of `back_propagate` templates are watched too: until
//! Milestone 5 lands, hand-edits there regenerate forward (and are
//! overwritten), guarded against our own writes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use watchexec::Watchexec;
use watchexec_signals::Signal;

use crate::config::{SOURCE_CONFIG_FILENAME, SourceKind};
use crate::generate::{self, GroupMember, Options, ProjectContext};

/// Self-trigger guard window: our own writes arriving inside it are ignored.
///
/// Matches the `smartworkspace` 500 ms debounce.
const GUARD_WINDOW: Duration = Duration::from_millis(500);

/// Watch for changes and regenerate; runs until interrupted.
///
/// Each cycle loads the project, writes the full plan once, then watches
/// until a config change forces a reload (picks up added/removed templates
/// and new watch paths) or an interrupt quits.
pub async fn run(options: &Options) -> crate::Result<()> {
    loop {
        let cycle = Cycle::start(options).await?;
        match cycle.spin().await? {
            SpinOutcome::Reload => {
                tracing::info!("configuration changed, reloading");
            }
            SpinOutcome::Quit => break,
        }
    }
    tracing::info!("watch stopped");
    Ok(())
}

/// Watcher-loop outcome: reload the cycle or quit entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpinOutcome {
    Reload,
    Quit,
}

/// One watch cycle: loaded context plus subscriptions for this generation.
struct Cycle {
    context: Arc<ProjectContext>,
    subscriptions: Arc<Subscriptions>,
    guard: Arc<WriteGuard>,
    regen_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Cycle {
    /// Load, write the full plan once, and subscribe to its inputs.
    async fn start(options: &Options) -> crate::Result<Self> {
        let context = Arc::new(generate::load_context(options).await?);
        let groups = generate::group_members(&context)?;
        let mut plan = Vec::with_capacity(groups.len());
        for (dest, members) in &groups {
            plan.push(crate::generate::PlannedWrite {
                dest: dest.clone(),
                content: generate::render_group(dest, members, &context)?,
            });
        }
        let guard = Arc::new(WriteGuard::new(GUARD_WINDOW));
        generate::write_plan(&plan)?;
        for write in &plan {
            guard.note_write(&write.dest);
        }
        tracing::info!(files = plan.len(), "generated configuration files");
        let subscriptions = Arc::new(Subscriptions::build(&context, &groups)?);
        Ok(Self {
            context,
            subscriptions,
            guard,
            regen_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Run the watcher until a config change (reload) or interrupt (quit).
    async fn spin(&self) -> crate::Result<SpinOutcome> {
        let reload = Arc::new(AtomicBool::new(false));
        let wx = Watchexec::new({
            let context = Arc::clone(&self.context);
            let subscriptions = Arc::clone(&self.subscriptions);
            let guard = Arc::clone(&self.guard);
            let regen_lock = Arc::clone(&self.regen_lock);
            let reload = Arc::clone(&reload);
            let runtime = tokio::runtime::Handle::current();

            move |mut action| {
                if action.signals().any(|signal| signal == Signal::Interrupt) {
                    action.quit();
                    return action;
                }
                let paths: HashSet<PathBuf> =
                    action.paths().map(|(path, _)| path.to_owned()).collect();
                match classify(&paths, &subscriptions, &guard) {
                    Dispatch::Reload => {
                        tracing::info!("configuration change detected, re-evaluating");
                        reload.store(true, Ordering::Relaxed);
                        action.quit();
                    }
                    Dispatch::Regenerate(dests) => {
                        let context = Arc::clone(&context);
                        let subscriptions = Arc::clone(&subscriptions);
                        let guard = Arc::clone(&guard);
                        let regen_lock = Arc::clone(&regen_lock);
                        runtime.spawn(async move {
                            let _locked = regen_lock.lock().await;
                            match regenerate(&context, &subscriptions, &dests) {
                                Ok(()) => {
                                    for dest in &dests {
                                        guard.note_write(dest);
                                        tracing::info!(
                                            dest = %dest.display(),
                                            "regenerated after file change"
                                        );
                                    }
                                }
                                Err(err) => {
                                    tracing::error!(
                                        "regeneration failed: {err:?} (watching continues)"
                                    );
                                }
                            }
                        });
                    }
                    Dispatch::Ignore => {}
                }
                action
            }
        })
        .map_err(|err| {
            crate::invalid(
                Path::new("watch"),
                format!("cannot start file watcher: {err}"),
            )
        })?;

        wx.config.pathset(self.subscriptions.watch_paths());
        wx.main()
            .await
            .map_err(|err| {
                crate::invalid(Path::new("watch"), format!("file watcher failed: {err}"))
            })?
            .map_err(|err| {
                crate::invalid(Path::new("watch"), format!("file watcher failed: {err}"))
            })?;

        Ok(if reload.load(Ordering::Relaxed) {
            SpinOutcome::Reload
        } else {
            SpinOutcome::Quit
        })
    }
}

/// Regenerate destination groups from current disk state.
fn regenerate(
    context: &ProjectContext,
    subscriptions: &Subscriptions,
    dests: &[PathBuf],
) -> crate::Result<()> {
    for dest in dests {
        let Some(members) = subscriptions.groups.get(dest) else {
            continue;
        };
        let content = generate::render_group(dest, members, context)?;
        generate::write_plan(&[crate::generate::PlannedWrite {
            dest: dest.clone(),
            content,
        }])?;
    }
    Ok(())
}

// ---- Subscriptions ----------------------------------------------------------

/// Watch subscriptions for one cycle: path maps plus watched directories.
struct Subscriptions {
    /// Override file → destination files it feeds.
    overrides: HashMap<PathBuf, Vec<PathBuf>>,
    /// Watched generated file → its destination (back-propagate members only).
    generated: HashMap<PathBuf, PathBuf>,
    /// Destination → group members for targeted regeneration.
    groups: HashMap<PathBuf, Vec<GroupMember>>,
    /// Project config file: change triggers a full reload.
    project_file: PathBuf,
    /// Source file for local sources: change triggers a full reload.
    source_file: Option<PathBuf>,
    /// Parent directories watched non-recursively.
    watch_dirs: Vec<PathBuf>,
}

impl Subscriptions {
    /// Build subscriptions from a loaded context and its destination groups.
    fn build(
        context: &ProjectContext,
        groups: &std::collections::BTreeMap<PathBuf, Vec<GroupMember>>,
    ) -> crate::Result<Self> {
        let mut overrides: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let mut generated: HashMap<PathBuf, PathBuf> = HashMap::new();
        let mut groups_map: HashMap<PathBuf, Vec<GroupMember>> = HashMap::new();
        let mut watch_dirs: HashSet<PathBuf> = HashSet::new();

        for (dest, members) in groups {
            let dest_abs = absolute(dest);
            groups_map.insert(dest_abs.clone(), members.clone());
            if let Some(parent) = dest_abs.parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    crate::invalid(
                        &dest_abs,
                        format!("cannot create generated directory: {err}"),
                    )
                })?;
                watch_dirs.insert(parent.to_path_buf());
            }
            let mut watches_generated = false;
            for member in members {
                let override_path = absolute(
                    &context
                        .project_root
                        .join(
                            member
                                .template
                                .resolved_override_dir(&context.source.configs),
                        )
                        .join(member.template.resolved_override_file().map_err(|err| {
                            crate::invalid(
                                &context.project_file,
                                format!("cannot subscribe template `{}`: {err}", member.name),
                            )
                        })?),
                );
                overrides
                    .entry(override_path)
                    .or_default()
                    .push(dest_abs.clone());
                if member.template.back_propagate {
                    watches_generated = true;
                }
            }
            if watches_generated {
                generated.insert(dest_abs.clone(), dest_abs.clone());
            }
        }

        for dir in override_dirs(context)? {
            std::fs::create_dir_all(&dir).map_err(|err| {
                crate::invalid(&dir, format!("cannot create override directory: {err}"))
            })?;
            watch_dirs.insert(dir);
        }

        let project_file = absolute(&context.project_file);
        if let Some(parent) = project_file.parent() {
            watch_dirs.insert(parent.to_path_buf());
        }
        let source_file = (context.kind == SourceKind::LocalDir)
            .then(|| absolute(&context.source_root.join(SOURCE_CONFIG_FILENAME)));
        if let Some(source) = source_file.as_ref()
            && let Some(parent) = source.parent()
        {
            watch_dirs.insert(parent.to_path_buf());
        }

        Ok(Self {
            overrides,
            generated,
            groups: groups_map,
            project_file,
            source_file,
            watch_dirs: watch_dirs.into_iter().collect(),
        })
    }

    /// Watchexec pathset: every watched directory, non-recursive.
    fn watch_paths(&self) -> Vec<watchexec::WatchedPath> {
        self.watch_dirs
            .iter()
            .map(watchexec::WatchedPath::non_recursive)
            .collect()
    }
}

/// Override directories in use by enabled templates (absolute).
fn override_dirs(context: &ProjectContext) -> crate::Result<Vec<PathBuf>> {
    let mut dirs = HashSet::new();
    for name in &context.enabled {
        let template = &context.source.templates[name];
        dirs.insert(absolute(
            &context
                .project_root
                .join(template.resolved_override_dir(&context.source.configs)),
        ));
    }
    Ok(dirs.into_iter().collect())
}

/// Absolute path, falling back to the raw path when the system call fails.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Same-file comparison robust to symlinks (macOS temp dirs) and deletions:
/// canonicalize when possible, else compare absolute paths.
fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => absolute(left) == absolute(right),
    }
}

// ---- Dispatch ---------------------------------------------------------------

/// Watch-event dispatch decision.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dispatch {
    /// A config file changed: full reload plus resubscribe.
    Reload,
    /// Regenerate exactly these destinations.
    Regenerate(Vec<PathBuf>),
    /// Nothing actionable (self-writes, unwatched paths).
    Ignore,
}

/// Classify changed paths against subscriptions (pure: unit-testable).
fn classify(
    paths: &HashSet<PathBuf>,
    subscriptions: &Subscriptions,
    guard: &WriteGuard,
) -> Dispatch {
    for path in paths {
        if same_file(path, &subscriptions.project_file)
            || subscriptions
                .source_file
                .as_ref()
                .is_some_and(|source| same_file(path, source))
        {
            return Dispatch::Reload;
        }
    }
    let mut dests: Vec<PathBuf> = Vec::new();
    for path in paths {
        if guard.is_self_write(path) {
            continue;
        }
        for (override_path, override_dests) in &subscriptions.overrides {
            if same_file(path, override_path) {
                dests.extend(override_dests.iter().cloned());
            }
        }
        if let Some((_, dest)) = subscriptions
            .generated
            .iter()
            .find(|(generated, _)| same_file(path, generated))
        {
            dests.push(dest.clone());
        }
    }
    dests.sort();
    dests.dedup();
    if dests.is_empty() {
        Dispatch::Ignore
    } else {
        Dispatch::Regenerate(dests)
    }
}

// ---- Self-trigger guard -----------------------------------------------------

/// Remembers our own writes; events inside the window are our echo.
struct WriteGuard {
    writes: Mutex<HashMap<PathBuf, Instant>>,
    window: Duration,
}

impl WriteGuard {
    fn new(window: Duration) -> Self {
        Self {
            writes: Mutex::new(HashMap::new()),
            window,
        }
    }

    /// Record a write we performed.
    fn note_write(&self, path: &Path) {
        if let Ok(mut writes) = self.writes.lock() {
            writes.insert(absolute(path), Instant::now());
        }
    }

    /// True when we wrote this path inside the guard window.
    fn is_self_write(&self, path: &Path) -> bool {
        let Ok(mut writes) = self.writes.lock() else {
            return false;
        };
        let now = Instant::now();
        writes.retain(|_, written| now.duration_since(*written) <= self.window);
        let key = absolute(path);
        writes.keys().any(|written| same_file(&key, written))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_subscriptions(dir: &Path) -> Subscriptions {
        let override_path = dir.join("override.json");
        let dest = dir.join("generated").join("out.json");
        let generated = dir.join("generated").join("live.json");
        Subscriptions {
            overrides: [(override_path, vec![dest.clone()])].into_iter().collect(),
            generated: [(generated.clone(), generated)].into_iter().collect(),
            groups: HashMap::new(),
            project_file: dir.join(".config").join("templatry.toml"),
            source_file: Some(dir.join("templates").join("templatry.source.toml")),
            watch_dirs: Vec::new(),
        }
    }

    fn set(paths: &[PathBuf]) -> HashSet<PathBuf> {
        paths.iter().cloned().collect()
    }

    #[test]
    fn dispatch_override_regenerates_its_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let subscriptions = test_subscriptions(&root);
        let guard = WriteGuard::new(GUARD_WINDOW);

        let paths = set(&[root.join("override.json")]);
        let Dispatch::Regenerate(dests) = classify(&paths, &subscriptions, &guard) else {
            panic!("expected regeneration");
        };
        assert_eq!(dests, [root.join("generated").join("out.json")]);
    }

    #[test]
    fn dispatch_config_changes_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let subscriptions = test_subscriptions(&root);
        let guard = WriteGuard::new(GUARD_WINDOW);

        let project = set(&[root.join(".config").join("templatry.toml")]);
        assert_eq!(classify(&project, &subscriptions, &guard), Dispatch::Reload);

        let source = set(&[root.join("templates").join("templatry.source.toml")]);
        assert_eq!(classify(&source, &subscriptions, &guard), Dispatch::Reload);
    }

    #[test]
    fn dispatch_ignores_unknown_and_self_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let subscriptions = test_subscriptions(&root);
        let guard = WriteGuard::new(GUARD_WINDOW);

        let unknown = set(&[root.join("unrelated.txt")]);
        assert_eq!(classify(&unknown, &subscriptions, &guard), Dispatch::Ignore);

        guard.note_write(&root.join("generated").join("live.json"));
        let echo = set(&[root.join("generated").join("live.json")]);
        assert_eq!(classify(&echo, &subscriptions, &guard), Dispatch::Ignore);

        // A mixed event still regenerates for the real change.
        let mixed = set(&[root.join("unrelated.txt"), root.join("override.json")]);
        assert!(matches!(
            classify(&mixed, &subscriptions, &guard),
            Dispatch::Regenerate(_)
        ));
    }

    #[test]
    fn guard_window_zero_never_matches() {
        let guard = WriteGuard::new(Duration::ZERO);
        let path = Path::new("/tmp/some-output.json");
        guard.note_write(path);
        assert!(!guard.is_self_write(path));
        assert!(!guard.is_self_write(Path::new("/tmp/other.json")));
    }

    #[test]
    fn same_file_survives_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let raw = dir.path().join("file.json");
        std::fs::write(&raw, "{}").expect("write");
        let canonical = raw.canonicalize().expect("canonicalize");
        assert!(same_file(&raw, &canonical));
        assert!(!same_file(&raw, &dir.path().join("other.json")));
    }
}
