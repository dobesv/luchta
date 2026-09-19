/// Build the argv (after the program) for running a package.json script the
/// way Yarn 6 does: `bash -c "<body> <escaped extra args>" yarn-script`.
pub fn build_bash_args(body: &str, extra_args: &[String]) -> Vec<String> {
    let mut script = body.to_owned();
    for arg in extra_args {
        script.push(' ');
        script.push_str(&bash_escape(arg));
    }
    vec!["-c".to_owned(), script, "yarn-script".to_owned()]
}

/// Single-quote an argument for POSIX shells. Empty strings become `''`.
pub fn bash_escape(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r#"'"'"'"#))
}

#[cfg(test)]
mod tests {
    use super::{bash_escape, build_bash_args};

    #[test]
    fn plain_body_without_args() {
        assert_eq!(
            build_bash_args("tsc -b", &[]),
            vec![
                "-c".to_owned(),
                "tsc -b".to_owned(),
                "yarn-script".to_owned()
            ]
        );
    }

    #[test]
    fn extra_args_are_escaped_and_appended() {
        let args = vec![
            "--watch".to_owned(),
            "src/a b.ts".to_owned(),
            "it's".to_owned(),
        ];
        assert_eq!(
            build_bash_args("jest", &args)[1],
            r#"jest '--watch' 'src/a b.ts' 'it'"'"'s'"#
        );
    }

    #[test]
    fn empty_arg_is_preserved() {
        assert_eq!(bash_escape(""), "''");
    }

    #[test]
    fn simple_arg_is_still_quoted() {
        assert_eq!(bash_escape("abc"), "'abc'");
    }
}
