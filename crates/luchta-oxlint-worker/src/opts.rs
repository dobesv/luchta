#![cfg(feature = "oxc")]

use std::collections::HashSet;

use luchta_worker::WorkerRequest;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OxlintOpts {
    pub fix: bool,
    pub suppress_all: bool,
    pub prune_suppressions: bool,
    pub quiet: bool,
    pub type_aware: bool,
    pub type_check: bool,
    pub type_check_only: bool,
}

impl OxlintOpts {
    pub fn from_request(req: &WorkerRequest) -> Self {
        let raw = req
            .env
            .get("OXLINT_OPTS")
            .map(String::as_str)
            .unwrap_or_default();
        Self::parse(raw)
    }

    pub fn parse(raw: &str) -> Self {
        let tokens: HashSet<&str> = raw.split_whitespace().collect();
        Self {
            fix: tokens.contains("--fix"),
            suppress_all: tokens.contains("--suppress-all"),
            prune_suppressions: tokens.contains("--prune-suppressions"),
            quiet: tokens.contains("--quiet") || tokens.contains("--no-warnings"),
            type_aware: tokens.contains("--type-aware"),
            type_check: tokens.contains("--type-check"),
            type_check_only: tokens.contains("--type-check-only"),
        }
    }

    pub fn suppression_prune_mode(&self) -> bool {
        self.prune_suppressions || self.fix
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use luchta_worker::WorkerRequest;

    use super::OxlintOpts;

    #[test]
    fn parse_empty_opts() {
        assert_eq!(OxlintOpts::parse(""), OxlintOpts::default());
    }

    #[test]
    fn parse_supported_flags() {
        let opts = OxlintOpts::parse(
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
    }

    #[test]
    fn parse_from_request_env() {
        let mut env = HashMap::new();
        env.insert(
            "OXLINT_OPTS".to_owned(),
            "--fix --quiet --type-aware --type-check".to_owned(),
        );
        let req = WorkerRequest::new("job", "lint").with_env(env);
        let opts = OxlintOpts::from_request(&req);
        assert!(opts.fix);
        assert!(opts.quiet);
        assert!(opts.type_aware);
        assert!(opts.type_check);
        assert!(!opts.suppress_all);
    }
}
