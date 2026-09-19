use std::path::{Path, PathBuf};
use std::process::Command;

pub fn resolve_on_path(name: &str, path_value: &str) -> Option<PathBuf> {
    std::env::split_paths(path_value)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Yarn's own version, from `packageManager` in the root package.json
/// (`yarn@4.18.0+sha512...`) or a `yarnPath: .yarn/releases/yarn-4.9.1.cjs`
/// line in `.yarnrc.yml`.
pub fn yarn_version_from_project(project_root: &Path) -> Option<String> {
    // `packageManager: "yarn@4.18.0+sha512..."` in the root `package.json`,
    // else a `yarnPath: .yarn/releases/yarn-4.9.1.cjs` line in `.yarnrc.yml`.
    // Written as two `?`-chained closures rather than nested `if let`s to
    // keep each lookup's own nesting shallow.
    let from_package_manager = || -> Option<String> {
        let raw = std::fs::read_to_string(project_root.join("package.json")).ok()?;
        let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let spec = value.get("packageManager").and_then(|v| v.as_str())?;
        version_prefix(spec.strip_prefix("yarn@")?)
    };
    let from_yarn_path = || -> Option<String> {
        let yarnrc = std::fs::read_to_string(project_root.join(".yarnrc.yml")).ok()?;
        let line = yarnrc
            .lines()
            .find(|line| line.trim_start().starts_with("yarnPath:"))?;
        let file_name = line
            .rsplit('/')
            .next()?
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        version_prefix(file_name.strip_prefix("yarn-")?)
    };
    from_package_manager().or_else(from_yarn_path)
}

/// Take a leading run of digits and dots (a version number), trimming any
/// trailing dot left behind by a following non-digit character (e.g. the `.`
/// before the extension in `yarn-4.9.1.cjs`). `None` if nothing was captured.
fn version_prefix(s: &str) -> Option<String> {
    let version: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let version = version.trim_end_matches('.').to_owned();
    (!version.is_empty()).then_some(version)
}

/// Run `<program> --version` with the given PATH and return the trimmed first
/// line. `HOME` is passed through from the process environment when set,
/// since corepack (which `yarn --version` may shell out to) needs it to find
/// its cache; everything else stays cleared.
pub fn probe_version(program: &Path, path_value: &str) -> Option<String> {
    let mut command = Command::new(program);
    command.arg("--version").env_clear().env("PATH", path_value);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
}

pub fn user_agent(yarn_version: &str, node_version: &str) -> String {
    format!(
        "yarn/{yarn_version} npm/? node/{node_version} {} {}",
        node_platform(),
        node_arch()
    )
}

pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_on_path_finds_first_executable_match() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        write_exec(&second.join("tool"));
        let path_value = format!("{}:{}", first.display(), second.display());
        assert_eq!(
            resolve_on_path("tool", &path_value),
            Some(second.join("tool"))
        );
        assert_eq!(resolve_on_path("missing", &path_value), None);
    }

    #[test]
    fn yarn_version_prefers_package_manager_field() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"x","packageManager":"yarn@4.18.0+sha512.abc"}"#,
        )
        .unwrap();
        assert_eq!(
            yarn_version_from_project(temp.path()),
            Some("4.18.0".to_owned())
        );
    }

    #[test]
    fn yarn_version_falls_back_to_yarn_path() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        std::fs::write(
            temp.path().join(".yarnrc.yml"),
            "nodeLinker: pnp\nyarnPath: .yarn/releases/yarn-4.9.1.cjs\n",
        )
        .unwrap();
        assert_eq!(
            yarn_version_from_project(temp.path()),
            Some("4.9.1".to_owned())
        );
    }

    #[test]
    fn yarn_version_is_none_without_hints() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert_eq!(yarn_version_from_project(temp.path()), None);
    }

    #[test]
    fn probe_version_runs_program() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("fakenode");
        write_exec_with(&fake, "#!/bin/sh\necho v22.1.0\n");
        assert_eq!(probe_version(&fake, ""), Some("v22.1.0".to_owned()));
    }

    #[test]
    fn user_agent_matches_yarn_format() {
        let agent = user_agent("4.18.0", "v22.1.0");
        assert!(
            agent.starts_with("yarn/4.18.0 npm/? node/v22.1.0 "),
            "{agent}"
        );
        assert!(
            agent.ends_with(&format!("{} {}", node_platform(), node_arch())),
            "{agent}"
        );
    }

    #[cfg(unix)]
    fn write_exec(path: &std::path::Path) {
        write_exec_with(path, "#!/bin/sh\nexit 0\n");
    }

    #[cfg(unix)]
    fn write_exec_with(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
