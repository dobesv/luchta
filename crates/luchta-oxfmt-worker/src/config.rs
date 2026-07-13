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

const CONFIG_FILENAMES: [&str; 2] = [".oxfmtrc.json", ".oxfmtrc.jsonc"];

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoadedConfig {
    pub options: JsFormatOptions,
    pub path: Option<PathBuf>,
    pub ignore_matcher: Option<Gitignore>,
    pub warnings: Vec<String>,
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
            // Skip this override when its excludeFiles pattern matches.
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
}

pub fn discover_config(cwd: &Path) -> Result<LoadedConfig, String> {
    let path = find_config_path(cwd);
    let parsed = match path.as_deref() {
        Some(path) => load_config_from_path(path)?,
        None => ParsedConfig {
            options: JsFormatOptions::new(),
            ignore_matcher: None,
            warnings: Vec::new(),
            config_root: cwd.to_path_buf(),
            overrides: Vec::new(),
        },
    };
    Ok(LoadedConfig {
        options: parsed.options,
        path,
        ignore_matcher: parsed.ignore_matcher,
        warnings: parsed.warnings,
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

    // Supported subset only. Still ignored: editorconfig merging,
    // JS/TS config files, plugins, sortImports, Tailwind sorting, JSDoc,
    // Svelte payloads, sortPackageJson.
    Ok(options)
}

fn apply_oxfmtrc_to_options(config: &OxfmtRc, options: &mut JsFormatOptions) -> Result<(), String> {
    if let Some(use_tabs) = config.use_tabs {
        options.indent_style = if use_tabs {
            IndentStyle::Tab
        } else {
            IndentStyle::Space
        };
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
        options.line_ending = match end_of_line {
            EndOfLine::Lf => LineEnding::Lf,
            EndOfLine::Crlf => LineEnding::Crlf,
            EndOfLine::Cr => LineEnding::Cr,
        };
    }
    if let Some(single_quote) = config.single_quote {
        options.quote_style = if single_quote {
            QuoteStyle::Single
        } else {
            QuoteStyle::Double
        };
    }
    if let Some(jsx_single_quote) = config.jsx_single_quote {
        options.jsx_quote_style = if jsx_single_quote {
            QuoteStyle::Single
        } else {
            QuoteStyle::Double
        };
    }
    if let Some(semi) = config.semi {
        options.semicolons = if semi {
            Semicolons::Always
        } else {
            Semicolons::AsNeeded
        };
    }
    if let Some(trailing_comma) = config.trailing_comma {
        options.trailing_commas = match trailing_comma {
            TrailingComma::All => TrailingCommas::All,
            TrailingComma::Es5 => TrailingCommas::Es5,
            TrailingComma::None => TrailingCommas::None,
        };
    }
    if let Some(bracket_spacing) = config.bracket_spacing {
        options.bracket_spacing = BracketSpacing::from(bracket_spacing);
    }
    if let Some(bracket_same_line) = config.bracket_same_line {
        options.bracket_same_line = BracketSameLine::from(bracket_same_line);
    }
    if let Some(arrow_parens) = config.arrow_parens {
        options.arrow_parentheses = match arrow_parens {
            ArrowParens::Avoid => ArrowParentheses::AsNeeded,
            ArrowParens::Always => ArrowParentheses::Always,
        };
    }
    if let Some(quote_props) = config.quote_props {
        options.quote_properties = match quote_props {
            QuoteProps::AsNeeded => QuoteProperties::AsNeeded,
            QuoteProps::Consistent => QuoteProperties::Consistent,
            QuoteProps::Preserve => QuoteProperties::Preserve,
        };
    }
    if let Some(single_attribute_per_line) = config.single_attribute_per_line {
        options.attribute_position = if single_attribute_per_line {
            AttributePosition::Multiline
        } else {
            AttributePosition::Auto
        };
    }
    if let Some(object_wrap) = config.object_wrap {
        options.expand = match object_wrap {
            ObjectWrap::Preserve => Expand::Auto,
            ObjectWrap::Collapse => Expand::Never,
        };
    }
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
    Ok(())
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
        // Validate the override's options eagerly so invalid values (e.g.
        // printWidth: 0) surface as a config error at load time instead of
        // panicking later in `LoadedConfig::options_for`.
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

struct ParsedConfig {
    options: JsFormatOptions,
    ignore_matcher: Option<Gitignore>,
    warnings: Vec<String>,
    config_root: PathBuf,
    overrides: Vec<OverrideMatcher>,
}

fn load_config_from_path(path: &Path) -> Result<ParsedConfig, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    // Strip comments for both .json and .jsonc — harmless for comment-free JSON
    let json = strip_json_comments(&source)
        .map_err(|error| format!("failed to strip comments in {}: {error}", path.display()))?;
    let config = parse_oxfmtrc(&json)
        .map_err(|error| format!("failed to load oxfmt config {}: {error}", path.display()))?;
    let options = options_from_oxfmtrc(&config)
        .map_err(|error| format!("failed to load oxfmt config {}: {error}", path.display()))?;
    let root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let (ignore_matcher, warnings) =
        build_ignore_matcher(config.ignore_patterns.as_deref().unwrap_or(&[]), &root);
    let overrides = build_override_matchers(config.overrides.as_deref().unwrap_or(&[]), &root)
        .map_err(|error| format!("failed to load oxfmt config {}: {error}", path.display()))?;
    Ok(ParsedConfig {
        options,
        ignore_matcher,
        warnings,
        config_root: root,
        overrides,
    })
}

fn strip_json_comments(source: &str) -> Result<String, std::io::Error> {
    let mut stripped = String::new();
    StripComments::new(source.as_bytes()).read_to_string(&mut stripped)?;
    Ok(stripped)
}

fn find_config_path(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        for filename in CONFIG_FILENAMES {
            let candidate = ancestor.join(filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use assert_fs::TempDir;
    use std::fs;

    use super::{discover_config, oxfmtrc_to_options, CONFIG_FILENAMES};

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
    fn oxfmtrc_ignores_unknown_keys() {
        let options = oxfmtrc_to_options(r#"{"semi":false,"unknownThing":123}"#).expect("parse");
        assert!(options.semicolons.is_as_needed());
    }

    #[test]
    fn oxfmtrc_jsonc_parses_comments() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[1]);
        fs::write(
            &config_path,
            "{\n  // comment\n  \"singleQuote\": true,\n  /* block */\n  \"semi\": false\n}\n",
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");

        assert_eq!(loaded.path.as_deref(), Some(config_path.as_path()));
        assert_eq!(loaded.options.quote_style.as_char(), '\'');
        assert!(loaded.options.semicolons.is_as_needed());
    }

    #[test]
    fn no_config_returns_default_options() {
        let temp = TempDir::new().expect("tempdir");
        let loaded = discover_config(temp.path()).expect("discover");
        let defaults = oxc_formatter::JsFormatOptions::new();

        assert!(loaded.path.is_none());
        assert_eq!(loaded.options.indent_style, defaults.indent_style);
        assert_eq!(
            loaded.options.indent_width.value(),
            defaults.indent_width.value()
        );
        assert_eq!(
            loaded.options.line_width.value(),
            defaults.line_width.value()
        );
        assert_eq!(loaded.options.quote_style, defaults.quote_style);
        assert_eq!(loaded.options.semicolons, defaults.semicolons);
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
    fn print_width_maps_to_line_width() {
        let options = oxfmtrc_to_options(r#"{"printWidth":100}"#).expect("parse");
        assert_eq!(options.line_width.value(), 100);
    }

    #[test]
    fn invalid_print_width_returns_error() {
        let err = oxfmtrc_to_options(r#"{"printWidth":0}"#).expect_err("invalid");
        assert!(err.contains("invalid .oxfmtrc printWidth"));
    }

    #[test]
    fn end_of_line_crlf_maps_to_crlf() {
        let options = oxfmtrc_to_options(r#"{"endOfLine":"crlf"}"#).expect("parse");
        assert!(matches!(
            options.line_ending,
            oxc_formatter_core::LineEnding::Crlf
        ));
    }

    #[test]
    fn single_quote_true_maps_to_single_quote_style() {
        let options = oxfmtrc_to_options(r#"{"singleQuote":true}"#).expect("parse");
        assert_eq!(options.quote_style.as_char(), '\'');
    }

    #[test]
    fn jsx_single_quote_true_maps_to_single_quote_style() {
        let options = oxfmtrc_to_options(r#"{"jsxSingleQuote":true}"#).expect("parse");
        assert_eq!(options.jsx_quote_style.as_char(), '\'');
    }

    #[test]
    fn semi_false_maps_to_as_needed() {
        let options = oxfmtrc_to_options(r#"{"semi":false}"#).expect("parse");
        assert!(options.semicolons.is_as_needed());
    }

    #[test]
    fn trailing_comma_none_maps_to_none() {
        let options = oxfmtrc_to_options(r#"{"trailingComma":"none"}"#).expect("parse");
        assert!(options.trailing_commas.is_none());
    }

    #[test]
    fn ignore_patterns_match_relative_paths_under_config_root() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(&config_path, r#"{"ignorePatterns":["dist/"]}"#).expect("config");

        let loaded = discover_config(temp.path()).expect("discover");
        let ignored = temp.path().join("dist/out.ts");
        let kept = temp.path().join("src/out.ts");

        assert!(loaded
            .ignore_matcher
            .as_ref()
            .expect("ignore matcher")
            .matched_path_or_any_parents(&ignored, false)
            .is_ignore());
        assert!(!loaded
            .ignore_matcher
            .as_ref()
            .expect("ignore matcher")
            .matched_path_or_any_parents(&kept, false)
            .is_ignore());
    }

    #[test]
    fn discover_config_anchors_parent_ignore_patterns_to_config_directory() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path();
        let pkg = repo.join("packages/app");
        fs::create_dir_all(pkg.join("src")).expect("src");
        fs::write(
            repo.join(CONFIG_FILENAMES[0]),
            r#"{"ignorePatterns":["/packages/app/dist/"]}"#,
        )
        .expect("config");

        let loaded = discover_config(&pkg).expect("discover");
        let ignored = repo.join("packages/app/dist/out.ts");
        let kept = repo.join("packages/app/src/out.ts");

        assert!(loaded
            .ignore_matcher
            .as_ref()
            .expect("ignore matcher")
            .matched_path_or_any_parents(&ignored, false)
            .is_ignore());
        assert!(!loaded
            .ignore_matcher
            .as_ref()
            .expect("ignore matcher")
            .matched_path_or_any_parents(&kept, false)
            .is_ignore());
    }

    #[test]
    fn bracket_spacing_false_maps_to_bracket_spacing_off() {
        let options = oxfmtrc_to_options(r#"{"bracketSpacing":false}"#).expect("parse");
        assert!(!options.bracket_spacing.value());
    }

    #[test]
    fn bracket_spacing_true_maps_to_bracket_spacing_on() {
        let options = oxfmtrc_to_options(r#"{"bracketSpacing":true}"#).expect("parse");
        assert!(options.bracket_spacing.value());
    }

    #[test]
    fn bracket_same_line_true_maps_to_bracket_same_line_on() {
        let options = oxfmtrc_to_options(r#"{"bracketSameLine":true}"#).expect("parse");
        assert!(options.bracket_same_line.value());
    }

    #[test]
    fn bracket_same_line_false_maps_to_bracket_same_line_off() {
        let options = oxfmtrc_to_options(r#"{"bracketSameLine":false}"#).expect("parse");
        assert!(!options.bracket_same_line.value());
    }

    #[test]
    fn arrow_parens_avoid_maps_to_as_needed() {
        let options = oxfmtrc_to_options(r#"{"arrowParens":"avoid"}"#).expect("parse");
        assert!(options.arrow_parentheses.is_as_needed());
    }

    #[test]
    fn arrow_parens_always_maps_to_always() {
        let options = oxfmtrc_to_options(r#"{"arrowParens":"always"}"#).expect("parse");
        assert!(options.arrow_parentheses.is_always());
    }

    #[test]
    fn quote_props_as_needed_maps_to_as_needed() {
        let options = oxfmtrc_to_options(r#"{"quoteProps":"as-needed"}"#).expect("parse");
        assert!(matches!(
            options.quote_properties,
            oxc_formatter::QuoteProperties::AsNeeded
        ));
    }

    #[test]
    fn quote_props_consistent_maps_to_consistent() {
        let options = oxfmtrc_to_options(r#"{"quoteProps":"consistent"}"#).expect("parse");
        assert!(options.quote_properties.is_consistent());
    }

    #[test]
    fn quote_props_preserve_maps_to_preserve() {
        let options = oxfmtrc_to_options(r#"{"quoteProps":"preserve"}"#).expect("parse");
        assert!(matches!(
            options.quote_properties,
            oxc_formatter::QuoteProperties::Preserve
        ));
    }

    #[test]
    fn single_attribute_per_line_true_maps_to_multiline() {
        let options = oxfmtrc_to_options(r#"{"singleAttributePerLine":true}"#).expect("parse");
        assert!(matches!(
            options.attribute_position,
            oxc_formatter::AttributePosition::Multiline
        ));
    }

    #[test]
    fn single_attribute_per_line_false_maps_to_auto() {
        let options = oxfmtrc_to_options(r#"{"singleAttributePerLine":false}"#).expect("parse");
        assert!(matches!(
            options.attribute_position,
            oxc_formatter::AttributePosition::Auto
        ));
    }

    #[test]
    fn object_wrap_preserve_maps_to_auto() {
        let options = oxfmtrc_to_options(r#"{"objectWrap":"preserve"}"#).expect("parse");
        assert!(matches!(options.expand, oxc_formatter::Expand::Auto));
    }

    #[test]
    fn object_wrap_collapse_maps_to_never() {
        let options = oxfmtrc_to_options(r#"{"objectWrap":"collapse"}"#).expect("parse");
        assert!(matches!(options.expand, oxc_formatter::Expand::Never));
    }

    #[test]
    fn html_whitespace_sensitivity_ignore_maps_to_true() {
        let options =
            oxfmtrc_to_options(r#"{"htmlWhitespaceSensitivity":"ignore"}"#).expect("parse");
        assert!(options.html_whitespace_sensitivity_ignore);
    }

    #[test]
    fn html_whitespace_sensitivity_css_maps_to_false() {
        let options = oxfmtrc_to_options(r#"{"htmlWhitespaceSensitivity":"css"}"#).expect("parse");
        assert!(!options.html_whitespace_sensitivity_ignore);
    }

    #[test]
    fn embedded_language_formatting_auto_maps_to_auto() {
        let options =
            oxfmtrc_to_options(r#"{"embeddedLanguageFormatting":"auto"}"#).expect("parse");
        assert!(options.embedded_language_formatting.is_auto());
    }

    #[test]
    fn embedded_language_formatting_off_maps_to_off() {
        let options = oxfmtrc_to_options(r#"{"embeddedLanguageFormatting":"off"}"#).expect("parse");
        assert!(options.embedded_language_formatting.is_off());
    }

    #[test]
    fn overrides_apply_per_file() {
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
                .options_for(&temp.path().join("src/example.json"))
                .line_width
                .value(),
            320
        );
        assert_eq!(
            loaded
                .options_for(&temp.path().join("src/example.ts"))
                .line_width
                .value(),
            80
        );
    }

    #[test]
    fn overrides_respect_exclude_files() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]);
        fs::write(
            &config_path,
            r#"{"printWidth":80,"overrides":[{"files":["*.json"],"excludeFiles":["package.json"],"options":{"printWidth":320}}]}"#,
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");

        // Non-excluded JSON gets the override.
        assert_eq!(
            loaded
                .options_for(&temp.path().join("src/example.json"))
                .line_width
                .value(),
            320
        );
        // Excluded file falls back to base options.
        assert_eq!(
            loaded
                .options_for(&temp.path().join("package.json"))
                .line_width
                .value(),
            80
        );
    }

    #[test]
    fn invalid_override_options_return_error_at_load_time() {
        // Regression: an invalid value inside an override must surface as a
        // config error at load time, not panic later in `options_for`.
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
}
