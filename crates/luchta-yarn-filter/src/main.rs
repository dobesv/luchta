use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use luchta_worker::{
    split_current_process_argv, DelegateHandle, ProxyError, ResolveResult, ResolveTask,
    WorkerMessage, WorkerResponse,
};
use serde_json::Value;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

/// Bound on how long the delegate may take to answer a forwarded `resolve`.
/// `resolve` runs during graph build and must be fast; a delegate that is alive
/// but never responds would otherwise hang the whole build. On timeout we treat
/// the task as pruned (clean error, no hang).
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
    let stage_args = argv.stage_args.into_iter().skip(1).collect::<Vec<_>>();
    let usage = "usage: luchta-yarn-filter [--script NAME]... [--dependency NAME]... -- <delegate command...>";

    let config = match parse_stage_args(&stage_args) {
        Ok(config) => config,
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
    let delegate = DelegateHandle::with_writers(
        argv.delegate_command,
        Arc::clone(&stdout_writer),
        Arc::new(Mutex::new(Box::new(tokio::io::stderr()))),
        Some("delegate stderr: ".to_owned()),
    );

    let mut exit_code = 0;
    let mut lines = BufReader::new(stdin()).lines();

    loop {
        let Some(line) = (match lines.next_line().await {
            Ok(line) => line,
            Err(error) => {
                eprintln!("failed to read worker stdin: {error}");
                exit_code = 1;
                break;
            }
        }) else {
            break;
        };

        let message = match serde_json::from_str::<WorkerMessage>(&line) {
            Ok(message) => message,
            Err(error) => {
                eprintln!("failed to parse worker message: {error}");
                exit_code = 1;
                break;
            }
        };

        match message {
            WorkerMessage::ResolveTask(resolve) => {
                let request_id = resolve.id.clone();
                if should_keep(&config, &resolve) {
                    if let Err(error) = delegate
                        .send_with_timeout(
                            WorkerMessage::ResolveTask(resolve),
                            RESOLVE_FORWARD_TIMEOUT,
                        )
                        .await
                    {
                        eprintln!("delegate failed before resolve decision: {error}");
                        let response =
                            WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                        if let Err(write_error) = write_response(&stdout_writer, &response).await {
                            eprintln!("failed to write resolve fallback: {write_error}");
                            exit_code = 1;
                            break;
                        }
                    }
                } else {
                    let response = WorkerResponse::resolved(request_id, ResolveResult::prune(None));
                    if let Err(error) = write_response(&stdout_writer, &response).await {
                        eprintln!("failed to write prune response: {error}");
                        exit_code = 1;
                        break;
                    }
                }
            }
            WorkerMessage::Run(request) => {
                if let Err(error) = delegate.send(WorkerMessage::Run(request)).await {
                    eprintln!("delegate failed: {error}");
                    exit_code = 1;
                    break;
                }
            }
        }
    }

    if let Err(error) = delegate.shutdown().await {
        eprintln!("failed to shut down delegate: {error}");
        exit_code = 1;
    }

    exit_code
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
    if should_check_default_script(config) && !has_script(resolve, resolve.resolved_script_name()) {
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

async fn write_response(
    writer: &SharedWriter,
    response: &WorkerResponse,
) -> Result<(), ProxyError> {
    let line = serde_json::to_string(response)?;
    let mut writer = writer.lock().await;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
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
    fn default_script_check_uses_resolved_script_name() {
        let resolve = resolve_task("build", "compile", &["compile"]);
        assert!(should_keep(
            &Config {
                scripts: Vec::new(),
                dependencies: Vec::new(),
            },
            &resolve,
        ));
    }
}
