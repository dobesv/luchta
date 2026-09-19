use std::path::Path;

/// Rewrite `NODE_OPTIONS` the way `@yarnpkg/plugin-pnp` does in its
/// `setupScriptEnvironment` hook: drop any pnp flags already present, then put
/// yarn's `--require`/`--experimental-loader` first and the remainder after.
pub fn merge_node_options(
    inherited: Option<&str>,
    pnp_cjs: &Path,
    pnp_loader: Option<&Path>,
) -> Option<String> {
    let remaining = strip_pnp_flags(inherited.unwrap_or(""));

    let mut yarn_flags = format!(
        "--require {}",
        quote_if_whitespace(&pnp_cjs.to_string_lossy())
    );
    if let Some(loader) = pnp_loader {
        yarn_flags.push_str(" --experimental-loader ");
        yarn_flags.push_str(&file_url(loader));
    }

    let merged = if remaining.is_empty() {
        yarn_flags
    } else {
        format!("{yarn_flags} {remaining}")
    };
    Some(merged)
}

/// Remove `--require <x>.pnp.cjs|.pnp.js`, `--experimental-loader <x>.pnp.loader.mjs`
/// and `--experimental-package-map <x>` (either `=value` or space separated)
/// from a NODE_OPTIONS string, collapsing whitespace like yarn's regex
/// replacement does.
fn strip_pnp_flags(value: &str) -> String {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let next = tokens.get(index + 1).copied();
        match token {
            "--require" if next.is_some_and(is_pnp_cjs) => index += 2,
            "--experimental-loader" if next.is_some_and(is_pnp_loader) => index += 2,
            "--experimental-package-map" if next.is_some() => index += 2,
            _ if token.starts_with("--experimental-package-map=") => index += 1,
            _ => {
                kept.push(token);
                index += 1;
            }
        }
    }
    kept.join(" ")
}

fn is_pnp_cjs(token: &str) -> bool {
    token.ends_with(".pnp.cjs") || token.ends_with(".pnp.js")
}

fn is_pnp_loader(token: &str) -> bool {
    token.ends_with(".pnp.loader.mjs")
}

fn quote_if_whitespace(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        serde_json::to_string(path).expect("string is always serializable")
    } else {
        path.to_owned()
    }
}

/// Mirror Node's `pathToFileURL().href` for an absolute POSIX path: percent
/// encode everything outside the unreserved set plus `/`.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~');
        if keep {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::merge_node_options;

    const CJS: &str = "/repo/.pnp.cjs";
    const LOADER: &str = "/repo/.pnp.loader.mjs";

    #[test]
    fn empty_inherited_yields_only_require() {
        assert_eq!(
            merge_node_options(None, Path::new(CJS), None),
            Some("--require /repo/.pnp.cjs".to_owned())
        );
    }

    #[test]
    fn loader_is_appended_as_file_url() {
        assert_eq!(
            merge_node_options(None, Path::new(CJS), Some(Path::new(LOADER))),
            Some(
                "--require /repo/.pnp.cjs --experimental-loader file:///repo/.pnp.loader.mjs"
                    .to_owned()
            )
        );
    }

    #[test]
    fn inherited_flags_follow_yarn_flags() {
        assert_eq!(
            merge_node_options(Some("--max-old-space-size=4096"), Path::new(CJS), None),
            Some("--require /repo/.pnp.cjs --max-old-space-size=4096".to_owned())
        );
    }

    #[test]
    fn stale_pnp_flags_are_stripped_from_inherited_value() {
        let inherited = "--require /old/.pnp.cjs --experimental-loader file:///old/.pnp.loader.mjs --no-warnings";
        assert_eq!(
            merge_node_options(Some(inherited), Path::new(CJS), None),
            Some("--require /repo/.pnp.cjs --no-warnings".to_owned())
        );
    }

    #[test]
    fn package_map_flag_is_stripped() {
        assert_eq!(
            merge_node_options(
                Some("--experimental-package-map=/x/.package-map.json --trace-warnings"),
                Path::new(CJS),
                None
            ),
            Some("--require /repo/.pnp.cjs --trace-warnings".to_owned())
        );
    }

    #[test]
    fn path_with_whitespace_is_json_quoted() {
        assert_eq!(
            merge_node_options(None, Path::new("/my repo/.pnp.cjs"), None),
            Some("--require \"/my repo/.pnp.cjs\"".to_owned())
        );
    }

    #[test]
    fn loader_url_percent_encodes_spaces() {
        assert_eq!(
            merge_node_options(
                None,
                Path::new("/repo/.pnp.cjs"),
                Some(Path::new("/my repo/.pnp.loader.mjs"))
            ),
            Some(
                "--require /repo/.pnp.cjs --experimental-loader file:///my%20repo/.pnp.loader.mjs"
                    .to_owned()
            )
        );
    }
}
