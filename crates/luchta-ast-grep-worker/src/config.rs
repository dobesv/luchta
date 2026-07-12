use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ast_grep_core::Language;
use ast_grep_language::SupportLang;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

const CONFIG_FILENAME: &str = "sgconfig.yml";
const SKIP_DIRS: [&str; 2] = [".git", "node_modules"];
const SUPPORTED_TOP_LEVEL_KEYS: &[&str] = &["ruleDirs", "languageGlobs"];

#[derive(Debug, Clone)]
pub struct DiscoveredConfig {
    pub config_path: PathBuf,
    pub config_dir: PathBuf,
    pub rule_files: Vec<PathBuf>,
    pub language_globs: Vec<LanguageGlobEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LanguageGlobEntry {
    pub language: SupportLang,
    pub matcher: GlobSet,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SgConfigFile {
    #[serde(default)]
    rule_dirs: Vec<String>,
    #[serde(default)]
    language_globs: IndexMap<SupportLang, Vec<String>>,
}

pub fn discover_config(cwd: &Path) -> Result<Option<DiscoveredConfig>, String> {
    let Some(config_path) = find_config_path(cwd) else {
        return Ok(None);
    };

    let config_dir = config_path
        .parent()
        .ok_or_else(|| format!("sgconfig.yml has no parent: {}", config_path.display()))?
        .to_path_buf();
    let text = std::fs::read_to_string(&config_path)
        .map_err(|error| format!("failed to read {}: {error}", config_path.display()))?;
    let parsed: SgConfigFile = serde_norway::from_str(&text)
        .map_err(|error| format!("failed to parse sgconfig.yml: {error}"))?;
    let warnings = discover_unsupported_key_warnings(&text)?;

    let mut rule_files = Vec::new();
    for rule_dir in parsed.rule_dirs {
        let resolved = config_dir.join(rule_dir);
        collect_rule_files(&resolved, &mut rule_files)?;
    }
    rule_files.sort();
    rule_files.dedup();

    let language_globs = build_language_globs(parsed.language_globs)?;

    Ok(Some(DiscoveredConfig {
        config_path,
        config_dir,
        rule_files,
        language_globs,
        warnings,
    }))
}

pub fn collect_source_files(
    cwd: &Path,
    config_dir: &Path,
    language_globs: &[LanguageGlobEntry],
) -> Result<Vec<PathBuf>, String> {
    let mut builder = WalkBuilder::new(cwd);
    builder
        .hidden(false)
        .git_ignore(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(true)
        .filter_entry(|entry| !should_skip_entry(entry));

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry =
            entry.map_err(|error| format!("failed to walk workspace for sources: {error}"))?;
        let path = entry.into_path();
        if path.is_dir() || resolve_language(&path, config_dir, language_globs).is_none() {
            continue;
        }
        files.push(path);
    }

    files.sort();
    files.dedup();
    Ok(files)
}

pub fn resolve_language(
    path: &Path,
    config_dir: &Path,
    language_globs: &[LanguageGlobEntry],
) -> Option<SupportLang> {
    let relative = path.strip_prefix(config_dir).unwrap_or(path);
    for entry in language_globs {
        if entry.matcher.is_match(relative) {
            return Some(entry.language);
        }
    }
    SupportLang::from_path(path)
}

fn build_language_globs(
    configured: IndexMap<SupportLang, Vec<String>>,
) -> Result<Vec<LanguageGlobEntry>, String> {
    configured
        .into_iter()
        .map(|(language, patterns)| {
            let mut builder = GlobSetBuilder::new();
            for pattern in patterns {
                let glob = Glob::new(&pattern).map_err(|error| {
                    format!(
                        "failed to parse languageGlobs entry for {} pattern {pattern:?}: {error}",
                        language_name(language)
                    )
                })?;
                builder.add(glob);
            }
            let matcher = builder.build().map_err(|error| {
                format!(
                    "failed to build languageGlobs entry for {}: {error}",
                    language_name(language)
                )
            })?;
            Ok(LanguageGlobEntry { language, matcher })
        })
        .collect()
}

fn discover_unsupported_key_warnings(text: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_norway::from_str(text)
        .map_err(|error| format!("failed to parse sgconfig.yml: {error}"))?;
    let Some(object) = value.as_object() else {
        return Ok(Vec::new());
    };

    let mut warnings = Vec::new();
    for key in object.keys() {
        if SUPPORTED_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            continue;
        }
        warnings.push(format!(
            "ast-grep worker: '{key}' not yet supported; ignoring"
        ));
    }
    warnings.sort();
    Ok(warnings)
}

fn language_name(language: SupportLang) -> &'static str {
    match language {
        SupportLang::Bash => "bash",
        SupportLang::C => "c",
        SupportLang::Cpp => "cpp",
        SupportLang::CSharp => "csharp",
        SupportLang::Css => "css",
        SupportLang::Dart => "dart",
        SupportLang::Elixir => "elixir",
        SupportLang::Go => "go",
        SupportLang::Haskell => "haskell",
        SupportLang::Hcl => "hcl",
        SupportLang::Html => "html",
        SupportLang::Java => "java",
        SupportLang::JavaScript => "javascript",
        SupportLang::Json => "json",
        SupportLang::Kotlin => "kotlin",
        SupportLang::Lua => "lua",
        SupportLang::Markdown => "markdown",
        SupportLang::Nix => "nix",
        SupportLang::Php => "php",
        SupportLang::Python => "python",
        SupportLang::Ruby => "ruby",
        SupportLang::Rust => "rust",
        SupportLang::Scala => "scala",
        SupportLang::Solidity => "solidity",
        SupportLang::Swift => "swift",
        SupportLang::Tsx => "tsx",
        SupportLang::TypeScript => "typescript",
        SupportLang::Yaml => "yaml",
    }
}

fn find_config_path(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        let candidate = ancestor.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn collect_rule_files(rule_dir: &Path, rule_files: &mut Vec<PathBuf>) -> Result<(), String> {
    if !rule_dir.exists() {
        return Ok(());
    }

    let mut builder = WalkBuilder::new(rule_dir);
    builder
        .hidden(false)
        .git_ignore(false)
        .ignore(false)
        .parents(false)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(true);

    for entry in builder.build() {
        let entry = entry.map_err(|error| {
            format!(
                "failed to walk rule directory {}: {error}",
                rule_dir.display()
            )
        })?;
        let path = entry.into_path();
        if path.is_file() && is_yaml_file(&path) {
            rule_files.push(path);
        }
    }

    Ok(())
}

fn should_skip_entry(entry: &DirEntry) -> bool {
    entry
        .path()
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| SKIP_DIRS.contains(&name))
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("yml" | "yaml")
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use assert_fs::TempDir;
    use ast_grep_language::SupportLang;

    use super::{collect_source_files, discover_config, resolve_language};

    #[test]
    fn discover_config_returns_none_when_absent() {
        let temp = TempDir::new().expect("tempdir");

        let discovered = discover_config(temp.path()).expect("discover");

        assert!(discovered.is_none());
    }

    #[test]
    fn discover_config_finds_sgconfig_in_cwd() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs: []\n").expect("config");

        let discovered = discover_config(temp.path())
            .expect("discover")
            .expect("config present");

        assert_eq!(discovered.config_path, temp.path().join("sgconfig.yml"));
        assert_eq!(discovered.config_dir, temp.path());
    }

    #[test]
    fn discover_config_finds_sgconfig_in_parent() {
        let temp = TempDir::new().expect("tempdir");
        let pkg = temp.path().join("packages/app");
        fs::create_dir_all(&pkg).expect("pkg");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs: []\n").expect("config");

        let discovered = discover_config(&pkg)
            .expect("discover")
            .expect("config present");

        assert_eq!(discovered.config_path, temp.path().join("sgconfig.yml"));
        assert_eq!(discovered.config_dir, temp.path());
    }

    #[test]
    fn discover_config_collects_rule_files_from_rule_dirs() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("rules/nested")).expect("rules");
        fs::write(temp.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").expect("config");
        fs::write(temp.path().join("rules/a.yml"), "id: a\n").expect("rule a");
        fs::write(temp.path().join("rules/nested/b.yaml"), "id: b\n").expect("rule b");
        fs::write(temp.path().join("rules/ignore.txt"), "id: c\n").expect("other file");

        let discovered = discover_config(temp.path())
            .expect("discover")
            .expect("config present");

        let relative = relative_paths(temp.path(), discovered.rule_files);
        assert_eq!(relative, vec!["rules/a.yml", "rules/nested/b.yaml"]);
    }

    #[test]
    fn discover_config_warns_for_unsupported_keys() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("sgconfig.yml"),
            "ruleDirs: []\nutilDirs:\n  - utils\nutils:\n  helper: {}\n",
        )
        .expect("config");

        let discovered = discover_config(temp.path())
            .expect("discover")
            .expect("config present");

        assert_eq!(
            discovered.warnings,
            vec![
                "ast-grep worker: 'utilDirs' not yet supported; ignoring".to_owned(),
                "ast-grep worker: 'utils' not yet supported; ignoring".to_owned(),
            ]
        );
    }

    #[test]
    fn discover_config_parses_language_globs_in_declared_order() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("sgconfig.yml"),
            "ruleDirs: []\nlanguageGlobs:\n  tsx:\n    - '*.ts'\n  javascript:\n    - '*.ts'\n",
        )
        .expect("config");

        let discovered = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        let source = temp.path().join("index.ts");
        fs::write(&source, "console.log('hi');\n").expect("source");

        assert_eq!(
            resolve_language(&source, &discovered.config_dir, &discovered.language_globs),
            Some(SupportLang::Tsx)
        );
    }

    #[test]
    fn language_globs_are_resolved_relative_to_config_dir_not_task_cwd() {
        let temp = TempDir::new().expect("tempdir");
        let pkg = temp.path().join("packages/app");
        fs::create_dir_all(pkg.join("nested")).expect("pkg dirs");
        fs::write(
            temp.path().join("sgconfig.yml"),
            "ruleDirs: []\nlanguageGlobs:\n  tsx:\n    - 'packages/app/**/*.ts'\n",
        )
        .expect("config");
        let source = pkg.join("nested/index.ts");
        fs::write(&source, "console.log('hi');\n").expect("source");

        let discovered = discover_config(&pkg)
            .expect("discover")
            .expect("config present");

        assert_eq!(
            resolve_language(&source, &discovered.config_dir, &discovered.language_globs),
            Some(SupportLang::Tsx)
        );
        assert_eq!(
            resolve_language(&source, &pkg, &discovered.language_globs),
            Some(SupportLang::TypeScript)
        );
    }

    #[test]
    fn collect_source_files_skips_gitignored_dirs() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join(".gitignore"), "/dist/\n").expect("gitignore");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::create_dir_all(temp.path().join("dist")).expect("dist");
        fs::write(temp.path().join("src/foo.ts"), "const foo = 1;\n").expect("src file");
        fs::write(temp.path().join("dist/out.ts"), "const out = 1;\n").expect("dist file");

        let files = collect_source_files(temp.path(), temp.path(), &[]).expect("collect");

        assert_eq!(relative_paths(temp.path(), files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_source_files_uses_language_globs_before_extension_map() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(
            temp.path().join("sgconfig.yml"),
            "ruleDirs: []\nlanguageGlobs:\n  yaml:\n    - 'src/*.custom'\n",
        )
        .expect("config");
        fs::write(temp.path().join("src/foo.custom"), "name: value\n").expect("source");

        let discovered = discover_config(temp.path())
            .expect("discover")
            .expect("config present");
        let files = collect_source_files(
            temp.path(),
            &discovered.config_dir,
            &discovered.language_globs,
        )
        .expect("collect");
        let relative = relative_paths(temp.path(), files);

        assert!(relative.contains(&"src/foo.custom".to_owned()));
    }

    #[test]
    fn collect_source_files_skips_node_modules() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::create_dir_all(temp.path().join("node_modules/pkg")).expect("node_modules");
        fs::create_dir_all(temp.path().join(".git/hooks")).expect("git");
        fs::write(temp.path().join("src/foo.ts"), "const foo = 1;\n").expect("src file");
        fs::write(
            temp.path().join("node_modules/pkg/ignored.ts"),
            "const ignored = 1;\n",
        )
        .expect("node_modules file");
        fs::write(
            temp.path().join(".git/hooks/ignored.js"),
            "console.log(1);\n",
        )
        .expect("git file");

        let files = collect_source_files(temp.path(), temp.path(), &[]).expect("collect");

        assert_eq!(relative_paths(temp.path(), files), vec!["src/foo.ts"]);
    }

    fn relative_paths(cwd: &Path, files: Vec<PathBuf>) -> Vec<String> {
        files
            .into_iter()
            .map(|path| {
                path.strip_prefix(cwd)
                    .expect("relative path")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }
}
