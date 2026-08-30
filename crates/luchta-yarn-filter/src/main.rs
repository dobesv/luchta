use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use luchta_worker::{
    run_concurrent_middleware, split_current_process_argv, version_requested,
    write_worker_response, DelegateHandle, ProxyError, ResolveResult, ResolveTask, SharedWriter,
    WorkerMessage, WorkerResponse,
};
use serde_json::Value;
use tokio::io::stdout;
use tokio::sync::Mutex;

/// Bound on how long the delegate may take to answer a forwarded `resolve`.
/// `resolve` runs during graph build and must be fast; a delegate that is alive
/// but never responds would otherwise hang the whole build. A timeout fails the
/// worker.
const RESOLVE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
struct Config {
    scripts: Vec<String>,
    dependencies: Vec<String>,
}

fn main() {
    let exit_code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async_main()),
        Err(error) => {
            eprintln!("failed to build tokio runtime: {error}");
            1
        }
    };

    if exit_code != 0 {
        process::exit(exit_code);
    }
}

async fn async_main() -> i32 {
    let argv = split_current_process_argv();
    if version_requested(
        &argv.stage_args,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return 0;
    }
    let stage_args = argv.stage_args.into_iter().skip(1).collect::<Vec<_>>();
    let usage = "usage: luchta-yarn-filter [--script NAME]... [--dependency NAME]... -- <delegate command...>";

    let config = match parse_stage_args(&stage_args) {
        Ok(config) => Arc::new(config),
        Err(error) => {
            eprintln!("{error}; {usage}");
            return 1;
        }
    };

    if argv.delegate_command.is_empty() {
        eprintln!("missing delegate command; {usage}");
        return 1;
    }

    let stdout_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let delegate = Arc::new(DelegateHandle::with_writers(
        argv.delegate_command,
        Arc::clone(&stdout_writer),
        Arc::new(Mutex::new(Box::new(tokio::io::stderr()))),
        Some("delegate stderr: ".to_owned()),
    ));

    let mut exit_code = 0;
    let dispatch_config = Arc::clone(&config);
    let dispatch_delegate = Arc::clone(&delegate);
    let dispatch_writer = Arc::clone(&stdout_writer);
    if let Err(error) = run_concurrent_middleware(move |message| {
        let config = Arc::clone(&dispatch_config);
        let delegate = Arc::clone(&dispatch_delegate);
        let stdout_writer = Arc::clone(&dispatch_writer);
        async move { dispatch_message(config, &delegate, &stdout_writer, message).await }
    })
    .await
    {
        eprintln!("{error}");
        exit_code = 1;
    }

    if let Err(error) = delegate.shutdown().await {
        eprintln!(
            "failed to shut down delegate: command={:?}, error={}",
            delegate.delegate_command(),
            error
        );
        exit_code = 1;
    }

    exit_code
}

async fn dispatch_message(
    config: Arc<Config>,
    delegate: &DelegateHandle,
    stdout_writer: &SharedWriter,
    message: WorkerMessage,
) -> Result<(), String> {
    match message {
        WorkerMessage::ResolveTask(resolve) => {
            let request_id = resolve.id.clone();
            let (resolve, keep) = tokio::task::spawn_blocking(move || {
                let keep = should_keep(&config, &resolve);
                (resolve, keep)
            })
            .await
            .map_err(|error| format!("yarn filter task failed: {error}"))?;

            if keep {
                if let Err(error) = delegate
                    .send_with_timeout(WorkerMessage::ResolveTask(resolve), RESOLVE_FORWARD_TIMEOUT)
                    .await
                {
                    return Err(delegate
                        .failure_message("delegate failed before resolve decision", error)
                        .await);
                }
                Ok(())
            } else {
                let response = WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                write_worker_response(stdout_writer, &response)
                    .await
                    .map_err(|error| format!("failed to write prune response: {error}"))
            }
        }
        WorkerMessage::Run(request) => match delegate.send(WorkerMessage::Run(request)).await {
            Ok(_) => Ok(()),
            Err(error) => Err(delegate.failure_message("delegate failed", error).await),
        },
    }
}

fn parse_stage_args(args: &[String]) -> Result<Config, String> {
    let mut scripts = Vec::new();
    let mut dependencies = Vec::new();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--script" => {
                index += 1;
                let Some(name) = args.get(index) else {
                    return Err("missing value for --script".to_owned());
                };
                scripts.push(name.clone());
            }
            "--dependency" => {
                index += 1;
                let Some(name) = args.get(index) else {
                    return Err("missing value for --dependency".to_owned());
                };
                dependencies.push(name.clone());
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
        index += 1;
    }

    Ok(Config {
        scripts,
        dependencies,
    })
}

fn should_keep(config: &Config, resolve: &ResolveTask) -> bool {
    // The filter checks for the task's own script name, not the resolved
    // `command`. The `command` exists for the underlying worker to run; using it
    // here would make the filter drop tasks whose `command` overrides the script
    // name even though the package declares the named script.
    if should_check_default_script(config) && !has_script(resolve, &resolve.name) {
        return false;
    }

    if !config
        .scripts
        .iter()
        .all(|script| has_script(resolve, script))
    {
        return false;
    }

    if !config.dependencies.is_empty() && !has_dependencies(resolve, &config.dependencies) {
        return false;
    }

    true
}

fn should_check_default_script(config: &Config) -> bool {
    config.scripts.is_empty() && config.dependencies.is_empty()
}

fn has_script(resolve: &ResolveTask, script_name: &str) -> bool {
    resolve
        .scripts
        .iter()
        .any(|candidate| candidate == script_name)
}

fn has_dependencies(resolve: &ResolveTask, dependencies: &[String]) -> bool {
    let package_json = match load_package_json(resolve) {
        Some(package_json) => package_json,
        None => return false,
    };

    dependencies
        .iter()
        .all(|dependency| package_json.has_dependency(dependency))
}

fn load_package_json(resolve: &ResolveTask) -> Option<PackageJson> {
    // Root-task resolve path can omit `cwd`; fallback to current_dir so dependency
    // checks still evaluate relative to launch dir/workspace root.
    let base_dir = match resolve_base_dir(resolve) {
        Ok(base_dir) => base_dir,
        Err(error) => {
            eprintln!("failed to resolve package base dir: {error}");
            return None;
        }
    };
    let path = base_dir.join("package.json");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(_) => return None,
    };
    let value = match serde_json::from_str::<Value>(&contents) {
        Ok(value) => value,
        Err(_) => return None,
    };

    Some(PackageJson { value })
}

fn resolve_base_dir(resolve: &ResolveTask) -> Result<PathBuf, ProxyError> {
    match &resolve.cwd {
        Some(cwd) => Ok(Path::new(cwd).to_path_buf()),
        None => Ok(std::env::current_dir()?),
    }
}

struct PackageJson {
    value: Value,
}

impl PackageJson {
    fn has_dependency(&self, name: &str) -> bool {
        dependency_map_contains(&self.value, "dependencies", name)
            || dependency_map_contains(&self.value, "devDependencies", name)
    }
}

fn dependency_map_contains(value: &Value, key: &str, dependency: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_object)
        .is_some_and(|dependencies| dependencies.contains_key(dependency))
}

#[cfg(test)]
mod tests {
    use luchta_worker::{ResolveMode, ResolveTask};

    use super::{parse_stage_args, should_check_default_script, should_keep, Config};

    fn resolve_task(name: &str, command: &str, scripts: &[&str]) -> ResolveTask {
        ResolveTask {
            id: format!("job-{name}"),
            name: name.to_owned(),
            command: command.to_owned(),
            package: "@repo/app".to_owned(),
            cwd: Some("packages/app".to_owned()),
            scripts: scripts.iter().map(|script| script.to_string()).collect(),
            inputs: Vec::new(),
            mode: ResolveMode::Run,
        }
    }

    #[test]
    fn parse_stage_args_collects_repeatable_flags() {
        let config = parse_stage_args(&[
            "--script".to_owned(),
            "build".to_owned(),
            "--script".to_owned(),
            "lint".to_owned(),
            "--dependency".to_owned(),
            "babel".to_owned(),
        ])
        .expect("parse args");

        assert_eq!(config.scripts, vec!["build", "lint"]);
        assert_eq!(config.dependencies, vec!["babel"]);
    }

    #[test]
    fn parse_stage_args_rejects_unknown_flag() {
        let error = parse_stage_args(&["--wat".to_owned()]).expect_err("unknown flag");
        assert!(error.contains("unknown argument `--wat`"));
    }

    #[test]
    fn parse_stage_args_requires_flag_values() {
        let script_error = parse_stage_args(&["--script".to_owned()]).expect_err("script value");
        assert!(script_error.contains("missing value for --script"));

        let dep_error =
            parse_stage_args(&["--dependency".to_owned()]).expect_err("dependency value");
        assert!(dep_error.contains("missing value for --dependency"));
    }

    #[test]
    fn default_script_check_only_applies_without_overrides() {
        assert!(should_check_default_script(&Config {
            scripts: Vec::new(),
            dependencies: Vec::new(),
        }));
        assert!(!should_check_default_script(&Config {
            scripts: vec!["build".to_owned()],
            dependencies: Vec::new(),
        }));
        assert!(!should_check_default_script(&Config {
            scripts: Vec::new(),
            dependencies: vec!["babel".to_owned()],
        }));
    }

    #[test]
    fn default_script_check_uses_task_name_not_command() {
        // The task name is the script the filter looks for. A `command`
        // override is for the worker to run and must be ignored here.
        let config = Config {
            scripts: Vec::new(),
            dependencies: Vec::new(),
        };

        // Package declares the task's own script name -> keep, even though the
        // `command` names a different (absent) script.
        let named = resolve_task("build", "compile", &["build"]);
        assert!(should_keep(&config, &named));

        // Package declares only the `command`'s script, not the task name ->
        // prune, because the filter ignores `command`.
        let command_only = resolve_task("build", "compile", &["compile"]);
        assert!(!should_keep(&config, &command_only));
    }
}
