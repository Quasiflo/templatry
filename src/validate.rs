//! Project and source validation diagnostics (Milestone 1).
//!
//! `templatry validate` auto-detects source versus project context, resolves
//! local-directory sources for project validation, and reports file/table/key
//! diagnostics with one suggested fix each. Remote source fetching lands in
//! Milestone 2; remote kinds validate structurally until then.

use std::path::{Path, PathBuf};

use crate::config::{
    self, PROJECT_CONFIG_PATH, ProjectConfig, SOURCE_CONFIG_FILENAME, SourceFile, SourceKind,
};

/// Validate the configuration in scope, auto-detecting source vs project context.
pub fn run(config: Option<&Path>) -> crate::Result<()> {
    let cwd = std::env::current_dir().map_err(|err| {
        crate::invalid(
            Path::new("."),
            format!("cannot determine working directory: {err}"),
        )
    })?;
    run_in(&cwd, config)
}

/// Validate with an explicit working directory (the test seam for [`run`]).
pub fn run_in(dir: &Path, config: Option<&Path>) -> crate::Result<()> {
    if let Some(path) = config {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            dir.join(path)
        };
        return validate_project_file(&path);
    }

    let project = dir.join(PROJECT_CONFIG_PATH);
    let source = dir.join(SOURCE_CONFIG_FILENAME);
    match (project.exists(), source.exists()) {
        (true, true) => Err(crate::invalid(
            dir,
            format!(
                "both `{PROJECT_CONFIG_PATH}` and `{SOURCE_CONFIG_FILENAME}` are present: keep exactly one (project repos hold the former, template repos the latter)"
            ),
        )),
        (true, false) => validate_project_file(&project),
        (false, true) => validate_source_file(&source),
        (false, false) => Err(crate::invalid(
            dir,
            format!(
                "found neither `{PROJECT_CONFIG_PATH}` nor `{SOURCE_CONFIG_FILENAME}`: run from a project or template repository (or pass `--config <PATH>`, see `templatry init` in Future Work)"
            ),
        )),
    }
}

/// Parse and validate a `templatry.source.toml` file.
fn validate_source_file(path: &Path) -> crate::Result<()> {
    let content = read_file(path)?;
    let source: SourceFile = crate::parse_toml(path, &content)?;
    let source_dir = path.parent().unwrap_or_else(|| Path::new("."));
    source.validate(source_dir, path)
}

/// Parse a project file, check it structurally, then validate the referenced source.
fn validate_project_file(path: &Path) -> crate::Result<()> {
    let content = read_file(path)?;
    let project: ProjectConfig = crate::parse_toml(path, &content)?;
    let source_ref = &project.source;
    let kind = source_ref.kind()?;

    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    match kind {
        SourceKind::LocalDir => {
            let local = source_ref
                .path
                .as_deref()
                .expect("kind() guarantees a non-empty `path`");
            let source_root = canonicalize(project_dir, local, path)?;
            validate_local_source(&source_root, source_ref.root.as_deref(), &project)
        }
        remote => Err(crate::Error::unimplemented(format!(
            "{remote} source resolution downloads the source (Milestone 2): structural project checks passed"
        ))),
    }
}

/// Resolve a local directory source and validate its source file plus label refs.
fn validate_local_source(
    source_root: &Path,
    root: Option<&str>,
    project: &ProjectConfig,
) -> crate::Result<()> {
    let source_dir: PathBuf = match root.filter(|root| !root.is_empty()) {
        Some(root) => source_root.join(root),
        None => source_root.to_path_buf(),
    };
    let source_file = source_dir.join(SOURCE_CONFIG_FILENAME);
    if !source_file.is_file() {
        return Err(crate::invalid(
            &source_file,
            format!(
                "no `{SOURCE_CONFIG_FILENAME}` under the resolved source root (hint: set `root` if it lives in a subdirectory)"
            ),
        ));
    }
    let content = read_file(&source_file)?;
    let source: SourceFile = crate::parse_toml(&source_file, &content)?;
    source.validate(&source_dir, &source_file)?;
    // Resolving also rejects unknown project labels against the source label set.
    config::resolve_enabled_templates(
        &source,
        &project.source.enable_labels,
        &project.source.disable_labels,
    )?;
    Ok(())
}

/// Join `target` onto `base` and canonicalize, mapping failures to diagnostics.
fn canonicalize(base: &Path, target: &str, project_file: &Path) -> crate::Result<PathBuf> {
    base.join(target).canonicalize().map_err(|err| {
        crate::invalid(
            project_file,
            format!("source directory `{target}` cannot be resolved: {err}"),
        )
    })
}

/// Read a config file, mapping IO failures to diagnostics.
fn read_file(path: &Path) -> crate::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|err| crate::invalid(path, format!("cannot read file: {err}")))
}
