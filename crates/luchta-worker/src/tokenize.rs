pub fn tokenize_command(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;

    for ch in raw.chars() {
        match quote {
            Some(active_quote) if ch == active_quote => quote = None,
            Some(_) => {
                current.push(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                started = true;
            }
            None => {
                current.push(ch);
                started = true;
            }
        }
    }

    if started {
        tokens.push(current);
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::tokenize_command;

    #[test]
    fn respects_quoted_segments() {
        assert_eq!(
            tokenize_command("--config '/a b/.oxlintrc.json' --fix \"two words\""),
            vec!["--config", "/a b/.oxlintrc.json", "--fix", "two words",]
        );
    }

    #[test]
    fn keeps_rest_of_unmatched_quote_as_one_token() {
        assert_eq!(
            tokenize_command("--config '/a b/.oxlintrc.json --fix"),
            vec!["--config", "/a b/.oxlintrc.json --fix"]
        );
    }

    #[test]
    fn preserves_empty_quoted_tokens() {
        assert_eq!(
            tokenize_command("--config \"\" --fix"),
            vec!["--config", "", "--fix"]
        );
    }
}
