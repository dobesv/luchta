#![cfg(feature = "oxc")]

use oxc_formatter::{CommentLineStrategy, JsdocOptions, LineWrappingStyle};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum JsdocUserConfig {
    Bool(bool),
    Object(JsdocConfig),
}

impl JsdocUserConfig {
    fn into_config(self) -> Option<JsdocConfig> {
        match self {
            Self::Bool(true) => Some(JsdocConfig::default()),
            Self::Bool(false) => None,
            Self::Object(config) => Some(config),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct JsdocConfig {
    capitalize_descriptions: Option<bool>,
    comment_line_strategy: Option<CommentLineStrategyConfig>,
    separate_tag_groups: Option<bool>,
    separate_returns_from_param: Option<bool>,
    bracket_spacing: Option<bool>,
    description_with_dot: Option<bool>,
    add_default_to_description: Option<bool>,
    prefer_code_fences: Option<bool>,
    line_wrapping_style: Option<LineWrappingStyleConfig>,
    description_tag: Option<bool>,
    keep_unparsable_example_indent: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum CommentLineStrategyConfig {
    SingleLine,
    Multiline,
    Keep,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum LineWrappingStyleConfig {
    Greedy,
    Balance,
}

pub(crate) fn resolve_jsdoc(config: Option<JsdocUserConfig>) -> Option<JsdocOptions> {
    let jsdoc_config = config.and_then(JsdocUserConfig::into_config)?;

    let mut jsdoc = JsdocOptions::default();
    apply_jsdoc_options(&jsdoc_config, &mut jsdoc);
    Some(jsdoc)
}

fn apply_jsdoc_options(config: &JsdocConfig, jsdoc: &mut JsdocOptions) {
    if let Some(value) = config.capitalize_descriptions {
        jsdoc.capitalize_descriptions = value;
    }
    if let Some(value) = config.comment_line_strategy {
        jsdoc.comment_line_strategy = map_comment_line_strategy(value);
    }
    if let Some(value) = config.separate_tag_groups {
        jsdoc.separate_tag_groups = value;
    }
    if let Some(value) = config.separate_returns_from_param {
        jsdoc.separate_returns_from_param = value;
    }
    if let Some(value) = config.bracket_spacing {
        jsdoc.bracket_spacing = value;
    }
    if let Some(value) = config.description_with_dot {
        jsdoc.description_with_dot = value;
    }
    if let Some(value) = config.add_default_to_description {
        jsdoc.add_default_to_description = value;
    }
    if let Some(value) = config.prefer_code_fences {
        jsdoc.prefer_code_fences = value;
    }
    if let Some(value) = config.line_wrapping_style {
        jsdoc.line_wrapping_style = map_line_wrapping_style(value);
    }
    if let Some(value) = config.description_tag {
        jsdoc.description_tag = value;
    }
    if let Some(value) = config.keep_unparsable_example_indent {
        jsdoc.keep_unparsable_example_indent = value;
    }
}

fn map_comment_line_strategy(config: CommentLineStrategyConfig) -> CommentLineStrategy {
    match config {
        CommentLineStrategyConfig::SingleLine => CommentLineStrategy::SingleLine,
        CommentLineStrategyConfig::Multiline => CommentLineStrategy::Multiline,
        CommentLineStrategyConfig::Keep => CommentLineStrategy::Keep,
    }
}

fn map_line_wrapping_style(config: LineWrappingStyleConfig) -> LineWrappingStyle {
    match config {
        LineWrappingStyleConfig::Greedy => LineWrappingStyle::Greedy,
        LineWrappingStyleConfig::Balance => LineWrappingStyle::Balance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsdoc_boolean_true_enables_defaults() {
        let config = Some(JsdocUserConfig::Bool(true));
        let result = resolve_jsdoc(config);
        assert!(result.is_some());
        let options = result.unwrap();
        assert!(options.capitalize_descriptions); // default is true
        assert!(!options.separate_tag_groups); // default is false
    }

    #[test]
    fn jsdoc_boolean_false_disables() {
        let config = Some(JsdocUserConfig::Bool(false));
        let result = resolve_jsdoc(config);
        assert!(result.is_none());
    }

    #[test]
    fn jsdoc_none_returns_none() {
        let result = resolve_jsdoc(None);
        assert!(result.is_none());
    }

    #[test]
    fn jsdoc_object_applies_overrides() {
        let config: JsdocConfig = serde_json::from_str(r#"{"bracketSpacing":true}"#).unwrap();
        let config = Some(JsdocUserConfig::Object(config));
        let result = resolve_jsdoc(config);
        assert!(result.is_some());
        let options = result.unwrap();
        assert!(options.bracket_spacing);
        assert!(options.capitalize_descriptions); // still default
    }

    #[test]
    fn jsdoc_comment_line_strategy_maps() {
        let config: JsdocConfig =
            serde_json::from_str(r#"{"commentLineStrategy":"multiline"}"#).unwrap();
        let config = Some(JsdocUserConfig::Object(config));
        let result = resolve_jsdoc(config);
        assert!(result.is_some());
        let options = result.unwrap();
        assert_eq!(
            options.comment_line_strategy,
            CommentLineStrategy::Multiline
        );
    }

    #[test]
    fn jsdoc_line_wrapping_style_maps() {
        let config: JsdocConfig =
            serde_json::from_str(r#"{"lineWrappingStyle":"balance"}"#).unwrap();
        let config = Some(JsdocUserConfig::Object(config));
        let result = resolve_jsdoc(config);
        assert!(result.is_some());
        let options = result.unwrap();
        assert_eq!(options.line_wrapping_style, LineWrappingStyle::Balance);
    }
}
