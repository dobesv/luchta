//! Reads rustqual's `ai-json` report and works out which findings a change
//! added or made worse.
//!
//! A finding is matched across the two trees by its file, category, function
//! and a subject taken from its detail text, never by line number, because an
//! edit near the top of a file moves every finding below it. Both reports
//! come from the same pinned rustqual, so a detail format that changes in a
//! later release still matches itself; only the metric parsing below can fall
//! behind, and then a finding is compared by presence alone.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::Deserialize;

/// One finding from rustqual's report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: u64,
    pub category: String,
    pub function: String,
    pub detail: String,
}

/// A finding in the change that the base doesn't have, or has with a smaller
/// metric (`was`).
#[derive(Debug, PartialEq, Eq)]
pub struct Regression {
    pub finding: Finding,
    pub was: Option<u64>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    pub regressions: Vec<Regression>,
    /// Base findings the change no longer has.
    pub resolved: usize,
}

#[derive(Deserialize)]
struct Report {
    findings_by_file: BTreeMap<String, Vec<RawFinding>>,
}

#[derive(Deserialize)]
struct RawFinding {
    category: String,
    #[serde(default)]
    detail: String,
    #[serde(default, rename = "fn")]
    function: String,
    #[serde(default)]
    line: u64,
}

pub fn parse_report(json: &[u8]) -> Result<Vec<Finding>, String> {
    let report: Report = serde_json::from_slice(json)
        .map_err(|error| format!("failed to parse rustqual's ai-json report: {error}"))?;
    Ok(report
        .findings_by_file
        .into_iter()
        .flat_map(|(file, findings)| {
            findings.into_iter().map(move |raw| Finding {
                file: file.clone(),
                line: raw.line,
                category: raw.category,
                function: raw.function,
                detail: raw.detail,
            })
        })
        .collect())
}

/// Compares the change's findings with the base's. `renames` maps a base
/// path to the path the change moved it to.
pub fn compare(
    base: &[Finding],
    head: &[Finding],
    renames: &BTreeMap<String, String>,
) -> Comparison {
    let mut before = group(base.iter().map(|finding| {
        let file = renames.get(&finding.file).unwrap_or(&finding.file);
        (Identity::new(file, finding), metric(finding))
    }));
    let after = group(
        head.iter()
            .map(|finding| (Identity::new(&finding.file, finding), finding)),
    );
    let resolved = count_resolved(&before, &after);
    let mut regressions: Vec<Regression> = after
        .into_iter()
        .flat_map(|(identity, findings)| {
            pair(findings, before.remove(&identity).unwrap_or_default())
        })
        .collect();
    regressions
        .sort_by(|a, b| (&a.finding.file, a.finding.line).cmp(&(&b.finding.file, b.finding.line)));
    Comparison {
        regressions,
        resolved,
    }
}

/// The base's findings with no counterpart in the change: for each identity,
/// how many more the base has than the change.
fn count_resolved<T, U>(
    before: &BTreeMap<Identity, Vec<T>>,
    after: &BTreeMap<Identity, Vec<U>>,
) -> usize {
    before
        .iter()
        .map(|(identity, old)| {
            old.len()
                .saturating_sub(after.get(identity).map_or(0, Vec::len))
        })
        .sum()
}

/// Collects values under their identity, keeping repeats.
fn group<T>(entries: impl Iterator<Item = (Identity, T)>) -> BTreeMap<Identity, Vec<T>> {
    entries.fold(BTreeMap::new(), |mut groups, (identity, value)| {
        groups.entry(identity).or_insert_with(Vec::new).push(value);
        groups
    })
}

/// Pairs the change's findings of one identity with the base's, worst with
/// worst, so a function that grew is reported once as worse rather than as
/// new. What is left unpaired on the change's side is new.
fn pair(mut findings: Vec<&Finding>, mut metrics: Vec<Option<u64>>) -> Vec<Regression> {
    findings.sort_by_key(|finding| (Reverse(metric(finding)), finding.line));
    metrics.sort_by_key(|metric| Reverse(*metric));
    findings
        .into_iter()
        .enumerate()
        .filter_map(|(index, finding)| match metrics.get(index) {
            None => Some(Regression {
                finding: finding.clone(),
                was: None,
            }),
            Some(&Some(was)) if metric(finding).is_some_and(|now| now > was) => Some(Regression {
                finding: finding.clone(),
                was: Some(was),
            }),
            Some(_) => None,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    file: String,
    category: String,
    function: String,
    subject: String,
}

impl Identity {
    fn new(file: &str, finding: &Finding) -> Self {
        Self {
            file: file.to_owned(),
            category: finding.category.clone(),
            function: finding.function.clone(),
            subject: subject(&finding.category, &finding.detail),
        }
    }
}

/// The part of a finding's detail text that tells it apart from other
/// findings of the same category in the same function.
fn subject(category: &str, detail: &str) -> String {
    match category {
        // The function identifies these. Their detail is a measurement, or
        // for `violation` a list of line numbers.
        "long_function"
        | "cognitive_complexity"
        | "cyclomatic_complexity"
        | "nesting_depth"
        | "srp_params"
        | "error_handling"
        | "unsafe_block"
        | "violation" => String::new(),
        // Counted per file. A file over the length limit may keep growing;
        // only crossing the limit, or splitting into more independent
        // clusters than allowed, is new.
        "srp_module" | "fragment" => String::new(),
        // "exact with a.rs:66, b.rs:39": the copies move, the kind doesn't.
        "duplicate" => first_word(detail).to_owned(),
        // "Runner: LCOM4=3, methods=16", and "BP-009: 2 constructions of
        // `TaskVersion` ..." where a function with several repeated types
        // gets one finding naming any of them, a different one from run to
        // run.
        "srp_struct" | "boilerplate" => detail.split(':').next().unwrap_or_default().to_owned(),
        _ => strip_line_numbers(detail),
    }
}

/// The measurement a finding's detail reports, where a bigger number is
/// worse.
fn metric(finding: &Finding) -> Option<u64> {
    let detail = finding.detail.as_str();
    let after = |marker: &str| detail.rfind(marker).map(|at| &detail[at + marker.len()..]);
    let measured = match finding.category.as_str() {
        // "62 (max 60)"
        "long_function" | "cognitive_complexity" | "cyclomatic_complexity" | "nesting_depth" => {
            Some(detail)
        }
        // "resolve_single_request: 7 (max 5)"
        "srp_params" => after(": "),
        // "error-handling smells 2 in task_spec_hash"
        "error_handling" => after("smells "),
        // "unsafe blocks 2 in try_build"
        "unsafe_block" => after("blocks "),
        // "Runner: LCOM4=3, methods=16"
        "srp_struct" => after("LCOM4="),
        _ => None,
    };
    measured.and_then(leading_number)
}

fn leading_number(text: &str) -> Option<u64> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    text[..end].parse().ok()
}

fn first_word(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or_default()
}

/// Drops the `:<line>` after each `.rs` path a detail mentions.
fn strip_line_numbers(detail: &str) -> String {
    let mut pieces = detail.split(".rs:");
    let first = pieces.next().unwrap_or_default().to_owned();
    pieces.fold(first, |mut stripped, piece| {
        let after_line = piece.trim_start_matches(|c: char| c.is_ascii_digit());
        // A colon with no line number after it belongs to the text.
        let separator = if after_line.len() == piece.len() {
            ".rs:"
        } else {
            ".rs"
        };
        stripped.push_str(separator);
        stripped.push_str(after_line);
        stripped
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(file: &str, line: u64, category: &str, function: &str, detail: &str) -> Finding {
        Finding {
            file: file.to_owned(),
            line,
            category: category.to_owned(),
            function: function.to_owned(),
            detail: detail.to_owned(),
        }
    }

    fn long_fn(line: u64, lines: u64) -> Finding {
        finding(
            "src/a.rs",
            line,
            "long_function",
            "run",
            &format!("{lines} (max 60)"),
        )
    }

    fn no_renames() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn parses_findings_by_file() {
        let json = br#"{"coverage":"call-graph-only","findings":2,"version":"1.8.3",
            "findings_by_file":{"src/a.rs":[
                {"category":"long_function","detail":"62 (max 60)","fn":"run","kind":"","line":31},
                {"category":"magic_number","detail":"magic number 4 in run","fn":"","kind":"","line":40}
            ]}}"#;

        let findings = parse_report(json).unwrap();

        assert_eq!(
            findings,
            vec![
                finding("src/a.rs", 31, "long_function", "run", "62 (max 60)"),
                finding("src/a.rs", 40, "magic_number", "", "magic number 4 in run"),
            ]
        );
    }

    #[test]
    fn findings_that_only_moved_are_not_regressions() {
        let base = [long_fn(10, 62)];
        let head = [long_fn(25, 62)];

        assert_eq!(compare(&base, &head, &no_renames()), Comparison::default());
    }

    #[test]
    fn a_new_finding_is_a_regression() {
        let added = finding("src/a.rs", 40, "magic_number", "", "magic number 4 in run");

        let comparison = compare(&[], std::slice::from_ref(&added), &no_renames());

        assert_eq!(
            comparison.regressions,
            vec![Regression {
                finding: added,
                was: None
            }]
        );
    }

    #[test]
    fn a_function_that_grew_is_reported_with_its_old_length() {
        let comparison = compare(&[long_fn(10, 62)], &[long_fn(10, 70)], &no_renames());

        assert_eq!(
            comparison.regressions,
            vec![Regression {
                finding: long_fn(10, 70),
                was: Some(62)
            }]
        );
    }

    #[test]
    fn a_function_that_shrank_is_not_a_regression() {
        let comparison = compare(&[long_fn(10, 70)], &[long_fn(10, 62)], &no_renames());

        assert!(comparison.regressions.is_empty());
    }

    #[test]
    fn a_long_file_may_grow_but_not_cross_the_limit() {
        let module = |file: &str, lines: u64| {
            finding(
                file,
                1,
                "srp_module",
                "",
                &format!("{file}: {lines} lines (max 300)"),
            )
        };
        let base = [module("src/long.rs", 400)];
        let head = [module("src/long.rs", 450), module("src/short.rs", 301)];

        let comparison = compare(&base, &head, &no_renames());

        assert_eq!(
            comparison.regressions,
            vec![Regression {
                finding: module("src/short.rs", 301),
                was: None
            }]
        );
    }

    #[test]
    fn a_second_identical_finding_is_a_regression() {
        let magic = |line| {
            finding(
                "src/a.rs",
                line,
                "magic_number",
                "",
                "magic number 4 in run",
            )
        };

        let comparison = compare(&[magic(10)], &[magic(10), magic(20)], &no_renames());

        assert_eq!(
            comparison.regressions,
            vec![Regression {
                finding: magic(20),
                was: None
            }]
        );
    }

    #[test]
    fn findings_follow_a_renamed_file() {
        let at = |file: &str| finding(file, 5, "violation", "run", "logic + calls (logic lines 7)");
        let renames = BTreeMap::from([("src/old.rs".to_owned(), "src/new.rs".to_owned())]);

        let comparison = compare(&[at("src/old.rs")], &[at("src/new.rs")], &renames);

        assert_eq!(comparison, Comparison::default());
    }

    #[test]
    fn findings_the_change_removed_count_as_resolved() {
        let base = [long_fn(10, 62), long_fn(80, 90)];

        let comparison = compare(&base, &[], &no_renames());

        assert_eq!(comparison.resolved, 2);
    }

    #[test]
    fn a_duplicate_whose_copies_moved_is_unchanged() {
        let base = [finding(
            "src/a.rs",
            3,
            "duplicate",
            "run",
            "exact with src/b.rs:66",
        )];
        let head = [finding(
            "src/a.rs",
            9,
            "duplicate",
            "run",
            "exact with src/b.rs:80, src/c.rs:12",
        )];

        assert_eq!(compare(&base, &head, &no_renames()), Comparison::default());
    }

    #[test]
    fn boilerplate_is_matched_by_its_rule_not_the_type_it_names() {
        let base = [finding(
            "src/a.rs",
            9,
            "boilerplate",
            "run",
            "BP-009: 2 constructions of `TaskChanges` with overlapping fields",
        )];
        let head = [finding(
            "src/a.rs",
            9,
            "boilerplate",
            "run",
            "BP-009: 2 constructions of `TaskVersion` with overlapping fields",
        )];

        assert_eq!(compare(&base, &head, &no_renames()), Comparison::default());
    }

    #[test]
    fn metrics_are_read_from_each_category_s_detail() {
        let cases = [
            ("cognitive_complexity", "18 (max 15)", Some(18)),
            ("srp_params", "parse_v2: 6 (max 5)", Some(6)),
            (
                "error_handling",
                "error-handling smells 2 in Verifier::keys",
                Some(2),
            ),
            ("unsafe_block", "unsafe blocks 3 in reap", Some(3)),
            ("srp_struct", "Runner: LCOM4=3, methods=16", Some(3)),
            ("magic_number", "magic number 4 in run", None),
        ];

        for (category, detail, expected) in cases {
            let measured = metric(&finding("src/a.rs", 1, category, "f", detail));
            assert_eq!(measured, expected, "{category}: {detail}");
        }
    }

    #[test]
    fn line_numbers_after_paths_are_dropped() {
        assert_eq!(
            strip_line_numbers("3 stmts also in src/a.rs:105 and src/b.rs:7"),
            "3 stmts also in src/a.rs and src/b.rs"
        );
        assert_eq!(
            strip_line_numbers("see src/a.rs: here"),
            "see src/a.rs: here"
        );
    }
}
