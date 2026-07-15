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
    process_items_in_parallel, InProcessOutcome, JobContext, ResolveResult, ResolveTask,
    TaskModification, Worker, WorkerRequest,
};
#[cfg(feature = "oxc")]
use tokio::task;

#[cfg(feature = "oxc")]
use crate::config::discover_config;
#[cfg(feature = "oxc")]
use crate::format::{format_path, relative_display};
#[cfg(feature = "oxc")]
use crate::opts::OxfmtOpts;

#[cfg_attr(not(feature = "oxc"), allow(dead_code))]
pub(crate) struct OxfmtWorker;

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
                        |path| format_file(path, &cwd, &loaded_config, opts),
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
fn format_file(
    path: &Path,
    cwd: &Path,
    loaded_config: &crate::config::LoadedConfig,
    opts: OxfmtOpts,
) -> FileOutcome {
    let relative = relative_display(cwd, path);
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            return FileOutcome::ReadError(format!("failed to read {}: {error}", path.display()));
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
}

pub(crate) fn collect_formattable_files(
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

fn should_skip_walk_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "node_modules" | ".git"))
}

pub(crate) fn is_formattable_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };
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

fn write_text_file(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

#[cfg(all(test, feature = "oxc"))]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use assert_fs::TempDir;
    use luchta_worker::{InProcessOutcome, JobContext, SharedWriter, Worker, WorkerRequest};

    use super::{collect_formattable_files, is_formattable_path, OxfmtWorker};

    fn relative_paths(cwd: &Path, paths: Vec<PathBuf>) -> Vec<String> {
        let mut out = paths
            .into_iter()
            .map(|path| {
                path.strip_prefix(cwd)
                    .expect("path within cwd")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect::<Vec<_>>();
        out.sort();
        out
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
        fs::create_dir_all(cwd.join("src/generated")).expect("src dir");
        fs::write(cwd.join(".gitignore"), "src/generated/\n").expect("gitignore");
        fs::write(cwd.join("src/generated/ignored.ts"), "export const z=3\n").expect("ignored");
        fs::write(cwd.join("src/kept.ts"), "export const kept=1\n").expect("kept");

        let (files, warnings) = collect_formattable_files(cwd, None).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/kept.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_tool_ignore_file() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        fs::write(cwd.join(".oxfmtignore"), "skip.ts\n").expect("ignore file");
        fs::write(cwd.join("src/keep.ts"), "export const keep=1\n").expect("keep");
        fs::write(cwd.join("src/skip.ts"), "export const skip=1\n").expect("skip");

        let (files, warnings) = collect_formattable_files(cwd, None).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/keep.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_config_ignore_patterns() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src/generated")).expect("src dir");
        fs::write(cwd.join("src/generated/skip.ts"), "export const skip=1\n").expect("skip");
        fs::write(cwd.join("src/keep.ts"), "export const keep=1\n").expect("keep");
        fs::write(
            cwd.join(".oxfmtrc.json"),
            "{\"ignorePatterns\":[\"src/generated/**\"]}",
        )
        .expect("config");

        let loaded = crate::config::discover_config(cwd).expect("discover");
        let (files, warnings) =
            collect_formattable_files(cwd, loaded.ignore_matcher.as_ref()).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(cwd, files), vec!["src/keep.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_repo_root_gitignore_from_package_subdir() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let pkg = root.join("packages/app");
        fs::create_dir_all(pkg.join("src/generated")).expect("src dir");
        fs::write(root.join(".gitignore"), "packages/app/src/generated/\n").expect("gitignore");
        fs::write(pkg.join("src/generated/skip.ts"), "export const skip=1\n").expect("skip");
        fs::write(pkg.join("src/keep.ts"), "export const keep=1\n").expect("keep");

        let (files, warnings) = collect_formattable_files(&pkg, None).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(&pkg, files), vec!["src/keep.ts"]);
    }

    #[test]
    fn collect_formattable_files_honors_parent_config_root_for_anchored_ignore_patterns() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let pkg = root.join("packages/app");
        fs::create_dir_all(pkg.join("src/generated")).expect("src dir");
        fs::write(
            root.join(".oxfmtrc.json"),
            "{\"ignorePatterns\":[\"packages/app/src/generated/**\"]}",
        )
        .expect("config");
        fs::write(pkg.join("src/generated/skip.ts"), "export const skip=1\n").expect("skip");
        fs::write(pkg.join("src/keep.ts"), "export const keep=1\n").expect("keep");

        let loaded = crate::config::discover_config(&pkg).expect("discover");
        let (files, warnings) =
            collect_formattable_files(&pkg, loaded.ignore_matcher.as_ref()).expect("collect");

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(relative_paths(&pkg, files), vec!["src/keep.ts"]);
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

    async fn run_with_source(
        command: &str,
        source: &str,
    ) -> (InProcessOutcome, String, String, String) {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        let target = cwd.join("src/index.ts");
        fs::write(&target, source).expect("write fixture");

        let req =
            WorkerRequest::new("pkg#format", command).with_cwd(cwd.to_string_lossy().to_string());
        let (outcome, stdout, stderr) = run_worker(&req).await;
        let contents = fs::read_to_string(&target).expect("read target");
        (outcome, stdout, stderr, contents)
    }

    async fn assert_unformatted_run(
        command: &str,
        expected_exit_code: i32,
        expect_rewritten: bool,
    ) {
        let original = "export const value={foo:'bar'}\n";
        let (outcome, stdout, stderr, contents) = run_with_source(command, original).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, expected_exit_code);
        assert_eq!(outputs, None);
        if expect_rewritten {
            assert_ne!(contents, original);
        } else {
            assert_eq!(contents, original);
        }
        assert!(
            stdout.is_empty(),
            "stdout capture not wired in unit harness"
        );
        assert!(stderr.is_empty(), "stderr: {stderr}");
    }

    #[tokio::test]
    async fn run_in_process_write_mode_rewrites_unformatted_files() {
        assert_unformatted_run("format", 0, true).await;
    }

    #[tokio::test]
    async fn run_in_process_check_mode_reports_nonzero_without_writing() {
        assert_unformatted_run("format --check", 1, false).await;
    }

    #[tokio::test]
    async fn run_in_process_fix_mode_from_command_rewrites_unformatted_files() {
        assert_unformatted_run("format --fix", 0, true).await;
    }

    #[tokio::test]
    async fn run_in_process_check_wins_over_fix_when_both_present() {
        assert_unformatted_run("format --fix --check", 1, false).await;
    }
}
