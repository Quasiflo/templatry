//! Project and source validation diagnostics (Milestone 1).
//!
//! `templatry validate` auto-detects project versus source context, resolves
//! the source (fetching remote kinds via [`crate::source::resolve`]), and
//! reports file/table/key diagnostics with one suggested fix each. A
//! repository holding both configs is both roles at once (a template source
//! that manages its own files with templatry): the project validates first,
//! then the source.

use std::path::Path;

use crate::config::{PROJECT_CONFIG_PATH, ProjectConfig, SOURCE_CONFIG_FILENAME, SourceFile};

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
        // Dual-role repository: a template source that is also a templatry
        // project. Both halves validate; the first failure wins.
        (true, true) => {
            validate_project_file(&project).await?;
            validate_source_file(&source)
        }
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

/// Parse a project file, resolve its sources, then validate every source config
/// plus the merged template set.
///
/// Remote sources are fetched through the cache first (see [`crate::source`]).
/// Same-name templates enabled in several sources fail; names defined in
/// several sources but enabled in one print a warning and pass.
async fn validate_project_file(path: &Path) -> crate::Result<()> {
    let content = crate::read_file(path)?;
    let project: ProjectConfig = crate::parse_toml(path, &content)?;
    let project_root = crate::config::project_root(path);
    let loaded =
        crate::generate::load_project_sources(&project, path, &project_root, false).await?;
    for warning in &loaded.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}
