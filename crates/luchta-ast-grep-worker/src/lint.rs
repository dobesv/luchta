use std::path::{Path, PathBuf};

use ast_grep_config::{from_yaml_string, GlobalRules, RuleCollection, RuleConfig, Severity};
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
    config: &DiscoveredConfig,
    files: Vec<PathBuf>,
) -> Result<Vec<Finding>, String> {
    let rules = load_rules(&config.rule_files)?;
    if rules.is_empty() {
        eprintln!("warning: ast-grep rule set is empty; skipping scan");
        return Ok(Vec::new());
    }
    scan_files_with_rules(
        cwd,
        &config.config_dir,
        rules,
        &config.language_globs,
        files,
    )
}

fn scan_files_with_rules(
    cwd: &Path,
    config_dir: &Path,
    rules: Vec<RuleConfig<SupportLang>>,
    language_globs: &[LanguageGlobEntry],
    files: Vec<PathBuf>,
) -> Result<Vec<Finding>, String> {
    let collection = RuleCollection::try_new(rules)
        .map_err(|error| format!("failed to build ast-grep rule collection: {error}"))?;
    scan_files_with_collection(cwd, config_dir, &collection, language_globs, files)
}

fn scan_files_with_collection(
    cwd: &Path,
    config_dir: &Path,
    collection: &RuleCollection<SupportLang>,
    language_globs: &[LanguageGlobEntry],
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
                    let mut per_file =
                        scan_file(cwd, config_dir, language_globs, collection, file.clone())?;
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

fn normalize_to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn rule_selection_path(file: &Path, config_dir: &Path) -> String {
    normalize_to_forward_slashes(file.strip_prefix(config_dir).unwrap_or(file))
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
    (
        &left.relative_uri,
        left.start_line,
        left.start_column,
        left.end_line,
        left.end_column,
        &left.rule_id,
        severity_rank(&left.severity),
        &left.message,
    )
        .cmp(&(
            &right.relative_uri,
            right.start_line,
            right.start_column,
            right.end_line,
            right.end_column,
            &right.rule_id,
            severity_rank(&right.severity),
            &right.message,
        ))
}

fn scan_file(
    cwd: &Path,
    config_dir: &Path,
    language_globs: &[LanguageGlobEntry],
    collection: &RuleCollection<SupportLang>,
    file: PathBuf,
) -> Result<Vec<Finding>, String> {
    let Some(lang) = resolve_language(&file, config_dir, language_globs) else {
        return Ok(Vec::new());
    };

    let source = std::fs::read_to_string(&file)
        .map_err(|error| format!("failed to read source {}: {error}", file.display()))?;
    let root = lang.ast_grep(&source);
    // Keep resolver + RuleCollection path. `for_path` re-derives language via hardcoded
    // extension lookup, which breaks languageGlobs remaps. RuleCollection contingent
    // files/ignores matching must see sgconfig-root-relative paths, not absolute or cwd-relative
    // ones, to mirror ast-grep CLI behavior for repo-root `files:` / `ignores:` globs.
    let rule_selection_path = rule_selection_path(&file, config_dir);
    let applicable_rules = collection.get_rule_from_lang(Path::new(&rule_selection_path), lang);
    if applicable_rules.is_empty() {
        return Ok(Vec::new());
    }

    let relative_uri = normalize_to_forward_slashes(file.strip_prefix(cwd).unwrap_or(&file));

    let mut findings = Vec::new();
    for rule_config in applicable_rules {
        if matches!(rule_config.severity, Severity::Off) {
            continue;
        }
        let matcher = &rule_config.matcher;
        for node_match in root.root().find_all(matcher) {
            let start = node_match.start_pos();
            let end = node_match.end_pos();
            findings.push(Finding {
                rule_id: rule_config.id.clone(),
                severity: rule_config.severity.clone(),
                message: rule_config.get_message(&node_match),
                relative_uri: relative_uri.clone(),
                start_line: start.line() + 1,
                start_column: start.column(&node_match) + 1,
                end_line: end.line() + 1,
                end_column: end.column(&node_match) + 1,
            });
        }
    }
    findings.sort_unstable_by(finding_sort_key);
    Ok(findings)
}

pub async fn scan_files_async(
    cwd: &Path,
    config: &DiscoveredConfig,
    files: Vec<PathBuf>,
) -> Result<Vec<Finding>, String> {
    let cwd = cwd.to_path_buf();
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let rules = load_rules(&config.rule_files)?;
        if rules.is_empty() {
            eprintln!("warning: ast-grep rule set is empty; skipping scan");
            return Ok(Vec::new());
        }

        scan_files_with_rules(
            &cwd,
            &config.config_dir,
            rules,
            &config.language_globs,
            files,
        )
    })
    .await
    .map_err(|error| format!("ast-grep worker join error: {error}"))?
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use assert_fs::TempDir;
    use ast_grep_config::Severity;

    use super::{finding_sort_key, rule_selection_path, scan_files, scan_files_with_collection};
    use crate::config::discover_config;

    #[test]
    fn trivial_rule_matching_source_produces_expected_finding() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        let rule_file = temp.path().join("rules/no-console-log.yml");
        fs::write(
            &rule_file,
            "id: no-console-log\nlanguage: TypeScript\nseverity: error\nmessage: No console.log allowed\nrule:\n  pattern: console.log($$$)\n",
        )
        .expect("rule");
        let source = temp.path().join("src/index.ts");
        fs::write(&source, "console.log('hi');\n").expect("source");

        let config = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        let findings = scan_files(temp.path(), &config, vec![source]).expect("scan");

        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.rule_id, "no-console-log");
        assert_eq!(finding.message, "No console.log allowed");
        assert_eq!(finding.relative_uri, "src/index.ts");
        assert_eq!(finding.start_line, 1);
        assert_eq!(finding.start_column, 1);
        assert_eq!(finding.end_line, 1);
        assert!(finding.end_column >= finding.start_column);
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
        let findings = scan_files(temp.path(), &config, vec![source.clone()])
            .expect("scan with languageGlobs");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n")
            .expect("control config");
        let control_config = discover_config(temp.path())
            .expect("discover control")
            .expect("control config present");
        let control = scan_files(temp.path(), &control_config, vec![source]).expect("control scan");

        assert_eq!(findings.len(), 1);
        assert!(
            control.is_empty(),
            "without languageGlobs .ts resolves to TypeScript, not tsx"
        );
    }

    #[test]
    fn severity_off_rules_emit_no_findings() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules")).expect("rules");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(
            temp.path().join("rules/no-console-log.yml"),
            "id: no-console-log\nlanguage: TypeScript\nseverity: off\nmessage: No console.log allowed\nrule:\n  pattern: console.log($$$)\n",
        )
        .expect("rule");
        let source = temp.path().join("src/index.ts");
        fs::write(&source, "console.log('hi');\n").expect("source");

        let config = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        let findings = scan_files(temp.path(), &config, vec![source]).expect("scan");

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

        let matching_findings =
            scan_files_with_collection(temp.path(), temp.path(), &collection, &[], vec![matching])
                .expect("matching scan");
        let non_matching_findings = scan_files_with_collection(
            temp.path(),
            temp.path(),
            &collection,
            &[],
            vec![non_matching],
        )
        .expect("non matching scan");

        assert_eq!(matching_findings.len(), 1);
        assert_eq!(matching_findings[0].relative_uri, "src/foo.spec.ts");
        assert!(non_matching_findings.is_empty());
    }

    #[test]
    fn rule_selection_path_is_config_root_relative() {
        let config_dir = Path::new("/repo");
        let file = Path::new("/repo/packages/app/src/index.ts");

        assert_eq!(
            rule_selection_path(file, config_dir),
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

        let matching_findings = scan_files(&pkg, &config, vec![matching]).expect("matching scan");
        let ignored_findings = scan_files(&pkg, &config, vec![ignored]).expect("ignored scan");
        let outside_findings = scan_files(&pkg, &config, vec![outside]).expect("outside scan");

        assert_eq!(matching_findings.len(), 1);
        assert_eq!(matching_findings[0].relative_uri, "src/index.ts");
        assert!(ignored_findings.is_empty());
        assert!(outside_findings.is_empty());
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

        let sequential = scan_files_with_collection(
            temp.path(),
            temp.path(),
            &collection,
            &[],
            vec![file_a, file_b],
        )
        .expect("single chunk scan");
        let parallel =
            scan_files_with_collection(temp.path(), temp.path(), &collection, &[], files)
                .expect("parallel scan");

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
