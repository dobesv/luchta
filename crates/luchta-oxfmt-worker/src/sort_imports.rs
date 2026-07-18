#![cfg(feature = "oxc")]

use std::collections::BTreeSet;

use oxc_formatter::{
    CustomGroupDefinition, GroupEntry, ImportModifier, ImportSelector, SortImportsOptions,
    SortOrder,
};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum SortImportsUserConfig {
    Bool(bool),
    Object(SortImportsConfig),
}

impl SortImportsUserConfig {
    fn into_config(self) -> Option<SortImportsConfig> {
        match self {
            Self::Bool(true) => Some(SortImportsConfig::default()),
            Self::Bool(false) => None,
            Self::Object(config) => Some(config),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SortImportsConfig {
    partition_by_newline: Option<bool>,
    partition_by_comment: Option<bool>,
    sort_side_effects: Option<bool>,
    order: Option<SortOrderConfig>,
    ignore_case: Option<bool>,
    newlines_between: Option<bool>,
    internal_pattern: Option<Vec<String>>,
    groups: Option<Vec<SortGroupItemConfig>>,
    custom_groups: Option<Vec<CustomGroupItemConfig>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SortOrderConfig {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SortGroupItemConfig {
    NewlinesBetween(NewlinesBetweenMarker),
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewlinesBetweenMarker {
    newlines_between: bool,
}

impl SortGroupItemConfig {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(value) => vec![value],
            Self::Multiple(values) => values,
            Self::NewlinesBetween(_) => {
                unreachable!("newlinesBetween markers handled before into_vec")
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CustomGroupItemConfig {
    group_name: String,
    element_name_pattern: Vec<String>,
    selector: Option<String>,
    modifiers: Option<Vec<String>>,
}

#[derive(Debug)]
struct ParsedSortImportGroups {
    groups: Vec<Vec<GroupEntry>>,
    newline_boundary_overrides: Vec<Option<bool>>,
}

pub(crate) fn resolve_sort_imports(
    config: Option<SortImportsUserConfig>,
) -> Result<Option<SortImportsOptions>, String> {
    let Some(sort_imports_config) = config.and_then(SortImportsUserConfig::into_config) else {
        return Ok(None);
    };

    let mut sort_imports = SortImportsOptions::default();
    apply_scalar_sort_import_options(&sort_imports_config, &mut sort_imports);

    if let Some(custom_groups) = sort_imports_config.custom_groups {
        sort_imports.custom_groups = parse_custom_groups(custom_groups)?;
    }
    if let Some(groups) = sort_imports_config.groups {
        let parsed_groups = parse_sort_import_groups(&sort_imports.custom_groups, groups)?;
        sort_imports.groups = parsed_groups.groups;
        sort_imports.newline_boundary_overrides = parsed_groups.newline_boundary_overrides;
    }

    sort_imports
        .validate()
        .map_err(|error| format!("Invalid `sortImports` configuration: {error}"))?;

    Ok(Some(sort_imports))
}

/// Copy the scalar (non-group) `sortImports` fields onto `sort_imports`,
/// leaving any field the user did not set at its oxc default.
fn apply_scalar_sort_import_options(
    config: &SortImportsConfig,
    sort_imports: &mut SortImportsOptions,
) {
    if let Some(value) = config.partition_by_newline {
        sort_imports.partition_by_newline = value;
    }
    if let Some(value) = config.partition_by_comment {
        sort_imports.partition_by_comment = value;
    }
    if let Some(value) = config.sort_side_effects {
        sort_imports.sort_side_effects = value;
    }
    if let Some(value) = config.order {
        sort_imports.order = map_sort_order(value);
    }
    if let Some(value) = config.ignore_case {
        sort_imports.ignore_case = value;
    }
    if let Some(value) = config.newlines_between {
        sort_imports.newlines_between = value;
    }
    if let Some(value) = &config.internal_pattern {
        sort_imports.internal_pattern = value.clone();
    }
}

fn map_sort_order(order: SortOrderConfig) -> SortOrder {
    match order {
        SortOrderConfig::Asc => SortOrder::Asc,
        SortOrderConfig::Desc => SortOrder::Desc,
    }
}

fn parse_custom_groups(
    groups: Vec<CustomGroupItemConfig>,
) -> Result<Vec<CustomGroupDefinition>, String> {
    groups.into_iter().map(parse_custom_group).collect()
}

fn parse_custom_group(group: CustomGroupItemConfig) -> Result<CustomGroupDefinition, String> {
    let CustomGroupItemConfig {
        group_name,
        element_name_pattern,
        selector,
        modifiers,
    } = group;

    let selector = parse_custom_group_selector(&group_name, selector)?;
    let modifiers = parse_custom_group_modifiers(&group_name, modifiers)?;

    Ok(CustomGroupDefinition {
        group_name,
        element_name_pattern,
        selector,
        modifiers,
    })
}

fn parse_custom_group_selector(
    group_name: &str,
    selector: Option<String>,
) -> Result<Option<ImportSelector>, String> {
    selector.map_or(Ok(None), |selector| {
        ImportSelector::parse(&selector).map(Some).ok_or_else(|| {
            format!(
                "Invalid `sortImports` configuration: unknown selector: `{selector}` in customGroups: `{group_name}`"
            )
        })
    })
}

fn parse_custom_group_modifiers(
    group_name: &str,
    modifiers: Option<Vec<String>>,
) -> Result<Vec<ImportModifier>, String> {
    modifiers
        .unwrap_or_default()
        .into_iter()
        .map(|modifier| {
            ImportModifier::parse(&modifier).ok_or_else(|| {
                format!(
                    "Invalid `sortImports` configuration: unknown modifier: `{modifier}` in customGroups: `{group_name}`"
                )
            })
        })
        .collect()
}

fn parse_sort_import_groups(
    custom_groups: &[CustomGroupDefinition],
    groups: Vec<SortGroupItemConfig>,
) -> Result<ParsedSortImportGroups, String> {
    let custom_group_names: BTreeSet<&str> = custom_groups
        .iter()
        .map(|group| group.group_name.as_str())
        .collect();
    let mut parsed = ParsedSortImportGroups {
        groups: Vec::new(),
        newline_boundary_overrides: Vec::new(),
    };
    let mut pending_override = None;

    for item in groups {
        match item {
            SortGroupItemConfig::NewlinesBetween(marker) => {
                pending_override = parse_newline_between_marker(
                    &parsed.groups,
                    pending_override,
                    marker.newlines_between,
                )?;
            }
            item => {
                push_sort_import_group(
                    &mut parsed,
                    &custom_group_names,
                    item.into_vec(),
                    &mut pending_override,
                )?;
            }
        }
    }

    if pending_override.is_some() {
        return Err("Invalid `sortImports` configuration: `{ \"newlinesBetween\" }` marker cannot appear at the end of `groups`".to_string());
    }

    Ok(parsed)
}

fn parse_newline_between_marker(
    groups: &[Vec<GroupEntry>],
    pending_override: Option<bool>,
    marker: bool,
) -> Result<Option<bool>, String> {
    if groups.is_empty() {
        return Err("Invalid `sortImports` configuration: `{ \"newlinesBetween\" }` marker cannot appear at the start of `groups`".to_string());
    }
    if pending_override.is_some() {
        return Err("Invalid `sortImports` configuration: consecutive `{ \"newlinesBetween\" }` markers are not allowed in `groups`".to_string());
    }
    Ok(Some(marker))
}

fn push_sort_import_group(
    parsed: &mut ParsedSortImportGroups,
    custom_group_names: &BTreeSet<&str>,
    names: Vec<String>,
    pending_override: &mut Option<bool>,
) -> Result<(), String> {
    if !parsed.groups.is_empty() {
        parsed
            .newline_boundary_overrides
            .push(pending_override.take());
    }
    parsed
        .groups
        .push(parse_group_entries(custom_group_names, names)?);
    Ok(())
}

fn parse_group_entries(
    custom_group_names: &BTreeSet<&str>,
    names: Vec<String>,
) -> Result<Vec<GroupEntry>, String> {
    names
        .into_iter()
        .map(|name| parse_group_entry(custom_group_names, name))
        .collect()
}

fn parse_group_entry(
    custom_group_names: &BTreeSet<&str>,
    name: String,
) -> Result<GroupEntry, String> {
    let entry = GroupEntry::parse(&name);
    match &entry {
        GroupEntry::Custom(custom_name) if !custom_group_names.contains(custom_name.as_str()) => {
            Err(format!(
                "Invalid `sortImports` configuration: unknown group name `{name}` in `groups`"
            ))
        }
        _ => Ok(entry),
    }
}
