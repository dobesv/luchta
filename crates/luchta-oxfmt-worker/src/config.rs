#![cfg(feature = "oxc")]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use json_strip_comments::StripComments;
use oxc_formatter::{
    BracketSameLine, BracketSpacing, JsFormatOptions, QuoteStyle, Semicolons, TrailingCommas,
};
use oxc_formatter_core::{IndentStyle, IndentWidth, LineEnding, LineWidth};
use serde::Deserialize;

const CONFIG_FILENAMES: [&str; 2] = [".oxfmtrc.json", ".oxfmtrc.jsonc"];

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoadedConfig {
    pub options: JsFormatOptions,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
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

pub fn discover_config(cwd: &Path) -> Result<LoadedConfig, String> {
    let path = find_config_path(cwd);
    let options = match path.as_deref() {
        Some(path) => load_options_from_path(path)?,
        None => JsFormatOptions::new(),
    };
    Ok(LoadedConfig { options, path })
}

pub fn oxfmtrc_to_options(json: &str) -> Result<JsFormatOptions, String> {
    let config: OxfmtRc =
        serde_json::from_str(json).map_err(|error| format!("failed to parse .oxfmtrc: {error}"))?;
    let mut options = JsFormatOptions::new();

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

    // Supported subset only. Intentionally ignored for now: overrides/per-file resolution,
    // ignore patterns, editorconfig merging, JS/TS config files, plugins, quoteProps,
    // arrowParens, singleAttributePerLine, objectWrap, htmlWhitespaceSensitivity,
    // embeddedLanguageFormatting, sortImports, Tailwind sorting, JSDoc, Svelte payloads.
    Ok(options)
}

fn load_options_from_path(path: &Path) -> Result<JsFormatOptions, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    // Strip comments for both .json and .jsonc — harmless for comment-free JSON
    let json = strip_json_comments(&source)
        .map_err(|error| format!("failed to strip comments in {}: {error}", path.display()))?;
    oxfmtrc_to_options(&json)
        .map_err(|error| format!("failed to load oxfmt config {}: {error}", path.display()))
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

    // ========================================
    // Fix 1: JSON file with comments parses correctly
    // ========================================
    #[test]
    fn oxfmtrc_json_parses_comments() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join(CONFIG_FILENAMES[0]); // .oxfmtrc.json
        fs::write(
            &config_path,
            "{\n  // comment in .json\n  \"singleQuote\": true,\n  /* block */\n  \"semi\": false\n}\n",
        )
        .expect("config");

        let loaded = discover_config(temp.path()).expect("discover");

        assert_eq!(loaded.path.as_deref(), Some(config_path.as_path()));
        assert_eq!(loaded.options.quote_style.as_char(), '\'');
        assert!(loaded.options.semicolons.is_as_needed());
    }

    // ========================================
    // Fix 2: Field mapping tests
    // ========================================
    use oxc_formatter::{QuoteStyle, Semicolons, TrailingCommas};
    use oxc_formatter_core::{IndentStyle, LineEnding};

    #[test]
    fn use_tabs_true_maps_to_tab_indent_style() {
        let options = oxfmtrc_to_options(r#"{"useTabs":true}"#).expect("parse");
        assert!(matches!(options.indent_style, IndentStyle::Tab));
    }

    #[test]
    fn use_tabs_false_maps_to_space_indent_style() {
        let options = oxfmtrc_to_options(r#"{"useTabs":false}"#).expect("parse");
        assert!(matches!(options.indent_style, IndentStyle::Space));
    }

    #[test]
    fn tab_width_maps_to_indent_width() {
        let options = oxfmtrc_to_options(r#"{"tabWidth":4}"#).expect("parse");
        assert_eq!(options.indent_width.value(), 4);
    }

    #[test]
    fn print_width_maps_to_line_width() {
        let options = oxfmtrc_to_options(r#"{"printWidth":100}"#).expect("parse");
        assert_eq!(options.line_width.value(), 100);
    }

    #[test]
    fn end_of_line_lf_maps_to_line_ending_lf() {
        let options = oxfmtrc_to_options(r#"{"endOfLine":"lf"}"#).expect("parse");
        assert!(matches!(options.line_ending, LineEnding::Lf));
    }

    #[test]
    fn end_of_line_crlf_maps_to_line_ending_crlf() {
        let options = oxfmtrc_to_options(r#"{"endOfLine":"crlf"}"#).expect("parse");
        assert!(matches!(options.line_ending, LineEnding::Crlf));
    }

    #[test]
    fn end_of_line_cr_maps_to_line_ending_cr() {
        let options = oxfmtrc_to_options(r#"{"endOfLine":"cr"}"#).expect("parse");
        assert!(matches!(options.line_ending, LineEnding::Cr));
    }

    #[test]
    fn single_quote_true_maps_to_single_quote_style() {
        let options = oxfmtrc_to_options(r#"{"singleQuote":true}"#).expect("parse");
        assert!(matches!(options.quote_style, QuoteStyle::Single));
    }

    #[test]
    fn single_quote_false_maps_to_double_quote_style() {
        let options = oxfmtrc_to_options(r#"{"singleQuote":false}"#).expect("parse");
        assert!(matches!(options.quote_style, QuoteStyle::Double));
    }

    #[test]
    fn jsx_single_quote_true_maps_to_single_jsx_quote_style() {
        let options = oxfmtrc_to_options(r#"{"jsxSingleQuote":true}"#).expect("parse");
        assert!(matches!(options.jsx_quote_style, QuoteStyle::Single));
    }

    #[test]
    fn jsx_single_quote_false_maps_to_double_jsx_quote_style() {
        let options = oxfmtrc_to_options(r#"{"jsxSingleQuote":false}"#).expect("parse");
        assert!(matches!(options.jsx_quote_style, QuoteStyle::Double));
    }

    #[test]
    fn semi_false_maps_to_as_needed_semicolons() {
        let options = oxfmtrc_to_options(r#"{"semi":false}"#).expect("parse");
        assert!(matches!(options.semicolons, Semicolons::AsNeeded));
    }

    #[test]
    fn semi_true_maps_to_always_semicolons() {
        let options = oxfmtrc_to_options(r#"{"semi":true}"#).expect("parse");
        assert!(matches!(options.semicolons, Semicolons::Always));
    }

    #[test]
    fn trailing_comma_all_maps_to_all() {
        let options = oxfmtrc_to_options(r#"{"trailingComma":"all"}"#).expect("parse");
        assert!(matches!(options.trailing_commas, TrailingCommas::All));
    }

    #[test]
    fn trailing_comma_es5_maps_to_es5() {
        let options = oxfmtrc_to_options(r#"{"trailingComma":"es5"}"#).expect("parse");
        assert!(matches!(options.trailing_commas, TrailingCommas::Es5));
    }

    #[test]
    fn trailing_comma_none_maps_to_none() {
        let options = oxfmtrc_to_options(r#"{"trailingComma":"none"}"#).expect("parse");
        assert!(matches!(options.trailing_commas, TrailingCommas::None));
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
}
