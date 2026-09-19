use std::collections::HashMap;

/// Parse a dotenv-style file the way yarn's `injectEnvironmentFiles` does:
/// `KEY=value` lines, optional `export`, `#` comments, double quotes with
/// `$VAR`/`${VAR}` expansion, single quotes taken literally. Expansion sees
/// `base` plus earlier lines of the same file.
pub fn parse_env_file(contents: &str, base: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut scope: HashMap<String, String> = base.clone();
    let mut parsed = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let resolved =
            if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
                inner.to_owned()
            } else if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                expand(inner, &scope)
            } else {
                expand(value, &scope)
            };
        scope.insert(key.to_owned(), resolved.clone());
        parsed.push((key.to_owned(), resolved));
    }
    parsed
}

fn expand(value: &str, scope: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&next) = chars.peek() {
            if next.is_ascii_alphanumeric() || next == '_' {
                name.push(next);
                chars.next();
            } else {
                break;
            }
        }
        let closed = if braced {
            if chars.peek() == Some(&'}') {
                chars.next();
                true
            } else {
                false
            }
        } else {
            true
        };
        if braced && !closed {
            // No closing brace: the whole `${name` is malformed, pass it through literally.
            out.push('$');
            out.push('{');
            out.push_str(&name);
        } else if name.is_empty() {
            // `$` with nothing name-like following it (bare `$`, or `${}`): literal.
            out.push('$');
            if braced {
                out.push('{');
                out.push('}');
            }
        } else if let Some(found) = scope.get(&name) {
            out.push_str(found);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::parse_env_file;

    #[test]
    fn parses_assignments_comments_quotes_and_expansion() {
        let base: HashMap<String, String> = [("HOME".to_owned(), "/home/me".to_owned())].into();
        let parsed = parse_env_file(
            "# comment\nFOO=bar\nexport QUOTED=\"a b\"\nSINGLE='raw $HOME'\nEXPANDED=${HOME}/x\nBARE=$HOME\n\nEMPTY=\n",
            &base,
        );
        assert_eq!(
            parsed,
            vec![
                ("FOO".to_owned(), "bar".to_owned()),
                ("QUOTED".to_owned(), "a b".to_owned()),
                ("SINGLE".to_owned(), "raw $HOME".to_owned()),
                ("EXPANDED".to_owned(), "/home/me/x".to_owned()),
                ("BARE".to_owned(), "/home/me".to_owned()),
                ("EMPTY".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn later_lines_see_earlier_definitions() {
        let parsed = parse_env_file("A=1\nB=${A}2\n", &HashMap::new());
        assert_eq!(parsed[1], ("B".to_owned(), "12".to_owned()));
    }

    #[test]
    fn malformed_patterns_pass_through_literally() {
        let base: HashMap<String, String> = [("FOO".to_owned(), "1".to_owned())].into();
        assert_eq!(
            parse_env_file("A=${}rest\n", &base),
            vec![("A".to_owned(), "${}rest".to_owned())]
        );
        assert_eq!(
            parse_env_file("B=${FOO\n", &base),
            vec![("B".to_owned(), "${FOO".to_owned())]
        );
        assert_eq!(
            parse_env_file("C=x$\n", &base),
            vec![("C".to_owned(), "x$".to_owned())]
        );
        assert_eq!(
            parse_env_file("D=$FOO$\n", &base),
            vec![("D".to_owned(), "1$".to_owned())]
        );
    }
}
