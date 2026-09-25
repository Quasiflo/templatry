//! Generation orchestration, check/dry-run diffing, and watch support.
//!
//! One-shot generation, `--check`, `--watch` (via [`crate::watch`]), and the
//! pure render core backing back-propagation safety replays.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::config::{
    self, Configs, PROJECT_CONFIG_PATH, ProjectConfig, SOURCE_CONFIG_FILENAME, SourceFile,
    SourceKind, Template,
};
use crate::merge::{self, EffectiveStrategy, GroupPart, Rendered};

/// Options for [`run`], mirroring `templatry generate` flags.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Keep watching and regenerate on change.
    pub watch: bool,
    /// Diff against disk without writing; exit 2 on difference.
    pub check: bool,
    /// Print planned writes without touching disk.
    pub dry_run: bool,
    /// Project config other than [`.config/templatry.toml`](crate::config::PROJECT_CONFIG_PATH).
    pub config: Option<PathBuf>,
    /// Never touch the network; fail on cache miss.
    pub offline: bool,
}

/// One planned file write: destination plus complete new content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    /// Absolute or project-relative destination file.
    pub dest: PathBuf,
    /// Complete bytes to write.
    pub content: Vec<u8>,
    /// Unix permission bits (masked to `0o7777`) mirrored from the template
    /// after writing; `None` leaves new files at default permissions.
    /// Only strategy `none` copies set this today.
    pub mode: Option<u32>,
}

/// Generate configuration files for the current project.
///
/// Always writes every enabled template (no content-hash short-circuiting);
/// `--check` diffs in memory and fails with [`crate::Error::CheckDifferences`],
/// `--dry-run` prints planned writes without touching disk.
pub async fn run(options: &Options) -> crate::Result<()> {
    if options.watch {
        if options.check || options.dry_run {
            return Err(crate::Error::Usage(
                "`--watch` cannot combine with `--check` or `--dry-run`: run them separately"
                    .to_string(),
            ));
        }
        return crate::watch::run(options).await;
    }
    let plan = plan(options).await?;
    if options.check {
        return apply_check(&plan);
    }
    if options.dry_run {
        for write in &plan {
            println!("would write {}", write.dest.display());
        }
        return Ok(());
    }
    write_plan(&plan)?;
    tracing::info!(files = plan.len(), "generated configuration files");
    Ok(())
}

/// Loaded project state shared by one-shot generation and watch cycles.
#[derive(Debug, Clone)]
pub(crate) struct ProjectContext {
    /// Project config file (change triggers a full reload in watch mode).
    pub project_file: PathBuf,
    /// Repository root: base for override, generated, and local-source paths.
    pub project_root: PathBuf,
    /// Per-source state in resolution order (singular first, else by name).
    pub entries: Vec<ContextEntry>,
}

/// One resolved project source: its validated definition plus selection.
#[derive(Debug, Clone)]
pub(crate) struct ContextEntry {
    /// Configured source name (`""` for singular projects).
    pub name: String,
    /// Source root holding `templatry.source.toml`.
    pub root_dir: PathBuf,
    /// Validated source definition (raw templates, pre-`extends`).
    pub source: SourceFile,
    /// `extends`-resolved templates (all, not just enabled).
    pub resolved: BTreeMap<String, Template>,
    /// Enabled template names, sorted.
    pub enabled: BTreeSet<String>,
    /// Source kind (local sources also watch their source file).
    pub kind: SourceKind,
}

/// One loaded source plus its shadowed-duplicate warnings, if any.
pub(crate) struct LoadedProject {
    /// Per-source state in resolution order.
    pub entries: Vec<ContextEntry>,
    /// Shadowed-duplicate warnings (same name defined in several sources but
    /// enabled in exactly one); printed by `validate`, ignored by `generate`.
    pub warnings: Vec<String>,
}

/// Locate, resolve, load, validate, and select: everything before rendering.
///
/// Every source resolves independently, runs abstract substitution and label
/// filtering within its own scope, then merges into one ordered template set
/// (see [`config::merge_active_templates`]).
pub(crate) async fn load_project_sources(
    project: &ProjectConfig,
    project_file: &Path,
    project_root: &Path,
    offline: bool,
) -> crate::Result<LoadedProject> {
    let named = crate::source::resolve_all(project, project_root, offline).await?;
    let refs = project.project_sources();
    let mut entries = Vec::with_capacity(named.len());
    for (source, (_, project_ref)) in named.iter().zip(refs) {
        let source_file = source.root_dir.join(SOURCE_CONFIG_FILENAME);
        let content = crate::read_file(&source_file)?;
        let file: SourceFile = crate::parse_toml(&source_file, &content)?;
        file.validate(&source.root_dir, &source_file)?;
        let resolved = file.resolved_templates(&source_file)?;
        let enabled = config::resolve_enabled_templates(&resolved, &file.default, project_ref)?;
        entries.push(ContextEntry {
            name: source.name.clone(),
            root_dir: source.root_dir.clone(),
            source: file,
            resolved,
            enabled,
            kind: source.kind,
        });
    }
    let views: Vec<config::SourceView<'_>> = entries
        .iter()
        .map(|entry| config::SourceView {
            name: entry.name.as_str(),
            templates: &entry.resolved,
            enabled: &entry.enabled,
            configs: &entry.source.configs,
        })
        .collect();
    let (_merged, warnings) = config::merge_active_templates(&views)?;
    config::validate_merged_destinations(&merged_active(&entries), project_file)?;
    Ok(LoadedProject { entries, warnings })
}

/// Active templates across all entries in (source, template) order.
fn merged_active(entries: &[ContextEntry]) -> Vec<config::ActiveTemplate> {
    let mut merged = Vec::new();
    for entry in entries {
        for name in &entry.enabled {
            merged.push(config::ActiveTemplate {
                source: entry.name.clone(),
                name: name.clone(),
                template: entry.resolved[name].clone(),
                configs: entry.source.configs.clone(),
            });
        }
    }
    merged
}

/// Locate, resolve, load, validate, and select: everything before rendering.
pub(crate) async fn load_context(options: &Options) -> crate::Result<ProjectContext> {
    let (project_file, project_root) = locate_project(options.config.as_deref())?;
    let content = crate::read_file(&project_file)?;
    let project: ProjectConfig = crate::parse_toml(&project_file, &content)?;
    let loaded =
        load_project_sources(&project, &project_file, &project_root, options.offline).await?;
    Ok(ProjectContext {
        project_file,
        project_root,
        entries: loaded.entries,
    })
}

/// One destination group member: everything needed to re-render it on change.
#[derive(Debug, Clone)]
pub(crate) struct GroupMember {
    /// Display name: bare for singular projects, `source:template` when merged.
    pub name: String,
    pub labels: BTreeSet<String>,
    pub template: Template,
    /// Owning source's root: template files read from here.
    pub source_root: PathBuf,
    /// Owning source's `[configs]` defaults for override directories.
    pub configs: Configs,
}

/// Group enabled templates by destination file.
///
/// Entries arrive in (source, template) order, so array unions and text
/// concatenations spanning sources stay deterministic.
pub(crate) fn group_members(
    context: &ProjectContext,
) -> crate::Result<BTreeMap<PathBuf, Vec<GroupMember>>> {
    let mut groups: BTreeMap<PathBuf, Vec<GroupMember>> = BTreeMap::new();
    for entry in &context.entries {
        for name in &entry.enabled {
            let template = &entry.resolved[name];
            let dest = context
                .project_root
                .join(template.resolved_generated_dir(&entry.source.configs))
                .join(template.resolved_generated_file()?);
            groups.entry(dest).or_default().push(GroupMember {
                name: config::qualified_name(&entry.name, name),
                labels: template.labels.clone(),
                template: template.clone(),
                source_root: entry.root_dir.clone(),
                configs: entry.source.configs.clone(),
            });
        }
    }
    Ok(groups)
}

/// Render one destination group into final bytes plus an optional mode.
///
/// The mode is the template's permission bits for single-member `none`
/// groups (a straight copy); shared destinations never preserve permissions.
pub(crate) struct RenderedGroup {
    pub content: Vec<u8>,
    pub mode: Option<u32>,
}

/// Render one destination group into its final bytes plus an optional mode.
pub(crate) fn render_group(
    dest: &Path,
    members: &[GroupMember],
    context: &ProjectContext,
) -> crate::Result<RenderedGroup> {
    // Shared destinations combine template-only output first, then apply the
    // shared override file once. The legacy per-member path applied a shared
    // override file once per contributor, duplicating `append_bottom`/`append_top`
    // text (and array-object pins in structured merges) N times.
    if members.len() > 1
        && let Some(group) = try_render_shared_group(members, context)?
    {
        return Ok(group);
    }
    render_group_legacy(dest, members, context)
}

/// Shared-destination fast path: template-only combine plus a single
/// application of the shared override file.
///
/// Returns `Ok(None)` when the group is not a uniform shared case this path
/// handles (distinct override/local files, mixed text strategies,
/// `replace`/`none` members, mixed structured formats); callers fall back to
/// the legacy per-member rendering so those groups keep their previous
/// behavior (and existing diagnostics).
fn try_render_shared_group(
    members: &[GroupMember],
    context: &ProjectContext,
) -> crate::Result<Option<RenderedGroup>> {
    struct Owned {
        strategy: EffectiveStrategy,
        policy: crate::config::ArrayPolicy,
        template_text: String,
        override_path: PathBuf,
        override_text: Option<String>,
        local_path: Option<PathBuf>,
        local_text: Option<String>,
    }

    let mut owned: Vec<Owned> = Vec::with_capacity(members.len());
    for member in members {
        let template_path = member.source_root.join(member.template.template_path()?);
        let template_bytes = crate::read_file_bytes(&template_path)?;
        let generated_filename = member.template.resolved_generated_file()?;
        let strategy = merge::effective_strategy(&member.template, &generated_filename)?;
        // `replace` emits one override verbatim and `none` copies raw bytes:
        // both are rejected or meaningless in shared groups, so leave them on
        // the legacy path (which preserves existing validation diagnostics).
        match strategy {
            EffectiveStrategy::Replace | EffectiveStrategy::None => return Ok(None),
            EffectiveStrategy::AppendTop
            | EffectiveStrategy::AppendBottom
            | EffectiveStrategy::Structured(_) => {}
        }
        let override_dir = context
            .project_root
            .join(member.template.resolved_override_dir(&member.configs));
        let override_path = override_dir.join(member.template.resolved_override_file()?);
        // `none` already returned above, so every remaining strategy decodes.
        let template_text = String::from_utf8(template_bytes).map_err(|err| {
            crate::invalid(&template_path, format!("file is not valid UTF-8: {err}"))
        })?;
        let override_text = match read_optional(&override_path, "override file")? {
            Some(bytes) => Some(decode(&bytes, &override_path)?),
            None => None,
        };
        let (local_path, local_text) = match member
            .template
            .local_override_file
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            Some(name) => {
                let local_path = override_dir.join(name);
                let local_text = match read_optional(&local_path, "local override file")? {
                    Some(bytes) => Some(decode(&bytes, &local_path)?),
                    None => None,
                };
                (Some(local_path), local_text)
            }
            None => (None, None),
        };
        owned.push(Owned {
            strategy,
            policy: member.template.array_policy.unwrap_or_default(),
            template_text,
            override_path,
            override_text,
            local_path,
            local_text,
        });
    }

    let uniform = owned
        .iter()
        .map(|layer| layer.strategy)
        .all(|strategy| strategy == owned[0].strategy);
    // Post-merge applies only when every member shares the same override file
    // and the same local file (both `None` counts as sharing "no local").
    // Distinct override files stay on the legacy per-member path: each override
    // scopes to its own template there, preserving attribution and the
    // interleaved ordering existing fixtures encode. A shared file on the
    // legacy path would apply once per contributor (the duplication bug).
    let shared_override = owned
        .iter()
        .map(|layer| layer.override_path.as_path())
        .all(|path| path == owned[0].override_path.as_path());
    let shared_local = owned
        .iter()
        .map(|layer| layer.local_path.as_deref())
        .all(|path| path == owned[0].local_path.as_deref());
    if !shared_override || !shared_local {
        return Ok(None);
    }
    // Borrowed views over `members` (names, labels) plus `owned` (texts).
    // Sharing was established above, so the paths are not needed further:
    // there is exactly one override file and one local file in play.
    let shared: Vec<SharedLayer<'_>> = members
        .iter()
        .zip(owned.iter())
        .map(|(member, layer)| SharedLayer {
            name: member.name.as_str(),
            labels: &member.labels,
            template_text: layer.template_text.as_str(),
            override_text: layer.override_text.as_deref(),
            local_text: layer.local_text.as_deref(),
            strategy: layer.strategy,
            policy: layer.policy,
        })
        .collect();
    match owned[0].strategy {
        EffectiveStrategy::Structured(_) => {
            if !uniform {
                return Ok(None);
            }
            let value = render_shared_structured(&shared)?;
            let format = match owned[0].strategy {
                EffectiveStrategy::Structured(format) => format,
                _ => unreachable!("checked uniform structured"),
            };
            let content = merge::serialize_doc(&value, format)?.into_bytes();
            Ok(Some(RenderedGroup {
                content,
                mode: None,
            }))
        }
        EffectiveStrategy::AppendTop | EffectiveStrategy::AppendBottom => {
            if !uniform {
                // Mixed `append_top`/`append_bottom` in one destination has no
                // defined override-after-merge order; keep legacy interleaving.
                return Ok(None);
            }
            // Keep the literal-marker warning semantics of `render_contents`.
            for (member, layer) in members.iter().zip(owned.iter()) {
                for text in [layer.override_text.as_deref(), layer.local_text.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    if text.contains(merge::DELETE_MARKER) {
                        tracing::warn!(
                            template = member.name.as_str(),
                            "override contains `{}` but the strategy is not a structured merge: kept literally",
                            merge::DELETE_MARKER
                        );
                        break;
                    }
                }
            }
            let content = render_shared_text(&shared).into_bytes();
            Ok(Some(RenderedGroup {
                content,
                mode: None,
            }))
        }
        EffectiveStrategy::Replace | EffectiveStrategy::None => Ok(None),
    }
}
/// Combine template-only structured values, then merge the shared override
/// (and local) file once.
///
/// Precondition: callers guarantee every layer shares the same override file
/// and the same local file (established before building the layers), so the
/// first present text in member-name order is the one file's content.
/// Templates must combine cleanly on their own; the override applies after the
/// completed merge.
pub(crate) fn render_shared_structured(
    layers: &[SharedLayer<'_>],
) -> crate::Result<serde_json::Value> {
    let format = match layers.first().map(|layer| layer.strategy) {
        Some(EffectiveStrategy::Structured(format)) => format,
        _ => {
            return Err(crate::invalid(
                Path::new("templatry.source.toml"),
                "internal error: shared structured render without a structured strategy"
                    .to_string(),
            ));
        }
    };
    // Template-only contributions, in member-name order for determinism.
    let mut order: Vec<usize> = (0..layers.len()).collect();
    order.sort_by(|left, right| layers[*left].name.cmp(layers[*right].name));
    let mut contributions = Vec::with_capacity(layers.len());
    for index in &order {
        let layer = &layers[*index];
        let value = merge::parse_doc(
            layer.template_text,
            format,
            &format!("template `{}`", layer.name),
        )?;
        contributions.push(merge::Contribution {
            template: layer.name,
            labels: layer.labels,
            value,
        });
    }
    let mut combined = merge::combine_structured(&contributions)?;

    // The one shared override file (first present text in name order).
    if let Some(layer) = order
        .iter()
        .map(|index| &layers[*index])
        .find(|layer| layer.override_text.is_some())
    {
        let text = layer.override_text.expect("found present override text");
        let value = merge::parse_doc(
            text,
            format,
            &format!("override for template `{}`", layer.name),
        )?;
        combined = merge::merge_structured(combined, value, layer.policy, layer.name)?;
    }

    // The one shared local file (first present text in name order).
    if let Some(layer) = order
        .iter()
        .map(|index| &layers[*index])
        .find(|layer| layer.local_text.is_some())
    {
        let text = layer.local_text.expect("found present local text");
        let value = merge::parse_doc(
            text,
            format,
            &format!("local override for template `{}`", layer.name),
        )?;
        combined = merge::merge_structured(combined, value, layer.policy, layer.name)?;
    }
    Ok(combined)
}
/// One member's texts plus provenance for shared-destination post-merge
/// rendering (used by forward generation and back-propagation replays).
///
/// Precondition: every layer in a call shares the same override file and the
/// same local file (callers establish this before building the layers).
pub(crate) struct SharedLayer<'a> {
    /// Contributing template name (for diagnostics and deterministic order).
    pub name: &'a str,
    /// Contributing template labels (for conflict diagnostics).
    pub labels: &'a BTreeSet<String>,
    /// Current template file text.
    pub template_text: &'a str,
    /// Current override file text (`None` when missing).
    pub override_text: Option<&'a str>,
    /// Current local override text (`None` when unset or missing).
    pub local_text: Option<&'a str>,
    /// Resolved strategy (uniform across the group).
    pub strategy: EffectiveStrategy,
    /// Merge policy for structured strategies.
    pub policy: crate::config::ArrayPolicy,
}

/// Combine template-only text parts, then append/prepend the shared override
/// (and local) file once.
///
/// Precondition: as for [`SharedLayer`], all layers share the same files, so
/// the first present text in member-name order is the one file's content.
pub(crate) fn render_shared_text(layers: &[SharedLayer<'_>]) -> String {
    let strategy = layers
        .first()
        .map(|layer| layer.strategy)
        .unwrap_or(EffectiveStrategy::AppendBottom);
    let mut order: Vec<usize> = (0..layers.len()).collect();
    order.sort_by(|left, right| layers[*left].name.cmp(layers[*right].name));

    let mut templates: Vec<(&str, &str)> = Vec::with_capacity(layers.len());
    for index in &order {
        let layer = &layers[*index];
        templates.push((layer.name, layer.template_text));
    }
    let combined_templates = merge::combine_text(&templates);

    let shared_override = order
        .iter()
        .map(|index| &layers[*index])
        .find_map(|layer| layer.override_text.filter(|text| !text.is_empty()));
    let shared_local = order
        .iter()
        .map(|index| &layers[*index])
        .find_map(|layer| layer.local_text.filter(|text| !text.is_empty()));

    let mut segments: Vec<&str> = Vec::with_capacity(3);
    if strategy == EffectiveStrategy::AppendTop {
        segments.extend(shared_local);
        segments.extend(shared_override);
        segments.push(combined_templates.as_str());
    } else {
        segments.push(combined_templates.as_str());
        segments.extend(shared_override);
        segments.extend(shared_local);
    }
    segments.join("\n")
}

/// Legacy per-member rendering plus destination combination.
///
/// Single-member groups and non-uniform shared groups render each template
/// with its override first, then combine. Uniform shared text/structured
/// groups use the post-merge path above instead.
fn render_group_legacy(
    dest: &Path,
    members: &[GroupMember],
    context: &ProjectContext,
) -> crate::Result<RenderedGroup> {
    let mut rendered = Vec::with_capacity(members.len());
    let mut modes = Vec::with_capacity(members.len());
    for member in members {
        let (output, mode) = render_template(
            &member.template,
            &member.name,
            &member.source_root,
            &context.project_root,
            &member.configs,
        )?;
        rendered.push((member.name.as_str(), &member.labels, output));
        modes.push(mode);
    }
    let parts: Vec<GroupPart<'_>> = rendered
        .iter()
        .map(|(name, labels, output)| GroupPart {
            template: name,
            labels,
            rendered: output,
        })
        .collect();
    let content = merge::combine_group(dest, &parts)?;
    let mode = if members.len() == 1 {
        modes.into_iter().next().flatten()
    } else {
        None
    };
    Ok(RenderedGroup { content, mode })
}

/// Compute the full write plan without touching disk (the test seam for [`run`]).
pub async fn plan(options: &Options) -> crate::Result<Vec<PlannedWrite>> {
    let context = load_context(options).await?;
    let groups = group_members(&context)?;
    render_plan(&context, &groups)
}

/// Render every destination group with forward preservation.
///
/// Shared by one-shot planning and watch cycles so both write identical bytes.
pub(crate) fn render_plan(
    context: &ProjectContext,
    groups: &BTreeMap<PathBuf, Vec<GroupMember>>,
) -> crate::Result<Vec<PlannedWrite>> {
    let mut plan = Vec::with_capacity(groups.len());
    for (dest, members) in groups {
        let group = render_group(dest, members, context)?;
        let content = preserve_group(dest, &group.content, members)?;
        plan.push(PlannedWrite {
            dest: dest.clone(),
            content,
            mode: group.mode,
        });
    }
    Ok(plan)
}

/// Apply forward preservation for a rendered group.
///
/// Groups without ignore lists pass through untouched. Otherwise the on-disk
/// file, when present, supplies maintained state (see [`crate::backprop`]).
pub(crate) fn preserve_group(
    dest: &Path,
    combined: &[u8],
    members: &[GroupMember],
) -> crate::Result<Vec<u8>> {
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut format = None;
    for member in members {
        for pattern in &member.template.backprop_ignore {
            keys.push(parse_ignore_pattern(pattern)?);
        }
        for pattern in &member.template.backprop_ignore_values {
            values.push(parse_ignore_pattern(pattern)?);
        }
        if format.is_none() {
            let generated = member.template.resolved_generated_file()?;
            if let crate::merge::EffectiveStrategy::Structured(found) =
                crate::merge::effective_strategy(&member.template, &generated)?
            {
                format = Some(found);
            }
        }
    }
    let Some(format) = format else {
        if keys.is_empty() && values.is_empty() {
            return Ok(combined.to_vec());
        }
        return Err(crate::invalid(
            dest,
            "internal error: preserved group has no structured format".to_string(),
        ));
    };
    crate::backprop::preserve_maintained(dest, combined, format, &keys, &values)
}

/// Parse an ignore pattern (validated upstream; failures stay loud).
fn parse_ignore_pattern(pattern: &str) -> crate::Result<crate::backprop::IgnorePattern> {
    crate::backprop::IgnorePattern::parse(pattern).map_err(|reason| {
        crate::invalid(
            Path::new("templatry.source.toml"),
            format!("invalid ignore pattern `{pattern}`: {reason}"),
        )
    })
}

/// Write every planned file atomically, mirroring template permissions
/// where the plan carries them.
pub(crate) fn write_plan(plan: &[PlannedWrite]) -> crate::Result<()> {
    for write in plan {
        atomic_write(&write.dest, &write.content, write.mode)?;
    }
    Ok(())
}

/// Render one template with its project override into a [`Rendered`] output,
/// plus the template's permission bits when the strategy is a straight copy.
///
/// Strategy `none` copies raw bytes with no decoding (non-UTF-8 survives)
/// and mirrors permissions at write time; every other strategy renders text
/// and leaves permissions alone.
pub(crate) fn render_template(
    template: &Template,
    name: &str,
    source_root: &Path,
    project_root: &Path,
    configs: &Configs,
) -> crate::Result<(Rendered, Option<u32>)> {
    let template_path = source_root.join(template.template_path()?);
    let template_bytes = crate::read_file_bytes(&template_path)?;
    let generated_filename = template.resolved_generated_file()?;
    let strategy = merge::effective_strategy(template, &generated_filename)?;

    let override_dir = project_root.join(template.resolved_override_dir(configs));
    let override_path = override_dir.join(template.resolved_override_file()?);
    if strategy == EffectiveStrategy::None {
        let local_present = match template
            .local_override_file
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            Some(name) => read_optional(&override_dir.join(name), "local override file")?.is_some(),
            None => false,
        };
        if read_optional(&override_path, "override file")?.is_some() || local_present {
            tracing::warn!(
                template = name,
                "overrides ignored: strategy `none` always copies the template verbatim"
            );
        }
        let mode = template_mode(&template_path)?;
        return Ok((Rendered::Bytes(template_bytes), mode));
    }
    let override_text = match read_optional(&override_path, "override file")? {
        Some(bytes) => Some(decode(&bytes, &override_path)?),
        None => None,
    };
    let local_text = match template
        .local_override_file
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    {
        Some(name) => {
            let local_path = override_dir.join(name);
            match read_optional(&local_path, "local override file")? {
                Some(bytes) => Some(decode(&bytes, &local_path)?),
                None => None,
            }
        }
        None => None,
    };

    match strategy {
        EffectiveStrategy::Replace => {
            let Some(text) = override_text else {
                return Err(crate::invalid(
                    &override_path,
                    format!(
                        "template `{name}` uses `replace` but no override file exists: create it (replace emits the override verbatim)"
                    ),
                ));
            };
            if local_text.is_some() {
                tracing::warn!(
                    template = name,
                    "local override ignored: strategy `replace` emits the override verbatim"
                );
            }
            Ok((Rendered::Text(text), None))
        }
        _ => {
            let template_text = decode(&template_bytes, &template_path)?;
            let rendered = render_contents(
                &template_text,
                override_text.as_deref(),
                local_text.as_deref(),
                strategy,
                template.array_policy.unwrap_or_default(),
                name,
            )?;
            Ok((rendered, None))
        }
    }
}

/// Template permission bits (masked to `0o7777`) for straight copies.
///
/// `None` off Unix, where permission bits have no meaning to mirror.
fn template_mode(path: &Path) -> crate::Result<Option<u32>> {
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(path)
            .map_err(|err| {
                crate::invalid(path, format!("cannot read template permissions: {err}"))
            })?
            .permissions()
            .mode()
            & 0o7777;
        Ok(Some(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Read an optional layer file: missing or whitespace-only means absent.
fn read_optional(path: &Path, what: &str) -> crate::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(None),
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(crate::invalid(path, format!("cannot read {what}: {err}"))),
    }
}

/// Render template, override, and local override text into a [`Rendered`] output.
///
/// Pure over file contents: file IO, missing-file handling, and strategy
/// resolution stay in [`render_template`]. Precedence is template, then
/// override, then local. Back-propagation safety replays call this directly.
pub(crate) fn render_contents(
    template_text: &str,
    override_text: Option<&str>,
    local_text: Option<&str>,
    strategy: EffectiveStrategy,
    policy: crate::config::ArrayPolicy,
    name: &str,
) -> crate::Result<Rendered> {
    match strategy {
        EffectiveStrategy::Replace => {
            let Some(text) = override_text else {
                return Err(crate::invalid(
                    Path::new("templatry.source.toml"),
                    format!(
                        "template `{name}` uses `replace` but no override content exists: create the override file (replace emits it verbatim)"
                    ),
                ));
            };
            if local_text.is_some() {
                tracing::warn!(
                    template = name,
                    "local override ignored: strategy `replace` emits the override verbatim"
                );
            }
            Ok(Rendered::Text(text.to_string()))
        }
        EffectiveStrategy::AppendTop | EffectiveStrategy::AppendBottom => {
            for text in [override_text, local_text].into_iter().flatten() {
                if text.contains(merge::DELETE_MARKER) {
                    tracing::warn!(
                        template = name,
                        "override contains `{}` but the strategy is not a structured merge: kept literally",
                        merge::DELETE_MARKER
                    );
                    break;
                }
            }
            // Segments stay precedence-ordered (highest first for top,
            // lowest first for bottom); missing layers vanish with no
            // stray separators. Emptied layers vanish too: forward reads
            // never produce them (whitespace-only files count as missing),
            // but back-propagation replays a reverted override as `Some("")`
            // and must reproduce the file exactly.
            let mut segments: Vec<&str> = Vec::with_capacity(3);
            if strategy == EffectiveStrategy::AppendTop {
                segments.extend(local_text.filter(|text| !text.is_empty()));
                segments.extend(override_text.filter(|text| !text.is_empty()));
                segments.push(template_text);
            } else {
                segments.push(template_text);
                segments.extend(override_text.filter(|text| !text.is_empty()));
                segments.extend(local_text.filter(|text| !text.is_empty()));
            }
            Ok(Rendered::Text(segments.join("\n")))
        }
        EffectiveStrategy::Structured(format) => {
            let base = merge::parse_doc(template_text, format, &format!("template `{name}`"))?;
            let merged = match override_text {
                Some(text) => {
                    let over =
                        merge::parse_doc(text, format, &format!("override for template `{name}`"))?;
                    merge::merge_structured(base, over, policy, name)?
                }
                None => base,
            };
            let merged = match local_text {
                Some(text) => {
                    let local = merge::parse_doc(
                        text,
                        format,
                        &format!("local override for template `{name}`"),
                    )?;
                    merge::merge_structured(merged, local, policy, name)?
                }
                None => merged,
            };
            Ok(Rendered::Structured {
                value: merged,
                format,
            })
        }
        EffectiveStrategy::None => {
            if override_text.is_some() || local_text.is_some() {
                tracing::warn!(
                    template = name,
                    "overrides ignored: strategy `none` always copies the template verbatim"
                );
            }
            Ok(Rendered::Text(template_text.to_string()))
        }
    }
}

/// Decode file bytes as UTF-8 for merging.
fn decode(bytes: &[u8], path: &Path) -> crate::Result<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|err| crate::invalid(path, format!("file is not valid UTF-8: {err}")))
}

/// Locate the project file and repository root.
///
/// Explicit `--config` wins (made absolute against the working directory);
/// otherwise the default path must exist, with a hint when run inside a
/// template repository by mistake. The root follows
/// [`config::project_root`], so canonical `.config/templatry.toml` layouts
/// resolve `.config/`-relative paths against the repository.
fn locate_project(config: Option<&Path>) -> crate::Result<(PathBuf, PathBuf)> {
    if let Some(path) = config {
        let cwd = std::env::current_dir().map_err(|err| {
            crate::invalid(
                Path::new("."),
                format!("cannot determine working directory: {err}"),
            )
        })?;
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        if !path.is_file() {
            return Err(crate::invalid(
                &path,
                "project config does not exist".to_string(),
            ));
        }
        let root = config::project_root(&path);
        return Ok((path, root));
    }
    let cwd = std::env::current_dir().map_err(|err| {
        crate::invalid(
            Path::new("."),
            format!("cannot determine working directory: {err}"),
        )
    })?;
    let path = cwd.join(PROJECT_CONFIG_PATH);
    if !path.is_file() {
        if cwd.join(SOURCE_CONFIG_FILENAME).is_file() {
            return Err(crate::invalid(
                &cwd,
                format!(
                    "this looks like a template repository (`{SOURCE_CONFIG_FILENAME}` present): `generate` runs in project repositories, template repositories use `templatry validate`"
                ),
            ));
        }
        return Err(crate::invalid(
            &cwd,
            format!(
                "no `{PROJECT_CONFIG_PATH}` found: run from a project repository or pass `--config <PATH>`"
            ),
        ));
    }
    Ok((path.clone(), config::project_root(&path)))
}

/// Diff the plan against disk: print differing paths, fail on any difference.
fn apply_check(plan: &[PlannedWrite]) -> crate::Result<()> {
    let mut differing = Vec::new();
    for write in plan {
        let current = std::fs::read(&write.dest).unwrap_or_default();
        if current != write.content {
            differing.push(write.dest.clone());
        }
    }
    if differing.is_empty() {
        return Ok(());
    }
    for dest in &differing {
        println!("{}", dest.display());
    }
    Err(crate::Error::CheckDifferences {
        count: differing.len(),
    })
}

/// Write a file atomically via temp-file-plus-rename in the same directory,
/// mirroring Unix permission bits when given.
pub(crate) fn atomic_write(dest: &Path, content: &[u8], mode: Option<u32>) -> crate::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|err| crate::invalid(dest, format!("cannot create parent directory: {err}")))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .map_err(|err| crate::invalid(dest, format!("cannot stage write: {err}")))?;
    std::io::Write::write_all(&mut staged, content)
        .map_err(|err| crate::invalid(dest, format!("cannot stage write: {err}")))?;
    staged
        .persist(dest)
        .map_err(|err| crate::invalid(dest, format!("cannot replace file: {}", err.error)))?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
            .map_err(|err| crate::invalid(dest, format!("cannot mirror permissions: {err}")))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::DocFormat;

    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("test json parses")
    }

    fn rendered_value(rendered: Rendered) -> serde_json::Value {
        match rendered {
            Rendered::Structured { value, .. } => value,
            Rendered::Text(text) => panic!("expected structured, got text: {text}"),
            Rendered::Bytes(bytes) => panic!("expected structured, got bytes: {bytes:?}"),
        }
    }

    fn rendered_text(rendered: Rendered) -> String {
        match rendered {
            Rendered::Text(text) => text,
            Rendered::Structured { .. } => panic!("expected text, got structured"),
            Rendered::Bytes(bytes) => panic!("expected text, got bytes: {bytes:?}"),
        }
    }

    const POLICY: crate::config::ArrayPolicy = crate::config::ArrayPolicy::Union;

    #[test]
    fn structured_local_wins_over_override_wins_over_template() {
        let rendered = render_contents(
            "{\"a\": 1, \"b\": 1, \"c\": 1}",
            Some("{\"b\": 2, \"c\": 2}"),
            Some("{\"c\": 3}"),
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(
            rendered_value(rendered),
            json("{\"a\": 1, \"b\": 2, \"c\": 3}")
        );
    }

    #[test]
    fn structured_layers_skip_missing() {
        // Template plus local, no override.
        let rendered = render_contents(
            "{\"a\": 1}",
            None,
            Some("{\"c\": 3}"),
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_value(rendered), json("{\"a\": 1, \"c\": 3}"));

        // Template plus override, no local.
        let rendered = render_contents(
            "{\"a\": 1}",
            Some("{\"b\": 2}"),
            None,
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_value(rendered), json("{\"a\": 1, \"b\": 2}"));

        // Template alone.
        let rendered = render_contents(
            "{\"a\": 1}",
            None,
            None,
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_value(rendered), json("{\"a\": 1}"));
    }

    #[test]
    fn append_orders_segments_by_precedence() {
        // Bottom: template, override, local.
        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T\nO\nL");

        // Top: local, override, template.
        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::AppendTop,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "L\nO\nT");

        // Missing layers vanish with no stray separators (legacy behavior).
        let rendered = render_contents(
            "T",
            Some("O"),
            None,
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T\nO");
        let rendered = render_contents(
            "T",
            None,
            None,
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T");
        let rendered = render_contents(
            "T",
            Some("O"),
            None,
            EffectiveStrategy::AppendTop,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "O\nT");

        // Local without override still applies.
        let rendered = render_contents(
            "T",
            None,
            Some("L"),
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T\nL");
        let rendered = render_contents(
            "T",
            None,
            Some("L"),
            EffectiveStrategy::AppendTop,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "L\nT");
    }

    #[test]
    fn replace_and_none_ignore_local() {
        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::Replace,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "O");

        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::None,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T");
    }

    #[test]
    fn none_strategy_copies_raw_bytes_and_mode() {
        use crate::config::Strategy;

        let dir = tempfile::tempdir().expect("tempdir");
        let templates = dir.path().join("templates");
        std::fs::create_dir_all(&templates).expect("mkdirs");
        // Invalid UTF-8 must survive: straight copies never decode.
        let bytes = b"#!/bin/sh\necho '\xff\xfe'\n".to_vec();
        std::fs::write(templates.join("run.bin"), &bytes).expect("write template");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                templates.join("run.bin"),
                std::fs::Permissions::from_mode(0o750),
            )
            .expect("chmod template");
        }
        let template = crate::config::Template {
            template: Some("run.bin".to_string()),
            extends: None,
            override_file: None,
            override_dir: None,
            local_override_file: None,
            generated_file: None,
            generated_dir: None,
            strategy: Some(Strategy::None),
            array_policy: None,
            back_propagate: None,
            backprop_ignore: Vec::new(),
            backprop_ignore_values: Vec::new(),
            labels: BTreeSet::new(),
        };
        let (rendered, mode) = render_template(
            &template,
            "run",
            &templates,
            dir.path(),
            &crate::config::Configs::default(),
        )
        .unwrap();
        match rendered {
            Rendered::Bytes(out) => assert_eq!(out, bytes),
            other => panic!("expected raw bytes, got {other:?}"),
        }
        #[cfg(unix)]
        assert_eq!(mode, Some(0o750), "template mode captured");
        #[cfg(not(unix))]
        assert_eq!(mode, None);
    }
}
