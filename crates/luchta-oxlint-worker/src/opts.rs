#![cfg(feature = "oxc")]

use std::collections::HashSet;
use std::path::PathBuf;

use luchta_worker::{tokenize::tokenize_command, WorkerRequest};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OxlintOpts {
    pub fix: bool,
    pub suppress_all: bool,
    pub prune_suppressions: bool,
    pub quiet: bool,
    pub type_aware: bool,
    pub type_check: bool,
    pub type_check_only: bool,
    pub config: Option<PathBuf>,
}

impl OxlintOpts {
    pub fn from_request(req: &WorkerRequest) -> Self {
        let mut tokens = tokenize_command(&req.command);
        if let Some(raw) = req.env.get("OXLINT_OPTS") {
            tokens.extend(tokenize_command(raw));
        }
        Self::parse_tokens(&tokens)
    }

    pub fn from_command(command: &str) -> Self {
        Self::parse_tokens(&tokenize_command(command))
    }

    fn parse_tokens(tokens: &[String]) -> Self {
        let token_set: HashSet<&str> = tokens.iter().map(String::as_str).collect();
        let mut config = None;

        for (index, token) in tokens.iter().enumerate() {
            if let Some(value) = token.strip_prefix("--config=") {
                if config.is_none() && !value.is_empty() {
                    config = Some(PathBuf::from(value));
                }
                continue;
            }
            if token == "--config" {
                if let Some(value) = tokens.get(index + 1) {
                    if config.is_none() && !value.is_empty() {
                        config = Some(PathBuf::from(value));
                    }
                }
            }
        }

        Self {
            fix: token_set.contains("--fix"),
            suppress_all: token_set.contains("--suppress-all"),
            prune_suppressions: token_set.contains("--prune-suppressions"),
            quiet: token_set.contains("--quiet") || token_set.contains("--no-warnings"),
            type_aware: token_set.contains("--type-aware"),
            type_check: token_set.contains("--type-check"),
            type_check_only: token_set.contains("--type-check-only"),
            config,
        }
    }

    pub fn suppression_prune_mode(&self) -> bool {
        self.prune_suppressions || self.fix
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use luchta_worker::{tokenize::tokenize_command, WorkerRequest};

    use super::OxlintOpts;

    #[test]
    fn tokenize_respects_quoted_segments() {
        assert_eq!(
            tokenize_command("--config '/a b/.oxlintrc.json' --fix \"two words\""),
            vec!["--config", "/a b/.oxlintrc.json", "--fix", "two words",]
        );
    }

    #[test]
    fn tokenize_keeps_rest_of_unmatched_quote_as_one_token() {
        assert_eq!(
            tokenize_command("--config '/a b/.oxlintrc.json --fix"),
            vec!["--config", "/a b/.oxlintrc.json --fix"]
        );
    }

    #[test]
    fn tokenize_preserves_empty_quoted_tokens() {
        assert_eq!(
            tokenize_command("--config \"\" --fix"),
            vec!["--config", "", "--fix"]
        );
    }

    #[test]
    fn parse_empty_opts() {
        assert_eq!(OxlintOpts::from_command(""), OxlintOpts::default());
    }

    #[test]
    fn parse_supported_flags() {
        let opts = OxlintOpts::from_command(
            "--fix --suppress-all --prune-suppressions --quiet --no-warnings --type-aware --type-check --type-check-only",
        );
        assert!(opts.fix);
        assert!(opts.suppress_all);
        assert!(opts.prune_suppressions);
        assert!(opts.quiet);
        assert!(opts.type_aware);
        assert!(opts.type_check);
        assert!(opts.type_check_only);
        assert!(opts.suppression_prune_mode());
        assert_eq!(opts.config, None);
    }

    #[test]
    fn parse_config_with_quoted_value() {
        let opts = OxlintOpts::from_command("--config '/a b/.oxlintrc.json'");

        assert_eq!(opts.config, Some(PathBuf::from("/a b/.oxlintrc.json")));
    }

    #[test]
    fn parse_config_with_inline_value() {
        let opts = OxlintOpts::from_command("--config=/x/.oxlintrc.json");

        assert_eq!(opts.config, Some(PathBuf::from("/x/.oxlintrc.json")));
    }

    #[test]
    fn parse_config_with_flags() {
        let opts = OxlintOpts::from_command("--config /y.json --fix");

        assert_eq!(opts.config, Some(PathBuf::from("/y.json")));
        assert!(opts.fix);
    }

    #[test]
    fn parse_config_treats_next_flag_as_value() {
        let opts = OxlintOpts::from_command("--config --fix --quiet");

        assert_eq!(opts.config, Some(PathBuf::from("--fix")));
        assert!(opts.fix);
        assert!(opts.quiet);
    }

    #[test]
    fn parse_empty_config_value_keeps_config_none() {
        let opts = OxlintOpts::from_command("--config \"\" --fix");

        assert_eq!(opts.config, None);
        assert!(opts.fix);
    }

    #[test]
    fn parse_from_request_uses_command_config() {
        let req = WorkerRequest::new("job", "lint --config ./command.oxlintrc.json");
        let opts = OxlintOpts::from_request(&req);

        assert_eq!(opts.config, Some(PathBuf::from("./command.oxlintrc.json")));
    }

    #[test]
    fn parse_from_request_uses_env_config() {
        let mut env = HashMap::new();
        env.insert(
            "OXLINT_OPTS".to_owned(),
            "--config './cfg dir/.oxlintrc.json'".to_owned(),
        );
        let req = WorkerRequest::new("job", "lint").with_env(env);
        let opts = OxlintOpts::from_request(&req);

        assert_eq!(opts.config, Some(PathBuf::from("./cfg dir/.oxlintrc.json")));
    }

    #[test]
    fn parse_from_request_prefers_command_config_over_env() {
        let mut env = HashMap::new();
        env.insert(
            "OXLINT_OPTS".to_owned(),
            "--config ./env.oxlintrc.json --quiet".to_owned(),
        );
        let req =
            WorkerRequest::new("job", "lint --config ./command.oxlintrc.json --fix").with_env(env);
        let opts = OxlintOpts::from_request(&req);

        assert_eq!(opts.config, Some(PathBuf::from("./command.oxlintrc.json")));
        assert!(opts.fix);
        assert!(opts.quiet);
    }

    #[test]
    fn parse_from_request_merges_boolean_flags_from_command_and_env() {
        let mut env = HashMap::new();
        env.insert("OXLINT_OPTS".to_owned(), "--quiet --type-aware".to_owned());
        let req = WorkerRequest::new("job", "lint --fix --type-check").with_env(env);
        let opts = OxlintOpts::from_request(&req);

        assert!(opts.fix);
        assert!(opts.quiet);
        assert!(opts.type_aware);
        assert!(opts.type_check);
    }

    #[test]
    fn parse_from_command_matches_request_parser() {
        let opts = OxlintOpts::from_command("--fix --config ../cfg/.oxlintrc.jsonc");

        assert!(opts.fix);
        assert_eq!(opts.config, Some(PathBuf::from("../cfg/.oxlintrc.jsonc")));
    }
}
