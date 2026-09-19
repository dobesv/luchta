use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::test_fixture::{FixtureOptions, PnpFixture};
use crate::{DirectExecError, YarnEnvComputer};

/// A PATH dir holding fake `node`, `yarn`, and a real `bash` symlink. Tests
/// put it first on PATH followed by the system dirs so script bodies can find
/// `env`, `printf`, and friends.
fn tool_dir(temp: &Path) -> String {
    let dir = temp.join("tools");
    std::fs::create_dir_all(&dir).unwrap();
    write_exec(&dir.join("node"), "#!/bin/sh\necho v22.1.0\n");
    write_exec(&dir.join("yarn"), "#!/bin/sh\necho 4.18.0\n");
    let real_bash = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("bash present on this machine");
    std::os::unix::fs::symlink(real_bash, dir.join("bash")).unwrap();
    dir.to_string_lossy().into_owned()
}

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Request env: the tool dir first, then system dirs.
fn base_env(tool_dir: &str) -> HashMap<String, String> {
    HashMap::from([
        ("PATH".to_owned(), format!("{tool_dir}:/usr/bin:/bin")),
        ("HOME".to_owned(), "/home/test".to_owned()),
    ])
}

/// Request env with ONLY the tool dir, for tests that remove a tool from it.
fn isolated_env(tool_dir: &str) -> HashMap<String, String> {
    HashMap::from([("PATH".to_owned(), tool_dir.to_owned())])
}

#[test]
fn builds_bash_job_with_yarn_environment() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(
        &temp.path().join("repo"),
        FixtureOptions {
            with_loader: true,
            ..FixtureOptions::default()
        },
    );
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());

    let job = computer
        .direct_job(&fixture.app_dir, "build", &[], &base_env(&path_value))
        .unwrap();

    assert_eq!(job.program, format!("{path_value}/bash"));
    assert_eq!(
        job.args,
        vec![
            "-c".to_owned(),
            "echo building".to_owned(),
            "yarn-script".to_owned()
        ]
    );

    let shim_dir = &job.env["BERRY_BIN_FOLDER"];
    assert_eq!(
        job.env["PATH"],
        format!("{shim_dir}:{path_value}:/usr/bin:/bin")
    );
    assert_eq!(job.env["npm_execpath"], format!("{shim_dir}/yarn"));
    assert_eq!(job.env["npm_node_execpath"], format!("{shim_dir}/node"));
    assert_eq!(job.env["INIT_CWD"], fixture.app_dir.to_string_lossy());
    assert_eq!(job.env["PROJECT_CWD"], fixture.root.to_string_lossy());
    assert_eq!(job.env["npm_package_name"], "@fixture/app");
    assert_eq!(job.env["npm_package_version"], "1.2.3");
    assert_eq!(
        job.env["npm_package_json"],
        fixture.app_dir.join("package.json").to_string_lossy()
    );
    assert_eq!(job.env["npm_lifecycle_event"], "build");
    assert!(job.env["npm_config_user_agent"].starts_with("yarn/4.18.0 npm/? node/v22.1.0 "));
    assert_eq!(
        job.env["NODE_OPTIONS"],
        format!(
            "--require {} --experimental-loader file://{}",
            fixture.root.join(".pnp.cjs").display(),
            fixture.root.join(".pnp.loader.mjs").display()
        )
    );
    assert_eq!(job.env["HOME"], "/home/test");

    for shim in [
        "node",
        "yarn",
        "yarnpkg",
        "run",
        "node-gyp",
        "app",
        "left-pad",
        "tool",
        "tool-extra",
        "native-thing",
    ] {
        assert!(
            Path::new(shim_dir).join(shim).is_file(),
            "missing shim {shim}"
        );
    }
}

#[test]
fn extra_args_are_appended_escaped() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let job = computer
        .direct_job(
            &fixture.app_dir,
            "args",
            &["a b".to_owned(), "c".to_owned()],
            &base_env(&path_value),
        )
        .unwrap();
    assert_eq!(job.args[1], "printf '%s\\n' \"$@\" 'a b' 'c'");
}

#[test]
fn env_file_values_are_injected_before_yarn_variables() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(
        &temp.path().join("repo"),
        FixtureOptions {
            env_file: Some("FROM_FILE=${HOME}/x\nNODE_OPTIONS=--no-warnings\n"),
            ..FixtureOptions::default()
        },
    );
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let job = computer
        .direct_job(&fixture.app_dir, "build", &[], &base_env(&path_value))
        .unwrap();
    assert_eq!(job.env["FROM_FILE"], "/home/test/x");
    assert_eq!(
        job.env["NODE_OPTIONS"],
        format!(
            "--require {} --no-warnings",
            fixture.root.join(".pnp.cjs").display()
        )
    );
}

#[test]
fn missing_script_is_a_typed_error() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let error = computer
        .direct_job(&fixture.app_dir, "nope", &[], &base_env(&path_value))
        .unwrap_err();
    assert!(
        matches!(error, DirectExecError::ScriptNotFound { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("`nope`"));
}

#[test]
fn missing_bash_is_a_typed_error() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    std::fs::remove_file(Path::new(&path_value).join("bash")).unwrap();
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let error = computer
        .direct_job(&fixture.app_dir, "build", &[], &isolated_env(&path_value))
        .unwrap_err();
    assert!(
        matches!(error, DirectExecError::ToolNotFound { name: "bash" }),
        "{error}"
    );
}

#[test]
fn no_manifest_is_a_typed_error() {
    let temp = tempfile::tempdir().unwrap();
    let path_value = tool_dir(temp.path());
    let root = temp.path().join("plain");
    std::fs::create_dir_all(root.join("packages/app")).unwrap();
    let mut computer = YarnEnvComputer::new(root.clone());
    let error = computer
        .direct_job(
            &root.join("packages/app"),
            "build",
            &[],
            &base_env(&path_value),
        )
        .unwrap_err();
    assert!(
        matches!(error, DirectExecError::NoPnpManifest { .. }),
        "{error}"
    );
}

#[test]
fn second_call_for_same_workspace_reuses_shim_dir_and_versions() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let first = computer
        .direct_job(&fixture.app_dir, "build", &[], &base_env(&path_value))
        .unwrap();
    // Break the fake node so a re-probe would change the user agent.
    write_exec(
        &Path::new(&path_value).join("node"),
        "#!/bin/sh\necho v99.0.0\n",
    );
    let second = computer
        .direct_job(&fixture.app_dir, "build", &[], &base_env(&path_value))
        .unwrap();
    assert_eq!(
        first.env["BERRY_BIN_FOLDER"],
        second.env["BERRY_BIN_FOLDER"]
    );
    assert_eq!(
        first.env["npm_config_user_agent"],
        second.env["npm_config_user_agent"]
    );
}

#[test]
fn script_actually_runs_under_bash_with_shims_on_path() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let job = computer
        .direct_job(&fixture.app_dir, "env", &[], &base_env(&path_value))
        .unwrap();
    let output = std::process::Command::new(&job.program)
        .args(&job.args)
        .env_clear()
        .envs(&job.env)
        .current_dir(&fixture.app_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("npm_lifecycle_event=env"), "{stdout}");
}

#[test]
fn unreadable_env_file_is_a_typed_error() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    // A directory named `.env.yarn` makes `read_to_string` fail with EISDIR,
    // not NotFound, so this exercises the non-benign error path.
    std::fs::create_dir(fixture.root.join(".env.yarn")).unwrap();
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let error = computer
        .direct_job(&fixture.app_dir, "build", &[], &base_env(&path_value))
        .unwrap_err();
    assert!(matches!(error, DirectExecError::Io { .. }), "{error}");
}

#[test]
fn root_workspace_resolves_via_top_level_entry() {
    // The root `package.json` has no `scripts`, so reaching `ScriptNotFound`
    // (rather than `WorkspaceNotInManifest`) proves the root workspace itself
    // was found in the manifest.
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let error = computer
        .direct_job(&fixture.root, "nope", &[], &base_env(&path_value))
        .unwrap_err();
    assert!(
        matches!(error, DirectExecError::ScriptNotFound { .. }),
        "{error}"
    );
}

#[test]
fn symlinked_project_root_resolves_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    PnpFixture::write(&real, FixtureOptions::default());
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let path_value = tool_dir(temp.path());

    let mut computer = YarnEnvComputer::new(link.clone());
    let job = computer
        .direct_job(
            &link.join("packages/app"),
            "build",
            &[],
            &base_env(&path_value),
        )
        .unwrap();

    let canonical_real = real.canonicalize().unwrap();
    assert_eq!(
        job.env["INIT_CWD"],
        canonical_real.join("packages/app").to_string_lossy()
    );
}

#[test]
fn relative_workspace_dir_is_resolved_against_project_root() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PnpFixture::write(&temp.path().join("repo"), FixtureOptions::default());
    let path_value = tool_dir(temp.path());
    let mut computer = YarnEnvComputer::new(fixture.root.clone());
    let job = computer
        .direct_job(
            Path::new("packages/app"),
            "build",
            &[],
            &base_env(&path_value),
        )
        .unwrap();
    assert_eq!(job.env["INIT_CWD"], fixture.app_dir.to_string_lossy());
}

#[test]
fn node_options_across_pnp_layouts() {
    for inline_manifest in [false, true] {
        for with_loader in [false, true] {
            let combo = format!("inline_manifest={inline_manifest} with_loader={with_loader}");
            let temp = tempfile::tempdir().unwrap();
            let fixture = PnpFixture::write(
                &temp.path().join("repo"),
                FixtureOptions {
                    inline_manifest,
                    with_loader,
                    ..FixtureOptions::default()
                },
            );
            let path_value = tool_dir(temp.path());
            let mut computer = YarnEnvComputer::new(fixture.root.clone());

            let job = computer
                .direct_job(&fixture.app_dir, "build", &[], &base_env(&path_value))
                .unwrap_or_else(|error| panic!("{combo}: direct_job failed: {error}"));

            let mut expected = format!("--require {}", fixture.root.join(".pnp.cjs").display());
            if with_loader {
                expected.push_str(&format!(
                    " --experimental-loader file://{}",
                    fixture.root.join(".pnp.loader.mjs").display()
                ));
            }
            assert_eq!(job.env["NODE_OPTIONS"], expected, "{combo}");

            let shim_dir = &job.env["BERRY_BIN_FOLDER"];
            assert!(
                Path::new(shim_dir).join("left-pad").is_file(),
                "{combo}: missing left-pad shim in {shim_dir}"
            );
        }
    }
}
