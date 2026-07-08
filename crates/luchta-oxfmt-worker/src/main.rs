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
use luchta_worker::{
    run_worker_main, InProcessOutcome, JobContext, ResolveResult, ResolveTask, TaskModification,
    Worker, WorkerRequest,
};
#[cfg(feature = "oxc")]
use tokio::task;

#[cfg(feature = "oxc")]
use crate::config::discover_config;
#[cfg(feature = "oxc")]
use crate::format::{format_path, relative_display};

#[cfg(feature = "oxc")]
struct OxfmtWorker;

#[cfg(feature = "oxc")]
impl Worker for OxfmtWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let mut inputs = BTreeSet::from([
            "package.json".to_owned(),
            "src/**".to_owned(),
            ".oxfmtrc.json".to_owned(),
            ".oxfmtrc.jsonc".to_owned(),
        ]);
        inputs.extend(req.inputs.iter().cloned());

        let Some(cwd) = req.cwd.as_deref() else {
            return ResolveResult::modify(TaskModification {
                inputs: Some(inputs.into_iter().collect()),
                ..TaskModification::default()
            });
        };

        let cwd = Path::new(cwd);
        let files = match collect_formattable_files(cwd) {
            Ok(files) => files,
            Err(error) => return ResolveResult::reject(error),
        };

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
            let files = match collect_formattable_files(&cwd) {
                Ok(files) => files,
                Err(error) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };
            let format_options = match task::spawn_blocking({
                let cwd = cwd.clone();
                move || discover_config(&cwd)
            })
            .await
            {
                Ok(Ok(config)) => config.options,
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

            if files.is_empty() {
                return InProcessOutcome::Done {
                    exit_code: 0,
                    outputs: None,
                };
            }

            let mut exit_code = 0;
            let mut would_reformat = false;

            for path in files {
                let source = match fs::read_to_string(&path) {
                    Ok(source) => source,
                    Err(error) => {
                        let _ = ctx
                            .emit_stderr(format!("failed to read {}: {error}", path.display()))
                            .await;
                        exit_code = 1;
                        continue;
                    }
                };
                let relative = relative_display(&cwd, &path);
                let path_for_blocking = path.clone();
                let source_for_blocking = source.clone();
                let options_for_blocking = format_options.clone();
                let result = match task::spawn_blocking(move || {
                    format_path(
                        &path_for_blocking,
                        &source_for_blocking,
                        &options_for_blocking,
                    )
                })
                .await
                {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => {
                        let _ = ctx.emit_stderr(error).await;
                        exit_code = 1;
                        continue;
                    }
                    Err(error) => {
                        let _ = ctx
                            .emit_stderr(format!("format task failed for {relative}: {error}"))
                            .await;
                        exit_code = 1;
                        continue;
                    }
                };

                if !result.changed {
                    continue;
                }

                if opts.check {
                    would_reformat = true;
                    if let Err(error) = ctx.emit_stdout(format!("would reformat: {relative}")).await
                    {
                        let _ = ctx
                            .emit_stderr(format!("failed to emit formatter log: {error}"))
                            .await;
                        return InProcessOutcome::Done {
                            exit_code: 1,
                            outputs: None,
                        };
                    }
                    continue;
                }

                if let Err(error) = write_text_file(&path, &result.formatted) {
                    let _ = ctx.emit_stderr(error).await;
                    exit_code = 1;
                    continue;
                }
                if let Err(error) = ctx.emit_stdout(format!("reformatted: {relative}")).await {
                    let _ = ctx
                        .emit_stderr(format!("failed to emit formatter log: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
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
fn collect_formattable_files(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();

    for relative in [Path::new("src"), Path::new("")] {
        let root = cwd.join(relative);
        if !root.exists() {
            continue;
        }
        if root.is_file() {
            if is_formattable_path(&root) {
                files.push(root);
            }
            continue;
        }
        collect_from_dir(&root, &mut files)?;
    }

    files.sort();
    files.dedup();
    Ok(files)
}

#[cfg(feature = "oxc")]
fn collect_from_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current)
            .map_err(|error| format!("failed to read directory {}: {error}", current.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "failed to read directory entry in {}: {error}",
                    current.display()
                )
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                format!("failed to read file type for {}: {error}", path.display())
            })?;
            if file_type.is_dir() {
                if should_skip_dir(&path) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() && is_formattable_path(&path) {
                files.push(path);
            }
        }
    }

    Ok(())
}

#[cfg(feature = "oxc")]
fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "node_modules" | ".git"))
}

#[cfg(feature = "oxc")]
fn is_formattable_path(path: &Path) -> bool {
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

#[cfg(feature = "oxc")]
fn write_text_file(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

#[cfg(feature = "oxc")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    run_worker_main(OxfmtWorker).await;
}

#[cfg(not(feature = "oxc"))]
fn main() {
    eprintln!("this binary was built without the 'oxc' feature; the oxfmt worker is unavailable");
    std::process::exit(1);
}

#[cfg(all(test, feature = "oxc"))]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    use assert_fs::TempDir;
    use luchta_worker::{InProcessOutcome, JobContext, SharedWriter, Worker, WorkerRequest};
    use oxc_formatter::JsFormatOptions;

    use super::{collect_formattable_files, is_formattable_path, OxfmtOpts, OxfmtWorker};
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

        let files = collect_formattable_files(cwd).expect("collect");
        let rel: Vec<_> = files
            .into_iter()
            .map(|path| {
                path.strip_prefix(cwd)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rel, vec!["package.js", "src/index.ts"]);
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

    #[test]
    fn format_path_uses_oxfmtrc_json_single_quote() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        let config_path = cwd.join(".oxfmtrc.json");
        fs::write(&config_path, "{\"singleQuote\":true,\"unknownKey\":123}").expect("config");
        let options = crate::config::discover_config(cwd)
            .expect("discover")
            .options;

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
        let options = crate::config::discover_config(cwd)
            .expect("discover")
            .options;

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
