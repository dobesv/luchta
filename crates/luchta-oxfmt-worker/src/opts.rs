#[cfg(feature = "oxc")]
use luchta_worker::{tokenize::tokenize_command, WorkerRequest};

#[cfg(feature = "oxc")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OxfmtOpts {
    pub(crate) check: bool,
    pub(crate) fix: bool,
}

#[cfg(feature = "oxc")]
impl OxfmtOpts {
    pub(crate) fn from_request(req: &WorkerRequest) -> Self {
        let mut tokens = tokenize_command(&req.command);
        if let Some(raw) = req.env.get("OXFMT_OPTS") {
            tokens.extend(tokenize_command(raw));
        }
        Self::parse_tokens(&tokens)
    }

    fn parse_tokens(tokens: &[String]) -> Self {
        let has_check = tokens.iter().any(|token| token == "--check");
        let has_fix = tokens.iter().any(|token| token == "--fix");

        Self {
            check: has_check,
            fix: has_fix,
        }
    }
}

#[cfg(all(test, feature = "oxc"))]
mod tests {
    use std::collections::HashMap;

    use luchta_worker::WorkerRequest;

    use super::OxfmtOpts;

    #[test]
    fn opts_recognize_check_flag_from_env() {
        let mut env = HashMap::new();
        env.insert("OXFMT_OPTS".to_owned(), "--check --unknown".to_owned());
        let req = WorkerRequest::new("pkg#format", "format").with_env(env);
        assert_eq!(
            OxfmtOpts::from_request(&req),
            OxfmtOpts {
                check: true,
                fix: false,
            }
        );
    }

    #[test]
    fn opts_recognize_check_flag_from_command() {
        let req = WorkerRequest::new("pkg#format", "format --check");
        assert_eq!(
            OxfmtOpts::from_request(&req),
            OxfmtOpts {
                check: true,
                fix: false,
            }
        );
    }

    #[test]
    fn opts_recognize_fix_flag_from_command_as_write_mode() {
        let req = WorkerRequest::new("pkg#format", "format --fix");
        assert_eq!(
            OxfmtOpts::from_request(&req),
            OxfmtOpts {
                check: false,
                fix: true,
            }
        );
    }

    #[test]
    fn opts_prefer_check_when_fix_and_check_are_both_present() {
        let req = WorkerRequest::new("pkg#format", "format --fix --check");
        assert_eq!(
            OxfmtOpts::from_request(&req),
            OxfmtOpts {
                check: true,
                fix: true,
            }
        );
    }
}
