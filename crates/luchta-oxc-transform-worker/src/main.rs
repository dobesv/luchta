#[cfg(feature = "oxc")]
mod transform;

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
    process_items_in_parallel, run_worker_main, InProcessOutcome, JobContext, ResolveResult,
    ResolveTask, TaskModification, Worker, WorkerRequest,
};
#[cfg(feature = "oxc")]
use tokio::task;

#[cfg(feature = "oxc")]
struct FileOutcome {
    produced: Vec<String>,
    errors: Vec<String>,
    failed: bool,
}
#[cfg(feature = "oxc")]
use crate::transform::{
    derive_env_name, is_transformable, output_path_for, relative_source_map_source_path,
    resolve_target_env, should_skip, source_map_output_path, source_mapping_url, transform_source,
};

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
        .block_on(async { run_worker_main(OxcTransformWorker).await });
}

#[cfg(not(feature = "oxc"))]
fn real_main() {
    eprintln!(
        "this binary was built without the 'oxc' feature; the oxc transform worker is unavailable"
    );
    std::process::exit(1);
}

#[cfg_attr(not(feature = "oxc"), allow(dead_code))]
struct OxcTransformWorker;

#[cfg(feature = "oxc")]
impl Worker for OxcTransformWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let mut inputs = BTreeSet::from([
            "package.json".to_owned(),
            "src/**".to_owned(),
            "babel.config.json".to_owned(),
            "babel.config.js".to_owned(),
            "babel.config.cjs".to_owned(),
        ]);
        inputs.extend(req.inputs.iter().cloned());

        let Some(cwd) = req.cwd.as_deref() else {
            return ResolveResult::modify(TaskModification {
                inputs: Some(inputs.into_iter().collect()),
                ..TaskModification::default()
            });
        };

        let src_root = Path::new(cwd).join("src");
        if !src_root.exists() {
            return ResolveResult::prune(Some(
                "no src directory found for oxc transform".to_owned(),
            ));
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
                    .emit_stderr("oxc transform worker requires cwd".to_owned())
                    .await;
                return InProcessOutcome::Done {
                    exit_code: 1,
                    outputs: None,
                };
            };

            let cwd = PathBuf::from(cwd);
            let src_root = cwd.join("src");
            if !src_root.exists() {
                return InProcessOutcome::Done {
                    exit_code: 0,
                    outputs: None,
                };
            }

            let env_name = derive_env_name(&req.id);
            let target_env = resolve_target_env(&env_name).to_owned();
            let out_root = cwd.join("dist").join(&env_name);
            if let Err(error) = fs::create_dir_all(&out_root) {
                let _ = ctx
                    .emit_stderr(format!("failed to create {}: {error}", out_root.display()))
                    .await;
                return InProcessOutcome::Done {
                    exit_code: 1,
                    outputs: None,
                };
            }

            let entries = match collect_src_entries(&src_root) {
                Ok(entries) => entries,
                Err(error) => {
                    let _ = ctx.emit_stderr(error).await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            let mut produced = BTreeSet::new();
            let mut exit_code = 0;
            let outcomes = match task::spawn_blocking({
                let cwd = cwd.clone();
                let src_root = src_root.clone();
                let out_root = out_root.clone();
                let target_env = target_env.clone();
                let entries = entries.clone();
                move || {
                    process_items_in_parallel(
                        &entries,
                        "oxc transform worker parallel transform thread panicked",
                        |source_path| {
                            let mut produced = Vec::new();
                            let mut errors = Vec::new();
                            let mut failed = false;

                            let relative = match source_path.strip_prefix(&src_root) {
                                Ok(relative) => relative,
                                Err(error) => {
                                    errors.push(format!(
                                        "failed to strip src root from {}: {error}",
                                        source_path.display()
                                    ));
                                    failed = true;
                                    return FileOutcome {
                                        produced,
                                        errors,
                                        failed,
                                    };
                                }
                            };

                            if should_skip(relative) {
                                return FileOutcome {
                                    produced,
                                    errors,
                                    failed,
                                };
                            }

                            let output_path = if is_transformable(source_path) {
                                match output_path_for(&src_root, &out_root, source_path) {
                                    Ok(output_path) => output_path,
                                    Err(error) => {
                                        errors.push(error);
                                        failed = true;
                                        return FileOutcome {
                                            produced,
                                            errors,
                                            failed,
                                        };
                                    }
                                }
                            } else {
                                out_root.join(relative)
                            };
                            if let Some(parent) = output_path.parent() {
                                if let Err(error) = fs::create_dir_all(parent) {
                                    errors.push(format!(
                                        "failed to create {}: {error}",
                                        parent.display()
                                    ));
                                    failed = true;
                                    return FileOutcome {
                                        produced,
                                        errors,
                                        failed,
                                    };
                                }
                            }

                            if is_transformable(source_path) {
                                let source = match fs::read_to_string(source_path) {
                                    Ok(source) => source,
                                    Err(error) => {
                                        errors.push(format!(
                                            "failed to read {}: {error}",
                                            source_path.display()
                                        ));
                                        failed = true;
                                        return FileOutcome {
                                            produced,
                                            errors,
                                            failed,
                                        };
                                    }
                                };
                                let source_map_path = source_map_output_path(&output_path);
                                let source_map_source_path =
                                    relative_source_map_source_path(&cwd, source_path);
                                let source_mapping_url = match source_mapping_url(&source_map_path)
                                {
                                    Ok(source_mapping_url) => source_mapping_url,
                                    Err(error) => {
                                        errors.push(error);
                                        failed = true;
                                        return FileOutcome {
                                            produced,
                                            errors,
                                            failed,
                                        };
                                    }
                                };
                                let result = match transform_source(
                                    source_path,
                                    &source,
                                    &target_env,
                                    &source_map_source_path,
                                    &source_mapping_url,
                                ) {
                                    Ok(result) => result,
                                    Err(errors_from_transform) => {
                                        errors.extend(errors_from_transform);
                                        failed = true;
                                        return FileOutcome {
                                            produced,
                                            errors,
                                            failed,
                                        };
                                    }
                                };
                                // `transform_source` already appends the `//# sourceMappingURL=...`
                                // line to `result.code` (via `source_mapping_url`), so write it as-is.
                                if let Err(error) = fs::write(&output_path, &result.code) {
                                    errors.push(format!(
                                        "failed to write {}: {error}",
                                        output_path.display()
                                    ));
                                    failed = true;
                                    return FileOutcome {
                                        produced,
                                        errors,
                                        failed,
                                    };
                                }
                                produced.push(normalize_path(
                                    output_path.strip_prefix(&cwd).unwrap_or(&output_path),
                                ));
                                if let Some(source_map_json) = result.source_map_json {
                                    if let Err(error) = fs::write(&source_map_path, source_map_json)
                                    {
                                        errors.push(format!(
                                            "failed to write {}: {error}",
                                            source_map_path.display()
                                        ));
                                        failed = true;
                                        return FileOutcome {
                                            produced,
                                            errors,
                                            failed,
                                        };
                                    }
                                    produced.push(normalize_path(
                                        source_map_path
                                            .strip_prefix(&cwd)
                                            .unwrap_or(&source_map_path),
                                    ));
                                }
                            } else if let Err(error) = fs::copy(source_path, &output_path) {
                                errors.push(format!(
                                    "failed to copy {} to {}: {error}",
                                    source_path.display(),
                                    output_path.display()
                                ));
                                failed = true;
                                return FileOutcome {
                                    produced,
                                    errors,
                                    failed,
                                };
                            } else {
                                produced.push(normalize_path(
                                    output_path.strip_prefix(&cwd).unwrap_or(&output_path),
                                ));
                            }

                            FileOutcome {
                                produced,
                                errors,
                                failed,
                            }
                        },
                    )
                }
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
                        .emit_stderr(format!("oxc transform parallel task failed: {error}"))
                        .await;
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            for outcome in outcomes {
                for error in outcome.errors {
                    let _ = ctx.emit_stderr(error).await;
                }
                if outcome.failed {
                    exit_code = 1;
                }
                produced.extend(outcome.produced);
            }

            if let Err(error) = cleanup_extra_files(&cwd, &out_root, &produced) {
                let _ = ctx.emit_stderr(error).await;
                exit_code = 1;
            }

            let outputs = if exit_code == 0 {
                Some(produced.into_iter().collect())
            } else {
                None
            };
            InProcessOutcome::Done { exit_code, outputs }
        }
    }
}

#[cfg(feature = "oxc")]
fn collect_src_entries(src_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut stack = vec![src_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|error| format!("failed to read directory {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "failed to read directory entry in {}: {error}",
                    dir.display()
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
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }

    files.sort();
    Ok(files)
}

#[cfg(feature = "oxc")]
fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "node_modules" | ".git"))
}

#[cfg(feature = "oxc")]
fn cleanup_extra_files(
    cwd: &Path,
    out_root: &Path,
    produced: &BTreeSet<String>,
) -> Result<(), String> {
    if !out_root.exists() {
        return Ok(());
    }

    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut stack = vec![out_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        dirs.push(dir.clone());
        let entries = fs::read_dir(&dir)
            .map_err(|error| format!("failed to read directory {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "failed to read directory entry in {}: {error}",
                    dir.display()
                )
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                format!("failed to read file type for {}: {error}", path.display())
            })?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }

    for path in files {
        let relative = normalize_path(path.strip_prefix(cwd).unwrap_or(&path));
        let extension = path.extension().and_then(OsStr::to_str);
        if produced.contains(&relative) || !matches!(extension, Some("js" | "map")) {
            continue;
        }
        fs::remove_file(&path)
            .map_err(|error| format!("failed to remove stale {}: {error}", path.display()))?;
    }

    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in dirs {
        if dir == out_root {
            continue;
        }
        if fs::read_dir(&dir)
            .map_err(|error| format!("failed to read directory {}: {error}", dir.display()))?
            .next()
            .is_none()
        {
            fs::remove_dir(&dir).map_err(|error| {
                format!(
                    "failed to remove empty directory {}: {error}",
                    dir.display()
                )
            })?;
        }
    }

    Ok(())
}

#[cfg(feature = "oxc")]
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(all(test, feature = "oxc"))]
mod tests {
    use assert_fs::TempDir;
    use luchta_worker::{InProcessOutcome, JobContext, SharedWriter, Worker, WorkerRequest};
    use std::fs;

    use super::OxcTransformWorker;

    #[tokio::test]
    async fn run_in_process_transforms_source_and_reports_outputs() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src/nested")).expect("src dir");
        fs::write(
            cwd.join("src/nested/example.ts"),
            "export const value: number = 1;\n",
        )
        .expect("write source");

        let req = WorkerRequest::new("pkg#build:node", "build:node")
            .with_cwd(cwd.to_string_lossy().to_string());
        let outcome = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        let outputs = outputs.expect("outputs");
        assert_eq!(
            outputs,
            vec![
                "dist/node/nested/example.js",
                "dist/node/nested/example.js.map",
            ]
        );
        let built =
            fs::read_to_string(cwd.join("dist/node/nested/example.js")).expect("built file");
        assert!(built.contains("export const value = 1;"));
        assert!(built.ends_with("\n//# sourceMappingURL=example.js.map\n"));
        let source_map: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(cwd.join("dist/node/nested/example.js.map")).expect("source map"),
        )
        .expect("valid source map json");
        assert_eq!(source_map["version"], 3);
        assert!(source_map["mappings"]
            .as_str()
            .is_some_and(|mappings| !mappings.is_empty()));
        assert_eq!(
            source_map["sources"],
            serde_json::json!(["src/nested/example.ts"])
        );
    }

    #[tokio::test]
    async fn run_in_process_copies_assets_and_cleans_stale_js_outputs() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src/assets")).expect("src assets");
        fs::create_dir_all(cwd.join("dist/js/stale")).expect("stale dir");
        fs::write(cwd.join("src/index.js"), "export const value = 1;\n").expect("write source");
        fs::write(cwd.join("src/assets/logo.svg"), "<svg />\n").expect("write asset");
        fs::write(cwd.join("dist/js/stale/old.js"), "old\n").expect("write stale js");
        fs::write(cwd.join("dist/js/stale/old.js.map"), "old map\n").expect("write stale map");
        fs::write(cwd.join("dist/js/stale/keep.txt"), "keep\n").expect("write stale asset");

        let req =
            WorkerRequest::new("pkg#build", "build").with_cwd(cwd.to_string_lossy().to_string());
        let outcome = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        let outputs = outputs.expect("outputs");
        assert_eq!(
            outputs,
            vec![
                "dist/js/assets/logo.svg",
                "dist/js/index.js",
                "dist/js/index.js.map",
            ],
            "outputs should be sorted cwd-relative paths"
        );
        assert_eq!(
            fs::read_to_string(cwd.join("dist/js/assets/logo.svg")).expect("copied asset"),
            "<svg />\n"
        );
        let emitted_js = fs::read_to_string(cwd.join("dist/js/index.js")).expect("emitted js");
        assert!(
            emitted_js.ends_with("\n//# sourceMappingURL=index.js.map\n"),
            "js should include sourceMappingURL"
        );
        let source_map: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(cwd.join("dist/js/index.js.map")).expect("source map"),
        )
        .expect("valid source map json");
        assert_eq!(source_map["version"], 3);
        assert!(source_map["mappings"]
            .as_str()
            .is_some_and(|mappings| !mappings.is_empty()));
        assert_eq!(source_map["sources"], serde_json::json!(["src/index.js"]));
        assert!(
            !cwd.join("dist/js/stale/old.js").exists(),
            "stale js removed"
        );
        assert!(
            !cwd.join("dist/js/stale/old.js.map").exists(),
            "stale sourcemap removed"
        );
        assert!(
            cwd.join("dist/js/stale/keep.txt").exists(),
            "non-js asset preserved"
        );
    }

    #[tokio::test]
    async fn run_in_process_skips_story_and_test_sources() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        fs::write(
            cwd.join("src/button.stories.tsx"),
            "export const Story = {};\n",
        )
        .expect("write story");
        fs::write(
            cwd.join("src/widget.unitTest.ts"),
            "export const spec = 1;\n",
        )
        .expect("write unit test");
        fs::write(cwd.join("src/real.ts"), "export const real: number = 1;\n")
            .expect("write real source");

        let req =
            WorkerRequest::new("pkg#build", "build").with_cwd(cwd.to_string_lossy().to_string());
        let outcome = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        assert_eq!(
            outputs.expect("outputs"),
            vec!["dist/js/real.js", "dist/js/real.js.map"]
        );
        assert!(!cwd.join("dist/js/button.stories.js").exists());
        assert!(!cwd.join("dist/js/widget.unitTest.js").exists());
        assert!(cwd.join("dist/js/real.js").exists());
    }

    async fn run_worker(req: &WorkerRequest) -> InProcessOutcome {
        let worker = OxcTransformWorker;
        let sink = tokio::io::sink();
        let writer: SharedWriter = std::sync::Arc::new(tokio::sync::Mutex::new(
            Box::new(sink) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>
        ));
        let ctx = JobContext::new(req.id.clone(), writer);
        worker.run_in_process(req, &ctx).await
    }
}
