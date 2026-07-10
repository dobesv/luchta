#[cfg(feature = "swc")]
mod args;
#[cfg(feature = "swc")]
mod transform;

#[cfg(feature = "swc")]
use std::collections::BTreeSet;
#[cfg(feature = "swc")]
use std::ffi::OsStr;
#[cfg(feature = "swc")]
use std::fs;
#[cfg(feature = "swc")]
use std::path::{Path, PathBuf};

#[cfg(feature = "swc")]
use luchta_worker::{
    run_worker_main, InProcessOutcome, JobContext, ResolveResult, ResolveTask, TaskModification,
    Worker, WorkerRequest,
};
#[cfg(feature = "swc")]
use tokio::task;

#[cfg(feature = "swc")]
use crate::args::SwcArgs;
#[cfg(feature = "swc")]
use crate::transform::{
    is_transformable, output_path_for, relative_source_map_source_path, should_skip,
    source_map_output_path, source_mapping_url, transform_source,
};

#[cfg(feature = "swc")]
struct SwcTransformWorker;

#[cfg(feature = "swc")]
impl Worker for SwcTransformWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        // Keep package-local `.swcrc` precise, plus workspace-root glob for any
        // ancestor/root `.swcrc` SWC may crawl into (`find_swcrc` parity; ref note e9b11d7a).
        let mut inputs = BTreeSet::from([
            "package.json".to_owned(),
            "src/**".to_owned(),
            ".swcrc".to_owned(),
            "#**/.swcrc".to_owned(),
        ]);

        if let Some(cwd) = req.cwd.as_deref() {
            if let Ok(args) = SwcArgs::parse(&req.command, Some(Path::new(cwd))) {
                args.add_config_file_input(Path::new(cwd), &mut inputs);
            }
        }

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
                "no src directory found for swc transform".to_owned(),
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
                    .emit_stderr("swc transform worker requires cwd".to_owned())
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

            let args = match SwcArgs::parse(&req.command, Some(&cwd)) {
                Ok(args) => args,
                Err(errors) => {
                    for error in errors {
                        let _ = ctx.emit_stderr(error).await;
                    }
                    return InProcessOutcome::Done {
                        exit_code: 1,
                        outputs: None,
                    };
                }
            };

            let out_root = args.out_root(&cwd);
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

            for source_path in entries {
                let relative = match source_path.strip_prefix(&cwd) {
                    Ok(relative) => normalize_path(relative),
                    Err(_) => normalize_path(&source_path),
                };

                if should_skip(&source_path) {
                    continue;
                }

                let output_path = if is_transformable(&source_path) {
                    match output_path_for(&src_root, &out_root, &source_path) {
                        Ok(output_path) => output_path,
                        Err(error) => {
                            let _ = ctx.emit_stderr(error).await;
                            exit_code = 1;
                            continue;
                        }
                    }
                } else {
                    match source_path.strip_prefix(&src_root) {
                        Ok(relative) => out_root.join(relative),
                        Err(error) => {
                            let _ = ctx
                                .emit_stderr(format!(
                                    "failed to derive relative path for {}: {error}",
                                    source_path.display()
                                ))
                                .await;
                            exit_code = 1;
                            continue;
                        }
                    }
                };
                if let Some(parent) = output_path.parent() {
                    if let Err(error) = fs::create_dir_all(parent) {
                        let _ = ctx
                            .emit_stderr(format!("failed to create {}: {error}", parent.display()))
                            .await;
                        exit_code = 1;
                        continue;
                    }
                }

                if is_transformable(&source_path) {
                    let source = match fs::read_to_string(&source_path) {
                        Ok(source) => source,
                        Err(error) => {
                            let _ = ctx
                                .emit_stderr(format!(
                                    "failed to read {}: {error}",
                                    source_path.display()
                                ))
                                .await;
                            exit_code = 1;
                            continue;
                        }
                    };
                    let source_map_path = source_map_output_path(&output_path);
                    let source_map_source_path =
                        relative_source_map_source_path(&cwd, &source_path);
                    let source_mapping_url = match source_mapping_url(&source_map_path) {
                        Ok(source_mapping_url) => source_mapping_url,
                        Err(error) => {
                            let _ = ctx.emit_stderr(error).await;
                            exit_code = 1;
                            continue;
                        }
                    };
                    let source_path_for_task = source_path.clone();
                    let args_for_task = args.clone();
                    let result = match task::spawn_blocking(move || {
                        transform_source(
                            &args_for_task,
                            &source_path_for_task,
                            &source,
                            &source_map_source_path,
                            &source_mapping_url,
                        )
                    })
                    .await
                    {
                        Ok(Ok(result)) => result,
                        Ok(Err(errors)) => {
                            for error in errors {
                                let _ = ctx.emit_stderr(error).await;
                            }
                            exit_code = 1;
                            continue;
                        }
                        Err(error) => {
                            let _ = ctx
                                .emit_stderr(format!(
                                    "transform task failed for {relative}: {error}"
                                ))
                                .await;
                            exit_code = 1;
                            continue;
                        }
                    };
                    if let Err(error) = fs::write(&output_path, &result.code) {
                        let _ = ctx
                            .emit_stderr(format!(
                                "failed to write {}: {error}",
                                output_path.display()
                            ))
                            .await;
                        exit_code = 1;
                        continue;
                    }
                    produced.insert(normalize_path(
                        output_path.strip_prefix(&cwd).unwrap_or(&output_path),
                    ));
                    if let Some(source_map_json) = result.source_map_json {
                        if let Err(error) = fs::write(&source_map_path, source_map_json) {
                            let _ = ctx
                                .emit_stderr(format!(
                                    "failed to write {}: {error}",
                                    source_map_path.display()
                                ))
                                .await;
                            exit_code = 1;
                            continue;
                        }
                        produced.insert(normalize_path(
                            source_map_path
                                .strip_prefix(&cwd)
                                .unwrap_or(&source_map_path),
                        ));
                    }
                } else {
                    if let Err(error) = fs::copy(&source_path, &output_path) {
                        let _ = ctx
                            .emit_stderr(format!(
                                "failed to copy {} to {}: {error}",
                                source_path.display(),
                                output_path.display()
                            ))
                            .await;
                        exit_code = 1;
                        continue;
                    }
                    produced.insert(normalize_path(
                        output_path.strip_prefix(&cwd).unwrap_or(&output_path),
                    ));
                }
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

#[cfg(feature = "swc")]
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

#[cfg(feature = "swc")]
fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "node_modules" | ".git"))
}

#[cfg(feature = "swc")]
fn should_keep_stale(path: &Path) -> bool {
    !matches!(path.extension().and_then(OsStr::to_str), Some("js" | "map"))
}

#[cfg(feature = "swc")]
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
        if produced.contains(&relative) || should_keep_stale(&path) {
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

#[cfg(feature = "swc")]
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(feature = "swc")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    run_worker_main(SwcTransformWorker).await;
}

#[cfg(not(feature = "swc"))]
fn main() {}

#[cfg(all(test, feature = "swc"))]
mod tests {
    use assert_fs::TempDir;
    use luchta_worker::{
        InProcessOutcome, JobContext, ResolveDecision, ResolveMode, ResolveTask, SharedWriter,
        Worker, WorkerRequest,
    };
    use std::fs;
    use std::path::Path;

    use super::SwcTransformWorker;

    #[test]
    fn resolve_task_includes_local_workspace_and_config_file_inputs() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("src")).expect("src dir");

        let result = SwcTransformWorker.resolve_task(&resolve_task(
            temp.path(),
            "--config-file configs/shared.swcrc",
        ));

        let ResolveDecision::Modify(modification) = result.decision else {
            panic!("expected modify decision");
        };
        let inputs = modification.inputs.expect("inputs");
        assert!(inputs.contains(&"package.json".to_owned()));
        assert!(inputs.contains(&"src/**".to_owned()));
        assert!(inputs.contains(&".swcrc".to_owned()));
        assert!(inputs.contains(&"#**/.swcrc".to_owned()));
        assert!(inputs.contains(&"configs/shared.swcrc".to_owned()));
    }

    #[test]
    fn resolve_task_prunes_without_src_directory() {
        let temp = TempDir::new().expect("tempdir");

        let result = SwcTransformWorker.resolve_task(&resolve_task(temp.path(), ""));

        match result.decision {
            ResolveDecision::Prune { reason } => {
                assert_eq!(
                    reason.as_deref(),
                    Some("no src directory found for swc transform")
                );
            }
            decision => panic!("expected prune decision, got {decision:?}"),
        }
    }

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
            vec!["dist/js/nested/example.js", "dist/js/nested/example.js.map"]
        );
        let emitted_js =
            fs::read_to_string(cwd.join("dist/js/nested/example.js")).expect("emitted js");
        assert!(emitted_js.contains("value = 1"));
        assert!(
            emitted_js.ends_with("\n//# sourceMappingURL=example.js.map\n"),
            "js should include sourceMappingURL"
        );
        let source_map: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(cwd.join("dist/js/nested/example.js.map")).expect("source map"),
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
    async fn run_in_process_honors_out_dir_flag() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        fs::write(
            cwd.join("src/index.ts"),
            "export const value: number = 1;\n",
        )
        .expect("write source");

        let req = WorkerRequest::new("pkg#build", "--out-dir dist/node")
            .with_cwd(cwd.to_string_lossy().to_string());
        let outcome = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        assert_eq!(
            outputs.expect("outputs"),
            vec!["dist/node/index.js", "dist/node/index.js.map"]
        );
        assert!(cwd.join("dist/node/index.js").exists());
        assert!(!cwd.join("dist/js/index.js").exists());
    }

    #[tokio::test]
    async fn run_in_process_copies_assets_and_removes_stale_js_outputs() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src/icons")).expect("src dir");
        fs::create_dir_all(cwd.join("dist/js/stale")).expect("dist dir");
        fs::write(
            cwd.join("src/index.ts"),
            "export const answer: number = 42;\n",
        )
        .expect("write source");
        fs::write(cwd.join("src/icons/logo.svg"), "<svg />\n").expect("write asset");
        fs::write(cwd.join("dist/js/stale/old.js"), "old\n").expect("write stale js");
        fs::write(cwd.join("dist/js/stale/old.js.map"), "{}\n").expect("write stale map");
        fs::write(cwd.join("dist/js/stale/keep.txt"), "keep\n").expect("write keep asset");

        let req =
            WorkerRequest::new("pkg#build", "build").with_cwd(cwd.to_string_lossy().to_string());
        let outcome = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_eq!(exit_code, 0);
        assert_eq!(
            outputs.expect("outputs"),
            vec![
                "dist/js/icons/logo.svg",
                "dist/js/index.js",
                "dist/js/index.js.map"
            ]
        );
        assert_eq!(
            fs::read_to_string(cwd.join("dist/js/icons/logo.svg")).expect("copied asset"),
            "<svg />\n"
        );
        let emitted_js = fs::read_to_string(cwd.join("dist/js/index.js")).expect("emitted js");
        assert!(emitted_js.contains("answer = 42"));
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
        assert_eq!(source_map["sources"], serde_json::json!(["src/index.ts"]));
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
    async fn run_in_process_returns_failure_for_invalid_source() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        fs::create_dir_all(cwd.join("src")).expect("src dir");
        fs::write(cwd.join("src/bad.ts"), "export const broken = ;\n").expect("write bad source");

        let req =
            WorkerRequest::new("pkg#build", "build").with_cwd(cwd.to_string_lossy().to_string());
        let outcome = run_worker(&req).await;

        let InProcessOutcome::Done { exit_code, outputs } = outcome else {
            panic!("expected done outcome");
        };
        assert_ne!(exit_code, 0);
        assert!(outputs.is_none());
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

    fn resolve_task(cwd: &Path, command: &str) -> ResolveTask {
        ResolveTask {
            id: "pkg#build".to_owned(),
            name: "build".to_owned(),
            command: command.to_owned(),
            package: "pkg".to_owned(),
            cwd: Some(cwd.display().to_string()),
            scripts: vec![],
            inputs: vec![],
            mode: ResolveMode::Run,
        }
    }

    async fn run_worker(req: &WorkerRequest) -> InProcessOutcome {
        let worker = SwcTransformWorker;
        let sink = tokio::io::sink();
        let writer: SharedWriter = std::sync::Arc::new(tokio::sync::Mutex::new(
            Box::new(sink) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>
        ));
        let ctx = JobContext::new(req.id.clone(), writer);
        worker.run_in_process(req, &ctx).await
    }
}
