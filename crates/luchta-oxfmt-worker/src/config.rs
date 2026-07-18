#![cfg(feature = "oxc")]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use json_strip_comments::StripComments;
use oxc_formatter::{
    ArrowParentheses, AttributePosition, BracketSameLine, BracketSpacing,
    EmbeddedLanguageFormatting, Expand, JsFormatOptions, QuoteProperties, QuoteStyle, Semicolons,
    TrailingCommas,
};
use oxc_formatter_core::{IndentStyle, IndentWidth, LineEnding, LineWidth};
use serde::Deserialize;
use serde_json::Value;

use crate::sort_imports;

const CONFIG_FILENAMES: [&str; 2] = [".oxfmtrc.json", ".oxfmtrc.jsonc"];

/// Upper bound on how many bytes of an `.oxfmtrc` file we read into memory.
/// Applied to the raw file read AND the comment-stripped output so an oversized
/// or malicious config cannot exhaust memory.
const MAX_CONFIG_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoadedConfig {
    pub options: JsFormatOptions,
    pub path: Option<PathBuf>,
    pub ignore_matcher: Option<Gitignore>,
    /// Non-fatal config problems worth surfacing (e.g. an unparseable ignore
    /// pattern). Emitted to stderr during execution; they do not fail the task.
    pub warnings: Vec<String>,
    /// Informational notices about `.oxfmtrc` keys the worker does not map
    /// (e.g. `plugins`, or options for a newer oxfmt). Emitted during execution;
    /// an unrecognized key is a normal forward-compat situation, not a failure,
    /// and never affects task resolution.
    pub unsupported_option_notices: Vec<String>,
    config_root: PathBuf,
    overrides: Vec<OverrideMatcher>,
}

#[derive(Debug, Clone)]
struct OverrideMatcher {
    matcher: Gitignore,
    exclude_matcher: Option<Gitignore>,
    options: OxfmtRc,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
struct OxfmtRc {
    use_tabs: Option<bool>,
    tab_width: Option<u8>,
    print_width: Option<u16>,
    end_of_line: Option<EndOfLine>,
    single_quote: Option<bool>,
    jsx_single_quote: Option<bool>,
    semi: Option<bool>,
    trailing_comma: Option<TrailingComma>,
    bracket_spacing: Option<bool>,
    bracket_same_line: Option<bool>,
    arrow_parens: Option<ArrowParens>,
    quote_props: Option<QuoteProps>,
    single_attribute_per_line: Option<bool>,
    object_wrap: Option<ObjectWrap>,
    html_whitespace_sensitivity: Option<HtmlWhitespaceSensitivity>,
    embedded_language_formatting: Option<EmbeddedLanguageFormattingOption>,
    // NOTE: `experimentalOperatorPosition` and `experimentalTernaries` are not
    // supported by this worker's oxc_formatter revision. They are intentionally
    // NOT fields here: serde ignores them, and they surface as unsupported-key
    // warnings via `collect_unknown_options` (they are absent from
    // `KNOWN_TOP_LEVEL_KEYS`). This keeps forward compatibility — a newer or
    // shared `.oxfmtrc` degrades with a warning instead of hard-failing the
    // whole repo's formatting.
    #[serde(alias = "experimentalSortImports")]
    sort_imports: Option<sort_imports::SortImportsUserConfig>,
    ignore_patterns: Option<Vec<String>>,
    overrides: Option<Vec<Override>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Override {
    files: OverrideFiles,
    #[serde(default)]
    exclude_files: Option<OverrideFiles>,
    #[serde(default)]
    options: OxfmtRc,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OverrideFiles {
    One(String),
    Many(Vec<String>),
}

impl Default for OverrideFiles {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

impl OverrideFiles {
    fn patterns(&self) -> Vec<&str> {
        match self {
            Self::One(pattern) => vec![pattern.as_str()],
            Self::Many(patterns) => patterns.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EndOfLine {
    Lf,
    Crlf,
    Cr,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TrailingComma {
    All,
    Es5,
    None,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ArrowParens {
    Avoid,
    Always,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum QuoteProps {
    #[serde(rename = "as-needed")]
    AsNeeded,
    #[serde(rename = "consistent")]
    Consistent,
    #[serde(rename = "preserve")]
    Preserve,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ObjectWrap {
    Preserve,
    Collapse,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum HtmlWhitespaceSensitivity {
    Css,
    Strict,
    Ignore,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EmbeddedLanguageFormattingOption {
    Auto,
    Off,
}

#[derive(Debug, Default)]
struct UnknownOptions {
    top_level: Vec<String>,
    override_options: Vec<Vec<String>>,
}

impl LoadedConfig {
    pub fn options_for(&self, path: &Path) -> JsFormatOptions {
        let mut options = self.options.clone();
        let relative_path = path
            .strip_prefix(&self.config_root)
            .unwrap_or(path)
            .to_path_buf();
        for override_matcher in &self.overrides {
            if !override_matcher
                .matcher
                .matched_path_or_any_parents(&relative_path, false)
                .is_ignore()
            {
                continue;
            }
            if let Some(exclude_matcher) = &override_matcher.exclude_matcher {
                if exclude_matcher
                    .matched_path_or_any_parents(&relative_path, false)
                    .is_ignore()
                {
                    continue;
                }
            }
            apply_oxfmtrc_to_options(&override_matcher.options, &mut options)
                .expect("override options already validated");
        }
        options
    }

    /// All diagnostics to surface at execution time: config `warnings` followed
    /// by informational `unsupported_option_notices`. Neither fails the task.
    pub fn diagnostics(&self) -> Vec<String> {
        let mut all = self.warnings.clone();
        all.extend(self.unsupported_option_notices.iter().cloned());
        all
    }
}

pub fn discover_config(cwd: &Path) -> Result<LoadedConfig, String> {
    let path = find_config_path(cwd);
    let parsed = match path.as_deref() {
        Some(path) => load_config_from_path(path)?,
        None => ParsedConfig {
            options: JsFormatOptions::new(),
            ignore_matcher: None,
            warnings: Vec::new(),
            unsupported_option_notices: Vec::new(),
            config_root: cwd.to_path_buf(),
            overrides: Vec::new(),
        },
    };
    Ok(LoadedConfig {
        options: parsed.options,
        path,
        ignore_matcher: parsed.ignore_matcher,
        warnings: parsed.warnings,
        unsupported_option_notices: parsed.unsupported_option_notices,
        config_root: parsed.config_root,
        overrides: parsed.overrides,
    })
}

#[cfg(test)]
fn oxfmtrc_to_options(json: &str) -> Result<JsFormatOptions, String> {
    let config = parse_oxfmtrc(json)?;
    options_from_oxfmtrc(&config)
}

fn options_from_oxfmtrc(config: &OxfmtRc) -> Result<JsFormatOptions, String> {
    let mut options = JsFormatOptions::new();
    apply_oxfmtrc_to_options(config, &mut options)?;

    // Supported subset only. Unsupported top-level keys are surfaced as warnings
    // during config load instead of hard errors to keep forward compatibility with
    // newer upstream .oxfmtrc files.
    Ok(options)
}

fn apply_oxfmtrc_to_options(config: &OxfmtRc, options: &mut JsFormatOptions) -> Result<(), String> {
    apply_layout_options(config, options)?;
    apply_quote_options(config, options);
    apply_punctuation_options(config, options);
    apply_markup_options(config, options);
    // Only assign when `sortImports` is explicitly present. Otherwise an
    // override that omits `sortImports` would overwrite an inherited top-level
    // value with `None` and silently disable sorting for matching files. An
    // explicit `sortImports: false` still flows through (resolves to `None`).
    if config.sort_imports.is_some() {
        options.sort_imports = sort_imports::resolve_sort_imports(config.sort_imports.clone())?;
    }
    Ok(())
}

fn apply_layout_options(config: &OxfmtRc, options: &mut JsFormatOptions) -> Result<(), String> {
    if let Some(use_tabs) = config.use_tabs {
        options.indent_style = map_indent_style(use_tabs);
    }
    if let Some(tab_width) = config.tab_width {
        options.indent_width = IndentWidth::try_from(tab_width)
            .map_err(|error| format!("invalid .oxfmtrc tabWidth: {error}"))?;
    }
    if let Some(print_width) = config.print_width {
        options.line_width = LineWidth::try_from(print_width)
            .map_err(|error| format!("invalid .oxfmtrc printWidth: {error}"))?;
    }
    if let Some(end_of_line) = config.end_of_line {
        options.line_ending = map_line_ending(end_of_line);
    }
    Ok(())
}

fn map_indent_style(use_tabs: bool) -> IndentStyle {
    if use_tabs {
        IndentStyle::Tab
    } else {
        IndentStyle::Space
    }
}

fn map_line_ending(end_of_line: EndOfLine) -> LineEnding {
    match end_of_line {
        EndOfLine::Lf => LineEnding::Lf,
        EndOfLine::Crlf => LineEnding::Crlf,
        EndOfLine::Cr => LineEnding::Cr,
    }
}

fn apply_quote_options(config: &OxfmtRc, options: &mut JsFormatOptions) {
    if let Some(single_quote) = config.single_quote {
        options.quote_style = map_quote_style(single_quote);
    }
    if let Some(jsx_single_quote) = config.jsx_single_quote {
        options.jsx_quote_style = map_quote_style(jsx_single_quote);
    }
    if let Some(quote_props) = config.quote_props {
        options.quote_properties = map_quote_properties(quote_props);
    }
}

fn apply_punctuation_options(config: &OxfmtRc, options: &mut JsFormatOptions) {
    if let Some(semi) = config.semi {
        options.semicolons = map_semicolons(semi);
    }
    if let Some(trailing_comma) = config.trailing_comma {
        options.trailing_commas = map_trailing_commas(trailing_comma);
    }
    if let Some(bracket_spacing) = config.bracket_spacing {
        options.bracket_spacing = BracketSpacing::from(bracket_spacing);
    }
    if let Some(bracket_same_line) = config.bracket_same_line {
        options.bracket_same_line = BracketSameLine::from(bracket_same_line);
    }
    if let Some(arrow_parens) = config.arrow_parens {
        options.arrow_parentheses = map_arrow_parentheses(arrow_parens);
    }
    if let Some(single_attribute_per_line) = config.single_attribute_per_line {
        options.attribute_position = map_attribute_position(single_attribute_per_line);
    }
    if let Some(object_wrap) = config.object_wrap {
        options.expand = map_expand(object_wrap);
    }
}

fn map_arrow_parentheses(arrow_parens: ArrowParens) -> ArrowParentheses {
    match arrow_parens {
        ArrowParens::Avoid => ArrowParentheses::AsNeeded,
        ArrowParens::Always => ArrowParentheses::Always,
    }
}

fn map_attribute_position(single_attribute_per_line: bool) -> AttributePosition {
    if single_attribute_per_line {
        AttributePosition::Multiline
    } else {
        AttributePosition::Auto
    }
}

fn map_expand(object_wrap: ObjectWrap) -> Expand {
    match object_wrap {
        ObjectWrap::Preserve => Expand::Auto,
        ObjectWrap::Collapse => Expand::Never,
    }
}

fn apply_markup_options(config: &OxfmtRc, options: &mut JsFormatOptions) {
    if let Some(html_whitespace_sensitivity) = config.html_whitespace_sensitivity {
        options.html_whitespace_sensitivity_ignore = matches!(
            html_whitespace_sensitivity,
            HtmlWhitespaceSensitivity::Ignore
        );
    }
    if let Some(embedded_language_formatting) = config.embedded_language_formatting {
        options.embedded_language_formatting = match embedded_language_formatting {
            EmbeddedLanguageFormattingOption::Auto => EmbeddedLanguageFormatting::Auto,
            EmbeddedLanguageFormattingOption::Off => EmbeddedLanguageFormatting::Off,
        };
    }
}

fn map_quote_style(single_quote: bool) -> QuoteStyle {
    if single_quote {
        QuoteStyle::Single
    } else {
        QuoteStyle::Double
    }
}

fn map_quote_properties(quote_props: QuoteProps) -> QuoteProperties {
    match quote_props {
        QuoteProps::AsNeeded => QuoteProperties::AsNeeded,
        QuoteProps::Consistent => QuoteProperties::Consistent,
        QuoteProps::Preserve => QuoteProperties::Preserve,
    }
}

fn map_semicolons(semi: bool) -> Semicolons {
    if semi {
        Semicolons::Always
    } else {
        Semicolons::AsNeeded
    }
}

fn map_trailing_commas(trailing_comma: TrailingComma) -> TrailingCommas {
    match trailing_comma {
        TrailingComma::All => TrailingCommas::All,
        TrailingComma::Es5 => TrailingCommas::Es5,
        TrailingComma::None => TrailingCommas::None,
    }
}

pub fn build_ignore_matcher(patterns: &[String], root: &Path) -> (Option<Gitignore>, Vec<String>) {
    if patterns.is_empty() {
        return (None, Vec::new());
    }
    let mut builder = GitignoreBuilder::new(root);
    let mut warnings = Vec::new();
    for pattern in patterns {
        if let Err(error) = builder.add_line(None, pattern) {
            warnings.push(format!(
                "warning: failed to parse ignore pattern {pattern:?} in {}: {error}",
                root.display()
            ));
        }
    }
    let matcher = match builder.build() {
        Ok(matcher) => Some(matcher),
        Err(error) => {
            warnings.push(format!(
                "warning: failed to build ignore matcher in {}: {error}",
                root.display()
            ));
            None
        }
    };
    (matcher, warnings)
}

fn build_override_matcher(patterns: &[&str], root: &Path) -> Result<Option<Gitignore>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder.add_line(None, pattern).map_err(|error| {
            format!("failed to parse override files pattern {pattern:?}: {error}")
        })?;
    }
    builder.build().map(Some).map_err(|error| {
        format!(
            "failed to build override matcher in {}: {error}",
            root.display()
        )
    })
}

fn build_override_matchers(
    overrides: &[Override],
    root: &Path,
) -> Result<Vec<OverrideMatcher>, String> {
    let mut matchers = Vec::new();
    for override_config in overrides {
        let mut scratch = JsFormatOptions::new();
        apply_oxfmtrc_to_options(&override_config.options, &mut scratch)
            .map_err(|error| format!("invalid override options: {error}"))?;

        let patterns = override_config.files.patterns();
        let Some(matcher) = build_override_matcher(&patterns, root)? else {
            continue;
        };
        let exclude_matcher = match &override_config.exclude_files {
            Some(exclude_files) => build_override_matcher(&exclude_files.patterns(), root)?,
            None => None,
        };
        matchers.push(OverrideMatcher {
            matcher,
            exclude_matcher,
            options: override_config.options.clone(),
        });
    }
    Ok(matchers)
}

fn parse_oxfmtrc(json: &str) -> Result<OxfmtRc, String> {
    serde_json::from_str(json).map_err(|error| format!("failed to parse .oxfmtrc: {error}"))
}

/// Formatter-option keys the worker recognizes. These are valid both at the
/// `.oxfmtrc` top level and inside an `overrides[].options` object. Single
/// source of truth to keep the two scopes in sync.
const KNOWN_FORMAT_OPTION_KEYS: &[&str] = &[
    "arrowParens",
    "bracketSameLine",
    "bracketSpacing",
    "embeddedLanguageFormatting",
    "endOfLine",
    "experimentalSortImports",
    "htmlWhitespaceSensitivity",
    "jsxSingleQuote",
    "objectWrap",
    "printWidth",
    "quoteProps",
    "semi",
    "singleAttributePerLine",
    "singleQuote",
    "sortImports",
    "tabWidth",
    "trailingComma",
    "useTabs",
];

/// Top-level-only keys (in addition to the shared formatter-option keys).
/// `$schema` is accepted for editor/JSON-schema tooling and intentionally
/// ignored by the worker.
const KNOWN_TOP_LEVEL_ONLY_KEYS: &[&str] = &["overrides", "ignorePatterns", "$schema"];

/// Keys the worker recognizes directly inside an `overrides[]` entry. Formatter
/// options belong under the nested `options` object, NOT at the entry root.
const KNOWN_OVERRIDE_ENTRY_KEYS: &[&str] = &["files", "excludeFiles", "options"];

/// Recognized sub-keys of the `sortImports` object (mirrors
/// `sort_imports::SortImportsConfig`). Used to surface typos nested inside a
/// `sortImports` object, which serde would otherwise silently drop.
const KNOWN_SORT_IMPORTS_KEYS: &[&str] = &[
    "partitionByNewline",
    "partitionByComment",
    "sortSideEffects",
    "order",
    "ignoreCase",
    "newlinesBetween",
    "internalPattern",
    "groups",
    "customGroups",
];

fn is_known(known: &[&str], key: &str) -> bool {
    known.contains(&key)
}

/// Both spellings under which a `sortImports` object may appear (the canonical
/// key and its accepted alias). Nested typos must be caught under either.
const SORT_IMPORTS_KEYS: &[&str] = &["sortImports", "experimentalSortImports"];

/// Unknown sub-keys inside a `sortImports` object (looked up under both the
/// canonical `sortImports` key and its `experimentalSortImports` alias). When
/// the value is a bool (enable/disable) there are no sub-keys; when it is an
/// object, each key not in [`KNOWN_SORT_IMPORTS_KEYS`] is reported using the
/// spelling the user actually wrote (e.g. `experimentalSortImports.<key>`).
fn unknown_sort_imports_keys(object: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut unknown = Vec::new();
    for parent in SORT_IMPORTS_KEYS {
        let Some(nested) = object.get(*parent).and_then(Value::as_object) else {
            continue;
        };
        unknown.extend(
            nested
                .keys()
                .filter(|key| !is_known(KNOWN_SORT_IMPORTS_KEYS, key))
                .map(|key| format!("{parent}.{key}")),
        );
    }
    unknown
}

/// Unknown option keys within a formatter-option object (used for both the
/// top-level config and an override's nested `options`): keys not recognized as
/// formatter options, plus unknown sub-keys nested inside `sortImports`
/// (or its `experimentalSortImports` alias).
fn unknown_format_option_keys(object: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut unknown: Vec<String> = object
        .keys()
        .filter(|key| !is_known(KNOWN_FORMAT_OPTION_KEYS, key))
        .cloned()
        .collect();
    unknown.extend(unknown_sort_imports_keys(object));
    unknown
}

/// Unknown formatter-option keys within a `.oxfmtrc` object at the top level
/// (formatter options plus `overrides` / `ignorePatterns`, and nested
/// `sortImports` sub-keys).
fn unknown_top_level_keys(object: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut unknown: Vec<String> = object
        .keys()
        .filter(|key| {
            !is_known(KNOWN_FORMAT_OPTION_KEYS, key) && !is_known(KNOWN_TOP_LEVEL_ONLY_KEYS, key)
        })
        .cloned()
        .collect();
    unknown.extend(unknown_sort_imports_keys(object));
    unknown
}

/// Unknown keys for a single `overrides[]` entry: unexpected keys at the entry
/// root (e.g. formatter options placed there instead of under `options`, which
/// serde would silently ignore) plus unknown keys inside the nested `options`
/// object (including nested `sortImports` sub-keys).
fn unknown_override_entry_keys(entry: &Value) -> Vec<String> {
    let Some(object) = entry.as_object() else {
        return Vec::new();
    };
    let mut unknown: Vec<String> = object
        .keys()
        .filter(|key| !is_known(KNOWN_OVERRIDE_ENTRY_KEYS, key))
        .cloned()
        .collect();
    if let Some(options) = object.get("options").and_then(Value::as_object) {
        unknown.extend(unknown_format_option_keys(options));
    }
    unknown
}

fn collect_override_unknown_options(object: &serde_json::Map<String, Value>) -> Vec<Vec<String>> {
    object
        .get("overrides")
        .and_then(Value::as_array)
        .map(|overrides| overrides.iter().map(unknown_override_entry_keys).collect())
        .unwrap_or_default()
}

fn collect_unknown_options(json: &str) -> Result<UnknownOptions, String> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| format!("failed to parse .oxfmtrc for unknown option scan: {error}"))?;
    let Some(object) = value.as_object() else {
        return Ok(UnknownOptions::default());
    };

    Ok(UnknownOptions {
        top_level: unknown_top_level_keys(object),
        override_options: collect_override_unknown_options(object),
    })
}

struct ParsedConfig {
    options: JsFormatOptions,
    ignore_matcher: Option<Gitignore>,
    warnings: Vec<String>,
    unsupported_option_notices: Vec<String>,
    config_root: PathBuf,
    overrides: Vec<OverrideMatcher>,
}

/// Read a file into a string, bounding the read to `limit` bytes so an oversized
/// or malicious `.oxfmtrc` cannot allocate unbounded memory. Reads at most
/// `limit + 1` bytes; if the file exceeds `limit` it is rejected explicitly with
/// a clear error rather than silently truncated (truncation would otherwise
/// surface later as a confusing "truncated JSON" parse error).
fn read_bounded(path: &Path, limit: u64) -> Result<String, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let mut raw = String::new();
    file.take(limit + 1)
        .read_to_string(&mut raw)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if raw.len() as u64 > limit {
        return Err(format!(
            "{} exceeds the maximum .oxfmtrc size of {limit} bytes",
            path.display()
        ));
    }
    Ok(raw)
}

fn load_config_from_path(path: &Path) -> Result<ParsedConfig, String> {
    let root = path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", path.display()))?;

    let raw = read_bounded(path, MAX_CONFIG_BYTES)?;
    let stripped = StripComments::new(raw.as_bytes());
    let mut json = String::new();
    std::io::Read::read_to_string(&mut stripped.take(MAX_CONFIG_BYTES + 1), &mut json)
        .map_err(|error| format!("failed to strip comments in {}: {error}", path.display()))?;

    let unknown = collect_unknown_options(&json)?;
    let config = parse_oxfmtrc(&json)?;
    let options = options_from_oxfmtrc(&config)?;

    // Unsupported/unknown keys are informational only — they must NOT reject the
    // task during resolution (an unrecognized key is normal forward-compat).
    let mut unsupported_option_notices = Vec::new();
    for key in unknown.top_level {
        unsupported_option_notices.push(format!(
            "warning: unsupported .oxfmtrc option `{key}` in {}; ignoring",
            path.display()
        ));
    }
    for (index, keys) in unknown.override_options.into_iter().enumerate() {
        for key in keys {
            unsupported_option_notices.push(format!(
                "warning: unsupported .oxfmtrc override option `{key}` in {} at overrides[{index}]; ignoring",
                path.display()
            ));
        }
    }

    // `warnings` holds genuine config problems (e.g. an unparseable ignore
    // pattern). Like the unsupported-key notices, these are surfaced to stderr
    // at execution time and never affect task resolution (resolution must not
    // silently prune a task over config problems).
    let (ignore_matcher, warnings) = match &config.ignore_patterns {
        Some(patterns) => build_ignore_matcher(patterns, root),
        None => (None, Vec::new()),
    };

    let overrides = match &config.overrides {
        Some(overrides) => build_override_matchers(overrides, root)?,
        None => Vec::new(),
    };

    Ok(ParsedConfig {
        options,
        ignore_matcher,
        warnings,
        unsupported_option_notices,
        config_root: root.to_path_buf(),
        overrides,
    })
}

fn find_config_path(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        for filename in CONFIG_FILENAMES {
            let candidate = dir.join(filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use assert_fs::TempDir;

    use super::{discover_config, oxfmtrc_to_options, read_bounded, CONFIG_FILENAMES};

    #[test]
    fn read_bounded_rejects_oversized_files() {
        // A file within the limit reads fine; a file over the limit is rejected
        // explicitly (not silently truncated). Uses a small limit to avoid
        // writing a real 10 MiB fixture.
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("data");

        fs::write(&path, "0123456789").expect("write");
        assert_eq!(read_bounded(&path, 16).expect("within limit"), "0123456789");
        assert_eq!(
            read_bounded(&path, 10).expect("exactly at limit"),
            "0123456789"
        );

        let err = read_bounded(&path, 9).expect_err("over limit");
        assert!(err.contains("exceeds the maximum .oxfmtrc size"), "{err}");
    }

    fn load_config_from_temp(config: &str) -> (TempDir, super::LoadedConfig) {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(&config_path, config).expect("config");
        let loaded = discover_config(temp.path()).expect("discover");
        (temp, loaded)
    }

    #[test]
    fn discover_config_walks_ancestors() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path();
        let nested = repo.join("packages/app/src");
        fs::create_dir_all(&nested).expect("nested");
        let config_path = repo.join(CONFIG_FILENAMES[0]);
        fs::write(&config_path, r#"{"singleQuote":true}"#).expect("config");

        let loaded = discover_config(&nested).expect("discover");

        assert_eq!(loaded.path.as_deref(), Some(config_path.as_path()));
        assert_eq!(loaded.options.quote_style.as_char(), '\'');
    }

    #[test]
    fn discover_config_prefers_nearest_ancestor() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path();
        let parent = repo.join("packages");
        let nested = parent.join("app");
        fs::create_dir_all(&nested).expect("nested");

        fs::write(repo.join(CONFIG_FILENAMES[0]), r#"{"singleQuote":false}"#).expect("root config");
        let nested_path = nested.join(CONFIG_FILENAMES[0]);
        fs::write(&nested_path, r#"{"singleQuote":true}"#).expect("nested config");

        let loaded = discover_config(&nested).expect("discover");

        assert_eq!(loaded.path.as_deref(), Some(nested_path.as_path()));
        assert_eq!(loaded.options.quote_style.as_char(), '\'');
    }

    /// Assert that `options` matches oxc_formatter's default format options for
    /// the fields the worker maps. Encapsulates the comparison so tests do not
    /// carry a large consecutive assertion block.
    fn assert_options_are_defaults(options: &oxc_formatter::JsFormatOptions) {
        let defaults = oxc_formatter::JsFormatOptions::new();
        // Compare mapped fields as grouped tuples to keep the assertion block
        // small (widths together, style/enum fields together).
        assert_eq!(
            (options.indent_width.value(), options.line_width.value()),
            (defaults.indent_width.value(), defaults.line_width.value()),
        );
        assert_eq!(
            (
                options.indent_style,
                options.quote_style,
                options.semicolons
            ),
            (
                defaults.indent_style,
                defaults.quote_style,
                defaults.semicolons,
            ),
        );
    }

    #[test]
    fn discover_config_handles_missing_file() {
        let temp = TempDir::new().expect("tempdir");
        let loaded = discover_config(temp.path()).expect("discover");

        assert!(loaded.path.is_none());
        assert_options_are_defaults(&loaded.options);
    }

    #[test]
    fn use_tabs_maps_to_tab_indent_style() {
        let options = oxfmtrc_to_options(r#"{"useTabs":true}"#).expect("parse");
        assert!(matches!(
            options.indent_style,
            oxc_formatter_core::IndentStyle::Tab
        ));
    }

    #[test]
    fn tab_width_maps_to_indent_width() {
        let options = oxfmtrc_to_options(r#"{"tabWidth":4}"#).expect("parse");
        assert_eq!(options.indent_width.value(), 4);
    }

    #[test]
    fn zero_tab_width_is_accepted() {
        // Unlike printWidth (where 0 is rejected by LineWidth), IndentWidth
        // accepts 0, so tabWidth: 0 maps through without error.
        let options = oxfmtrc_to_options(r#"{"tabWidth":0}"#).expect("parse");
        assert_eq!(options.indent_width.value(), 0);
    }

    #[test]
    fn print_width_maps_to_line_width() {
        let options = oxfmtrc_to_options(r#"{"printWidth":100}"#).expect("parse");
        assert_eq!(options.line_width.value(), 100);
    }

    #[test]
    fn invalid_print_width_errors() {
        let err = oxfmtrc_to_options(r#"{"printWidth":0}"#).expect_err("should error");
        assert!(err.contains("invalid .oxfmtrc printWidth"), "{err}");
    }

    #[test]
    fn end_of_line_maps() {
        let lf = oxfmtrc_to_options(r#"{"endOfLine":"lf"}"#).expect("parse");
        let crlf = oxfmtrc_to_options(r#"{"endOfLine":"crlf"}"#).expect("parse");
        let cr = oxfmtrc_to_options(r#"{"endOfLine":"cr"}"#).expect("parse");

        assert_eq!(lf.line_ending, oxc_formatter_core::LineEnding::Lf);
        assert_eq!(crlf.line_ending, oxc_formatter_core::LineEnding::Crlf);
        assert_eq!(cr.line_ending, oxc_formatter_core::LineEnding::Cr);
    }

    #[test]
    fn single_quote_maps() {
        let options = oxfmtrc_to_options(r#"{"singleQuote":true}"#).expect("parse");
        assert_eq!(options.quote_style.as_char(), '\'');
    }

    #[test]
    fn jsx_single_quote_maps() {
        let options = oxfmtrc_to_options(r#"{"jsxSingleQuote":true}"#).expect("parse");
        assert_eq!(options.jsx_quote_style.as_char(), '\'');
    }

    #[test]
    fn quote_props_maps() {
        let options = oxfmtrc_to_options(r#"{"quoteProps":"consistent"}"#).expect("parse");
        assert_eq!(
            options.quote_properties,
            oxc_formatter::QuoteProperties::Consistent
        );
    }

    #[test]
    fn semi_maps() {
        let options = oxfmtrc_to_options(r#"{"semi":false}"#).expect("parse");
        assert_eq!(options.semicolons, oxc_formatter::Semicolons::AsNeeded);
    }

    #[test]
    fn trailing_comma_maps() {
        let options = oxfmtrc_to_options(r#"{"trailingComma":"es5"}"#).expect("parse");
        assert_eq!(options.trailing_commas, oxc_formatter::TrailingCommas::Es5);
    }

    #[test]
    fn bracket_options_map() {
        let options = oxfmtrc_to_options(r#"{"bracketSpacing":false,"bracketSameLine":true}"#)
            .expect("parse");
        assert_eq!(
            options.bracket_spacing,
            oxc_formatter::BracketSpacing::from(false)
        );
        assert_eq!(
            options.bracket_same_line,
            oxc_formatter::BracketSameLine::from(true)
        );
    }

    #[test]
    fn arrow_parens_maps() {
        let options = oxfmtrc_to_options(r#"{"arrowParens":"always"}"#).expect("parse");
        assert_eq!(
            options.arrow_parentheses,
            oxc_formatter::ArrowParentheses::Always
        );
    }

    #[test]
    fn single_attribute_per_line_maps() {
        let options = oxfmtrc_to_options(r#"{"singleAttributePerLine":true}"#).expect("parse");
        assert_eq!(
            options.attribute_position,
            oxc_formatter::AttributePosition::Multiline
        );
    }

    #[test]
    fn object_wrap_maps() {
        let options = oxfmtrc_to_options(r#"{"objectWrap":"collapse"}"#).expect("parse");
        assert_eq!(options.expand, oxc_formatter::Expand::Never);
    }

    #[test]
    fn markup_options_map() {
        let options = oxfmtrc_to_options(
            r#"{"htmlWhitespaceSensitivity":"ignore","embeddedLanguageFormatting":"off"}"#,
        )
        .expect("parse");
        assert!(options.html_whitespace_sensitivity_ignore);
        assert_eq!(
            options.embedded_language_formatting,
            oxc_formatter::EmbeddedLanguageFormatting::Off
        );
    }

    #[test]
    fn unsupported_experimental_options_warn_and_do_not_fail() {
        // Unsupported experimental options must NOT hard-fail config load
        // (that would stop formatting the whole repo for a newer/shared
        // .oxfmtrc). They are ignored and surfaced as warnings instead.
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"experimentalTernaries":true,"experimentalOperatorPosition":"start"}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover should not fail");
        // Unsupported keys are informational notices, NOT reject-worthy warnings.
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let notices = loaded.unsupported_option_notices.join("\n");
        assert!(
            loaded.unsupported_option_notices.len() == 2
                && notices.contains("experimentalTernaries")
                && notices.contains("experimentalOperatorPosition"),
            "{notices}"
        );
    }

    #[test]
    fn sort_imports_boolean_true_enables_defaults() {
        let options = oxfmtrc_to_options(r#"{"sortImports":true}"#).expect("parse");
        assert!(options.sort_imports.is_some());
    }

    #[test]
    fn sort_imports_boolean_false_disables_sorting() {
        let options = oxfmtrc_to_options(r#"{"sortImports":false}"#).expect("parse");
        assert!(options.sort_imports.is_none());
    }

    #[test]
    fn sort_imports_object_maps_scalar_options() {
        let options = oxfmtrc_to_options(
            // NOTE: `partitionByNewline` and `newlinesBetween` cannot both be
            // enabled — oxc_formatter's `SortImportsOptions::validate()` rejects
            // that combination — so this scalar-coverage case sets
            // `newlinesBetween:false`.
            r#"{"sortImports":{"partitionByNewline":true,"partitionByComment":true,"sortSideEffects":true,"order":"desc","ignoreCase":true,"newlinesBetween":false,"internalPattern":["^~/**"]}}"#,
        )
        .expect("parse");
        let sort_imports = options.sort_imports.expect("sort imports");
        // Grouped into two tuple comparisons to keep the assertion block small:
        // the four boolean flags, then order + internal pattern.
        assert_eq!(
            (
                sort_imports.partition_by_newline,
                sort_imports.partition_by_comment,
                sort_imports.sort_side_effects,
                sort_imports.ignore_case,
                sort_imports.newlines_between,
            ),
            (true, true, true, true, false),
        );
        assert_eq!(sort_imports.order, oxc_formatter::SortOrder::Desc);
        assert_eq!(sort_imports.internal_pattern, vec!["^~/**"]);
    }

    #[test]
    fn sort_imports_custom_groups_map() {
        let options = oxfmtrc_to_options(
            r#"{"sortImports":{"customGroups":[{"groupName":"utils","elementNamePattern":["^@app/utils$"],"selector":"import","modifiers":["type"]}]}}"#,
        )
        .expect("parse");
        let sort_imports = options.sort_imports.expect("sort imports");
        // Compare the whole parsed custom group in one assertion to keep the
        // block small.
        assert_eq!(
            sort_imports.custom_groups,
            vec![oxc_formatter::CustomGroupDefinition {
                group_name: "utils".to_owned(),
                element_name_pattern: vec!["^@app/utils$".to_owned()],
                selector: Some(oxc_formatter::ImportSelector::Import),
                modifiers: vec![oxc_formatter::ImportModifier::Type],
            }]
        );
    }

    #[test]
    fn sort_imports_groups_map() {
        let options = oxfmtrc_to_options(
            r#"{"sortImports":{"customGroups":[{"groupName":"utils","elementNamePattern":["^@app/utils$"]}],"groups":[["builtin","external"],{"newlinesBetween":true},["internal","utils"]]}}"#,
        )
        .expect("parse");
        let sort_imports = options.sort_imports.expect("sort imports");
        assert_eq!(sort_imports.groups.len(), 2);
        assert_eq!(sort_imports.groups[0].len(), 2);
        assert_eq!(sort_imports.groups[1].len(), 2);
        assert_eq!(sort_imports.newline_boundary_overrides, vec![Some(true)]);
    }

    #[test]
    fn sort_imports_groups_accept_flat_string_items() {
        // A `groups` entry may be a bare string (single group) as well as an
        // array of group names. Cover the flat-string variant.
        let options = oxfmtrc_to_options(
            r#"{"sortImports":{"groups":["builtin","external",["internal","sibling"]]}}"#,
        )
        .expect("parse");
        let sort_imports = options.sort_imports.expect("sort imports");
        assert_eq!(sort_imports.groups.len(), 3);
        assert_eq!(sort_imports.groups[0].len(), 1);
        assert_eq!(sort_imports.groups[1].len(), 1);
        assert_eq!(sort_imports.groups[2].len(), 2);
    }

    #[test]
    fn sort_imports_unknown_custom_group_errors() {
        let err = oxfmtrc_to_options(r#"{"sortImports":{"groups":[["unknown-group"]]}}"#)
            .expect_err("should error");
        assert!(err.contains("unknown group name `unknown-group`"), "{err}");
    }

    #[test]
    fn sort_imports_marker_at_start_errors() {
        let err = oxfmtrc_to_options(
            r#"{"sortImports":{"groups":[{"newlinesBetween":true},["builtin"]]}}"#,
        )
        .expect_err("should error");
        assert!(err.contains("cannot appear at the start"), "{err}");
    }

    #[test]
    fn sort_imports_marker_at_end_errors() {
        let err = oxfmtrc_to_options(
            r#"{"sortImports":{"groups":[["builtin"],{"newlinesBetween":true}]}}"#,
        )
        .expect_err("should error");
        assert!(err.contains("cannot appear at the end"), "{err}");
    }

    #[test]
    fn sort_imports_consecutive_markers_error() {
        let err = oxfmtrc_to_options(
            r#"{"sortImports":{"groups":[["builtin"],{"newlinesBetween":true},{"newlinesBetween":false},["external"]]}}"#,
        )
        .expect_err("should error");
        assert!(err.contains("consecutive"), "{err}");
    }

    #[test]
    fn sort_imports_unknown_selector_errors() {
        let err = oxfmtrc_to_options(
            r#"{"sortImports":{"customGroups":[{"groupName":"utils","elementNamePattern":[],"selector":"wat"}]}}"#,
        )
        .expect_err("should error");
        assert!(err.contains("unknown selector"), "{err}");
    }

    #[test]
    fn sort_imports_unknown_modifier_errors() {
        let err = oxfmtrc_to_options(
            r#"{"sortImports":{"customGroups":[{"groupName":"utils","elementNamePattern":[],"modifiers":["wat"]}]}}"#,
        )
        .expect_err("should error");
        assert!(err.contains("unknown modifier"), "{err}");
    }

    #[test]
    fn alias_experimental_sort_imports_is_supported() {
        let options = oxfmtrc_to_options(r#"{"experimentalSortImports":true}"#).expect("parse");
        assert!(options.sort_imports.is_some());
    }

    #[test]
    fn invalid_ignore_pattern_emits_warning() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        // A lone backslash is a dangling escape that `ignore`'s glob parser
        // rejects (`[` is accepted, so it would NOT emit a warning).
        fs::write(&config_path, r#"{"ignorePatterns":["\\"]}"#).expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("warning: failed to parse ignore pattern"));
    }

    #[test]
    fn ignore_pattern_filters_path_and_parents() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(&config_path, r#"{"ignorePatterns":["dist/"]}"#).expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        let matcher = loaded.ignore_matcher.as_ref().expect("matcher");
        assert!(matcher
            .matched_path_or_any_parents(Path::new("dist/foo.js"), false)
            .is_ignore());
        assert!(!matcher
            .matched_path_or_any_parents(Path::new("src/foo.js"), false)
            .is_ignore());
    }

    #[test]
    fn unsupported_top_level_keys_become_notices_not_warnings() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(&config_path, r#"{"singleQuote":true,"plugins":["foo"]}"#).expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert_eq!(loaded.options.quote_style.as_char(), '\'');
        // Unsupported keys are informational notices — NOT reject-worthy
        // warnings — so they never affect task resolution.
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.unsupported_option_notices.len(), 1);
        assert!(
            loaded.unsupported_option_notices[0].contains("unsupported .oxfmtrc option `plugins`")
        );
    }

    #[test]
    fn schema_key_is_accepted_without_notice() {
        // `$schema` is commonly added for editor autocompletion; it must not be
        // reported as an unsupported option.
        let (_temp, loaded) = load_config_from_temp(
            r#"{"$schema":"https://example.com/oxfmtrc.json","singleQuote":true}"#,
        );
        assert!(
            loaded.unsupported_option_notices.is_empty(),
            "{:?}",
            loaded.unsupported_option_notices
        );
    }

    #[test]
    fn unsupported_override_keys_become_notices() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"overrides":[{"files":["*.ts"],"plugins":["foo"],"options":{"singleQuote":true}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.unsupported_option_notices.len(), 1);
        assert!(loaded.unsupported_option_notices[0]
            .contains("unsupported .oxfmtrc override option `plugins`"));
    }

    #[test]
    fn formatter_option_at_override_root_is_noticed_not_silently_ignored() {
        // A formatter option placed at the override ENTRY root (instead of under
        // `options`) is silently ignored by serde. It must be surfaced as a
        // notice — this is the exact silent-drop class that issue #242 targets.
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"overrides":[{"files":["*.ts"],"singleQuote":true}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.unsupported_option_notices.len(), 1);
        assert!(loaded.unsupported_option_notices[0]
            .contains("unsupported .oxfmtrc override option `singleQuote`"));
    }

    #[test]
    fn unknown_key_inside_override_options_is_noticed() {
        // A typo/unknown key inside `overrides[].options` is silently dropped by
        // serde; the scanner must recurse into `options` and surface a notice.
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"overrides":[{"files":["*.ts"],"options":{"singelQuote":true}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.unsupported_option_notices.len(), 1);
        assert!(loaded.unsupported_option_notices[0]
            .contains("unsupported .oxfmtrc override option `singelQuote`"));
    }

    #[test]
    fn unknown_key_inside_sort_imports_object_is_noticed() {
        // A typo inside the `sortImports` object is silently dropped by serde;
        // the scanner must recurse into `sortImports` and surface it (reported
        // as `sortImports.<key>`). Covers both the top-level and override scopes.
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"sortImports":{"custmGroups":[]},"overrides":[{"files":["*.ts"],"options":{"sortImports":{"ignoreCse":true}}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let notices = loaded.unsupported_option_notices.join("\n");
        assert!(
            loaded.unsupported_option_notices.len() == 2
                && notices.contains("`sortImports.custmGroups`")
                && notices.contains("`sortImports.ignoreCse`"),
            "{notices}"
        );
    }

    #[test]
    fn unknown_key_inside_experimental_sort_imports_alias_is_noticed() {
        // The `experimentalSortImports` alias also takes a nested object; a typo
        // inside it must be surfaced too (reported under the alias spelling),
        // not silently dropped.
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"experimentalSortImports":{"custmGroups":[]}}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.unsupported_option_notices.len(), 1);
        assert!(
            loaded.unsupported_option_notices[0].contains("`experimentalSortImports.custmGroups`")
        );
    }

    #[test]
    fn valid_sort_imports_object_produces_no_notices() {
        // A fully-valid `sortImports` object (and the bool form) must not be
        // mistaken for unknown keys.
        let (_temp, loaded) = load_config_from_temp(
            r#"{"sortImports":{"order":"desc","ignoreCase":true,"groups":[["builtin"]]}}"#,
        );
        assert!(
            loaded.unsupported_option_notices.is_empty(),
            "{:?}",
            loaded.unsupported_option_notices
        );
    }

    #[test]
    fn override_files_accept_string_array_and_excludes() {
        let (temp, loaded) = load_config_from_temp(
            r#"{"printWidth":80,"overrides":[{"files":"*.json","excludeFiles":"package.json","options":{"printWidth":120}},{"files":["*.md","*.mdx"],"options":{"printWidth":140}}]}"#,
        );

        assert_override_widths(
            &loaded,
            temp.path(),
            &[
                ("foo.json", 120),
                ("docs/readme.mdx", 140),
                ("src/example.json", 120),
                ("package.json", 80),
            ],
        );
    }

    #[test]
    fn override_with_empty_files_is_ignored() {
        let (temp, loaded) = load_config_from_temp(
            r#"{"printWidth":80,"overrides":[{"files":[],"options":{"printWidth":120}}]}"#,
        );
        assert_eq!(
            loaded
                .options_for(&temp.path().join("foo.json"))
                .line_width
                .value(),
            80
        );
    }

    #[test]
    fn override_glob_is_treated_as_literal_by_gitignore() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"overrides":[{"files":"[","options":{"semi":false}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert!(loaded.overrides.len() == 1);
    }

    #[test]
    fn override_applies_to_matching_file() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"singleQuote":false,"overrides":[{"files":["*.ts"],"options":{"singleQuote":true}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        let ts = loaded.options_for(&temp.path().join("src/example.ts"));
        let json = loaded.options_for(&temp.path().join("src/example.json"));

        assert_eq!(ts.quote_style.as_char(), '\'');
        assert_eq!(json.quote_style.as_char(), '"');
    }

    #[test]
    fn override_omitting_sort_imports_preserves_top_level_sorting() {
        // Regression: a top-level `sortImports` must NOT be silently cleared by
        // an override that changes an unrelated option and omits `sortImports`.
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"sortImports":true,"overrides":[{"files":["*.ts"],"options":{"singleQuote":true}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        let ts = loaded.options_for(&temp.path().join("src/example.ts"));

        // Override applied (singleQuote) AND top-level sortImports preserved.
        assert_eq!(ts.quote_style.as_char(), '\'');
        assert!(
            ts.sort_imports.is_some(),
            "override omitting sortImports must not disable inherited sorting"
        );
    }

    #[test]
    fn override_can_explicitly_disable_sort_imports() {
        // An override that explicitly sets `sortImports: false` DOES clear the
        // inherited top-level setting for matching files.
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"sortImports":true,"overrides":[{"files":["*.ts"],"options":{"sortImports":false}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        let ts = loaded.options_for(&temp.path().join("src/example.ts"));
        let json = loaded.options_for(&temp.path().join("src/example.json"));

        assert!(ts.sort_imports.is_none(), "explicit false must disable");
        assert!(
            json.sort_imports.is_some(),
            "non-matching file keeps sorting"
        );
    }

    #[test]
    fn override_relative_matching_uses_config_root() {
        let temp = TempDir::new().expect("tempdir");
        let nested = temp.path().join("packages/app");
        fs::create_dir_all(nested.join("src")).expect("mkdirs");
        let config_path = nested.join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"singleQuote":false,"overrides":[{"files":["src/**/*.ts"],"options":{"singleQuote":true}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(&nested).expect("discover");
        let ts = loaded.options_for(&nested.join("src/example.ts"));
        assert_eq!(ts.quote_style.as_char(), '\'');
    }

    #[test]
    fn invalid_override_options_return_error_at_load_time() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"overrides":[{"files":["*.json"],"options":{"printWidth":0}}]}"#,
        )
        .expect("config");

        let result = discover_config(temp.path());
        assert!(result.is_err(), "expected load error, got {result:?}");
    }

    #[test]
    fn overrides_apply_in_order_last_match_wins() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"printWidth":80,"overrides":[{"files":["*.ts"],"options":{"printWidth":100}},{"files":["*.ts"],"options":{"printWidth":120}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert_eq!(
            loaded
                .options_for(&temp.path().join("src/example.ts"))
                .line_width
                .value(),
            120
        );
    }

    #[test]
    fn overrides_match_deeply_nested_paths() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"printWidth":80,"overrides":[{"files":["*.json"],"options":{"printWidth":320}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        assert_eq!(
            loaded
                .options_for(&temp.path().join("a/b/c/deep.json"))
                .line_width
                .value(),
            320
        );
    }

    fn assert_override_widths(loaded: &super::LoadedConfig, root: &Path, cases: &[(&str, u16)]) {
        for (relative_path, expected_width) in cases {
            assert_eq!(
                loaded
                    .options_for(&root.join(relative_path))
                    .line_width
                    .value(),
                *expected_width,
                "unexpected printWidth for {relative_path}"
            );
        }
    }
}
