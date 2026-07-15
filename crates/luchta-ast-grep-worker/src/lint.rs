use std::path::{Path, PathBuf};

use ast_grep_config::{from_yaml_string, GlobalRules, RuleCollection, RuleConfig, Severity};
use ast_grep_core::{tree_sitter::StrDoc, NodeMatch};
use ast_grep_language::{LanguageExt, SupportLang};

use crate::config::{resolve_language, DiscoveredConfig, LanguageGlobEntry};

#[derive(Clone, Debug)]
pub struct Finding {
    pub rule_id: String,
    pub severity: Severity,
    pub message: String,
    pub relative_uri: String,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    pub findings: Vec<Finding>,
    pub fixed_files: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy)]
struct ScanContext<'a> {
    cwd: &'a Path,
    repo_root: &'a Path,
    config_dir: &'a Path,
    language_globs: &'a [LanguageGlobEntry],
}

impl<'a> ScanContext<'a> {
    fn selection_path(&self, file: &Path) -> String {
        normalize_to_forward_slashes(file.strip_prefix(self.config_dir).unwrap_or(file))
    }

    fn relative_uri(&self, file: &Path) -> String {
        luchta_worker::paths::repo_relative(file, self.repo_root)
    }

    fn resolve_language(&self, file: &Path) -> Option<SupportLang> {
        resolve_language(file, self.config_dir, self.language_globs)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileEdit {
    position: usize,
    deleted_length: usize,
    inserted_text: Vec<u8>,
    rule_id: String,
}

pub fn load_rules(rule_files: &[PathBuf]) -> Result<Vec<RuleConfig<SupportLang>>, String> {
    let mut loaded = Vec::new();
    for rule_file in rule_files {
        let yaml = std::fs::read_to_string(rule_file)
            .map_err(|error| format!("failed to read {}: {error}", rule_file.display()))?;
        let mut rules = from_yaml_string(&yaml, &GlobalRules::default())
            .map_err(|error| format!("failed to load {}: {error}", rule_file.display()))?;
        loaded.append(&mut rules);
    }
    loaded.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    Ok(loaded)
}

#[cfg(test)]
pub fn scan_files(
    cwd: &Path,
    repo_root: &Path,
    config: &DiscoveredConfig,
    files: Vec<PathBuf>,
    fix: bool,
) -> Result<ScanResult, String> {
    let rules = load_rules(&config.rule_files)?;
    if rules.is_empty() {
        eprintln!("warning: ast-grep rule set is empty; skipping scan");
        return Ok(ScanResult::default());
    }
    let context = ScanContext {
        cwd,
        repo_root,
        config_dir: &config.config_dir,
        language_globs: &config.language_globs,
    };
    scan_files_with_rules(context, rules, files, fix)
}

fn scan_files_with_rules(
    context: ScanContext<'_>,
    rules: Vec<RuleConfig<SupportLang>>,
    files: Vec<PathBuf>,
    fix: bool,
) -> Result<ScanResult, String> {
    let collection = RuleCollection::try_new(rules)
        .map_err(|error| format!("failed to build ast-grep rule collection: {error}"))?;
    let (fixed_files, warnings) = if fix {
        apply_fixes(context, &collection, &files)?
    } else {
        (Vec::new(), Vec::new())
    };
    let findings = scan_files_with_collection(context, &collection, files)?;
    Ok(ScanResult {
        findings,
        fixed_files,
        warnings,
    })
}

fn scan_files_with_collection(
    context: ScanContext<'_>,
    collection: &RuleCollection<SupportLang>,
    files: Vec<PathBuf>,
) -> Result<Vec<Finding>, String> {
    let threads = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .min(files.len().max(1));
    let chunk_size = files.len().max(1).div_ceil(threads);

    let mut findings = std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for chunk in files.chunks(chunk_size) {
            jobs.push(scope.spawn(move || {
                let mut chunk_findings = Vec::new();
                for file in chunk {
                    let mut per_file = scan_file(context, collection, file.clone())?;
                    chunk_findings.append(&mut per_file);
                }
                Ok::<Vec<Finding>, String>(chunk_findings)
            }));
        }

        let mut merged = Vec::new();
        for job in jobs {
            let chunk_findings = job
                .join()
                .map_err(|_| "ast-grep worker parallel scan thread panicked".to_owned())??;
            merged.extend(chunk_findings);
        }
        Ok::<Vec<Finding>, String>(merged)
    })?;
    findings.sort_unstable_by(finding_sort_key);
    Ok(findings)
}

fn apply_fixes(
    context: ScanContext<'_>,
    collection: &RuleCollection<SupportLang>,
    files: &[PathBuf],
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut fixed = Vec::new();
    let mut warnings = Vec::new();
    for file in files {
        let apply = apply_fixes_to_file(context, collection, file)?;
        if apply.fixed {
            fixed.push(context.relative_uri(file));
        }
        warnings.extend(apply.warnings);
    }
    fixed.sort();
    Ok((fixed, warnings))
}

struct ApplyFixesResult {
    fixed: bool,
    warnings: Vec<String>,
}

fn apply_fixes_to_file(
    context: ScanContext<'_>,
    collection: &RuleCollection<SupportLang>,
    file: &Path,
) -> Result<ApplyFixesResult, String> {
    let source = std::fs::read_to_string(file)
        .map_err(|error| format!("failed to read {}: {error}", file.display()))?;
    let Some(lang) = context.resolve_language(file) else {
        return Ok(ApplyFixesResult {
            fixed: false,
            warnings: Vec::new(),
        });
    };
    let root = lang.ast_grep(source.clone());
    let selection_path = context.selection_path(file);
    let applicable_rules = collection.get_rule_from_lang(Path::new(&selection_path), lang);
    if applicable_rules.is_empty() {
        return Ok(ApplyFixesResult {
            fixed: false,
            warnings: Vec::new(),
        });
    }

    let candidate_edits = collect_candidate_edits(&root, applicable_rules)?;
    let (accepted_edits, warnings) = select_non_overlapping_edits(file, candidate_edits);
    if accepted_edits.is_empty() {
        return Ok(ApplyFixesResult {
            fixed: false,
            warnings,
        });
    }

    // Guard against writing outside the task root (e.g. via a symlinked source
    // file). Canonicalize both the root and the target and reject any target
    // whose real path escapes the root; no fix is applied in that case.
    ensure_within_root(context.cwd, file)?;

    let rewritten = apply_edits(source.into_bytes(), accepted_edits)?;
    write_atomically(file, rewritten.as_bytes())?;
    Ok(ApplyFixesResult {
        fixed: true,
        warnings,
    })
}

/// Rejects `file` if its canonical path is not inside the canonical `root`,
/// preventing fixes from escaping the task directory through symlinks or `..`.
fn ensure_within_root(root: &Path, file: &Path) -> Result<(), String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("failed to resolve task root {}: {error}", root.display()))?;
    let canonical_file = file
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", file.display()))?;
    if canonical_file.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(format!(
            "refusing to fix {} outside task root {}",
            canonical_file.display(),
            canonical_root.display()
        ))
    }
}

/// Writes `contents` to `path` atomically: a temporary file in the same
/// directory is fully written and then renamed over the original. The
/// temporary file is removed if any step fails.
fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "failed to write {}: file has no parent directory",
            path.display()
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        format!(
            "failed to write {}: file has no final component",
            path.display()
        )
    })?;
    let temp_path = parent.join(format!(".{}.tmp", file_name.to_string_lossy()));

    std::fs::write(&temp_path, contents).map_err(|error| {
        format!(
            "failed to write temporary file {}: {error}",
            temp_path.display()
        )
    })?;
    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("failed to write {}: {error}", path.display()));
    }
    Ok(())
}

fn collect_candidate_edits<'a>(
    root: &'a ast_grep_core::AstGrep<StrDoc<SupportLang>>,
    applicable_rules: Vec<&'a RuleConfig<SupportLang>>,
) -> Result<Vec<FileEdit>, String> {
    let mut edits = Vec::new();
    for rule in applicable_rules {
        if matches!(rule.severity, Severity::Off) {
            continue;
        }
        let fixers = rule
            .get_fixer()
            .map_err(|error| format!("failed to build fixer for {}: {error}", rule.id))?;
        if fixers.is_empty() {
            continue;
        }
        let fixer = &fixers[0];
        for matched in root.root().find_all(&rule.matcher) {
            let edit = matched.make_edit(&rule.matcher, fixer);
            edits.push(FileEdit {
                position: edit.position,
                deleted_length: edit.deleted_length,
                inserted_text: edit.inserted_text,
                rule_id: rule.id.clone(),
            });
        }
    }
    Ok(edits)
}

fn select_non_overlapping_edits(
    file: &Path,
    mut edits: Vec<FileEdit>,
) -> (Vec<FileEdit>, Vec<String>) {
    edits.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then_with(|| right.deleted_length.cmp(&left.deleted_length))
            .then_with(|| left.rule_id.cmp(&right.rule_id))
    });

    let mut accepted = Vec::new();
    let mut warnings = Vec::new();
    let mut last_end = 0usize;
    for edit in edits {
        let start = edit.position;
        let end = edit.position + edit.deleted_length;
        if !accepted.is_empty() && start < last_end {
            warnings.push(format!(
                "warning: skipped conflicting fix from rule {} for {} at byte range {}..{}",
                edit.rule_id,
                file.display(),
                start,
                end
            ));
            continue;
        }
        last_end = end;
        accepted.push(edit);
    }
    (accepted, warnings)
}

fn apply_edits(mut source: Vec<u8>, edits: Vec<FileEdit>) -> Result<String, String> {
    for edit in edits.into_iter().rev() {
        let end = edit.position + edit.deleted_length;
        source.splice(edit.position..end, edit.inserted_text);
    }
    String::from_utf8(source).map_err(|error| format!("failed to decode rewritten source: {error}"))
}

fn normalize_to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn severity_rank(severity: &Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
        Severity::Hint => 3,
        Severity::Off => 4,
    }
}

fn finding_sort_key(left: &Finding, right: &Finding) -> std::cmp::Ordering {
    left.relative_uri
        .cmp(&right.relative_uri)
        .then_with(|| left.start_line.cmp(&right.start_line))
        .then_with(|| left.start_column.cmp(&right.start_column))
        .then_with(|| severity_rank(&left.severity).cmp(&severity_rank(&right.severity)))
        .then_with(|| left.rule_id.cmp(&right.rule_id))
        .then_with(|| left.message.cmp(&right.message))
}

fn scan_file(
    context: ScanContext<'_>,
    collection: &RuleCollection<SupportLang>,
    file: PathBuf,
) -> Result<Vec<Finding>, String> {
    let source = std::fs::read_to_string(&file)
        .map_err(|error| format!("failed to read {}: {error}", file.display()))?;
    let Some(lang) = context.resolve_language(&file) else {
        return Ok(Vec::new());
    };
    let root = lang.ast_grep(source);
    let relative_uri = context.relative_uri(&file);
    let selection_path = context.selection_path(&file);
    let applicable_rules = collection.get_rule_from_lang(Path::new(&selection_path), lang);
    if applicable_rules.is_empty() {
        return Ok(Vec::new());
    }

    let mut findings = Vec::new();
    for rule in applicable_rules {
        for matched in root.root().find_all(&rule.matcher) {
            if matches!(rule.severity, Severity::Off) {
                continue;
            }
            findings.push(build_finding(rule, &matched, &relative_uri));
        }
    }
    Ok(findings)
}

fn build_finding<D>(
    rule: &RuleConfig<SupportLang>,
    matched: &NodeMatch<'_, D>,
    relative_uri: &str,
) -> Finding
where
    D: ast_grep_core::Doc,
{
    let start = matched.start_pos();
    let end = matched.end_pos();
    Finding {
        rule_id: rule.id.clone(),
        severity: rule.severity.clone(),
        message: rule.get_message(matched),
        relative_uri: relative_uri.to_owned(),
        start_line: start.line() + 1,
        start_column: start.column(matched) + 1,
        end_line: end.line() + 1,
        end_column: end.column(matched) + 1,
    }
}

pub async fn scan_files_async(
    cwd: &Path,
    repo_root: &Path,
    config: &DiscoveredConfig,
    files: Vec<PathBuf>,
    fix: bool,
) -> Result<ScanResult, String> {
    let cwd = cwd.to_path_buf();
    let repo_root = repo_root.to_path_buf();
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let rules = load_rules(&config.rule_files)?;
        if rules.is_empty() {
            return Ok(ScanResult::default());
        }
        let context = ScanContext {
            cwd: &cwd,
            repo_root: &repo_root,
            config_dir: &config.config_dir,
            language_globs: &config.language_globs,
        };
        scan_files_with_rules(context, rules, files, fix)
    })
    .await
    .map_err(|error| format!("ast-grep worker join error: {error}"))?
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use ast_grep_config::Severity;
    use tempfile::TempDir;

    use super::{
        ensure_within_root, finding_sort_key, scan_files, scan_files_with_collection,
        write_atomically, ScanContext,
    };
    use crate::config::discover_config;

    fn write_basic_rule_fixture(temp: &TempDir, rule_body: &str, source: &str) -> PathBuf {
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(temp.path().join("rules/no-console-log.yml"), rule_body).expect("rule");
        let source_path = temp.path().join("src/index.ts");
        fs::write(&source_path, source).expect("source");
        source_path
    }

    fn scan_basic_fixture(temp: &TempDir, source: PathBuf, fix: bool) -> super::ScanResult {
        let config = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        scan_files(temp.path(), temp.path(), &config, vec![source], fix).expect("scan")
    }

    #[test]
    fn ensure_within_root_accepts_file_inside_root() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let file = root.join("src/index.ts");
        fs::create_dir_all(file.parent().unwrap()).expect("dirs");
        fs::write(&file, "x").expect("write");

        assert!(ensure_within_root(root, &file).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_within_root_rejects_symlink_escaping_root() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        let target = outside.join("secret.ts");
        fs::write(&target, "secret").expect("target");
        // A source path inside the root that is really a symlink to a file
        // outside the root must be rejected.
        let link = root.join("index.ts");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let error = ensure_within_root(&root, &link).expect_err("must reject escaping symlink");
        assert!(
            error.contains("outside task root"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn write_atomically_replaces_contents_and_leaves_no_temp_file() {
        let temp = TempDir::new().expect("tempdir");
        let file = temp.path().join("out.ts");
        fs::write(&file, "old\n").expect("seed");

        write_atomically(&file, b"new contents\n").expect("write");

        assert_eq!(fs::read_to_string(&file).expect("read"), "new contents\n");
        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary file left behind");
    }

    struct FixCase {
        rule_body: &'static str,
        fix: bool,
        expected_fixed_files: &'static [&'static str],
        expected_finding_count: usize,
        expected_source: &'static str,
    }

    impl FixCase {
        fn run(&self) {
            let temp = TempDir::new().expect("tempdir");
            let source_path =
                write_basic_rule_fixture(&temp, self.rule_body, "console.log('hi');\n");
            let result = scan_basic_fixture(&temp, source_path.clone(), self.fix);
            let rewritten = fs::read_to_string(&source_path).expect("read rewritten");

            assert_eq!(result.fixed_files, self.expected_fixed_files);
            assert_eq!(result.findings.len(), self.expected_finding_count);
            assert_eq!(rewritten, self.expected_source);
        }
    }

    struct ExpectedFinding {
        rule_id: &'static str,
        message: &'static str,
        relative_uri: &'static str,
        start_line: usize,
        start_column: usize,
    }

    impl ExpectedFinding {
        fn assert_matches(&self, finding: &super::Finding) {
            let actual = (
                finding.rule_id.as_str(),
                finding.message.as_str(),
                finding.relative_uri.as_str(),
                finding.start_line,
                finding.start_column,
            );
            let expected = (
                self.rule_id,
                self.message,
                self.relative_uri,
                self.start_line,
                self.start_column,
            );
            assert_eq!(actual, expected);
            assert!(finding.end_column >= finding.start_column);
        }
    }

    #[test]
    fn scan_reports_match_details() {
        let temp = TempDir::new().expect("tempdir");
        let source = write_basic_rule_fixture(
            &temp,
            "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed\nrule:\n  pattern: console.log($$$)\n",
            "console.log('hi');\n",
        );

        let findings = scan_basic_fixture(&temp, source, false).findings;

        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        ExpectedFinding {
            rule_id: "no-console-log",
            message: "No console.log allowed",
            relative_uri: "src/index.ts",
            start_line: 1,
            start_column: 1,
        }
        .assert_matches(finding);
        assert_eq!(finding.end_line, 1);
    }

    #[test]
    fn fix_mode_behavior_matrix() {
        const ERROR_RULE: &str = "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed\nrule:\n  pattern: console.log($ARG)\nfix: logger.info($ARG)\n";
        const OFF_RULE: &str = "id: no-console-log\nlanguage: TypeScript\nseverity: off\nmessage: No console.log allowed\nrule:\n  pattern: console.log($ARG)\nfix: logger.info($ARG)\n";

        // fix on + error severity: file rewritten, no remaining findings.
        // fix off: file untouched, finding still reported.
        // fix on + severity off: neither rewritten nor reported.
        let cases = [
            FixCase {
                rule_body: ERROR_RULE,
                fix: true,
                expected_fixed_files: &["src/index.ts"],
                expected_finding_count: 0,
                expected_source: "logger.info('hi');\n",
            },
            FixCase {
                rule_body: ERROR_RULE,
                fix: false,
                expected_fixed_files: &[],
                expected_finding_count: 1,
                expected_source: "console.log('hi');\n",
            },
            FixCase {
                rule_body: OFF_RULE,
                fix: true,
                expected_fixed_files: &[],
                expected_finding_count: 0,
                expected_source: "console.log('hi');\n",
            },
        ];

        for case in &cases {
            case.run();
        }
    }

    #[test]
    fn fix_preserves_multibyte_utf8_around_edits() {
        // The fixer replaces `console.log(...)` with `logger.info(...)`; the
        // argument holds multibyte characters both inside the replaced range and
        // in the untouched trailing line. Byte-offset splicing must keep every
        // codepoint intact and yield valid UTF-8.
        let temp = TempDir::new().expect("tempdir");
        let source = write_basic_rule_fixture(
            &temp,
            "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed\nrule:\n  pattern: console.log($ARG)\nfix: logger.info($ARG)\n",
            "console.log('café ☕ 日本語');\nconst greeting = 'naïve → ✅';\n",
        );

        let result = scan_basic_fixture(&temp, source.clone(), true);
        let rewritten = fs::read_to_string(&source).expect("read rewritten");

        assert_eq!(result.fixed_files, vec!["src/index.ts"]);
        assert_eq!(
            rewritten,
            "logger.info('café ☕ 日本語');\nconst greeting = 'naïve → ✅';\n"
        );
        assert!(std::str::from_utf8(rewritten.as_bytes()).is_ok());
    }

    #[test]
    fn fix_respects_rule_scoping() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(
            temp.path().join("rules/fix-tsx-only.yml"),
            "id: fix-tsx-only\nlanguage: tsx\nseverity: error\nmessage: rewrite console\nfiles:\n  - '**/*.tsx'\nrule:\n  pattern: console.log($ARG)\nfix: logger.info($ARG)\n",
        )
        .expect("rule");
        let source = temp.path().join("src/index.ts");
        let original = "console.log('hi');\n";
        fs::write(&source, original).expect("source");

        let result = scan_basic_fixture(&temp, source.clone(), true);

        assert!(result.fixed_files.is_empty());
        assert!(result.findings.is_empty());
        assert_eq!(fs::read_to_string(&source).expect("unchanged"), original);
    }

    #[test]
    fn overlapping_edits_skip_conflicting_fix_and_keep_valid_output() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(
            temp.path().join("rules/fix-a.yml"),
            "id: fix-a\nlanguage: TypeScript\nseverity: error\nmessage: rewrite A\nrule:\n  pattern: console.log($ARG)\nfix: logger.info($ARG)\n",
        )
        .expect("rule a");
        fs::write(
            temp.path().join("rules/fix-b.yml"),
            "id: fix-b\nlanguage: TypeScript\nseverity: error\nmessage: rewrite B\nrule:\n  pattern: console.log($ARG)\nfix: debug.info($ARG)\n",
        )
        .expect("rule b");
        let source = temp.path().join("src/index.ts");
        fs::write(&source, "console.log('hi');\n").expect("source");

        let result = scan_basic_fixture(&temp, source.clone(), true);
        let rewritten = fs::read_to_string(&source).expect("rewritten");

        assert_eq!(result.fixed_files, vec!["src/index.ts"]);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("skipped conflicting fix"));
        assert!(rewritten == "logger.info('hi');\n" || rewritten == "debug.info('hi');\n");
        assert!(std::str::from_utf8(rewritten.as_bytes()).is_ok());
    }

    #[test]
    fn language_globs_allow_ts_file_to_match_tsx_rule() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(
            temp.path().join("sgconfig.yml"),
            "ruleDirs:\n  - rules\nlanguageGlobs:\n  tsx:\n    - '*.ts'\n",
        )
        .expect("config");
        let rule_file = temp.path().join("rules/no-console-log.yml");
        fs::write(
            &rule_file,
            "id: no-console-log\nlanguage: tsx\nseverity: error\nmessage: No console.log allowed\nrule:\n  pattern: console.log($$$)\n",
        )
        .expect("rule");
        let source = temp.path().join("src/index.ts");
        fs::write(&source, "console.log('hi');\n").expect("source");

        let config = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        let findings = scan_files(
            temp.path(),
            temp.path(),
            &config,
            vec![source.clone()],
            false,
        )
        .expect("scan with languageGlobs")
        .findings;
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n")
            .expect("control config");
        let control_config = discover_config(temp.path())
            .expect("discover control")
            .expect("control config present");
        let control = scan_files(
            temp.path(),
            temp.path(),
            &control_config,
            vec![source],
            false,
        )
        .expect("control scan")
        .findings;

        assert_eq!(findings.len(), 1);
        assert!(
            control.is_empty(),
            "without languageGlobs .ts resolves to TypeScript, not tsx"
        );
    }

    #[test]
    fn severity_off_rules_emit_no_findings() {
        let temp = TempDir::new().expect("tempdir");
        let source = write_basic_rule_fixture(
            &temp,
            "id: no-console-log\nlanguage: TypeScript\nseverity: off\nmessage: No console.log allowed\nrule:\n  pattern: console.log($$$)\n",
            "console.log('hi');\n",
        );

        let findings = scan_basic_fixture(&temp, source, false).findings;

        assert!(findings.is_empty());
    }

    #[test]
    fn finding_sort_key_stabilizes_output_order() {
        let mut findings = [
            super::Finding {
                rule_id: "b".to_owned(),
                severity: Severity::Warning,
                message: "second".to_owned(),
                relative_uri: "src/b.ts".to_owned(),
                start_line: 2,
                start_column: 1,
                end_line: 2,
                end_column: 10,
            },
            super::Finding {
                rule_id: "a".to_owned(),
                severity: Severity::Error,
                message: "first".to_owned(),
                relative_uri: "src/a.ts".to_owned(),
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 10,
            },
        ];

        findings.sort_unstable_by(finding_sort_key);

        assert_eq!(findings[0].relative_uri, "src/a.ts");
        assert_eq!(findings[1].relative_uri, "src/b.ts");
    }

    #[test]
    fn contingent_files_glob_filters_rules_through_get_rule_from_lang() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        let rule_file = temp.path().join("no-console-log.yml");
        fs::write(
            &rule_file,
            "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed\nfiles:\n  - '**/*.spec.ts'\nrule:\n  pattern: console.log($$$)\n",
        )
        .expect("rule");
        let matching = temp.path().join("src/foo.spec.ts");
        let non_matching = temp.path().join("src/foo.ts");
        fs::write(&matching, "console.log('match');\n").expect("matching source");
        fs::write(&non_matching, "console.log('miss');\n").expect("non matching source");

        let rules = crate::lint::load_rules(&[rule_file]).expect("load rules");
        let collection = ast_grep_config::RuleCollection::try_new(rules).expect("rule collection");
        let context = ScanContext {
            cwd: temp.path(),
            repo_root: temp.path(),
            config_dir: temp.path(),
            language_globs: &[],
        };

        let matching_findings = scan_files_with_collection(context, &collection, vec![matching])
            .expect("matching scan");
        let non_matching_findings =
            scan_files_with_collection(context, &collection, vec![non_matching])
                .expect("non matching scan");

        assert_eq!(matching_findings.len(), 1);
        assert_eq!(matching_findings[0].relative_uri, "src/foo.spec.ts");
        assert!(non_matching_findings.is_empty());
    }

    #[test]
    fn selection_path_is_config_root_relative() {
        let context = ScanContext {
            cwd: Path::new("/repo"),
            repo_root: Path::new("/repo"),
            config_dir: Path::new("/repo"),
            language_globs: &[],
        };
        let file = Path::new("/repo/packages/app/src/index.ts");

        assert_eq!(
            context.selection_path(file),
            "packages/app/src/index.ts".to_owned()
        );
    }

    #[test]
    fn repo_root_relative_files_and_ignores_apply_during_per_package_scan() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let pkg = root.join("packages/app");
        fs::create_dir_all(root.join("rules")).expect("rules");
        fs::create_dir_all(pkg.join("src/generated")).expect("generated");
        fs::create_dir_all(root.join("tools")).expect("tools");
        fs::write(root.join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        let rule_file = root.join("rules/no-console-in-packages.yml");
        fs::write(
            &rule_file,
            "id: no-console-in-packages\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed in packages\nfiles:\n  - packages/**/src/**\nignores:\n  - '**/generated/**'\nrule:\n  pattern: console.log($$$)\n",
        )
        .expect("rule");
        let matching = pkg.join("src/index.ts");
        let ignored = pkg.join("src/generated/x.ts");
        let outside = root.join("tools/x.ts");
        fs::write(&matching, "console.log('match');\n").expect("matching");
        fs::write(&ignored, "console.log('ignored');\n").expect("ignored");
        fs::write(&outside, "console.log('outside');\n").expect("outside");

        let config = discover_config(&pkg)
            .expect("discover")
            .expect("config present");

        let matching_findings = scan_files(&pkg, &pkg, &config, vec![matching], false)
            .expect("matching scan")
            .findings;
        let ignored_findings = scan_files(&pkg, &pkg, &config, vec![ignored], false)
            .expect("ignored scan")
            .findings;
        let outside_findings = scan_files(&pkg, &pkg, &config, vec![outside], false)
            .expect("outside scan")
            .findings;

        assert_eq!(matching_findings.len(), 1);
        assert_eq!(matching_findings[0].relative_uri, "src/index.ts");
        assert!(ignored_findings.is_empty());
        assert!(outside_findings.is_empty());
    }

    #[test]
    fn relative_uri_is_repo_root_relative_for_sub_package_scan() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let pkg = root.join("packages/app");
        fs::create_dir_all(root.join("rules")).expect("rules");
        fs::create_dir_all(pkg.join("src")).expect("pkg src");
        fs::write(root.join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(
            root.join("rules/no-console-log.yml"),
            "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed\nrule:\n  pattern: console.log($$$)\n",
        )
        .expect("rule");
        let source = pkg.join("src/index.ts");
        fs::write(&source, "console.log('hi');\n").expect("source");

        let config = discover_config(&pkg)
            .expect("discover")
            .expect("config present");

        let findings = scan_files(&pkg, root, &config, vec![source], false)
            .expect("scan")
            .findings;

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].relative_uri, "packages/app/src/index.ts");
        assert!(findings[0].relative_uri.starts_with("packages/app/"));
    }

    #[test]
    fn parallel_scan_produces_same_sorted_findings_as_single_chunk_scan() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::create_dir_all(temp.path().join("src/a")).expect("src a");
        fs::create_dir_all(temp.path().join("src/b")).expect("src b");
        let rule_file = temp.path().join("rules/no-console-log.yml");
        fs::write(
            &rule_file,
            "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed\nrule:\n  pattern: console.log($$$)\n",
        )
        .expect("rule");
        let file_a = temp.path().join("src/a/index.ts");
        let file_b = temp.path().join("src/b/index.ts");
        fs::write(&file_a, "console.log('a');\nconsole.log('aa');\n").expect("file a");
        fs::write(&file_b, "console.log('b');\n").expect("file b");
        let files = vec![file_b.clone(), file_a.clone()];
        let rules = crate::lint::load_rules(&[rule_file]).expect("load rules");
        let collection = ast_grep_config::RuleCollection::try_new(rules).expect("rule collection");
        let context = ScanContext {
            cwd: temp.path(),
            repo_root: temp.path(),
            config_dir: temp.path(),
            language_globs: &[],
        };

        let sequential = scan_files_with_collection(context, &collection, vec![file_a, file_b])
            .expect("single chunk scan");
        let parallel =
            scan_files_with_collection(context, &collection, files).expect("parallel scan");

        assert_eq!(parallel.len(), sequential.len());
        assert_eq!(
            parallel
                .iter()
                .map(|finding| (
                    &finding.relative_uri,
                    finding.start_line,
                    finding.start_column,
                    &finding.rule_id
                ))
                .collect::<Vec<_>>(),
            sequential
                .iter()
                .map(|finding| (
                    &finding.relative_uri,
                    finding.start_line,
                    finding.start_column,
                    &finding.rule_id
                ))
                .collect::<Vec<_>>()
        );
    }
}
