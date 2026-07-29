//! Path-glob semantics shared by every part of luchta that matches file paths.
//!
//! One definition lives here so `inputs`, `outputs`, workspace discovery, watch
//! patterns, and the file-exists filter cannot drift apart. Two settings differ
//! from globset's defaults:
//!
//! - `literal_separator` keeps `*` and `?` inside a single directory level.
//!   `.gitignore`, Turborepo, and lage all behave this way; globset's default
//!   would let `src/*.ts` match `src/deep/a.ts`.
//! - `backslash_escape` is forced on so `\!` and `\*` escape identically on
//!   every platform. globset disables it on Windows because `\` is a path
//!   separator there, but luchta patterns are always written with `/`.
//!
//! Globs that match package or task *names* rather than paths deliberately do
//! not use this module — `@scope/pkg` contains a `/`, so `-p '*'` must be able
//! to cross it.

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Error produced while compiling a pattern.
pub type GlobError = globset::Error;

/// Compiles one pattern with luchta's path-glob semantics.
pub fn build_path_glob(pattern: &str) -> Result<Glob, GlobError> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
}

/// Builds a [`GlobSet`] from patterns.
pub fn build_path_globset<S: AsRef<str>>(patterns: &[S]) -> Result<GlobSet, GlobError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(build_path_glob(pattern.as_ref())?);
    }
    builder.build()
}

/// Splits a leading `!` off a pattern.
///
/// Returns `(true, body)` for a negated pattern and `(false, pattern)`
/// otherwise. A leading `\!` is an escaped literal `!` rather than negation;
/// the backslash is left in place for globset (or [`unescape_literal`]) to
/// consume.
pub fn split_negation(pattern: &str) -> (bool, &str) {
    match pattern.strip_prefix('!') {
        Some(body) => (true, body),
        None => (false, pattern),
    }
}

/// Returns true when `pattern` excludes rather than includes.
pub fn is_negated(pattern: &str) -> bool {
    split_negation(pattern).0
}

/// Removes glob escape backslashes from a pattern that contains no wildcards.
///
/// Literal (non-glob) patterns bypass glob compilation and are used directly as
/// paths, so an escape like `\!important.txt` has to be unescaped by hand or it
/// would be looked up with the backslash still in the filename.
pub fn unescape_literal(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(escaped) = chars.next() {
                out.push(escaped);
                continue;
            }
            // Trailing lone backslash: keep it rather than silently dropping.
            out.push(ch);
        } else {
            out.push(ch);
        }
    }
    out
}

/// Matches paths against include patterns minus `!`-negated exclude patterns.
///
/// A path matches when at least one include matches it and no exclude does.
/// Order does not matter and negations always win, which is how Turborepo
/// describes them and how a config file's pattern list reads. `.gitignore`
/// differs here: there the last matching line wins, so a later rule can
/// re-include something an earlier rule excluded.
#[derive(Debug, Clone)]
pub struct PathMatcher {
    includes: GlobSet,
    excludes: GlobSet,
}

impl PathMatcher {
    /// Compiles a pattern list, routing negated patterns into the exclude set.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self, GlobError> {
        let mut includes = GlobSetBuilder::new();
        let mut excludes = GlobSetBuilder::new();

        for pattern in patterns {
            let (negated, body) = split_negation(pattern.as_ref());
            let glob = build_path_glob(body)?;
            if negated {
                excludes.add(glob);
            } else {
                includes.add(glob);
            }
        }

        Ok(Self {
            includes: includes.build()?,
            excludes: excludes.build()?,
        })
    }

    /// Returns true when `path` is included by some pattern and excluded by none.
    pub fn is_match(&self, path: impl AsRef<std::path::Path>) -> bool {
        let path = path.as_ref();
        self.includes.is_match(path) && !self.excludes.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_matcher_applies_negation() {
        let matcher = PathMatcher::new(&["src/**/*.ts", "!src/**/*.test.ts"]).expect("build");

        assert!(matcher.is_match("src/a.ts"));
        assert!(!matcher.is_match("src/a.test.ts"));
    }

    #[test]
    fn path_matcher_negation_wins_regardless_of_order() {
        let reversed = PathMatcher::new(&["!src/**/*.test.ts", "src/**/*.ts"]).expect("build");

        assert!(reversed.is_match("src/a.ts"));
        assert!(
            !reversed.is_match("src/a.test.ts"),
            "unlike .gitignore, a later include must not re-include an exclusion"
        );
    }

    #[test]
    fn path_matcher_without_includes_matches_nothing() {
        let matcher = PathMatcher::new(&["!src/**"]).expect("build");

        assert!(!matcher.is_match("src/a.ts"));
        assert!(!matcher.is_match("other/a.ts"));
    }

    #[test]
    fn split_negation_detects_leading_bang() {
        assert_eq!(split_negation("!src/*.ts"), (true, "src/*.ts"));
        assert_eq!(split_negation("src/*.ts"), (false, "src/*.ts"));
    }

    #[test]
    fn split_negation_leaves_escaped_bang_alone() {
        assert_eq!(
            split_negation(r"\!important.txt"),
            (false, r"\!important.txt")
        );
    }

    #[test]
    fn unescape_literal_strips_escape_backslashes() {
        assert_eq!(unescape_literal(r"\!important.txt"), "!important.txt");
        assert_eq!(unescape_literal("plain.txt"), "plain.txt");
    }

    #[test]
    fn single_star_stays_within_one_directory_level() {
        let set = build_path_globset(&["src/*.ts"]).expect("build globset");

        assert!(set.is_match("src/a.ts"));
        assert!(!set.is_match("src/deep/a.ts"));
    }

    #[test]
    fn double_star_crosses_directory_levels() {
        let set = build_path_globset(&["src/**/*.ts"]).expect("build globset");

        assert!(set.is_match("src/a.ts"));
        assert!(set.is_match("src/deep/nested/a.ts"));
    }

    #[test]
    fn backslash_escapes_metacharacters() {
        let set = build_path_globset(&[r"src/\*.ts"]).expect("build globset");

        assert!(set.is_match("src/*.ts"));
        assert!(!set.is_match("src/a.ts"));
    }
}
