//! Structured and text merge strategies (Milestone 3).
//!
//! Deep merge with `union`/`replace` array policies, the `_TEMPLATRY_DELETE_`
//! value marker, `append_top`/`append_bottom`/`replace` text strategies, and
//! shared-destination additive combination. Structured formats share one
//! `serde_json::Value` intermediate representation (lossy: comments are
//! dropped, keys are re-sorted on output, TOML datetimes are rejected).

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::config::{ArrayPolicy, Strategy, Template};

/// Override value marker deleting the key from merged output.
///
/// All current and future markers share the `_TEMPLATRY_` prefix plus trailing
/// underscore convention. Only interpreted in structured merges: under text
/// strategies it passes through literally.
pub const DELETE_MARKER: &str = "_TEMPLATRY_DELETE_";

// ---- Strategies -------------------------------------------------------------

/// Structured document formats (auto-detected on file extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFormat {
    Json,
    JsonC,
    Yaml,
    Toml,
}

/// Detect the structured format from a filename extension (case-insensitive).
/// Returns `None` for unknown or missing extensions (text fallback).
pub fn format_for(filename: &str) -> Option<DocFormat> {
    let extension = Path::new(filename).extension()?.to_str()?.to_lowercase();
    match extension.as_str() {
        "json" => Some(DocFormat::Json),
        "jsonc" => Some(DocFormat::JsonC),
        "yaml" | "yml" => Some(DocFormat::Yaml),
        "toml" => Some(DocFormat::Toml),
        _ => None,
    }
}

/// Effective per-template strategy after auto-detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveStrategy {
    /// Structured deep merge in the detected format.
    Structured(DocFormat),
    /// Override bytes, newline, then template bytes.
    AppendTop,
    /// Template bytes, newline, then override bytes (unknown-extension fallback).
    AppendBottom,
    /// Emit the override file verbatim.
    Replace,
    /// Copy the template file verbatim, ignoring any override.
    None,
}

/// Resolve a template's strategy: explicit values win, otherwise auto-detect
/// on the generated filename (structured formats merge, anything else appends
/// the override at the bottom).
pub fn effective_strategy(
    template: &Template,
    generated_filename: &str,
) -> crate::Result<EffectiveStrategy> {
    match template.strategy {
        Some(Strategy::AppendTop) => Ok(EffectiveStrategy::AppendTop),
        Some(Strategy::AppendBottom) => Ok(EffectiveStrategy::AppendBottom),
        Some(Strategy::Replace) => Ok(EffectiveStrategy::Replace),
        Some(Strategy::None) => Ok(EffectiveStrategy::None),
        Some(Strategy::Merge) => match format_for(generated_filename) {
            Some(format) => Ok(EffectiveStrategy::Structured(format)),
            None => Err(crate::invalid(
                Path::new("templatry.source.toml"),
                format!(
                    "strategy `merge` needs a structured file extension (json, jsonc, yaml, yml, toml): `{generated_filename}` has none"
                ),
            )),
        },
        None => Ok(format_for(generated_filename)
            .map(EffectiveStrategy::Structured)
            .unwrap_or(EffectiveStrategy::AppendBottom)),
    }
}

/// Strategy families: structured merges combine as values, text as bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Structured,
    Text,
}

/// Classify a template's strategy for shared-destination validation.
pub fn family_of(template: &Template, generated_filename: &str) -> crate::Result<Family> {
    Ok(match effective_strategy(template, generated_filename)? {
        EffectiveStrategy::Structured(_) => Family::Structured,
        _ => Family::Text,
    })
}

// ---- Format bridges ---------------------------------------------------------

/// Parse a structured document; `origin` names the file in diagnostics.
pub fn parse_doc(text: &str, format: DocFormat, origin: &str) -> crate::Result<Value> {
    let context = Path::new(origin);
    match format {
        DocFormat::Json => serde_json::from_str(text)
            .map_err(|err| crate::invalid(context, format!("is not valid JSON: {err}"))),
        DocFormat::JsonC => {
            let mut owned = text.to_string();
            json_strip_comments::strip(&mut owned).map_err(|err| {
                crate::invalid(context, format!("cannot strip JSONC comments: {err}"))
            })?;
            serde_json::from_str(&owned)
                .map_err(|err| crate::invalid(context, format!("is not valid JSONC: {err}")))
        }
        DocFormat::Yaml => yaml_serde::from_str(text)
            .map_err(|err| crate::invalid(context, format!("is not valid YAML: {err}"))),
        DocFormat::Toml => {
            let parsed: toml::Value = toml::from_str(text)
                .map_err(|err| crate::invalid(context, format!("is not valid TOML: {err}")))?;
            reject_datetimes(&parsed, origin)?;
            serde_json::to_value(&parsed)
                .map_err(|err| crate::invalid(context, format!("cannot represent as JSON: {err}")))
        }
    }
}

/// TOML datetimes have no JSON representation: fail loudly with a path.
fn reject_datetimes(value: &toml::Value, origin: &str) -> crate::Result<()> {
    reject_datetimes_at(value, origin, String::new())
}

fn reject_datetimes_at(value: &toml::Value, origin: &str, path: String) -> crate::Result<()> {
    match value {
        toml::Value::Datetime(_) => {
            let at = if path.is_empty() {
                "(document root)".to_string()
            } else {
                format!("`{path}`")
            };
            Err(crate::invalid(
                Path::new(origin),
                format!(
                    "TOML datetime at {at} has no JSON representation: rewrite it as a string (comments and datetimes are lossy in v1)"
                ),
            ))
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let child = if path.is_empty() {
                    format!("[{index}]")
                } else {
                    format!("{path}[{index}]")
                };
                reject_datetimes_at(item, origin, child)?;
            }
            Ok(())
        }
        toml::Value::Table(table) => {
            for (key, item) in table {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                reject_datetimes_at(item, origin, child)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Serialize a merged document deterministically: nulls stripped, keys sorted
/// (via ordered maps), exactly one trailing newline.
pub fn serialize_doc(value: &Value, format: DocFormat) -> crate::Result<String> {
    let mut owned = value.clone();
    strip_nulls(&mut owned);
    let context = Path::new("templatry.source.toml");
    let mut output = match format {
        DocFormat::Json | DocFormat::JsonC => serde_json::to_string_pretty(&owned)
            .map_err(|err| crate::invalid(context, format!("cannot serialize JSON: {err}")))?,
        DocFormat::Yaml => yaml_serde::to_string(&owned)
            .map_err(|err| crate::invalid(context, format!("cannot serialize YAML: {err}")))?,
        DocFormat::Toml => {
            let converted = json_to_toml(&owned)?;
            toml::to_string_pretty(&converted)
                .map_err(|err| crate::invalid(context, format!("cannot serialize TOML: {err}")))?
        }
    };
    if !output.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

/// Nulls have no TOML representation and no merge meaning: drop them
/// recursively before serializing (the `smartworkspace` behavior).
fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, child| !child.is_null());
            for child in map.values_mut() {
                strip_nulls(child);
            }
        }
        Value::Array(items) => {
            items.retain(|child| !child.is_null());
            for child in items.iter_mut() {
                strip_nulls(child);
            }
        }
        _ => {}
    }
}

/// Convert the JSON intermediate to TOML with precise errors.
///
/// `u64` beyond `i64::MAX` and `null` (stripped before this runs, but defended
/// anyway) have no TOML representation.
fn json_to_toml(value: &Value) -> crate::Result<toml::Value> {
    let context = Path::new("templatry.source.toml");
    match value {
        Value::Null => Err(crate::invalid(
            context,
            "null has no TOML representation".to_string(),
        )),
        Value::Bool(flag) => Ok(toml::Value::Boolean(*flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Ok(toml::Value::Integer(int))
            } else if let Some(uint) = number.as_u64() {
                i64::try_from(uint).map(toml::Value::Integer).map_err(|_| {
                    crate::invalid(context, format!("integer `{uint}` exceeds TOML range"))
                })
            } else if let Some(float) = number.as_f64() {
                Ok(toml::Value::Float(float))
            } else {
                Err(crate::invalid(
                    context,
                    format!("number `{number}` has no TOML representation"),
                ))
            }
        }
        Value::String(text) => Ok(toml::Value::String(text.clone())),
        Value::Array(items) => {
            let mut converted = Vec::with_capacity(items.len());
            for item in items {
                converted.push(json_to_toml(item)?);
            }
            Ok(toml::Value::Array(converted))
        }
        Value::Object(map) => {
            let mut table = toml::map::Map::new();
            for (key, child) in map {
                table.insert(key.clone(), json_to_toml(child)?);
            }
            Ok(toml::Value::Table(table))
        }
    }
}

// ---- Deep merge -------------------------------------------------------------

/// Merge an override document into a template document.
///
/// Objects recurse with the override winning; scalars and type mismatches
/// replace; arrays follow `policy`. A `{DELETE_MARKER}` override value deletes
/// its key; the marker at the document root or inside arrays is an error.
pub fn merge_structured(
    base: Value,
    over: Value,
    policy: ArrayPolicy,
    template: &str,
) -> crate::Result<Value> {
    if over.as_str() == Some(DELETE_MARKER) {
        return Err(crate::invalid(
            Path::new("templatry.source.toml"),
            format!(
                "template `{template}`: `{DELETE_MARKER}` at the document root is meaningless (it deletes a key, so place it as a key's value)"
            ),
        ));
    }
    let mut merged = base;
    merge_into(&mut merged, over, policy, template, String::new())?;
    Ok(merged)
}

fn merge_into(
    base: &mut Value,
    over: Value,
    policy: ArrayPolicy,
    template: &str,
    path: String,
) -> crate::Result<()> {
    match (base, over) {
        (Value::Object(base_map), Value::Object(over_map)) => {
            for (key, over_value) in over_map {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if over_value.as_str() == Some(DELETE_MARKER) {
                    base_map.remove(&key);
                    continue;
                }
                match base_map.get_mut(&key) {
                    Some(base_value) => {
                        merge_into(base_value, over_value, policy, template, child)?;
                    }
                    None => {
                        base_map.insert(key, over_value);
                    }
                }
            }
            Ok(())
        }
        (Value::Array(base_items), Value::Array(over_items)) => {
            for item in &over_items {
                if item.as_str() == Some(DELETE_MARKER) {
                    return Err(crate::invalid(
                        Path::new("templatry.source.toml"),
                        format!(
                            "template `{template}`: `{DELETE_MARKER}` inside an array at `{}` is meaningless (arrays delete via `array_policy = \"replace\"`): remove it",
                            display_path(&path)
                        ),
                    ));
                }
            }
            match policy {
                ArrayPolicy::Union => {
                    for item in over_items {
                        if is_scalar(&item) {
                            if !base_items.contains(&item) {
                                base_items.push(item);
                            }
                        } else {
                            base_items.push(item);
                        }
                    }
                }
                ArrayPolicy::Replace => {
                    *base_items = over_items;
                }
            }
            Ok(())
        }
        (base_slot, over_value) => {
            *base_slot = over_value;
            Ok(())
        }
    }
}

/// JSON scalars: deduped in `union` arrays, replaced everywhere else.
fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "(document root)".to_string()
    } else {
        path.to_string()
    }
}

// ---- Shared-destination combination -----------------------------------------

/// One contributor to a shared destination file.
pub struct Contribution<'a> {
    /// Contributing template name (for conflict diagnostics).
    pub template: &'a str,
    /// Contributing template labels (for conflict diagnostics).
    pub labels: &'a BTreeSet<String>,
    /// Merged template-plus-override output.
    pub value: Value,
}

/// A rendered template awaiting destination combination.
#[derive(Debug, Clone)]
pub enum Rendered {
    /// Merged structured output plus its serialization format.
    Structured { value: Value, format: DocFormat },
    /// Complete text output (append/replace/copied bytes).
    Text(String),
}

/// One grouped render plus its provenance.
pub struct GroupPart<'a> {
    /// Contributing template name.
    pub template: &'a str,
    /// Contributing template labels.
    pub labels: &'a BTreeSet<String>,
    /// Rendered output.
    pub rendered: &'a Rendered,
}

/// Combine one destination group into final bytes.
///
/// Structured groups fold additively: disjoint keys union, equal values pass,
/// and any conflicting leaf fails naming the path, template, and labels.
/// Text groups concatenate in template-name order with newline separators.
pub fn combine_group(dest: &Path, parts: &[GroupPart<'_>]) -> crate::Result<Vec<u8>> {
    let Some(first) = parts.first() else {
        return Err(crate::invalid(
            dest,
            "cannot combine zero contributions".to_string(),
        ));
    };
    match first.rendered {
        Rendered::Structured { format, .. } => {
            let format = *format;
            let mut contributions = Vec::with_capacity(parts.len());
            for part in parts {
                match part.rendered {
                    Rendered::Structured {
                        value,
                        format: part_format,
                    } => {
                        if *part_format != format {
                            return Err(crate::invalid(
                                dest,
                                format!(
                                    "template `{}` serializes differently than its destination group: mixed formats cannot combine",
                                    part.template
                                ),
                            ));
                        }
                        contributions.push(Contribution {
                            template: part.template,
                            labels: part.labels,
                            value: value.clone(),
                        });
                    }
                    Rendered::Text(_) => {
                        return Err(crate::invalid(
                            dest,
                            format!(
                                "template `{}` renders text while its destination group merges structured output: mixed families cannot combine",
                                part.template
                            ),
                        ));
                    }
                }
            }
            serialize_doc(&combine_structured(&contributions)?, format).map(String::into_bytes)
        }
        Rendered::Text(_) => {
            let mut texts: Vec<(&str, &str)> = Vec::with_capacity(parts.len());
            for part in parts {
                match part.rendered {
                    Rendered::Text(text) => texts.push((part.template, text.as_str())),
                    Rendered::Structured { .. } => {
                        return Err(crate::invalid(
                            dest,
                            format!(
                                "template `{}` merges structured output while its destination group renders text: mixed families cannot combine",
                                part.template
                            ),
                        ));
                    }
                }
            }
            Ok(combine_text(&texts).into_bytes())
        }
    }
}

/// Fold structured contributions order-independently (see [`combine_group`]).
pub fn combine_structured(contributions: &[Contribution<'_>]) -> crate::Result<Value> {
    let mut iter = contributions.iter();
    let Some(first) = iter.next() else {
        return Err(crate::invalid(
            Path::new("templatry.source.toml"),
            "cannot combine zero contributions".to_string(),
        ));
    };
    let mut combined = first.value.clone();
    for contribution in iter {
        combine_into(
            &mut combined,
            &contribution.value,
            contribution,
            String::new(),
        )?;
    }
    Ok(combined)
}

fn combine_into(
    combined: &mut Value,
    incoming: &Value,
    contribution: &Contribution<'_>,
    path: String,
) -> crate::Result<()> {
    match (combined, incoming) {
        (Value::Object(combined_map), Value::Object(incoming_map)) => {
            for (key, incoming_value) in incoming_map {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match combined_map.get_mut(key) {
                    Some(existing) => {
                        combine_into(existing, incoming_value, contribution, child)?;
                    }
                    None => {
                        combined_map.insert(key.clone(), incoming_value.clone());
                    }
                }
            }
            Ok(())
        }
        (Value::Array(combined_items), Value::Array(incoming_items)) => {
            if combined_items != incoming_items {
                return Err(conflict(
                    contribution,
                    &path,
                    &Value::Array(combined_items.clone()),
                    &Value::Array(incoming_items.clone()),
                ));
            }
            Ok(())
        }
        (existing, incoming_value) => {
            if existing != incoming_value {
                return Err(conflict(contribution, &path, existing, incoming_value));
            }
            Ok(())
        }
    }
}

/// Conflict diagnostic naming the path, template, labels, and both values.
fn conflict(
    contribution: &Contribution<'_>,
    path: &str,
    existing: &Value,
    incoming: &Value,
) -> crate::Error {
    let at = if path.is_empty() {
        "(document root)".to_string()
    } else {
        format!("`{path}`")
    };
    let labels = if contribution.labels.is_empty() {
        "(no labels)".to_string()
    } else {
        contribution
            .labels
            .iter()
            .map(|label| format!("`{label}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    crate::invalid(
        Path::new("templatry.source.toml"),
        format!(
            "conflicting values at {at}: template `{}` (labels: {labels}) sets {}, but combined output has {}: resolve the conflict in the source templates or project overrides",
            contribution.template,
            compact(incoming),
            compact(existing),
        ),
    )
}

/// Single-line value rendering for diagnostics.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "(unprintable)".to_string())
}

/// Concatenate text parts in template-name order with newline separators.
pub fn combine_text(parts: &[(&str, &str)]) -> String {
    let mut sorted = parts.to_vec();
    sorted.sort_by(|left, right| left.0.cmp(right.0));
    sorted
        .iter()
        .map(|(_, content)| *content)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_with(strategy: Option<Strategy>) -> Template {
        Template {
            template: "app.json".to_string(),
            override_file: None,
            override_dir: None,
            local_override_file: None,
            generated_file: None,
            generated_dir: None,
            strategy,
            array_policy: ArrayPolicy::Union,
            back_propagate: false,
            labels: BTreeSet::new(),
            backprop_ignore: Vec::new(),
            backprop_ignore_values: Vec::new(),
        }
    }

    fn json(text: &str) -> Value {
        serde_json::from_str(text).expect("json fixture")
    }

    #[test]
    fn format_detection() {
        assert_eq!(format_for("a.json"), Some(DocFormat::Json));
        assert_eq!(format_for("a.JSONC"), Some(DocFormat::JsonC));
        assert_eq!(format_for("a.Yml"), Some(DocFormat::Yaml));
        assert_eq!(format_for("a.yaml"), Some(DocFormat::Yaml));
        assert_eq!(format_for("a.toml"), Some(DocFormat::Toml));
        assert_eq!(format_for("a.gitignore"), None);
        assert_eq!(format_for("no-extension"), None);
    }

    #[test]
    fn strategy_resolution() {
        let auto = template_with(None);
        assert_eq!(
            effective_strategy(&auto, "a.json").unwrap(),
            EffectiveStrategy::Structured(DocFormat::Json)
        );
        assert_eq!(
            effective_strategy(&auto, ".gitignore").unwrap(),
            EffectiveStrategy::AppendBottom
        );
        assert_eq!(
            effective_strategy(&template_with(Some(Strategy::AppendTop)), "a.json").unwrap(),
            EffectiveStrategy::AppendTop
        );
        assert_eq!(
            effective_strategy(&template_with(Some(Strategy::Replace)), "a.json").unwrap(),
            EffectiveStrategy::Replace
        );
        assert_eq!(
            effective_strategy(&template_with(Some(Strategy::None)), "a.json").unwrap(),
            EffectiveStrategy::None
        );
        assert_eq!(
            effective_strategy(&template_with(Some(Strategy::Merge)), "a.toml").unwrap(),
            EffectiveStrategy::Structured(DocFormat::Toml)
        );
        let err =
            effective_strategy(&template_with(Some(Strategy::Merge)), ".gitignore").unwrap_err();
        assert!(
            err.to_string()
                .contains("needs a structured file extension"),
            "{err:?}"
        );
    }

    #[test]
    fn format_bridges_parse() {
        assert_eq!(
            parse_doc("{\"a\": 1}", DocFormat::Json, "t").unwrap(),
            json("{\"a\": 1}")
        );
        assert_eq!(
            parse_doc(
                "// comment\n{\"a\": 1 /* trailing */}",
                DocFormat::JsonC,
                "t"
            )
            .unwrap(),
            json("{\"a\": 1}")
        );
        assert_eq!(
            parse_doc("a: 1\nb:\n  - x\n", DocFormat::Yaml, "t").unwrap(),
            json("{\"a\": 1, \"b\": [\"x\"]}")
        );
        assert_eq!(
            parse_doc("a = 1\n[b]\nc = \"x\"\n", DocFormat::Toml, "t").unwrap(),
            json("{\"a\": 1, \"b\": {\"c\": \"x\"}}")
        );
        assert!(
            parse_doc("{broken", DocFormat::Json, "origin-file")
                .unwrap_err()
                .to_string()
                .contains("origin-file")
        );
        assert!(parse_doc("a: [unclosed", DocFormat::Yaml, "t").is_err());
        assert!(parse_doc("a = ", DocFormat::Toml, "t").is_err());
    }

    #[test]
    fn toml_datetimes_rejected_with_path() {
        let err =
            parse_doc("a = 1\nwhen = 2026-09-20T00:00:00Z\n", DocFormat::Toml, "t").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("datetime"), "{message}");
        assert!(message.contains("`when`"), "{message}");
    }

    #[test]
    fn deep_merge_objects_scalars_and_mismatches() {
        let merged = merge_structured(
            json("{\"keep\": 1, \"nest\": {\"a\": 1, \"b\": 2}, \"flip\": [1], \"scalar\": 1}"),
            json("{\"nest\": {\"b\": 20, \"c\": 3}, \"flip\": {\"now\": \"object\"}, \"scalar\": \"s\", \"new\": true}"),
            ArrayPolicy::Union,
            "t",
        )
        .unwrap();
        assert_eq!(
            merged,
            json(
                "{\"keep\": 1, \"nest\": {\"a\": 1, \"b\": 20, \"c\": 3}, \"flip\": {\"now\": \"object\"}, \"scalar\": \"s\", \"new\": true}"
            )
        );
    }

    #[test]
    fn array_policies() {
        let base = json("{\"u\": [1, 2], \"r\": [1, 2], \"o\": [{\"a\": 1}]}");
        let over = json("{\"u\": [2, 3], \"r\": [2, 3], \"o\": [{\"a\": 1}]}");
        let unioned =
            merge_structured(base.clone(), over.clone(), ArrayPolicy::Union, "t").unwrap();
        assert_eq!(unioned["u"], json("[1, 2, 3]"));
        assert_eq!(unioned["o"], json("[{\"a\": 1}, {\"a\": 1}]"));
        let replaced = merge_structured(base, over, ArrayPolicy::Replace, "t").unwrap();
        assert_eq!(replaced["r"], json("[2, 3]"));
    }

    #[test]
    fn deletion_marker_removes_keys() {
        let merged = merge_structured(
            json("{\"drop\": 1, \"nest\": {\"drop\": 2, \"keep\": 3}, \"keep\": 4}"),
            json(
                "{\"drop\": \"_TEMPLATRY_DELETE_\", \"nest\": {\"drop\": \"_TEMPLATRY_DELETE_\"}}",
            ),
            ArrayPolicy::Union,
            "t",
        )
        .unwrap();
        assert_eq!(merged, json("{\"nest\": {\"keep\": 3}, \"keep\": 4}"));
    }

    #[test]
    fn deletion_marker_misuse_errors() {
        let err = merge_structured(
            json("{\"a\": 1}"),
            json("\"_TEMPLATRY_DELETE_\""),
            ArrayPolicy::Union,
            "t",
        )
        .unwrap_err();
        assert!(err.to_string().contains("document root"), "{err:?}");

        let err = merge_structured(
            json("{\"a\": [1]}"),
            json("{\"a\": [\"_TEMPLATRY_DELETE_\"]}"),
            ArrayPolicy::Union,
            "my-template",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("my-template"), "{message}");
        assert!(message.contains("`a`"), "{message}");

        // Marker is literal under non-merge handling: merge_structured with a
        // marker-valued override inside replace-policy arrays still errors
        // (replace wholesale would leak the marker into output).
        let err = merge_structured(
            json("{\"a\": [1]}"),
            json("{\"a\": [\"_TEMPLATRY_DELETE_\"]}"),
            ArrayPolicy::Replace,
            "t",
        )
        .unwrap_err();
        assert!(err.to_string().contains(DELETE_MARKER), "{err:?}");
    }

    #[test]
    fn serialization_is_sorted_stable_and_null_free() {
        let value = json("{\"z\": 1, \"a\": {\"d\": null, \"b\": 2}, \"n\": null}");
        let rendered = serialize_doc(&value, DocFormat::Json).unwrap();
        assert_eq!(
            rendered,
            "{\n  \"a\": {\n    \"b\": 2\n  },\n  \"z\": 1\n}\n"
        );
        assert!(rendered.ends_with('\n'));

        let yaml = serialize_doc(&value, DocFormat::Yaml).unwrap();
        assert!(yaml.contains("a:\n  b: 2"), "{yaml}");
        assert!(!yaml.contains("null"), "{yaml}");

        let toml =
            serialize_doc(&json("{\"b\": 1, \"a\": {\"x\": true}}"), DocFormat::Toml).unwrap();
        assert!(toml.contains("[a]"), "{toml}");
    }

    #[test]
    fn toml_integers_and_ranges() {
        let rendered =
            serialize_doc(&json("{\"small\": 3, \"list\": [1, 2]}"), DocFormat::Toml).unwrap();
        assert!(rendered.contains("small = 3"), "{rendered}");
        let huge = serde_json::from_str::<Value>("{\"big\": 18446744073709551615}").unwrap();
        assert!(serialize_doc(&huge, DocFormat::Toml).is_err());
    }

    fn contribution<'a>(
        template: &'a str,
        labels: &'a BTreeSet<String>,
        text: &str,
    ) -> Contribution<'a> {
        Contribution {
            template,
            labels,
            value: json(text),
        }
    }

    #[test]
    fn shared_combination_unions_and_dedups() {
        let rust: BTreeSet<String> = ["rust".to_string()].into_iter().collect();
        let dart: BTreeSet<String> = ["dart".to_string()].into_iter().collect();
        let combined = combine_structured(&[
            contribution("rust", &rust, "{\"a\": 1, \"same\": [1]}"),
            contribution("dart", &dart, "{\"b\": 2, \"same\": [1]}"),
        ])
        .unwrap();
        assert_eq!(combined, json("{\"a\": 1, \"b\": 2, \"same\": [1]}"));
    }

    #[test]
    fn shared_conflicts_name_path_templates_and_values() {
        let rust: BTreeSet<String> = ["rust".to_string()].into_iter().collect();
        let dart: BTreeSet<String> = ["dart".to_string()].into_iter().collect();
        let err = combine_structured(&[
            contribution("rust-settings", &rust, "{\"editor\": {\"size\": 14}}"),
            contribution("dart-settings", &dart, "{\"editor\": {\"size\": 16}}"),
        ])
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("`editor.size`"), "{message}");
        assert!(message.contains("dart-settings"), "{message}");
        assert!(message.contains("`dart`"), "{message}");
        assert!(message.contains("16"), "{message}");
        assert!(message.contains("14"), "{message}");
    }

    #[test]
    fn shared_array_conflicts() {
        let labels: BTreeSet<String> = BTreeSet::new();
        let err = combine_structured(&[
            contribution("a", &labels, "{\"list\": [1, 2]}"),
            contribution("b", &labels, "{\"list\": [1, 3]}"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("`list`"), "{err:?}");

        let unlabeled = combine_structured(&[
            contribution("a", &labels, "{\"x\": 1}"),
            contribution("b", &labels, "{\"x\": 1}"),
        ])
        .unwrap();
        assert_eq!(unlabeled, json("{\"x\": 1}"));
    }

    #[test]
    fn text_combination_is_name_ordered() {
        let combined = combine_text(&[("zeta", "z = 1"), ("alpha", "a = 1")]);
        assert_eq!(combined, "a = 1\nz = 1");
    }

    #[test]
    fn group_combination_dispatch() {
        let labels: BTreeSet<String> = BTreeSet::new();
        let structured = Rendered::Structured {
            value: json("{\"a\": 1}"),
            format: DocFormat::Json,
        };
        let text = Rendered::Text("hello".to_string());
        let dest = Path::new("out.json");

        let parts = [GroupPart {
            template: "a",
            labels: &labels,
            rendered: &structured,
        }];
        let bytes = combine_group(dest, &parts).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "{\n  \"a\": 1\n}\n");

        let mixed = [
            GroupPart {
                template: "a",
                labels: &labels,
                rendered: &structured,
            },
            GroupPart {
                template: "b",
                labels: &labels,
                rendered: &text,
            },
        ];
        assert!(combine_group(dest, &mixed).is_err());
    }

    #[test]
    fn family_classification() {
        use crate::merge::Family;

        let auto = template_with(None);
        assert_eq!(family_of(&auto, "a.yaml").unwrap(), Family::Structured);
        assert_eq!(family_of(&auto, "a.txt").unwrap(), Family::Text);
        assert_eq!(
            family_of(&template_with(Some(Strategy::Replace)), "a.json").unwrap(),
            Family::Text
        );
        assert_eq!(
            family_of(&template_with(Some(Strategy::None)), "LICENSE").unwrap(),
            Family::Text
        );
    }
}
