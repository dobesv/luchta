#![cfg(feature = "oxc")]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};
use oxc_linter::{
    ConfigStore, ConfigStoreBuilder, ExternalPluginStore, LintIgnoreMatcher, Oxlintrc,
};
use rustc_hash::{FxBuildHasher, FxHashMap};

const ROOT_CONFIG_FILENAMES: [&str; 2] = [".oxlintrc.json", ".oxlintrc.jsonc"];
const TS_CONFIG_FILENAME: &str = "oxlint.config.ts";
const SKIP_DIRS: [&str; 2] = [".git", "node_modules"];

#[derive(Debug)]
pub struct LoadedConfig {
    pub store: ConfigStore,
    pub root_config_path: Option<PathBuf>,
    pub saw_only_unsupported_ts_config: bool,
    pub ignore_patterns: Vec<String>,
    pub ignore_base: PathBuf,
    pub warnings: Vec<String>,
}

pub fn discover_config(cwd: &Path) -> Result<LoadedConfig, String> {
    let root_config_path = find_root_config_path(cwd);
    let saw_only_unsupported_ts_config =
        root_config_path.is_none() && find_root_ts_config_path(cwd).is_some();
    let ignore_base = root_config_path
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(cwd)
        .to_path_buf();
    let oxlintrc = match root_config_path.as_deref() {
        Some(path) => Oxlintrc::from_file(path)
            .map_err(|error| format!("failed to load oxlint config {}: {error}", path.display()))?,
        None => Oxlintrc::default(),
    };
    let ignore_patterns = oxlintrc.ignore_patterns.clone();
    let mut external_plugin_store = ExternalPluginStore::new(true);
    let base_config =
        ConfigStoreBuilder::from_oxlintrc(false, oxlintrc, None, &mut external_plugin_store, None)
            .map_err(|error| format!("failed to build oxlint config: {error}"))?
            .build(&mut external_plugin_store)
            .map_err(|error| format!("failed to finalize oxlint config: {error}"))?;
    let nested = FxHashMap::with_capacity_and_hasher(0, FxBuildHasher);
    Ok(LoadedConfig {
        store: ConfigStore::new(base_config, nested, external_plugin_store),
        root_config_path,
        saw_only_unsupported_ts_config,
        ignore_patterns,
        ignore_base,
        warnings: Vec::new(),
    })
}

pub fn collect_target_files(
    cwd: &Path,
    ignore_patterns: &[String],
    ignore_base: &Path,
) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let ignore_matcher = LintIgnoreMatcher::new(ignore_patterns, ignore_base, Vec::new());
    let mut builder = WalkBuilder::new(cwd);
    builder
        .hidden(false)
        .git_ignore(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        // Intentional: keep existing worker behavior for symlink traversal.
        .follow_links(true)
        .filter_entry(|entry| !should_skip_entry(entry));

    let mut warnings = Vec::new();
    let tool_ignore = cwd.join(".oxlintignore");
    if tool_ignore.is_file() {
        if let Some(error) = builder.add_ignore(&tool_ignore) {
            warnings.push(format!(
                "warning: failed to load {}: {error}",
                tool_ignore.display()
            ));
        }
    }

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry =
            entry.map_err(|error| format!("failed to walk workspace for sources: {error}"))?;
        let path = entry.into_path();
        if path.is_dir() || !is_js_ts_source(&path) || ignore_matcher.should_ignore(&path) {
            continue;
        }
        files.push(path);
    }
    files.sort();
    files.dedup();
    Ok((files, warnings))
}

fn find_root_config_path(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        for filename in ROOT_CONFIG_FILENAMES {
            let candidate = ancestor.join(filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_root_ts_config_path(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        let candidate = ancestor.join(TS_CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn should_skip_entry(entry: &DirEntry) -> bool {
    entry
        .path()
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| SKIP_DIRS.contains(&name))
}

fn is_js_ts_source(path: &Path) -> bool {
    // Intentionally include .d.ts here: linting declarations is useful, unlike formatter pass.
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts")
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use assert_fs::TempDir;

    use super::{collect_target_files, discover_config};

    #[test]
    fn collect_target_files_skips_gitignored_directory() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::write(cwd.join(".gitignore"), "/dist/\n").expect("gitignore");
        fs::create_dir_all(cwd.join("src")).expect("src");
        fs::create_dir_all(cwd.join("dist")).expect("dist");
        fs::write(cwd.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(cwd.join("dist/out.js"), "export const out = 1;\n").expect("dist file");

        let (files, warnings) = collect_target_files(cwd, &[], cwd).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_target_files_honors_repo_root_gitignore_from_package_subdir() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path();
        let pkg = repo.join("packages/app");
        fs::write(repo.join(".gitignore"), "/packages/app/dist/\n").expect("gitignore");
        fs::create_dir_all(pkg.join("src")).expect("src");
        fs::create_dir_all(pkg.join("dist")).expect("dist");
        fs::write(pkg.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(pkg.join("dist/out.js"), "export const out = 1;\n").expect("dist file");

        let (files, warnings) = collect_target_files(&pkg, &[], &pkg).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(&pkg, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_target_files_honors_tool_ignore_file() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::write(cwd.join(".oxlintignore"), "generated.ts\n").expect("ignore file");
        fs::create_dir_all(cwd.join("src")).expect("src");
        fs::write(cwd.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(cwd.join("generated.ts"), "export const generated = 1;\n").expect("generated");

        let (files, warnings) = collect_target_files(cwd, &[], cwd).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_target_files_honors_config_ignore_patterns() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::write(
            cwd.join(".oxlintrc.json"),
            r#"{"ignorePatterns":["generated.ts"]}"#,
        )
        .expect("config");
        fs::create_dir_all(cwd.join("src")).expect("src");
        fs::write(cwd.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(cwd.join("generated.ts"), "export const generated = 1;\n").expect("generated");

        let loaded = discover_config(cwd).expect("discover");
        let (files, warnings) =
            collect_target_files(cwd, &loaded.ignore_patterns, &loaded.ignore_base)
                .expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_target_files_honors_parent_config_root_for_anchored_ignore_patterns() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path();
        let pkg = repo.join("packages/app");
        fs::write(
            repo.join(".oxlintrc.json"),
            r#"{"ignorePatterns":["/packages/app/dist/"]}"#,
        )
        .expect("config");
        fs::create_dir_all(pkg.join("src")).expect("src");
        fs::create_dir_all(pkg.join("dist")).expect("dist");
        fs::write(pkg.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(pkg.join("dist/out.js"), "export const out = 1;\n").expect("dist file");

        let loaded = discover_config(&pkg).expect("discover");
        let (files, warnings) =
            collect_target_files(&pkg, &loaded.ignore_patterns, &loaded.ignore_base)
                .expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(&pkg, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_target_files_skips_node_modules_and_git_without_gitignore() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src");
        fs::create_dir_all(cwd.join("node_modules/pkg")).expect("node modules");
        fs::create_dir_all(cwd.join(".git/hooks")).expect("git");
        fs::write(cwd.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(
            cwd.join("node_modules/pkg/ignored.ts"),
            "export const ignored = 1;\n",
        )
        .expect("node module file");
        fs::write(cwd.join(".git/hooks/ignored.js"), "console.log(1);\n").expect("git file");

        let (files, warnings) = collect_target_files(cwd, &[], cwd).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/foo.ts"]);
    }

    fn relative_paths(cwd: &std::path::Path, files: Vec<std::path::PathBuf>) -> Vec<String> {
        files
            .into_iter()
            .map(|path| {
                path.strip_prefix(cwd)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }
}
