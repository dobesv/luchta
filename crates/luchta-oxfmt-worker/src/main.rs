#[cfg(feature = "oxc")]
mod config;
#[cfg(feature = "oxc")]
mod format;

#[cfg(feature = "oxc")]
use std::collections::BTreeSet;
#[cfg(feature = "oxc")]
use std::ffi::OsStr;
#[cfg(feature = "oxc")]
use std::fs;
#[cfg(feature = "oxc")]
use std::path::{Path, PathBuf};

#[cfg(feature = "oxc")]
use ignore::{gitignore::Gitignore, WalkBuilder};
#[cfg(feature = "oxc")]
use luchta_worker::{
    process_items_in_parallel, run_worker_main, InProcessOutcome, JobContext, ResolveResult,
    ResolveTask, TaskModification, Worker, WorkerRequest,
};
#[cfg(feature = "oxc")]
use tokio::task;

#[cfg(feature = "oxc")]
use crate::config::discover_config;
#[cfg(feature = "oxc")]
use crate::format::{format_path, relative_display};

fn main() {
    if luchta_worker::version_requested(
        &std::env::args().collect::<Vec<_>>(),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return;
    }

    real_main();
}

#[cfg(feature = "oxc")]
fn real_main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async { run_worker_main(OxfmtWorker).await });
}

#[cfg(not(feature = "oxc"))]
fn real_main() {
    eprintln!("this binary was built without the 'oxc' feature; the oxfmt worker is unavailable");
    std::process::exit(1);
}

#[cfg_attr(not(feature = "oxc"), allow(dead_code))]
struct OxfmtWorker;

#[cfg(feature = "oxc")]
impl Worker for OxfmtWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let mut inputs = BTreeSet::from([
            "package.json".to_owned(),
            "src/**".to_owned(),
            ".oxfmtrc.json".to_owned(),
            ".oxfmtrc.jsonc".to_owned(),
            ".gitignore".to_owned(),
            ".ignore".to_owned(),
            ".oxfmtignore".to_owned(),
        ]);
        inputs.extend(req.inputs.iter().cloned());

        let Some(cwd) = req.cwd.as_deref() else {
            return ResolveResult::modify(TaskModification {
                inputs: Some(inputs.into_iter().collect()),
                ..TaskModification::default()
            });
        };

        let cwd = Path::new(cwd);
        let config = match discover_config(cwd) {
            Ok(config) => config,
            Err(error) => return ResolveResult::reject(error),
        };
        if !config.warnings.is_empty() {
            return ResolveResult::reject(config.warnings.join("; "));
        }
        let (files, warnings) = match collect_formattable_files(cwd, config.ignore_matcher.as_ref())
        {
            Ok(result) => result,
            Err(error) => return ResolveResult::reject(error),
        };
        if !warnings.is_empty() {
            return ResolveResult::reject(warnings.join("; "));
        }

        if files.is_empty() {
            return ResolveResult::prune(Some("no JS/TS source files found for oxfmt".to_owned()));
        }

        ResolveResult::modify(TaskModification {
            inputs: Some(inputs.into_iter().collect()),
            ..TaskModification::default()
        })
    }

    fn build_command(&self, _req: &WorkerRequest) -> String {
        String::new()
    }

    #[allow(clippy::manual_async_fn)]
    fn run_in_process(
        &self,
        req: &WorkerRequest,
        ctx: &JobContext,
    ) -> impl std::future::Future<Output = InProcessOutcome> + Send {
        async move {
            let Some(cwd) = req.cwd.as_deref() else {
                let _ = ctx
                    .emit_stderr("oxfmt worker requires cwd".to_owned())
                    .await;
                return InProcessOutcome::Done {
                    exit_code: 1,
                    outputs: None,
                };
            };

            let cwd = PathBuf::from(cwd);
            let opts = OxfmtOpts::from_request(req);
            let loaded_config = match task::spawn_blocking({
                let cwd = cwd.clone();
                move || discover_config(&cwd)
            })
            .await
            {
                Ok(Ok(config)) => config,
                Ok(Err(error)) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
                Err(error) => {
                    let _ = ctx
                        .emit_stderr(format!("failed to resolve oxfmt config: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };
            let warnings = loaded_config.warnings.clone();
            let (files, collection_warnings) = match task::spawn_blocking({
                let cwd = cwd.clone();
                let ignore_matcher = loaded_config.ignore_matcher.clone();
                move || collect_formattable_files(&cwd, ignore_matcher.as_ref())
            })
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
                Err(error) => {
                    let _ = ctx
                        .emit_stderr(format!("failed to collect oxfmt sources: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };
            for warning in warnings.into_iter().chain(collection_warnings) {
                if let Err(error) = ctx.emit_stderr(warning).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit formatter log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            }
            if files.is_empty() {
                return InProcessOutcome::Done {
                    exit_code: 0,
                    outputs: None,
                };
            }

            let mut exit_code = 0;
            let mut would_reformat = false;
            let outcomes = match task::spawn_blocking({
                let cwd = cwd.clone();
                let loaded_config = loaded_config.clone();
                let files = files.clone();
                move || {
                    process_items_in_parallel(
                        &files,
                        "oxfmt worker parallel format thread panicked",
                        |path| {
                            let relative = relative_display(&cwd, path);
                            let source = match fs::read_to_string(path) {
                                Ok(source) => source,
                                Err(error) => {
                                    return FileOutcome::ReadError(format!(
                                        "failed to read {}: {error}",
                                        path.display()
                                    ));
                                }
                            };
                            let options = loaded_config.options_for(path);
                            let result = match format_path(path, &source, &options) {
                                Ok(result) => result,
                                Err(error) => return FileOutcome::FormatError(error),
                            };

                            if !result.changed {
                                return FileOutcome::Unchanged;
                            }

                            if opts.check {
                                return FileOutcome::WouldReformat { relative };
                            }

                            if let Err(error) = write_text_file(path, &result.formatted) {
                                return FileOutcome::WriteError(error);
                            }

                            FileOutcome::Reformatted { relative }
                        },
                    )
                }
            })
            .await
            {
                Ok(Ok(outcomes)) => outcomes,
                Ok(Err(error)) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
                Err(error) => {
                    let _ = ctx
                        .emit_stderr(format!("oxfmt parallel task failed: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            for outcome in outcomes {
                match outcome {
                    FileOutcome::ReadError(error)
                    | FileOutcome::FormatError(error)
                    | FileOutcome::WriteError(error) => {
                        let _ = ctx.emit_stderr(error).await;
                        exit_code = 1;
                    }
                    FileOutcome::Unchanged => {}
                    FileOutcome::WouldReformat { relative } => {
                        would_reformat = true;
                        if let Err(error) =
                            ctx.emit_stdout(format!("would reformat: {relative}")).await
                        {
                            let _ = ctx
                                .emit_stderr(format!("failed to emit formatter log: {error}"))
                                .await;
                            return InProcessOutcome::Done {
                                exit_code: 1,
                                outputs: None,
                            };
                        }
                    }
                    FileOutcome::Reformatted { relative } => {
                        if let Err(error) =
                            ctx.emit_stdout(format!("reformatted: {relative}")).await
                        {
                            let _ = ctx
                                .emit_stderr(format!("failed to emit formatter log: {error}"))
                                .await;
                            return InProcessOutcome::Done {
                                exit_code: 1,
                                outputs: None,
                            };
                        }
                    }
                }
            }

            if opts.check && would_reformat {
                exit_code = 1;
            }

            InProcessOutcome::Done {
                exit_code,
                outputs: None,
            }
        }
    }
}

#[cfg(feature = "oxc")]
enum FileOutcome {
    ReadError(String),
    FormatError(String),
    WriteError(String),
    Unchanged,
    WouldReformat { relative: String },
    Reformatted { relative: String },
}

#[cfg(feature = "oxc")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OxfmtOpts {
    check: bool,
}

#[cfg(feature = "oxc")]
impl OxfmtOpts {
    fn from_request(req: &WorkerRequest) -> Self {
        let mut opts = Self::default();
        if let Some(raw) = req.env.get("OXFMT_OPTS") {
            for token in raw.split_whitespace() {
                if token == "--check" {
                    opts.check = true;
                }
            }
        }
        opts
    }
}

#[cfg(feature = "oxc")]
fn collect_formattable_files(
    cwd: &Path,
    config_ignore_matcher: Option<&Gitignore>,
) -> Result<(Vec<PathBuf>, Vec<String>), String> {
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
        .filter_entry(|entry| !should_skip_walk_entry(entry.path()));

    let mut warnings = Vec::new();
    let tool_ignore = cwd.join(".oxfmtignore");
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
        if path.is_dir() || !is_formattable_path(&path) {
            continue;
        }
        if let Some(ignore_matcher) = config_ignore_matcher {
            if ignore_matcher
                .matched_path_or_any_parents(&path, false)
                .is_ignore()
            {
                continue;
            }
        }
        files.push(path);
    }

    files.sort();
    files.dedup();
    Ok((files, warnings))
}

#[cfg(feature = "oxc")]
fn should_skip_walk_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "node_modules" | ".git"))
}

#[cfg(feature = "oxc")]
fn is_formattable_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };
    // Intentionally exclude .d.ts here: formatter should not rewrite declaration files.
    if !matches!(
        extension,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts"
    ) {
        return false;
    }
    let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    !file_name.ends_with(".d.ts")
}

#[cfg(feature = "oxc")]
fn write_text_file(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

#[cfg(all(test, feature = "oxc"))]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use assert_fs::TempDir;
    use luchta_worker::{InProcessOutcome, JobContext, SharedWriter, Worker, WorkerRequest};
    use oxc_formatter::JsFormatOptions;

    use super::{collect_formattable_files, is_formattable_path, OxfmtOpts, OxfmtWorker};
    use crate::config::discover_config;
    use crate::format::format_path;

    #[test]
    fn opts_recognize_check_flag() {
        let mut env = HashMap::new();
        env.insert("OXFMT_OPTS".to_owned(), "--check --unknown".to_owned());
        let req = WorkerRequest::new("pkg#format", "format").with_env(env);
        assert_eq!(OxfmtOpts::from_request(&req), OxfmtOpts { check: true });
    }

    #[test]
    fn collect_formattable_files_skips_node_modules_and_git() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src/nested")).expect("src dir");
        fs::create_dir_all(cwd.join("node_modules/pkg")).expect("node modules");
        fs::create_dir_all(cwd.join(".git/hooks")).expect("git dir");
        fs::write(cwd.join("src/index.ts"), "export const x=1\n").expect("src file");
        fs::write(cwd.join("package.js"), "module.exports={a:1}\n").expect("root file");
        fs::write(
            cwd.join("node_modules/pkg/ignored.ts"),
            "export const y=2\n",
        )
        .expect("ignored");
        fs::write(cwd.join(".git/hooks/ignored.js"), "console.log(1)\n").expect("ignored");

        let (files, warnings) = collect_formattable_files(cwd, None).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(
            relative_paths(cwd, files),
            vec!["package.js", "src/index.ts"]
        );
    }

    #[test]
    fn collect_formattable_files_skips_gitignored_directory() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::write(cwd.join(".gitignore"), "/dist/\n").expect("gitignore");
        fs::create_dir_all(cwd.join("src")).expect("src");
        fs::create_dir_all(cwd.join("dist/browser")).expect("dist");
        fs::write(cwd.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(cwd.join("dist/browser/out.js"), "export const out = 1;\n").expect("dist file");

        let (files, warnings) = collect_formattable_files(cwd, None).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_repo_root_gitignore_from_package_subdir() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path();
        let pkg = repo.join("packages/app");
        fs::write(repo.join(".gitignore"), "/packages/app/dist/\n").expect("gitignore");
        fs::create_dir_all(pkg.join("src")).expect("src");
        fs::create_dir_all(pkg.join("dist")).expect("dist");
        fs::write(pkg.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(pkg.join("dist/out.js"), "export const out = 1;\n").expect("dist file");

        let (files, warnings) = collect_formattable_files(&pkg, None).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(&pkg, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_tool_ignore_file() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::write(cwd.join(".oxfmtignore"), "generated.ts\n").expect("ignore file");
        fs::create_dir_all(cwd.join("src")).expect("src");
        fs::write(cwd.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(cwd.join("generated.ts"), "export const generated = 1;\n").expect("generated");

        let (files, warnings) = collect_formattable_files(cwd, None).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_config_ignore_patterns() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::write(
            cwd.join(".oxfmtrc.json"),
            r#"{"ignorePatterns":["generated.ts"]}"#,
        )
        .expect("config");
        fs::create_dir_all(cwd.join("src")).expect("src");
        fs::write(cwd.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(cwd.join("generated.ts"), "export const generated = 1;\n").expect("generated");

        let loaded = discover_config(cwd).expect("discover");
        let (files, warnings) =
            collect_formattable_files(cwd, loaded.ignore_matcher.as_ref()).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_parent_config_root_for_anchored_ignore_patterns() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path();
        let pkg = repo.join("packages/app");
        fs::write(
            repo.join(".oxfmtrc.json"),
            r#"{"ignorePatterns":["/packages/app/dist/"]}"#,
        )
        .expect("config");
        fs::create_dir_all(pkg.join("src")).expect("src");
        fs::create_dir_all(pkg.join("dist")).expect("dist");
        fs::write(pkg.join("src/foo.ts"), "export const foo = 1;\n").expect("src file");
        fs::write(pkg.join("dist/out.js"), "export const out = 1;\n").expect("dist file");

        let loaded = discover_config(&pkg).expect("discover");
        let (files, warnings) =
            collect_formattable_files(&pkg, loaded.ignore_matcher.as_ref()).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(&pkg, files), vec!["src/foo.ts"]);
    }

    #[test]
    fn formattable_extensions_match_worker_scope() {
        for name in [
            "a.js", "a.jsx", "a.ts", "a.tsx", "a.mjs", "a.cjs", "a.mts", "a.cts",
        ] {
            assert!(is_formattable_path(Path::new(name)), "{name}");
        }
        for name in ["a.json", "a.css", "a.d.ts", "a.map"] {
            assert!(!is_formattable_path(Path::new(name)), "{name}");
        }
    }

    #[tokio::test]
    async fn run_in_process_write_mode_rewrites_unformatted_files() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        let target = cwd.join("src/index.ts");
        let original = "export const value={foo:'bar'}\n";
        fs::write(&target, original).expect("write fixture");

        let req =
            WorkerRequest::new("pkg#format", "format").with_cwd(cwd.to_string_lossy().to_string());
        let (outcome, stdout, stderr) = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        assert_eq!(outputs, None);
        let rewritten = fs::read_to_string(&target).expect("rewritten");
        assert_ne!(rewritten, original);
        assert!(
            stdout.is_empty(),
            "stdout capture not wired in unit harness"
        );
        assert!(stderr.is_empty(), "stderr: {stderr}");
    }

    #[tokio::test]
    async fn run_in_process_check_mode_reports_nonzero_without_writing() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        let target = cwd.join("src/index.ts");
        let original = "export const value={foo:'bar'}\n";
        fs::write(&target, original).expect("write fixture");

        let mut env = HashMap::new();
        env.insert("OXFMT_OPTS".to_owned(), "--check".to_owned());
        let req = WorkerRequest::new("pkg#format", "format")
            .with_cwd(cwd.to_string_lossy().to_string())
            .with_env(env);
        let (outcome, stdout, stderr) = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 1);
        assert_eq!(outputs, None);
        assert_eq!(fs::read_to_string(&target).expect("unchanged"), original);
        assert!(
            stdout.is_empty(),
            "stdout capture not wired in unit harness"
        );
        assert!(stderr.is_empty(), "stderr: {stderr}");
    }

    #[tokio::test]
    async fn run_in_process_formatted_fixture_is_noop_in_both_modes() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        let target = cwd.join("src/index.ts");
        let formatted = "export const value = { foo: \"bar\" };\n";
        fs::write(&target, formatted).expect("write fixture");

        let write_req =
            WorkerRequest::new("pkg#format", "format").with_cwd(cwd.to_string_lossy().to_string());
        let (write_outcome, write_stdout, write_stderr) = run_worker(&write_req).await;
        let InProcessOutcome::Done { exit_code, .. } = write_outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        assert!(write_stdout.is_empty(), "stdout: {write_stdout}");
        assert!(write_stderr.is_empty(), "stderr: {write_stderr}");
        assert_eq!(
            fs::read_to_string(&target).expect("write contents"),
            formatted
        );

        let mut env = HashMap::new();
        env.insert("OXFMT_OPTS".to_owned(), "--check".to_owned());
        let check_req = WorkerRequest::new("pkg#format", "format")
            .with_cwd(cwd.to_string_lossy().to_string())
            .with_env(env);
        let (check_outcome, check_stdout, check_stderr) = run_worker(&check_req).await;
        let InProcessOutcome::Done { exit_code, .. } = check_outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        assert!(check_stdout.is_empty(), "stdout: {check_stdout}");
        assert!(check_stderr.is_empty(), "stderr: {check_stderr}");
        assert_eq!(
            fs::read_to_string(&target).expect("check contents"),
            formatted
        );
    }

    fn relative_paths(cwd: &Path, files: Vec<PathBuf>) -> Vec<String> {
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

    #[test]
    fn format_path_uses_oxfmtrc_json_single_quote() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        let config_path = cwd.join(".oxfmtrc.json");
        fs::write(&config_path, "{\"singleQuote\":true,\"unknownKey\":123}").expect("config");
        let loaded = crate::config::discover_config(cwd).expect("discover");
        let options = loaded.options_for(&cwd.join("src/example.ts"));

        let result = format_path(
            Path::new("src/example.ts"),
            "export const value = { foo: \"bar\" };\n",
            &options,
        )
        .expect("format");

        assert_eq!(result.formatted, "export const value = { foo: 'bar' };\n");
    }

    #[test]
    fn format_path_uses_oxfmtrc_jsonc_comments() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        let config_path = cwd.join(".oxfmtrc.jsonc");
        fs::write(&config_path, "{\n  // comment\n  \"semi\": false\n}\n").expect("config");
        let loaded = crate::config::discover_config(cwd).expect("discover");
        let options = loaded.options_for(&cwd.join("src/example.ts"));

        let result = format_path(
            Path::new("src/example.ts"),
            "export const value = { foo: \"bar\" };\n",
            &options,
        )
        .expect("format");

        assert_eq!(result.formatted, "export const value = { foo: \"bar\" }\n");
    }

    #[test]
    fn format_path_without_config_matches_default_options() {
        let source = "export const value={foo:'bar'}\n";
        let from_worker_default = format_path(
            Path::new("src/example.ts"),
            source,
            &crate::config::discover_config(Path::new("."))
                .expect("discover")
                .options,
        )
        .expect("format");
        let explicit_default =
            format_path(Path::new("src/example.ts"), source, &JsFormatOptions::new())
                .expect("format");

        assert_eq!(from_worker_default.formatted, explicit_default.formatted);
        assert_eq!(from_worker_default.changed, explicit_default.changed);
    }

    #[test]
    fn format_path_uses_oxfmtrc_overrides_per_file() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        let config_path = cwd.join(".oxfmtrc.json");
        // Base printWidth is 80; the *.ts override bumps it to 320. A wide array
        // literal wraps onto multiple lines at width 80 but stays on a single
        // line at width 320, so the formatted output differs per file. This
        // verifies main.rs's formatting path applies the per-file resolved
        // options rather than a single global config.
        fs::write(
            &config_path,
            "{\"printWidth\":80,\"overrides\":[{\"files\":[\"*.ts\"],\"options\":{\"printWidth\":320}}]}",
        )
        .expect("config");
        let loaded = crate::config::discover_config(cwd).expect("discover");

        let source =
            "export const value = [1111111, 2222222, 3333333, 4444444, 5555555, 6666666, 7777777];\n";

        // .ts file matches the override -> width 320 -> stays on one line.
        let ts_path = cwd.join("src/example.ts");
        let ts_result =
            format_path(&ts_path, source, &loaded.options_for(&ts_path)).expect("format ts");
        assert_eq!(
            ts_result.formatted.trim_end().lines().count(),
            1,
            "expected single-line output at width 320, got: {:?}",
            ts_result.formatted
        );

        // .mts file does not match the override -> base width 80 -> wraps.
        let base_path = cwd.join("src/example.mts");
        let base_result =
            format_path(&base_path, source, &loaded.options_for(&base_path)).expect("format base");
        assert!(
            base_result.formatted.lines().count() > 1,
            "expected wrapped output at width 80, got: {:?}",
            base_result.formatted
        );
    }

    async fn run_worker(req: &WorkerRequest) -> (InProcessOutcome, String, String) {
        let worker = OxfmtWorker;
        let sink = tokio::io::sink();
        let writer: SharedWriter = std::sync::Arc::new(tokio::sync::Mutex::new(
            Box::new(sink) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>
        ));
        let ctx = JobContext::new(req.id.clone(), writer);
        let outcome = worker.run_in_process(req, &ctx).await;
        (outcome, String::new(), String::new())
    }
}
