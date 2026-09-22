//! Two-way sync for `back_propagate` templates (Milestone 5).
//!
//! Hand-edits to watched generated files diff back into project overrides so
//! the next template bump reproduces them. Every fold passes through an
//! in-memory safety replay (template pipeline re-run, compared against the
//! expected bytes); mismatches fail loudly with the conflicting content saved
//! to a temp snapshot, leaving files untouched, and watch mode keeps running.
//! Template updates win over pending edits, then the captured diff reapplies
//! (three-way merge for structured formats).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::ArrayPolicy;
use crate::merge::{self, DELETE_MARKER, DocFormat, EffectiveStrategy};

/// Conflict snapshot directory name inside the system temp dir.
///
/// On safety-check mismatch the conflicting generated content is recoverable
/// here at `<stem>.<timestamp>.conflict.<ext>`.
pub const CONFLICT_SNAPSHOT_DIR_NAME: &str = "templatry-conflicts";

/// One group member's fresh file contents for back-propagation.
pub struct MemberView<'a> {
    /// Contributing template name.
    pub name: &'a str,
    /// Contributing template labels.
    pub labels: &'a BTreeSet<String>,
    /// Override file path, when known (needed to collapse identical writes).
    pub override_path: Option<&'a Path>,
    /// Current template file text.
    pub template_text: &'a str,
    /// Current override file text (`None` when missing).
    pub override_text: Option<&'a str>,
    /// Current local override text (`None` when unset or missing).
    pub local_text: Option<&'a str>,
    /// Resolved strategy.
    pub strategy: EffectiveStrategy,
    /// Merge policy for structured strategies.
    pub policy: ArrayPolicy,
    /// Whether this member accepts back-propagation.
    pub back_propagate: bool,
    /// Fully ignored paths (edits never fold, values never overwritten).
    pub ignore_keys: Vec<IgnorePattern>,
    /// Value-ignored paths (adds/deletes sync, value changes ignored).
    pub ignore_values: Vec<IgnorePattern>,
}

// ---- Ignore patterns ----------------------------------------------------------

/// A dot-separated ignore pattern with `*` single-segment wildcards.
///
/// A pattern matches its own path and its entire subtree, so `machine` covers
/// `machine.cpu.cores`. Arrays match wholesale at the array path (diffs never
/// descend into arrays). Parse with [`IgnorePattern::parse`], which rejects
/// empty segments and `**` (prefix patterns already cover subtrees).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnorePattern {
    segments: Vec<PatternSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternSegment {
    Exact(String),
    Any,
}

impl IgnorePattern {
    /// Parse and validate a pattern; dots always split (no escaping in v1).
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return Err("must not be empty".to_string());
        }
        let mut segments = Vec::new();
        for segment in trimmed.split('.') {
            match segment {
                "" => {
                    return Err(
                        "has an empty segment (no leading, trailing, or doubled dots)".to_string(),
                    );
                }
                "**" => {
                    return Err(
                        "`**` is not supported: prefix patterns already cover subtrees".to_string(),
                    );
                }
                "*" => segments.push(PatternSegment::Any),
                exact => segments.push(PatternSegment::Exact(exact.to_string())),
            }
        }
        Ok(Self { segments })
    }

    /// True for the pattern's own path and everything beneath it.
    ///
    /// Each path segment is split on dots first, so a flat dotted key and
    /// its nested equivalent address the same setting.
    pub fn matches(&self, path: &[String]) -> bool {
        let atoms: Vec<&str> = path.iter().flat_map(|segment| segment.split('.')).collect();
        if atoms.len() < self.segments.len() {
            return false;
        }
        self.segments
            .iter()
            .zip(atoms.iter())
            .all(|(segment, actual)| match segment {
                PatternSegment::Any => true,
                PatternSegment::Exact(want) => want == actual,
            })
    }
}

/// True when any pattern matches the path (or its subtree).
fn matches_any(path: &[String], patterns: &[IgnorePattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches(path))
}

/// Back-propagation outcome: applied, or nothing to do.
///
/// All failure modes are loud [`crate::Error`]s (mismatch with snapshot,
/// ambiguity, unparsable files); watch mode logs them and keeps watching.
#[derive(Debug)]
pub enum BackpropOutcome {
    /// Exactly one contributor accepted the edit.
    Applied {
        /// Contributing template name (owns the rewritten override).
        member: String,
        /// New override file content, or `None` when the fold left the
        /// override unchanged (callers skip the rewrite).
        new_override_text: Option<String>,
        /// Forward-regenerated bytes with maintained state restored.
        replay_bytes: Vec<u8>,
    },
    /// No back-propagate member, or no detectable change.
    NoChange,
}

/// Fold a hand-edit into overrides: `last_bytes` is the previous generated
/// state, `current_bytes` the hand-edited state (the safety target).
pub fn backpropagate(
    dest: &Path,
    last_bytes: &[u8],
    current_bytes: &[u8],
    members: &[MemberView<'_>],
) -> crate::Result<BackpropOutcome> {
    let candidates: Vec<&MemberView<'_>> = members
        .iter()
        .filter(|member| member.back_propagate)
        .collect();
    if candidates.is_empty() || last_bytes == current_bytes {
        return Ok(BackpropOutcome::NoChange);
    }
    match candidates[0].strategy {
        EffectiveStrategy::Structured(format) => {
            let base = parse_generated(last_bytes, dest, format, "previous")?;
            let target = parse_generated(current_bytes, dest, format, "current")?;
            let ops = diff_values(&base, &target);
            if ops.is_empty() {
                return Ok(BackpropOutcome::NoChange);
            }
            try_candidates(
                dest,
                members,
                &candidates,
                &ops,
                &SafetyTarget {
                    value: &target,
                    user_bytes: current_bytes,
                },
                true,
            )
        }
        text => {
            require_single_text_member(dest, members)?;
            let current = decode(
                current_bytes,
                dest,
                "generated file (hand-edited content must be UTF-8)",
            )?;
            let tentative = fold_text(
                candidates[0].template_text,
                candidates[0].local_text,
                &current,
                text,
                candidates[0].name,
                dest,
            )?;
            replay_text(dest, candidates[0], &tentative, current_bytes, true)
        }
    }
}

/// Reapply a captured hand-edit after a template update won.
///
/// Three-way merge for structured formats: the user diff (last vs captured)
/// applies onto the fresh forward output, folds into overrides against the
/// new template, and replays must reproduce that desired output. Text formats
/// fold the captured bytes against the new template and must reproduce them
/// exactly (template-region edits fail loudly).
pub fn reapply(
    dest: &Path,
    last_bytes: &[u8],
    captured_bytes: &[u8],
    forward_bytes: &[u8],
    members: &[MemberView<'_>],
) -> crate::Result<BackpropOutcome> {
    let candidates: Vec<&MemberView<'_>> = members
        .iter()
        .filter(|member| member.back_propagate)
        .collect();
    if candidates.is_empty() || last_bytes == captured_bytes {
        return Ok(BackpropOutcome::NoChange);
    }
    match candidates[0].strategy {
        EffectiveStrategy::Structured(format) => {
            let base = parse_generated(last_bytes, dest, format, "previous")?;
            let captured = parse_generated(captured_bytes, dest, format, "hand-edited")?;
            let forward = parse_generated(forward_bytes, dest, format, "regenerated")?;
            let ops = diff_values(&base, &captured);
            if ops.is_empty() {
                return Ok(BackpropOutcome::NoChange);
            }
            let mut desired = forward;
            apply_ops_plain(&mut desired, &ops);
            try_candidates(
                dest,
                members,
                &candidates,
                &ops,
                &SafetyTarget {
                    value: &desired,
                    user_bytes: captured_bytes,
                },
                false,
            )
        }
        text => {
            require_single_text_member(dest, members)?;
            let captured = decode(captured_bytes, dest, "hand-edited content must be UTF-8")?;
            let tentative = fold_text(
                candidates[0].template_text,
                candidates[0].local_text,
                &captured,
                text,
                candidates[0].name,
                dest,
            )?;
            replay_text(dest, candidates[0], &tentative, captured_bytes, false)
        }
    }
}

/// Text back-propagation needs exactly one group member to attribute edits.
fn require_single_text_member(dest: &Path, members: &[MemberView<'_>]) -> crate::Result<()> {
    if members.len() == 1 {
        return Ok(());
    }
    Err(crate::invalid(
        dest,
        "back propagation into a shared text destination cannot attribute edits to one override: use structured merge strategies for label-split files".to_string(),
    ))
}

/// Parse generated bytes as a structured document for diffing.
fn parse_generated(
    bytes: &[u8],
    dest: &Path,
    format: DocFormat,
    which: &str,
) -> crate::Result<Value> {
    let text = decode(
        bytes,
        dest,
        &format!("{which} generated content must be UTF-8"),
    )?;
    merge::parse_doc(
        &text,
        format,
        &format!("{which} generated file `{}`", dest.display()),
    )
}

/// Decode bytes with an actionable diagnostic.
fn decode(bytes: &[u8], dest: &Path, what: &str) -> crate::Result<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|err| crate::invalid(dest, format!("{what} is not valid UTF-8: {err}")))
}

// ---- Diff -------------------------------------------------------------------

/// One user edit operation: set a path, or delete it.
#[derive(Debug, Clone, PartialEq)]
enum DiffOp {
    /// Path set to a value. `added` distinguishes brand-new keys (existence
    /// syncs for value-ignored paths) from changed values (ignored there).
    /// Arrays always compare wholesale: differing arrays count as changed.
    Set {
        path: Vec<String>,
        value: Value,
        added: bool,
    },
    /// Path removed.
    Delete { path: Vec<String> },
}

/// Diff previous vs current generated documents into operations.
///
/// Objects recurse; arrays compare wholesale (no index paths); scalars and
/// type changes record as sets.
fn diff_values(last: &Value, current: &Value) -> Vec<DiffOp> {
    let mut ops = Vec::new();
    diff_into(last, current, &mut Vec::new(), &mut ops);
    ops
}

fn diff_into(last: &Value, current: &Value, path: &mut Vec<String>, ops: &mut Vec<DiffOp>) {
    match (last, current) {
        (Value::Object(last_map), Value::Object(current_map)) => {
            for (key, current_value) in current_map {
                path.push(key.clone());
                match last_map.get(key) {
                    Some(last_value) => diff_into(last_value, current_value, path, ops),
                    None => ops.push(DiffOp::Set {
                        path: path.clone(),
                        value: current_value.clone(),
                        added: true,
                    }),
                }
                path.pop();
            }
            for key in last_map.keys() {
                if !current_map.contains_key(key) {
                    path.push(key.clone());
                    ops.push(DiffOp::Delete { path: path.clone() });
                    path.pop();
                }
            }
        }
        (Value::Array(last_items), Value::Array(current_items)) => {
            if last_items != current_items {
                ops.push(DiffOp::Set {
                    path: path.clone(),
                    value: current.clone(),
                    added: false,
                });
            }
        }
        (last_value, current_value) => {
            if last_value != current_value {
                ops.push(DiffOp::Set {
                    path: path.clone(),
                    value: current_value.clone(),
                    added: false,
                });
            }
        }
    }
}

// ---- Path helpers -----------------------------------------------------------

/// Read a nested value by path segments (empty path reads the document).
fn get_path<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut current = value;
    for segment in path {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Set a nested value, creating intermediate objects (errors through arrays,
/// scalars, or at the document root whole-replace... see below).
///
/// An empty path replaces the whole document (only used when the diff itself
/// replaced the root, e.g. object-to-array).
fn set_path_strict(slot: &mut Value, path: &[String], value: Value) -> crate::Result<()> {
    let Some((first, rest)) = path.split_first() else {
        *slot = value;
        return Ok(());
    };
    if slot.is_null() {
        *slot = Value::Object(Default::default());
    }
    match slot {
        Value::Object(map) => {
            let child = map
                .entry(first.clone())
                .or_insert(Value::Object(Default::default()));
            if !rest.is_empty() && !child.is_object() && !child.is_null() {
                return Err(crate::invalid(
                    Path::new("templatry.source.toml"),
                    format!(
                        "cannot fold change at `{}`: `{first}` is not an object in the override",
                        path.join(".")
                    ),
                ));
            }
            set_path_strict(child, rest, value)
        }
        _ => Err(crate::invalid(
            Path::new("templatry.source.toml"),
            format!(
                "cannot fold change at `{}`: override holds a non-object where an object is needed",
                path.join(".")
            ),
        )),
    }
}

/// Forgiving set for desired-output computation: replaces anything in the way.
fn set_path_forgiving(slot: &mut Value, path: &[String], value: Value) {
    let Some((first, rest)) = path.split_first() else {
        *slot = value;
        return;
    };
    if !slot.is_object() {
        *slot = Value::Object(Default::default());
    }
    if let Value::Object(map) = slot {
        let child = map
            .entry(first.clone())
            .or_insert(Value::Object(Default::default()));
        set_path_forgiving(child, rest, value);
    }
}

/// Delete a nested value (no-op when absent; empty path is an error).
///
/// Deliberately leaves emptied parent objects in place: pruning them could
/// drop user-written structure the safety replay expects.
fn remove_path(slot: &mut Value, path: &[String]) -> crate::Result<()> {
    let Some((first, rest)) = path.split_first() else {
        return Err(crate::invalid(
            Path::new("templatry.source.toml"),
            "cannot fold a document-root deletion: delete the override file instead".to_string(),
        ));
    };
    match slot {
        Value::Object(map) => {
            if rest.is_empty() {
                map.remove(first);
            } else if let Some(child) = map.get_mut(first) {
                remove_path(child, rest)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Apply operations plainly (desired-output computation, no template logic).
fn apply_ops_plain(base: &mut Value, ops: &[DiffOp]) {
    for op in ops {
        match op {
            DiffOp::Set { path, value, .. } => set_path_forgiving(base, path, value.clone()),
            DiffOp::Delete { path } => {
                let _ = remove_path(base, path);
            }
        }
    }
}

// ---- Ignore lists -------------------------------------------------------------

/// True when an op is suppressed by a candidate's ignore lists.
///
/// Fully ignored paths drop everything; value-ignored paths drop only
/// changed values (adds and deletes still sync existence).
fn op_ignored(op: &DiffOp, keys: &[IgnorePattern], values: &[IgnorePattern]) -> bool {
    match op {
        DiffOp::Set { path, added, .. } => {
            matches_any(path, keys) || (!added && matches_any(path, values))
        }
        DiffOp::Delete { path } => matches_any(path, keys),
    }
}

/// Masked equality for safety replays: ignored paths match unconditionally,
/// value-ignored paths match values but still sync presence, and everything
/// else compares exactly.
fn compare_masked(
    replay: &Value,
    target: &Value,
    path: &mut Vec<String>,
    keys: &[IgnorePattern],
    values: &[IgnorePattern],
) -> bool {
    if matches_any(path, keys) {
        return true;
    }
    match (replay, target) {
        (Value::Object(replay_map), Value::Object(target_map)) => {
            for (key, replay_value) in replay_map {
                path.push(key.clone());
                let equal = if matches_any(path, keys) {
                    true
                } else {
                    match target_map.get(key) {
                        Some(target_value) => {
                            compare_masked(replay_value, target_value, path, keys, values)
                        }
                        None => false,
                    }
                };
                path.pop();
                if !equal {
                    return false;
                }
            }
            for key in target_map.keys() {
                if replay_map.contains_key(key) {
                    continue;
                }
                path.push(key.clone());
                let equal = matches_any(path, keys);
                path.pop();
                if !equal {
                    return false;
                }
            }
            true
        }
        (replay_value, target_value) => {
            if matches_any(path, values) {
                return true;
            }
            replay_value == target_value
        }
    }
}

/// Apply forward preservation for a freshly rendered group.
///
/// Groups without ignore lists pass through untouched (no disk reads).
/// Otherwise the on-disk file, when present, supplies maintained state.
pub(crate) fn preserve_maintained(
    dest: &Path,
    combined: &[u8],
    format: DocFormat,
    keys: &[IgnorePattern],
    values: &[IgnorePattern],
) -> crate::Result<Vec<u8>> {
    if keys.is_empty() && values.is_empty() {
        return Ok(combined.to_vec());
    }
    let disk = match std::fs::read(dest) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(combined.to_vec());
        }
        Err(err) => {
            return Err(crate::invalid(
                dest,
                format!("cannot read generated file: {err}"),
            ));
        }
    };
    let base = parse_generated(combined, dest, format, "generated")?;
    let disk_value = parse_generated(&disk, dest, format, "existing")?;
    let mut out = base;
    restore_into(&mut out, &disk_value, &mut Vec::new(), keys, values);
    merge::serialize_doc(&out, format).map(String::into_bytes)
}

/// Merge maintained state from disk into fresh output.
///
/// Ignored paths take the on-disk subtree wholesale when present (and are
/// removed when the disk lacks them, including user-added keys the pipeline
/// never produced); value-ignored paths take the on-disk subtree when present
/// but keep merged output otherwise, so template-side presence changes still
/// flow. Non-matching paths keep merged output.
fn restore_into(
    out: &mut Value,
    disk: &Value,
    path: &mut Vec<String>,
    keys: &[IgnorePattern],
    values: &[IgnorePattern],
) {
    let (Value::Object(out_map), Value::Object(disk_map)) = (out, disk) else {
        return;
    };
    let mut remove = Vec::new();
    for key in out_map.keys().cloned().collect::<Vec<_>>() {
        path.push(key.clone());
        let ignored = matches_any(path, keys);
        let value_ignored = matches_any(path, values);
        match disk_map.get(&key) {
            Some(disk_value) if ignored || value_ignored => {
                out_map.insert(key.clone(), disk_value.clone());
            }
            None if ignored => {
                remove.push(key.clone());
            }
            Some(disk_value) => {
                if let (Some(out_child), Value::Object(_)) = (out_map.get_mut(&key), disk_value) {
                    restore_into(out_child, disk_value, path, keys, values);
                }
            }
            None => {}
        }
        path.pop();
    }
    for key in remove {
        out_map.remove(&key);
    }
    // User-maintained keys the pipeline never produced still persist.
    // Values-lists are excluded: their presence follows the merge, and adds
    // already fold through the override.
    for (key, disk_value) in disk_map {
        if out_map.contains_key(key) {
            continue;
        }
        path.push(key.clone());
        let ignored = matches_any(path, keys);
        path.pop();
        if ignored {
            out_map.insert(key.clone(), disk_value.clone());
        }
    }
}

// ---- Structured fold ----------------------------------------------------------

/// Fold operations into one candidate's override.
///
/// Decisions run against the base layer (`merge(template, local)`), so the
/// local file behaves as part of the template: reverting to a local value
/// cleans the override pin, and deletions consult base membership.
fn fold_candidate(
    candidate: &MemberView<'_>,
    format: DocFormat,
    ops: &[DiffOp],
) -> crate::Result<Value> {
    let mut tentative = match candidate.override_text {
        Some(text) => merge::parse_doc(
            text,
            format,
            &format!("override for template `{}`", candidate.name),
        )?,
        None => Value::Null,
    };
    let template_value = merge::parse_doc(
        candidate.template_text,
        format,
        &format!("template `{}`", candidate.name),
    )?;
    let base_value = match candidate.local_text {
        Some(text) => {
            let local = merge::parse_doc(
                text,
                format,
                &format!("local override for template `{}`", candidate.name),
            )?;
            merge::merge_structured(template_value, local, candidate.policy, candidate.name)?
        }
        None => template_value,
    };
    for op in ops {
        match op {
            DiffOp::Set { path, value, .. } => {
                if !path.is_empty() && get_path(&base_value, path) == Some(value) {
                    remove_path(&mut tentative, path)?;
                } else {
                    set_path_strict(&mut tentative, path, value.clone())?;
                }
            }
            DiffOp::Delete { path } => {
                if get_path(&base_value, path).is_some() {
                    set_path_strict(
                        &mut tentative,
                        path,
                        Value::String(DELETE_MARKER.to_string()),
                    )?;
                } else {
                    remove_path(&mut tentative, path)?;
                }
            }
        }
    }
    Ok(tentative)
}

/// Try every candidate: filter ops by its ignore lists, fold, replay the full
/// pipeline, compare masked.
///
/// Pipeline errors (e.g. combine conflicts from a wrong attribution) reject
/// the candidate. Zero passes fail loudly with a snapshot; several passes
/// mean genuine ambiguity.
fn try_candidates(
    dest: &Path,
    members: &[MemberView<'_>],
    candidates: &[&MemberView<'_>],
    ops: &[DiffOp],
    target: &SafetyTarget<'_>,
    generated_untouched: bool,
) -> crate::Result<BackpropOutcome> {
    let mut passing = Vec::new();
    for candidate in candidates {
        let format = match candidate.strategy {
            EffectiveStrategy::Structured(format) => format,
            _ => continue,
        };
        let filtered: Vec<DiffOp> = ops
            .iter()
            .filter(|op| !op_ignored(op, &candidate.ignore_keys, &candidate.ignore_values))
            .cloned()
            .collect();
        let tentative = match fold_candidate(candidate, format, &filtered) {
            Ok(tentative) => tentative,
            Err(err) => {
                tracing::debug!(
                    template = candidate.name,
                    "back-propagation fold rejected: {err}"
                );
                continue;
            }
        };
        match safety_replay(
            candidate,
            members,
            &tentative,
            target.value,
            &candidate.ignore_keys,
            &candidate.ignore_values,
        ) {
            Ok(replay) => {
                let restored = match restore_replay(
                    dest,
                    replay,
                    target.user_bytes,
                    format,
                    &candidate.ignore_keys,
                    &candidate.ignore_values,
                    candidate.name,
                ) {
                    Ok(restored) => restored,
                    Err(err) => {
                        tracing::debug!(
                            template = candidate.name,
                            "back-propagation restore rejected: {err}"
                        );
                        continue;
                    }
                };
                let current_override = match candidate.override_text {
                    Some(text) => merge::parse_doc(
                        text,
                        format,
                        &format!("override for template `{}`", candidate.name),
                    )?,
                    None => Value::Null,
                };
                // Skip the rewrite when the fold changed nothing: preserves
                // the user's formatting and comments.
                let new_override_text = if tentative == current_override {
                    None
                } else {
                    Some(merge::serialize_doc(&tentative, format)?)
                };
                passing.push((
                    candidate.name,
                    candidate.override_path.map(Path::to_path_buf),
                    new_override_text,
                    restored,
                ));
            }
            Err(reason) => {
                tracing::debug!(
                    template = candidate.name,
                    "back-propagation replay rejected: {reason}"
                );
            }
        }
    }
    match passing.len() {
        0 => {
            let snapshot = write_snapshot(dest, target.user_bytes);
            Err(mismatch_error(
                dest,
                &snapshot,
                generated_untouched,
                candidates
                    .iter()
                    .any(|candidate| candidate.local_text.is_some()),
            ))
        }
        1 => {
            let (name, _, new_override_text, replay_bytes) =
                passing.pop().expect("one passing candidate");
            Ok(BackpropOutcome::Applied {
                member: name.to_string(),
                new_override_text,
                replay_bytes,
            })
        }
        _ => {
            // Collapse when every passing candidate agrees on every
            // observable outcome (see `collapsing`).
            if collapsing(&passing) {
                let (name, _, new_override_text, replay_bytes) =
                    passing.into_iter().next().expect("passing is non-empty");
                return Ok(BackpropOutcome::Applied {
                    member: name.to_string(),
                    new_override_text,
                    replay_bytes,
                });
            }
            let mut names: Vec<&str> = passing.iter().map(|(name, _, _, _)| *name).collect();
            names.sort();
            Err(crate::invalid(
                dest,
                format!(
                    "back propagation is ambiguous: the edit is reproducible through {}: edit the override files directly to disambiguate",
                    names
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ))
        }
    }
}

/// One passing candidate: template name, override path, override text, replay bytes.
type Passing<'a> = (&'a str, Option<PathBuf>, Option<String>, Vec<u8>);

/// True when all passing candidates agree on every observable outcome.
///
/// Outcomes are the replay bytes plus the set of override writes, where a
/// `None` text means no write. Candidates that fold nothing therefore
/// collapse regardless of paths; divergent writes stay ambiguous. A write to
/// an unknown file can never collapse.
fn collapsing(passing: &[Passing<'_>]) -> bool {
    let [(_, _, _, first_replay), rest @ ..] = passing else {
        return false;
    };
    if rest.iter().any(|(_, _, _, replay)| replay != first_replay) {
        return false;
    }
    let mut writes = std::collections::BTreeSet::new();
    for (_, path, text, _) in passing {
        match (path, text) {
            (Some(path), Some(text)) => {
                writes.insert((path.clone(), text.clone()));
            }
            (_, None) => {}
            (None, Some(_)) => return false,
        }
    }
    writes.len() <= 1
}

/// What the safety replay compares against and recovers from.
struct SafetyTarget<'a> {
    /// Comparison value (desired output, compared masked).
    value: &'a Value,
    /// User bytes: maintained state restores from these; snapshotted on failure.
    user_bytes: &'a [u8],
}

/// Restore maintained state into replay output and serialize.
///
/// Ignored paths take the user's subtree (or stay removed when the user has
/// none); everything else keeps pipeline output. Returns bytes safe to write.
fn restore_replay(
    dest: &Path,
    replay: Value,
    user_bytes: &[u8],
    format: DocFormat,
    ignore_keys: &[IgnorePattern],
    ignore_values: &[IgnorePattern],
    origin: &str,
) -> crate::Result<Vec<u8>> {
    let mut restored = replay;
    if !ignore_keys.is_empty() || !ignore_values.is_empty() {
        let user = parse_generated(user_bytes, dest, format, "hand-edited")?;
        restore_into(
            &mut restored,
            &user,
            &mut Vec::new(),
            ignore_keys,
            ignore_values,
        );
    }
    merge::serialize_doc(&restored, format)
        .map(String::into_bytes)
        .map_err(|err| {
            crate::invalid(
                Path::new(origin),
                format!("cannot serialize back-propagated output: {err}"),
            )
        })
}

/// Replay the full pipeline with a tentative override and masked-compare.
///
/// Returns the parsed replay on masked equality, else a rejection reason.
fn safety_replay(
    candidate: &MemberView<'_>,
    members: &[MemberView<'_>],
    tentative: &Value,
    target: &Value,
    ignore_keys: &[IgnorePattern],
    ignore_values: &[IgnorePattern],
) -> Result<Value, String> {
    let format = match candidate.strategy {
        EffectiveStrategy::Structured(format) => format,
        _ => return Err("non-structured candidate in structured replay".to_string()),
    };
    let tentative_text = merge::serialize_doc(tentative, format).map_err(|err| format!("{err}"))?;
    let mut contributions = Vec::with_capacity(members.len());
    for member in members {
        // A fold that changed nothing replays the original override state:
        // serializing a Null tentative would pass explicit JSON `null`,
        // which replaces the whole document, while a missing override
        // skips the merge entirely.
        let text = if member.name == candidate.name {
            if tentative.is_null() && member.override_text.is_none() {
                None
            } else {
                Some(tentative_text.as_str())
            }
        } else {
            member.override_text
        };
        let rendered = crate::generate::render_contents(
            member.template_text,
            text,
            member.local_text,
            member.strategy,
            member.policy,
            member.name,
        )
        .map_err(|err| format!("{err}"))?;
        match rendered {
            merge::Rendered::Structured { value, .. } => contributions.push(merge::Contribution {
                template: member.name,
                labels: member.labels,
                value,
            }),
            merge::Rendered::Text(_) => {
                return Err("mixed text output in structured replay".to_string());
            }
        }
    }
    let combined = merge::combine_structured(&contributions).map_err(|err| format!("{err}"))?;
    let replay_text = merge::serialize_doc(&combined, format).map_err(|err| format!("{err}"))?;
    let replay: Value =
        merge::parse_doc(&replay_text, format, "safety replay").map_err(|err| format!("{err}"))?;
    let mut path = Vec::new();
    if compare_masked(&replay, target, &mut path, ignore_keys, ignore_values) {
        Ok(replay)
    } else {
        Err("replay output differs from the generated file".to_string())
    }
}

// ---- Text fold ------------------------------------------------------------------

/// Fold a hand-edit into a text override.
///
/// `replace` takes the content verbatim; `append_*` strips the known template
/// and local portions to recover the middle override segment (edits outside
/// it fail loudly instead of misattributing).
fn fold_text(
    template_text: &str,
    local_text: Option<&str>,
    current: &str,
    strategy: EffectiveStrategy,
    name: &str,
    dest: &Path,
) -> crate::Result<String> {
    /// Strip one separator-joined segment off a side.
    fn strip_segment<'a>(text: &'a str, segment: &str, prefix: bool) -> Option<&'a str> {
        if prefix {
            text.strip_prefix(segment)
                .and_then(|rest| rest.strip_prefix('\n').or(Some(rest)))
        } else {
            text.strip_suffix(segment)
                .and_then(|rest| rest.strip_suffix('\n').or(Some(rest)))
        }
    }
    let attribution = || {
        crate::invalid(
            dest,
            format!(
                "template `{name}`: the edit touches outside the override portion of the file, which cannot fold back: edit the override file directly"
            ),
        )
    };
    match strategy {
        EffectiveStrategy::Replace => Ok(current.to_string()),
        EffectiveStrategy::AppendBottom => {
            // template \n override [\n local]: strip the template head, then
            // the local tail when a local file exists.
            let mut rest = strip_segment(current, template_text, true).ok_or_else(attribution)?;
            if let Some(local) = local_text {
                rest = strip_segment(rest, local, false).ok_or_else(attribution)?;
            }
            Ok(rest.to_string())
        }
        EffectiveStrategy::AppendTop => {
            // [local \n] override \n template: strip the local head when a
            // local file exists, then the template tail.
            let mut rest = current;
            if let Some(local) = local_text {
                rest = strip_segment(rest, local, true).ok_or_else(attribution)?;
            }
            Ok(strip_segment(rest, template_text, false)
                .ok_or_else(attribution)?
                .to_string())
        }
        EffectiveStrategy::Structured(_) => Err(crate::invalid(
            dest,
            "internal error: text fold called for a structured template".to_string(),
        )),
        EffectiveStrategy::None => Err(crate::invalid(
            dest,
            format!(
                "template `{name}`: strategy `none` copies the template verbatim with no override to fold into"
            ),
        )),
    }
}

/// Replay a text candidate and compare bytes exactly.
fn replay_text(
    dest: &Path,
    candidate: &MemberView<'_>,
    tentative: &str,
    target_bytes: &[u8],
    generated_untouched: bool,
) -> crate::Result<BackpropOutcome> {
    let rendered = crate::generate::render_contents(
        candidate.template_text,
        Some(tentative),
        candidate.local_text,
        candidate.strategy,
        candidate.policy,
        candidate.name,
    )
    .map_err(|err| {
        tracing::debug!(
            template = candidate.name,
            "back-propagation replay rejected: {err}"
        );
        let snapshot = write_snapshot(dest, target_bytes);
        mismatch_error(
            dest,
            &snapshot,
            generated_untouched,
            candidate.local_text.is_some(),
        )
    })?;
    match rendered {
        merge::Rendered::Text(bytes) if bytes.as_bytes() == target_bytes => {
            // Skip the rewrite when the fold changed nothing.
            let unchanged = candidate.override_text == Some(tentative);
            Ok(BackpropOutcome::Applied {
                member: candidate.name.to_string(),
                new_override_text: (!unchanged).then(|| tentative.to_string()),
                replay_bytes: bytes.into_bytes(),
            })
        }
        _ => {
            let snapshot = write_snapshot(dest, target_bytes);
            Err(mismatch_error(
                dest,
                &snapshot,
                generated_untouched,
                candidate.local_text.is_some(),
            ))
        }
    }
}

// ---- Snapshots and diagnostics ----------------------------------------------------

/// Save conflicting content for recovery; best-effort, always returns the path.
pub(crate) fn write_snapshot(dest: &Path, content: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(CONFLICT_SNAPSHOT_DIR_NAME);
    let stem = dest
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("generated");
    let extension = dest
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("{stem}.{timestamp}.conflict{extension}"));
    if let Err(err) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, content)) {
        tracing::warn!("cannot write conflict snapshot: {err}");
    }
    path
}

/// Loud safety-mismatch error naming the recovery snapshot.
fn mismatch_error(
    dest: &Path,
    snapshot: &Path,
    generated_untouched: bool,
    local_involved: bool,
) -> crate::Error {
    let state = if generated_untouched {
        "both files left untouched"
    } else {
        "template output kept"
    };
    let local_hint = if local_involved {
        " A local override file is also in play: keys it defines always win over the shared override, so edits to those keys cannot fold back — edit the local file itself instead."
    } else {
        ""
    };
    crate::invalid(
        dest,
        format!(
            "back propagation safety check failed: replaying the computed override through the template does not reproduce the generated file ({state}). Common cause: array edits under `array_policy = \"union\"`, which re-adds template items — switch the template to `\"replace\"` or edit the override directly.{local_hint} Conflicting generated content saved to `{}`",
            snapshot.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn view<'a>(
        name: &'a str,
        label_set: &'a BTreeSet<String>,
        template_text: &'a str,
        override_text: Option<&'a str>,
        strategy: EffectiveStrategy,
    ) -> MemberView<'a> {
        local_member(
            name,
            label_set,
            template_text,
            override_text,
            None,
            strategy,
        )
    }

    fn local_member<'a>(
        name: &'a str,
        label_set: &'a BTreeSet<String>,
        template_text: &'a str,
        override_text: Option<&'a str>,
        local_text: Option<&'a str>,
        strategy: EffectiveStrategy,
    ) -> MemberView<'a> {
        viewed_member(
            name,
            label_set,
            None,
            template_text,
            override_text,
            local_text,
            strategy,
        )
    }

    fn viewed_member<'a>(
        name: &'a str,
        label_set: &'a BTreeSet<String>,
        override_path: Option<&'a Path>,
        template_text: &'a str,
        override_text: Option<&'a str>,
        local_text: Option<&'a str>,
        strategy: EffectiveStrategy,
    ) -> MemberView<'a> {
        MemberView {
            name,
            labels: label_set,
            override_path,
            template_text,
            override_text,
            local_text,
            strategy,
            policy: ArrayPolicy::Union,
            back_propagate: true,
            ignore_keys: Vec::new(),
            ignore_values: Vec::new(),
        }
    }

    fn json_member<'a>(
        name: &'a str,
        label_set: &'a BTreeSet<String>,
        template_text: &'a str,
        override_text: Option<&'a str>,
    ) -> MemberView<'a> {
        view(
            name,
            label_set,
            template_text,
            override_text,
            EffectiveStrategy::Structured(DocFormat::Json),
        )
    }

    fn parse(text: &str) -> Value {
        merge::parse_doc(text, DocFormat::Json, "test").expect("test json parses")
    }

    fn last_of(template_text: &str, override_text: Option<&str>) -> Vec<u8> {
        let base = parse(template_text);
        let merged = match override_text {
            Some(text) => {
                merge::merge_structured(base, parse(text), ArrayPolicy::Union, "t").unwrap()
            }
            None => base,
        };
        merge::serialize_doc(&merged, DocFormat::Json)
            .unwrap()
            .into_bytes()
    }

    fn applied(outcome: BackpropOutcome) -> (String, Option<String>, Vec<u8>) {
        match outcome {
            BackpropOutcome::Applied {
                member,
                new_override_text,
                replay_bytes,
            } => (member, new_override_text, replay_bytes),
            BackpropOutcome::NoChange => panic!("expected Applied, got NoChange"),
        }
    }

    fn applied_override(outcome: BackpropOutcome) -> (String, String, Vec<u8>) {
        let (member, override_text, replay) = applied(outcome);
        (
            member,
            override_text.expect("expected an override rewrite"),
            replay,
        )
    }

    #[test]
    fn scalar_change_folds_into_override() {
        let labels = labels(&["rust"]);
        let last = last_of("{\"a\": 1, \"b\": 2}", Some("{\"b\": 20}"));
        let current = b"{\"a\": 1, \"b\": 30}\n";
        let members = [json_member(
            "app",
            &labels,
            "{\"a\": 1, \"b\": 2}",
            Some("{\"b\": 20}"),
        )];
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        let (member, _, replay) = applied_override(outcome);
        assert_eq!(member, "app");
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"a\": 1, \"b\": 30}")
        );
    }

    #[test]
    fn added_key_folds_into_override() {
        let labels = labels(&["rust"]);
        let last = last_of("{\"a\": 1}", None);
        let current = b"{\"a\": 1, \"fresh\": true}\n";
        let members = [json_member("app", &labels, "{\"a\": 1}", None)];
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        let (_, _, replay) = applied_override(outcome);
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"a\": 1, \"fresh\": true}")
        );
    }

    #[test]
    fn deleted_template_key_becomes_marker() {
        let labels = labels(&["rust"]);
        let last = last_of("{\"drop\": 1, \"keep\": 2}", None);
        let current = b"{\"keep\": 2}\n";
        let members = [json_member(
            "app",
            &labels,
            "{\"drop\": 1, \"keep\": 2}",
            None,
        )];
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        let (_, text, _) = applied_override(outcome);
        assert_eq!(parse(&text), parse("{\"drop\": \"_TEMPLATRY_DELETE_\"}"));
    }

    #[test]
    fn deleted_override_key_leaves_the_override() {
        let labels = labels(&["rust"]);
        let last = last_of("{\"a\": 1}", Some("{\"extra\": 9}"));
        let current = b"{\"a\": 1}\n";
        let members = [json_member(
            "app",
            &labels,
            "{\"a\": 1}",
            Some("{\"extra\": 9}"),
        )];
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        let (_, text, _) = applied_override(outcome);
        assert_eq!(parse(&text), parse("{}"));
    }

    #[test]
    fn revert_to_template_value_cleans_the_override() {
        let labels = labels(&["rust"]);
        let last = last_of("{\"k\": 1}", Some("{\"k\": 2}"));
        let current = b"{\"k\": 1}\n";
        let members = [json_member(
            "app",
            &labels,
            "{\"k\": 1}",
            Some("{\"k\": 2}"),
        )];
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        let (_, text, replay) = applied_override(outcome);
        assert_eq!(parse(&text), parse("{}"));
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"k\": 1}")
        );
    }

    #[test]
    fn array_edit_under_union_fails_loudly() {
        let labels = labels(&["rust"]);
        let last = last_of("{\"l\": [1, 2]}", None);
        let current = b"{\"l\": [2]}\n";
        let members = [json_member("app", &labels, "{\"l\": [1, 2]}", None)];
        let err = backpropagate(Path::new("out.json"), &last, current, &members).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("safety check failed"), "{message}");
        assert!(message.contains("union"), "{message}");
        assert!(message.contains("saved to"), "{message}");
    }

    #[test]
    fn identical_edits_are_ambiguous() {
        let rust = labels(&["rust"]);
        let dart = labels(&["dart"]);
        let last = last_of("{\"a\": 1}", None);
        let current = b"{\"a\": 1, \"z\": 3}\n";
        let members = [
            json_member("rust-part", &rust, "{\"a\": 1}", None),
            json_member("dart-part", &dart, "{\"a\": 1}", None),
        ];
        let err = backpropagate(Path::new("out.json"), &last, current, &members).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("rust-part"), "{message}");
        assert!(message.contains("dart-part"), "{message}");
    }

    #[test]
    fn identical_writes_to_a_shared_override_collapse() {
        // Same override file, same fold, same replay: attribution is moot.
        let shared = Path::new("shared.json");
        let rust = labels(&["rust"]);
        let dart = labels(&["dart"]);
        let last = last_of("{\"a\": 1}", None);
        let current = b"{\"a\": 1, \"z\": 3}\n";
        let mut members = [
            viewed_member(
                "rust-part",
                &rust,
                Some(shared),
                "{\"a\": 1}",
                None,
                None,
                EffectiveStrategy::Structured(DocFormat::Json),
            ),
            viewed_member(
                "dart-part",
                &dart,
                Some(shared),
                "{\"a\": 1}",
                None,
                None,
                EffectiveStrategy::Structured(DocFormat::Json),
            ),
        ];
        for member in &mut members {
            member.ignore_keys = vec![IgnorePattern::parse("z").unwrap()];
        }
        // All ops filter out in every candidate, no override files exist:
        // attribution is moot, the restored replay applies, no rewrite.
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        match outcome {
            BackpropOutcome::Applied {
                member,
                new_override_text,
                replay_bytes,
            } => {
                assert!(
                    new_override_text.is_none(),
                    "nothing folded, nothing to write"
                );
                assert_eq!(
                    parse(std::str::from_utf8(&replay_bytes).unwrap()),
                    parse("{\"a\": 1, \"z\": 3}")
                );
                assert!(["rust-part", "dart-part"].contains(&member.as_str()));
            }
            BackpropOutcome::NoChange => panic!("expected Applied"),
        }
    }

    #[test]
    fn identical_text_different_files_stay_ambiguous() {
        // Same folded content but different override files: still ambiguous,
        // since two files would change.
        let rust = labels(&["rust"]);
        let dart = labels(&["dart"]);
        let last = last_of("{\"a\": 1}", None);
        let current = b"{\"a\": 1, \"z\": 3}\n";
        let members = [
            viewed_member(
                "rust-part",
                &rust,
                Some(Path::new("rust.json")),
                "{\"a\": 1}",
                None,
                None,
                EffectiveStrategy::Structured(DocFormat::Json),
            ),
            viewed_member(
                "dart-part",
                &dart,
                Some(Path::new("dart.json")),
                "{\"a\": 1}",
                None,
                None,
                EffectiveStrategy::Structured(DocFormat::Json),
            ),
        ];
        let err = backpropagate(Path::new("out.json"), &last, current, &members).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err:?}");
    }

    #[test]
    fn no_writes_anywhere_collapses_regardless_of_paths() {
        // Every op filters out, so no candidate writes anything and all
        // replays match: the differing (but write-free) override paths do
        // not make this ambiguous.
        let rust = labels(&["rust"]);
        let dart = labels(&["dart"]);
        let last = last_of("{\"a\": 1}", None);
        let current = b"{\"a\": 1, \"z\": 3}\n";
        let members = [
            viewed_member(
                "rust-part",
                &rust,
                Some(Path::new("rust.json")),
                "{\"a\": 1}",
                None,
                None,
                EffectiveStrategy::Structured(DocFormat::Json),
            ),
            viewed_member(
                "dart-part",
                &dart,
                Some(Path::new("dart.json")),
                "{\"a\": 1}",
                None,
                None,
                EffectiveStrategy::Structured(DocFormat::Json),
            ),
        ];
        let mut ignored = members;
        for member in &mut ignored {
            member.ignore_keys = vec![IgnorePattern::parse("z").unwrap()];
        }
        let outcome = backpropagate(Path::new("out.json"), &last, current, &ignored).unwrap();
        match outcome {
            BackpropOutcome::Applied {
                new_override_text,
                replay_bytes,
                ..
            } => {
                assert!(
                    new_override_text.is_none(),
                    "nothing folded, nothing to write"
                );
                assert_eq!(
                    parse(std::str::from_utf8(&replay_bytes).unwrap()),
                    parse("{\"a\": 1, \"z\": 3}")
                );
            }
            BackpropOutcome::NoChange => panic!("expected Applied"),
        }
    }

    #[test]
    fn shared_edit_scopes_to_the_right_contributor() {
        let rust = labels(&["rust"]);
        let dart = labels(&["dart"]);
        let rust_last = parse("{\"x\": 1}");
        let dart_last = parse("{\"y\": 1}");
        let combined = {
            let contributions = [
                merge::Contribution {
                    template: "a",
                    labels: &rust,
                    value: rust_last,
                },
                merge::Contribution {
                    template: "b",
                    labels: &dart,
                    value: dart_last,
                },
            ];
            merge::serialize_doc(
                &merge::combine_structured(&contributions).unwrap(),
                DocFormat::Json,
            )
            .unwrap()
            .into_bytes()
        };
        let current = b"{\"x\": 2, \"y\": 1}\n";
        let members = [
            json_member("a", &rust, "{\"x\": 1}", None),
            json_member("b", &dart, "{\"y\": 1}", None),
        ];
        let outcome = backpropagate(Path::new("out.json"), &combined, current, &members).unwrap();
        let (member, _, replay) = applied_override(outcome);
        assert_eq!(member, "a");
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"x\": 2, \"y\": 1}")
        );
    }

    #[test]
    fn no_change_and_no_candidates() {
        let labels = labels(&["rust"]);
        let last = last_of("{\"a\": 1}", None);
        let members = [json_member("app", &labels, "{\"a\": 1}", None)];
        assert!(matches!(
            backpropagate(Path::new("o"), &last, &last, &members).unwrap(),
            BackpropOutcome::NoChange
        ));

        let mut plain = view(
            "app",
            &labels,
            "{\"a\": 1}",
            None,
            EffectiveStrategy::Structured(DocFormat::Json),
        );
        plain.back_propagate = false;
        assert!(matches!(
            backpropagate(Path::new("o"), &last, b"{\"a\": 2}", &[plain]).unwrap(),
            BackpropOutcome::NoChange
        ));
    }

    #[test]
    fn text_append_round_trip() {
        let labels = labels(&[]);
        let template_text = "TEMPLATRY\n";
        let members = [view(
            "banner",
            &labels,
            template_text,
            None,
            EffectiveStrategy::AppendBottom,
        )];
        let last = b"TEMPLATRY\n";
        let current = b"TEMPLATRY\n\nO=1\n";
        let outcome = backpropagate(Path::new("out.txt"), last, current, &members).unwrap();
        let (member, _, replay) = applied_override(outcome);
        assert_eq!(member, "banner");
        assert_eq!(replay, current);
    }

    #[test]
    fn text_template_region_edit_fails() {
        let labels = labels(&[]);
        let members = [view(
            "banner",
            &labels,
            "TEMPLATRY\n",
            None,
            EffectiveStrategy::AppendBottom,
        )];
        let err = backpropagate(Path::new("out.txt"), b"TEMPLATRY\n", b"CHANGED\n", &members)
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the override portion"),
            "{err:?}"
        );
    }

    #[test]
    fn none_strategy_has_no_override_to_fold_into() {
        let labels = labels(&[]);
        let members = [view(
            "license",
            &labels,
            "MIT\n",
            Some("WRONG\n"),
            EffectiveStrategy::None,
        )];
        let err =
            backpropagate(Path::new("out.txt"), b"MIT\n", b"CHANGED\n", &members).unwrap_err();
        assert!(
            err.to_string().contains("no override to fold into"),
            "{err:?}"
        );
    }

    #[test]
    fn replace_round_trip() {
        let labels = labels(&[]);
        let members = [view(
            "env",
            &labels,
            "A=template\n",
            Some("A=1\n"),
            EffectiveStrategy::Replace,
        )];
        let outcome = backpropagate(Path::new("out.env"), b"A=1\n", b"A=2\n", &members).unwrap();
        let (_, _, replay) = applied_override(outcome);
        assert_eq!(replay, b"A=2\n");
    }

    #[test]
    fn reapply_survives_a_template_bump() {
        let labels = labels(&["rust"]);
        let template_old = "{\"k\": 1, \"u\": \"old\"}";
        let last = last_of(template_old, None);
        let captured = b"{\"k\": 2, \"u\": \"old\"}\n";

        let template_new = "{\"k\": 1, \"u\": \"new\", \"n\": true}";
        let forward = merge::serialize_doc(&parse(template_new), DocFormat::Json)
            .unwrap()
            .into_bytes();
        let members = [json_member("app", &labels, template_new, None)];
        let outcome = reapply(Path::new("out.json"), &last, captured, &forward, &members).unwrap();
        let (member, _, replay) = applied_override(outcome);
        assert_eq!(member, "app");
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"k\": 2, \"u\": \"new\", \"n\": true}")
        );
    }

    #[test]
    fn flat_dotted_ignored_key_stays_maintained() {
        // VS Code shape: flat dotted keys. Editing an ignored one folds
        // nothing and preserves the value; editing a sibling folds normally.
        let labels = labels(&["common"]);
        let template_text = "{\"editor.fontSize\": 14, \"java.jdt.ls.java.home\": \"/tpl\"}";
        let last = last_of(template_text, None);
        let current = b"{\"editor.fontSize\": 16, \"java.jdt.ls.java.home\": \"/hand\"}\n";
        let mut member = json_member("settings", &labels, template_text, None);
        member.ignore_keys = vec![pattern("java.jdt.ls.java.home")];
        let outcome = backpropagate(Path::new("settings.json"), &last, current, &[member]).unwrap();
        match outcome {
            BackpropOutcome::Applied {
                new_override_text,
                replay_bytes,
                ..
            } => {
                // Only the sibling folds; the ignored key is absent.
                assert_eq!(
                    parse(&new_override_text.expect("override written")),
                    parse("{\"editor.fontSize\": 16}")
                );
                assert_eq!(
                    parse(std::str::from_utf8(&replay_bytes).unwrap()),
                    parse("{\"editor.fontSize\": 16, \"java.jdt.ls.java.home\": \"/hand\"}")
                );
            }
            BackpropOutcome::NoChange => panic!("expected Applied"),
        }
    }

    #[test]
    fn ignored_only_edit_without_override_applies() {
        // All ops filter out and no override file exists: the tentative
        // stays Null, which must replay as a *missing* override (skipping
        // the merge), not as explicit JSON null (which would replace the
        // whole document and fail safety). Regression test for ignored
        // machine-path edits with no override file present.
        let labels = labels(&["common"]);
        let template_text =
            "{\"java.jdt.ls.java.home\": \"/tpl\", \"java.import.gradle.java.home\": \"/tpl\"}";
        let last = last_of(template_text, None);
        let current =
            b"{\"java.jdt.ls.java.home\": \"/hand\", \"java.import.gradle.java.home\": \"/hand\"}\n";
        let mut member = json_member("settings", &labels, template_text, None);
        member.ignore_keys = vec![
            pattern("java.jdt.ls.java.home"),
            pattern("java.import.gradle.java.home"),
        ];
        let outcome = backpropagate(Path::new("settings.json"), &last, current, &[member]).unwrap();
        match outcome {
            BackpropOutcome::Applied {
                new_override_text,
                replay_bytes,
                ..
            } => {
                assert!(new_override_text.is_none(), "no override to write");
                assert_eq!(
                    parse(std::str::from_utf8(&replay_bytes).unwrap()),
                    parse(
                        "{\"java.jdt.ls.java.home\": \"/hand\", \"java.import.gradle.java.home\": \"/hand\"}"
                    )
                );
            }
            BackpropOutcome::NoChange => panic!("expected Applied"),
        }
    }

    #[test]
    fn edit_to_local_pinned_key_fails_loudly() {
        // The local layer always outranks the override, so no override
        // content can reproduce shared=40 while local pins 30: loud error
        // pointing at the local file, both files untouched.
        let labels = labels(&["rust"]);
        let last = last_of_three(
            "{\"a\": 1, \"shared\": 1}",
            Some("{\"shared\": 20}"),
            "{\"local\": true, \"shared\": 30}",
        );
        let current = b"{\n  \"a\": 1,\n  \"local\": true,\n  \"shared\": 40\n}\n";
        let members = [local_member(
            "app",
            &labels,
            "{\"a\": 1, \"shared\": 1}",
            Some("{\"shared\": 20}"),
            Some("{\"local\": true, \"shared\": 30}"),
            EffectiveStrategy::Structured(DocFormat::Json),
        )];
        let err = backpropagate(Path::new("out.json"), &last, current, &members).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("safety check failed"), "{message}");
        assert!(message.contains("local override file"), "{message}");
    }

    #[test]
    fn snapshot_file_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("settings.json");
        let path = write_snapshot(&dest, b"{}");
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        let name = path.file_name().expect("name").to_string_lossy();
        assert!(name.starts_with("settings.") && name.contains(".conflict.json"));
    }

    fn last_of_three(
        template_text: &str,
        override_text: Option<&str>,
        local_text: &str,
    ) -> Vec<u8> {
        let base = parse(template_text);
        let merged = match override_text {
            Some(text) => {
                merge::merge_structured(base, parse(text), ArrayPolicy::Union, "t").unwrap()
            }
            None => base,
        };
        let merged =
            merge::merge_structured(merged, parse(local_text), ArrayPolicy::Union, "t").unwrap();
        merge::serialize_doc(&merged, DocFormat::Json)
            .unwrap()
            .into_bytes()
    }

    #[test]
    fn local_layer_flows_into_safety_replay() {
        let labels = labels(&["rust"]);
        let last = last_of_three(
            "{\"a\": 1, \"b\": 1}",
            Some("{\"b\": 2}"),
            "{\"b\": 3, \"c\": 9}",
        );
        let current = b"{\"a\": 10, \"b\": 3, \"c\": 9}\n";
        let members = [local_member(
            "app",
            &labels,
            "{\"a\": 1, \"b\": 1}",
            Some("{\"b\": 2}"),
            Some("{\"b\": 3, \"c\": 9}"),
            EffectiveStrategy::Structured(DocFormat::Json),
        )];
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        let (member, override_text, replay) = applied_override(outcome);
        assert_eq!(member, "app");
        // Fold targets the main override; the local layer is untouched.
        assert_eq!(parse(&override_text), parse("{\"a\": 10, \"b\": 2}"));
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"a\": 10, \"b\": 3, \"c\": 9}")
        );
    }

    #[test]
    fn revert_to_local_value_cleans_override() {
        // The edit matches a newly added local value: folding pins nothing,
        // and the replay still reproduces the file through the local layer.
        let labels = labels(&["rust"]);
        let last = last_of("{\"k\": 1}", Some("{\"k\": 2}"));
        let current = b"{\"k\": 3}\n";
        let members = [local_member(
            "app",
            &labels,
            "{\"k\": 1}",
            Some("{\"k\": 2}"),
            Some("{\"k\": 3}"),
            EffectiveStrategy::Structured(DocFormat::Json),
        )];
        let outcome = backpropagate(Path::new("out.json"), &last, current, &members).unwrap();
        let (_, text, replay) = applied_override(outcome);
        assert_eq!(parse(&text), parse("{}"));
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"k\": 3}")
        );
    }

    #[test]
    fn delete_local_held_key_fails_loudly() {
        // The local layer always re-adds the key, so no override content can
        // reproduce the deletion: the safety check must fail, not silently pass.
        let labels = labels(&["rust"]);
        let last = last_of_three("{\"a\": 1}", None, "{\"d\": 1}");
        let current = b"{\"a\": 1}\n";
        let members = [local_member(
            "app",
            &labels,
            "{\"a\": 1}",
            None,
            Some("{\"d\": 1}"),
            EffectiveStrategy::Structured(DocFormat::Json),
        )];
        let err = backpropagate(Path::new("out.json"), &last, current, &members).unwrap_err();
        assert!(err.to_string().contains("safety check failed"), "{err:?}");
    }

    #[test]
    fn shared_local_scopes_to_its_contributor() {
        let rust = labels(&["rust"]);
        let dart = labels(&["dart"]);
        let rust_last = parse("{\"x\": 1, \"lx\": 1}");
        let dart_last = parse("{\"y\": 1}");
        let combined = {
            let contributions = [
                merge::Contribution {
                    template: "a",
                    labels: &rust,
                    value: rust_last,
                },
                merge::Contribution {
                    template: "b",
                    labels: &dart,
                    value: dart_last,
                },
            ];
            merge::serialize_doc(
                &merge::combine_structured(&contributions).unwrap(),
                DocFormat::Json,
            )
            .unwrap()
            .into_bytes()
        };
        let current = b"{\"lx\": 1, \"x\": 2, \"y\": 1}\n";
        let members = [
            local_member(
                "a",
                &rust,
                "{\"x\": 1}",
                None,
                Some("{\"lx\": 1}"),
                EffectiveStrategy::Structured(DocFormat::Json),
            ),
            json_member("b", &dart, "{\"y\": 1}", None),
        ];
        let outcome = backpropagate(Path::new("out.json"), &combined, current, &members).unwrap();
        let (member, text, _) = applied_override(outcome);
        assert_eq!(member, "a");
        assert_eq!(parse(&text), parse("{\"x\": 2}"));
    }

    #[test]
    fn append_three_layer_fold_recovers_middle() {
        let labels = labels(&[]);
        let members = [local_member(
            "banner",
            &labels,
            "T\n",
            Some("O\n"),
            Some("L\n"),
            EffectiveStrategy::AppendBottom,
        )];
        let last = b"T\n\nO\n\nL\n";
        let current = b"T\n\nO2\n\nL\n";
        let outcome = backpropagate(Path::new("out.txt"), last, current, &members).unwrap();
        let (_, text, replay) = applied_override(outcome);
        assert_eq!(text, "O2\n");
        assert_eq!(replay, current);
    }

    #[test]
    fn append_local_region_edit_fails() {
        let labels = labels(&[]);
        let members = [local_member(
            "banner",
            &labels,
            "T\n",
            Some("O\n"),
            Some("L\n"),
            EffectiveStrategy::AppendBottom,
        )];
        let last = b"T\n\nO\n\nL\n";
        let err = backpropagate(Path::new("out.txt"), last, b"T\n\nO\n\nCHANGED\n", &members)
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the override portion"),
            "{err:?}"
        );
    }

    #[test]
    fn reapply_with_local_survives_template_bump() {
        let labels = labels(&["rust"]);
        let last = last_of_three("{\"k\": 1}", None, "{\"u\": 9}");
        let captured = b"{\"k\": 2, \"u\": 9}\n";
        let template_new = "{\"k\": 1, \"u\": \"new\"}";
        let forward = last_of_three(template_new, None, "{\"u\": 9}");
        let members = [local_member(
            "app",
            &labels,
            template_new,
            None,
            Some("{\"u\": 9}"),
            EffectiveStrategy::Structured(DocFormat::Json),
        )];
        let outcome = reapply(Path::new("out.json"), &last, captured, &forward, &members).unwrap();
        let (member, text, replay) = applied_override(outcome);
        assert_eq!(member, "app");
        assert_eq!(parse(&text), parse("{\"k\": 2}"));
        assert_eq!(
            parse(std::str::from_utf8(&replay).unwrap()),
            parse("{\"k\": 2, \"u\": 9}")
        );
    }

    fn pattern(text: &str) -> IgnorePattern {
        IgnorePattern::parse(text).expect("valid pattern")
    }

    fn path(segments: &[&str]) -> Vec<String> {
        segments.iter().map(|segment| segment.to_string()).collect()
    }

    #[test]
    fn ignore_patterns_match_paths_and_subtrees() {
        let machine = pattern("machine");
        assert!(machine.matches(&path(&["machine"])));
        assert!(machine.matches(&path(&["machine", "cpu", "cores"])));
        assert!(!machine.matches(&path(&["other"])));
        assert!(!machine.matches(&path(&[])));

        let wildcard = pattern("editor.*");
        assert!(wildcard.matches(&path(&["editor", "fontSize"])));
        assert!(wildcard.matches(&path(&["editor", "fontSize", "deep"])));
        assert!(!wildcard.matches(&path(&["editor"])));
        assert!(!wildcard.matches(&path(&["other", "x"])));

        let nested = pattern("a.*.c");
        assert!(nested.matches(&path(&["a", "b", "c"])));
        assert!(!nested.matches(&path(&["a", "b"])));
        assert!(nested.matches(&path(&["a", "b", "c", "d"])));
    }

    #[test]
    fn ignore_patterns_match_flat_dotted_keys() {
        // VS Code settings norm: one map key holding dots.
        let java_home = pattern("java.jdt.ls.java.home");
        assert!(java_home.matches(&path(&["java.jdt.ls.java.home"])));
        assert!(!java_home.matches(&path(&["java.jdt.ls.other.home"])));
        assert!(!java_home.matches(&path(&["java"])));

        // Flat keys match nested paths and vice versa: same setting.
        assert!(java_home.matches(&path(&["java", "jdt", "ls", "java.home"])));

        // Wildcards and subtrees work on flat keys too.
        let machine = pattern("machine.*");
        assert!(machine.matches(&path(&["machine.cpu"])));
        assert!(machine.matches(&path(&["machine.cpu", "cores"])));
        assert!(!machine.matches(&path(&["machine"])));
    }

    #[test]
    fn ignore_patterns_reject_bad_syntax() {
        for bad in ["", "   ", "a..b", ".a", "a.", "**.x", "a.**"] {
            assert!(IgnorePattern::parse(bad).is_err(), "should reject `{bad}`");
        }
        assert!(IgnorePattern::parse("a.*.b").is_ok());
    }

    #[test]
    fn op_filtering_respects_both_lists() {
        let keys = [pattern("locked")];
        let values = [pattern("val")];
        let set_changed = |p: &[&str]| DiffOp::Set {
            path: path(p),
            value: Value::Null,
            added: false,
        };
        let set_added = |p: &[&str]| DiffOp::Set {
            path: path(p),
            value: Value::Null,
            added: true,
        };

        assert!(op_ignored(&set_changed(&["locked", "x"]), &keys, &values));
        assert!(op_ignored(
            &DiffOp::Delete {
                path: path(&["locked"])
            },
            &keys,
            &values
        ));
        assert!(op_ignored(&set_changed(&["val"]), &keys, &values));
        assert!(!op_ignored(&set_added(&["val"]), &keys, &values));
        assert!(!op_ignored(
            &DiffOp::Delete {
                path: path(&["val"])
            },
            &keys,
            &values
        ));
        assert!(!op_ignored(&set_changed(&["other"]), &keys, &values));
    }

    #[test]
    fn masked_compare_skips_ignored_state() {
        let keys = [pattern("locked")];
        let values = [pattern("val")];
        let mut here = Vec::new();

        // Value-only differences under ignored paths pass.
        assert!(compare_masked(
            &parse("{\"locked\": 1, \"val\": 1, \"keep\": 1}"),
            &parse("{\"locked\": 2, \"val\": 2, \"keep\": 1}"),
            &mut here,
            &keys,
            &values
        ));
        // Presence differences still fail, except under fully ignored paths.
        assert!(!compare_masked(
            &parse("{\"val\": 1}"),
            &parse("{}"),
            &mut here,
            &keys,
            &values
        ));
        assert!(compare_masked(
            &parse("{\"locked\": 1}"),
            &parse("{}"),
            &mut here,
            &keys,
            &values
        ));
        // Untouched paths compare exactly.
        assert!(!compare_masked(
            &parse("{\"keep\": 1}"),
            &parse("{\"keep\": 2}"),
            &mut here,
            &keys,
            &values
        ));
    }

    #[test]
    fn restore_maintains_disk_state() {
        let keys = [pattern("locked")];
        let values = [pattern("val")];
        let none: [IgnorePattern; 0] = [];

        // Ignored paths take disk values; disk absence removes (non-ignored
        // keys like `gone` always keep merged output).
        let mut out = parse("{\"locked\": 1, \"gone\": 1, \"keep\": 1}");
        restore_into(
            &mut out,
            &parse("{\"locked\": 2}"),
            &mut Vec::new(),
            &keys,
            &none,
        );
        assert_eq!(out, parse("{\"locked\": 2, \"gone\": 1, \"keep\": 1}"));

        // Disk absence removes ignored subtrees wholesale.
        let stale = [pattern("stale")];
        let mut out = parse("{\"stale\": {\"x\": 1}, \"keep\": 1}");
        restore_into(&mut out, &parse("{}"), &mut Vec::new(), &stale, &none);
        assert_eq!(out, parse("{\"keep\": 1}"));

        // Value-ignored paths take disk values when present, keep merged otherwise.
        let mut out = parse("{\"val\": 1, \"fresh\": 1}");
        restore_into(
            &mut out,
            &parse("{\"val\": 2}"),
            &mut Vec::new(),
            &none,
            &values,
        );
        assert_eq!(out, parse("{\"val\": 2, \"fresh\": 1}"));
    }
}
