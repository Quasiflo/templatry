//! Project and source validation diagnostics (Milestone 1).
//!
//! `templatry validate` auto-detects source versus project context, resolves
//! the source (fetching remote kinds via [`crate::source::resolve`]), and
//! reports file/table/key diagnostics with one suggested fix each.

use std::path::Path;

use crate::config::{self, PROJECT_CONFIG_PATH, ProjectConfig, SOURCE_CONFIG_FILENAME, SourceFile};

/// Validate the configuration in scope, auto-detecting source vs project context.
pub async fn run(config: Option<&Path>) -> crate::Result<()> {
    let cwd = std::env::current_dir().map_err(|err| {
        crate::invalid(
            Path::new("."),
            format!("cannot determine working directory: {err}"),
        )
    })?;
    run_in(&cwd, config).await
}

/// Validate with an explicit working directory (the test seam for [`run`]).
pub async fn run_in(dir: &Path, config: Option<&Path>) -> crate::Result<()> {
    if let Some(path) = config {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            dir.join(path)
        };
        return validate_project_file(&path).await;
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
        (true, false) => validate_project_file(&project).await,
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
    let content = crate::read_file(path)?;
    let source: SourceFile = crate::parse_toml(path, &content)?;
    let source_dir = path.parent().unwrap_or_else(|| Path::new("."));
    source.validate(source_dir, path)
}

/// Parse a project file, resolve its source, then validate the source config.
///
/// Remote sources are fetched through the cache first (see [`crate::source`]).
async fn validate_project_file(path: &Path) -> crate::Result<()> {
    let content = crate::read_file(path)?;
    let project: ProjectConfig = crate::parse_toml(path, &content)?;
    let project_root = crate::config::project_root(path);
    let resolved = crate::source::resolve(&project.source, &project_root, false).await?;
    let source_file = resolved.root_dir.join(SOURCE_CONFIG_FILENAME);
    let content = crate::read_file(&source_file)?;
    let source: SourceFile = crate::parse_toml(&source_file, &content)?;
    source.validate(&resolved.root_dir, &source_file)?;
    // Resolving also rejects unknown project labels and template names
    // against the source label and template sets.
    config::resolve_enabled_templates(&source, &project.source)?;
    Ok(())
}
