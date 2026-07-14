use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Args, Parser, Subcommand};
use serde::Deserialize;

/// Project automation tasks for the Luchta workspace.
#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Luchta project automation tasks", long_about = None, version)]
struct Cli {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Debug, Subcommand)]
enum XtaskCommand {
    /// Install all workspace binary crates via `cargo install --path`.
    Install,
    /// Build Go TypeScript worker into target output directory.
    BuildWorker(BuildWorkerArgs),
}

#[derive(Debug, Args)]
struct BuildWorkerArgs {
    /// Rust target triple to build for. Defaults to host triple.
    #[arg(long)]
    target: Option<String>,
    /// Override output directory for built worker binary.
    #[arg(long)]
    out_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        XtaskCommand::Install => install_bins(),
        XtaskCommand::BuildWorker(args) => build_worker(args),
    }
}

fn build_worker(args: BuildWorkerArgs) -> ExitCode {
    match try_build_worker(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn try_build_worker(args: BuildWorkerArgs) -> Result<(), String> {
    let repo_root = repo_root()?;
    let target = args.target.unwrap_or(host_target_triple()?);
    let out_dir = args
        .out_dir
        .unwrap_or_else(|| repo_root.join("target").join(&target).join("release"));
    let output_path = build_worker_to(&repo_root, &target, &out_dir)?;

    println!("Built {}", output_path.display());
    Ok(())
}

fn build_worker_to(repo_root: &Path, target: &str, out_dir: &Path) -> Result<PathBuf, String> {
    let vendor_dir = repo_root.join("vendor/tsgo");
    ensure_tsgo_submodule_initialized(&vendor_dir)?;

    let go_target = go_target_for_rust_triple(target)?;
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("failed to determine current directory: {error}"))?;
    let out_dir = resolve_out_dir(&current_dir, out_dir);
    std::fs::create_dir_all(&out_dir).map_err(|error| {
        format!(
            "failed to create output directory {}: {error}",
            out_dir.display()
        )
    })?;

    let patch_path = repo_root.join("patches/tsgo.patch");
    let output_path = out_dir.join(worker_binary_name(go_target.goos));

    reset_tsgo_worktree(&vendor_dir)?;
    apply_tsgo_patch(&vendor_dir, &patch_path)?;

    let build_result = go_build_worker(&vendor_dir, &output_path, go_target);
    let reset_result = reset_tsgo_worktree(&vendor_dir);

    build_result?;
    reset_result?;
    Ok(output_path)
}

fn resolve_out_dir(cwd: &Path, out_dir: &Path) -> PathBuf {
    if out_dir.is_absolute() {
        out_dir.to_path_buf()
    } else {
        cwd.join(out_dir)
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().map(Path::to_path_buf).ok_or_else(|| {
        format!(
            "failed to determine repository root from {}",
            manifest_dir.display()
        )
    })
}

fn ensure_tsgo_submodule_initialized(vendor_dir: &Path) -> Result<(), String> {
    if vendor_dir.join(".git").exists() {
        Ok(())
    } else {
        Err("vendor/tsgo not initialized — run: git submodule update --init".to_string())
    }
}

fn host_target_triple() -> Result<String, String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(rustc)
        .arg("-vV")
        .output()
        .map_err(|error| format!("failed to run rustc -vV: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "rustc -vV exited with {}",
            exit_code_label(output.status.code())
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("rustc -vV produced non-UTF-8 output: {error}"))?;

    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .ok_or_else(|| "failed to find host triple in rustc -vV output".to_string())
}

fn apply_tsgo_patch(vendor_dir: &Path, patch_path: &Path) -> Result<(), String> {
    let check_status = run_command(
        Command::new("git")
            .arg("-C")
            .arg(vendor_dir)
            .arg("apply")
            .arg("--check")
            .arg(patch_path),
        "failed to run git apply --check",
    )?;

    if !check_status.success() {
        return Err("patches/tsgo.patch does not apply to vendor/tsgo — rebase needed".to_string());
    }

    let apply_status = run_command(
        Command::new("git")
            .arg("-C")
            .arg(vendor_dir)
            .arg("apply")
            .arg(patch_path),
        "failed to run git apply",
    )?;

    if apply_status.success() {
        Ok(())
    } else {
        Err(format!(
            "git apply exited with {}",
            exit_code_label(apply_status.code())
        ))
    }
}

fn reset_tsgo_worktree(vendor_dir: &Path) -> Result<(), String> {
    let checkout_status = run_command(
        Command::new("git")
            .arg("-C")
            .arg(vendor_dir)
            .arg("checkout")
            .arg("."),
        "failed to run git checkout .",
    )?;

    if !checkout_status.success() {
        return Err(format!(
            "git checkout . exited with {}",
            exit_code_label(checkout_status.code())
        ));
    }

    let clean_status = run_command(
        Command::new("git")
            .arg("-C")
            .arg(vendor_dir)
            .arg("clean")
            .arg("-fd"),
        "failed to run git clean -fd",
    )?;

    if clean_status.success() {
        Ok(())
    } else {
        Err(format!(
            "git clean -fd exited with {}",
            exit_code_label(clean_status.code())
        ))
    }
}

#[allow(clippy::suspicious_command_arg_space)]
fn go_build_worker(
    vendor_dir: &Path,
    output_path: &Path,
    go_target: GoTarget,
) -> Result<(), String> {
    let status = Command::new("go")
        .current_dir(vendor_dir)
        .env("CGO_ENABLED", "0")
        .env("GOOS", go_target.goos)
        .env("GOARCH", go_target.goarch)
        .arg("build")
        .arg("-trimpath")
        .arg("-ldflags")
        .arg("-s -w")
        .arg("-o")
        .arg(output_path)
        .arg("./cmd/luchta-tsc-worker")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("failed to run go build: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "go build exited with {}",
            exit_code_label(status.code())
        ))
    }
}

fn run_command(
    command: &mut Command,
    spawn_error: &str,
) -> Result<std::process::ExitStatus, String> {
    command
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("{spawn_error}: {error}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GoTarget {
    goos: &'static str,
    goarch: &'static str,
}

fn go_target_for_rust_triple(target: &str) -> Result<GoTarget, String> {
    match target {
        "x86_64-unknown-linux-musl" | "x86_64-unknown-linux-gnu" => Ok(GoTarget {
            goos: "linux",
            goarch: "amd64",
        }),
        "aarch64-unknown-linux-musl" | "aarch64-unknown-linux-gnu" => Ok(GoTarget {
            goos: "linux",
            goarch: "arm64",
        }),
        "x86_64-apple-darwin" => Ok(GoTarget {
            goos: "darwin",
            goarch: "amd64",
        }),
        "aarch64-apple-darwin" => Ok(GoTarget {
            goos: "darwin",
            goarch: "arm64",
        }),
        "x86_64-pc-windows-msvc" => Ok(GoTarget {
            goos: "windows",
            goarch: "amd64",
        }),
        "aarch64-pc-windows-msvc" => Ok(GoTarget {
            goos: "windows",
            goarch: "arm64",
        }),
        "i686-pc-windows-msvc" => Ok(GoTarget {
            goos: "windows",
            goarch: "386",
        }),
        _ => Err(format!(
            "unsupported target `{target}`. Supported targets: {}",
            supported_target_triples().join(", ")
        )),
    }
}

fn supported_target_triples() -> &'static [&'static str] {
    &[
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "i686-pc-windows-msvc",
    ]
}

fn worker_binary_name(goos: &str) -> &'static OsStr {
    if goos == "windows" {
        OsStr::new("luchta-tsc-worker.exe")
    } else {
        OsStr::new("luchta-tsc-worker")
    }
}

fn install_bins() -> ExitCode {
    match try_install_bins() {
        Ok(summary) => {
            println!("\nSummary: installed {summary}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn try_install_bins() -> Result<String, String> {
    let metadata =
        workspace_metadata().map_err(|error| format!("failed to load cargo metadata: {error}"))?;
    let packages = workspace_bin_packages(&metadata);
    let total = packages.len();

    if total == 0 {
        println!("No workspace binary crates found.");
    } else {
        println!("Installing {total} workspace binary crate(s)...");
    }

    let mut installed = 0usize;
    for package in &packages {
        println!(
            "\n==> Installing {} from {}",
            package.name,
            package.crate_dir.display()
        );

        if let Err(error) = cargo_install(&package.crate_dir) {
            return Err(format!(
                "\nInstall failed for {}: {error}\nSummary: installed {installed}/{total} crate(s).",
                package.name
            ));
        }

        installed += 1;
    }

    println!("\n==> Installing luchta-tsc-worker");
    let installed_worker_path = install_host_worker()?;
    println!("Installed {}", installed_worker_path.display());

    Ok(format!("{installed}/{total} crate(s) + tsc worker"))
}

fn install_host_worker() -> Result<PathBuf, String> {
    let repo_root = repo_root()?;
    let target = host_target_triple()?;
    let build_out_dir = repo_root.join("target").join(&target).join("release");
    let built_path = build_worker_to(&repo_root, &target, &build_out_dir)?;

    let bin_dir = cargo_install_bin_dir_from_env(&cargo_install_env())?;
    std::fs::create_dir_all(&bin_dir).map_err(|error| {
        format!(
            "failed to create cargo install bin directory {}: {error}",
            bin_dir.display()
        )
    })?;

    let installed_path = bin_dir.join(built_path.file_name().ok_or_else(|| {
        format!(
            "built worker path {} has no file name",
            built_path.display()
        )
    })?);
    std::fs::copy(&built_path, &installed_path).map_err(|error| {
        format!(
            "failed to copy {} to {}: {error}",
            built_path.display(),
            installed_path.display()
        )
    })?;

    Ok(installed_path)
}

fn cargo_install_env() -> HashMap<String, OsString> {
    std::env::vars_os()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value))
        .collect()
}

fn cargo_install_bin_dir_from_env(env: &HashMap<String, OsString>) -> Result<PathBuf, String> {
    if let Some(root) = env
        .get("CARGO_INSTALL_ROOT")
        .filter(|v| is_non_empty_env(v))
    {
        return Ok(PathBuf::from(root).join("bin"));
    }

    if let Some(cargo_home) = env.get("CARGO_HOME").filter(|v| is_non_empty_env(v)) {
        return Ok(PathBuf::from(cargo_home).join("bin"));
    }

    cargo_home_base_dir(env).map(|dir| dir.join(".cargo").join("bin"))
}

fn cargo_home_base_dir(env: &HashMap<String, OsString>) -> Result<PathBuf, String> {
    if let Some(home) = env.get("HOME").filter(|v| is_non_empty_env(v)) {
        return Ok(PathBuf::from(home));
    }

    if let Some(user_profile) = env.get("USERPROFILE").filter(|v| is_non_empty_env(v)) {
        return Ok(PathBuf::from(user_profile));
    }

    Err("failed to determine cargo install root: set CARGO_INSTALL_ROOT, CARGO_HOME, HOME, or USERPROFILE".to_string())
}

/// Returns true if the env value is non-empty and not whitespace-only.
fn is_non_empty_env(value: &OsStr) -> bool {
    !value.to_string_lossy().trim().is_empty()
}

/// The cargo executable to invoke. Prefer the `CARGO` env var (set by cargo when
/// running through the `cargo xtask` alias) so we stay on the same toolchain,
/// falling back to `cargo` on `PATH`.
fn cargo_bin() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn workspace_metadata() -> Result<Metadata, String> {
    let output = Command::new(cargo_bin())
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|error| format!("failed to run cargo metadata: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "cargo metadata exited with {}\n{}",
            exit_code_label(output.status.code()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("failed to parse cargo metadata JSON: {error}"))
}

fn workspace_bin_packages(metadata: &Metadata) -> Vec<WorkspaceBinPackage> {
    let mut packages: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .filter(|package| package.name != env!("CARGO_PKG_NAME"))
        .filter(|package| package.targets.iter().any(Target::is_bin))
        .filter_map(|package| {
            crate_dir(&package.manifest_path).map(|crate_dir| WorkspaceBinPackage {
                name: package.name.clone(),
                crate_dir,
            })
        })
        .collect();

    packages.sort_by(|left, right| left.name.cmp(&right.name));
    packages
}

fn crate_dir(manifest_path: &Path) -> Option<PathBuf> {
    manifest_path.parent().map(Path::to_path_buf)
}

fn cargo_install(crate_dir: &Path) -> Result<(), String> {
    let status = Command::new(cargo_bin())
        .arg("install")
        .arg("--path")
        .arg(crate_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("failed to run cargo install: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo install exited with {}",
            exit_code_label(status.code())
        ))
    }
}

fn exit_code_label(code: Option<i32>) -> String {
    code.map_or_else(|| String::from("signal"), |code| code.to_string())
}

#[derive(Debug, Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
struct Target {
    kind: Vec<String>,
}

impl Target {
    fn is_bin(&self) -> bool {
        self.kind.iter().any(|kind| kind == "bin")
    }
}

#[derive(Debug)]
struct WorkspaceBinPackage {
    name: String,
    crate_dir: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cargo metadata JSON modelling a workspace with two bin crates, one lib
    /// crate, the xtask crate itself, and a non-member (dependency) bin crate.
    ///
    /// The bin members are listed out of alphabetical order (`zebra-tool`
    /// before `luchta-cli`) so the sort-order test actually exercises the
    /// sorting step rather than passing on pre-ordered input.
    const SAMPLE_METADATA: &str = r#"{
        "packages": [
            {
                "id": "zebra-tool 0.1.0 (path+file:///repo/crates/zebra-tool)",
                "name": "zebra-tool",
                "manifest_path": "/repo/crates/zebra-tool/Cargo.toml",
                "targets": [{"kind": ["bin"]}]
            },
            {
                "id": "luchta-cli 0.1.0 (path+file:///repo/crates/luchta-cli)",
                "name": "luchta-cli",
                "manifest_path": "/repo/crates/luchta-cli/Cargo.toml",
                "targets": [{"kind": ["bin"]}, {"kind": ["lib"]}]
            },
            {
                "id": "luchta-yarn-worker 0.1.0 (path+file:///repo/crates/luchta-yarn-worker)",
                "name": "luchta-yarn-worker",
                "manifest_path": "/repo/crates/luchta-yarn-worker/Cargo.toml",
                "targets": [{"kind": ["bin"]}]
            },
            {
                "id": "luchta-bash-worker 0.1.0 (path+file:///repo/crates/luchta-bash-worker)",
                "name": "luchta-bash-worker",
                "manifest_path": "/repo/crates/luchta-bash-worker/Cargo.toml",
                "targets": [{"kind": ["bin"]}]
            },
            {
                "id": "luchta-types 0.1.0 (path+file:///repo/crates/luchta-types)",
                "name": "luchta-types",
                "manifest_path": "/repo/crates/luchta-types/Cargo.toml",
                "targets": [{"kind": ["lib"]}]
            },
            {
                "id": "xtask 0.1.0 (path+file:///repo/xtask)",
                "name": "xtask",
                "manifest_path": "/repo/xtask/Cargo.toml",
                "targets": [{"kind": ["bin"]}]
            },
            {
                "id": "some-dep 1.0.0 (registry+https://example.com)",
                "name": "some-dep",
                "manifest_path": "/cache/some-dep/Cargo.toml",
                "targets": [{"kind": ["bin"]}]
            }
        ],
        "workspace_members": [
            "zebra-tool 0.1.0 (path+file:///repo/crates/zebra-tool)",
            "luchta-cli 0.1.0 (path+file:///repo/crates/luchta-cli)",
            "luchta-yarn-worker 0.1.0 (path+file:///repo/crates/luchta-yarn-worker)",
            "luchta-bash-worker 0.1.0 (path+file:///repo/crates/luchta-bash-worker)",
            "luchta-types 0.1.0 (path+file:///repo/crates/luchta-types)",
            "xtask 0.1.0 (path+file:///repo/xtask)"
        ]
    }"#;

    fn sample() -> Metadata {
        serde_json::from_str(SAMPLE_METADATA).expect("sample metadata parses")
    }

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(value)))
            .collect()
    }

    #[test]
    fn target_is_bin_detects_bin_kind() {
        assert!(Target {
            kind: vec!["bin".to_string()]
        }
        .is_bin());
        assert!(Target {
            kind: vec!["lib".to_string(), "bin".to_string()]
        }
        .is_bin());
        assert!(!Target {
            kind: vec!["lib".to_string()]
        }
        .is_bin());
    }

    #[test]
    fn selects_only_workspace_bin_crates() {
        let names: Vec<_> = workspace_bin_packages(&sample())
            .into_iter()
            .map(|package| package.name)
            .collect();
        // Returned sorted by name even though `zebra-tool` appears first in
        // the metadata input.
        assert_eq!(
            names,
            vec![
                "luchta-bash-worker",
                "luchta-cli",
                "luchta-yarn-worker",
                "zebra-tool"
            ]
        );
    }

    #[test]
    fn excludes_xtask_itself() {
        let names: Vec<_> = workspace_bin_packages(&sample())
            .into_iter()
            .map(|package| package.name)
            .collect();
        assert!(!names.contains(&"xtask".to_string()));
    }

    #[test]
    fn excludes_lib_only_and_non_member_crates() {
        let names: Vec<_> = workspace_bin_packages(&sample())
            .into_iter()
            .map(|package| package.name)
            .collect();
        // lib-only member excluded, registry dependency (non-member) excluded.
        assert!(!names.contains(&"luchta-types".to_string()));
        assert!(!names.contains(&"some-dep".to_string()));
    }

    #[test]
    fn results_are_sorted_by_name() {
        let names: Vec<_> = workspace_bin_packages(&sample())
            .into_iter()
            .map(|package| package.name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn crate_dir_resolves_from_manifest_parent() {
        let packages = workspace_bin_packages(&sample());
        let cli = packages
            .iter()
            .find(|package| package.name == "luchta-cli")
            .expect("luchta-cli present");
        assert_eq!(cli.crate_dir, PathBuf::from("/repo/crates/luchta-cli"));
    }

    #[test]
    fn crate_dir_returns_parent_directory() {
        assert_eq!(
            crate_dir(Path::new("/repo/crates/foo/Cargo.toml")),
            Some(PathBuf::from("/repo/crates/foo"))
        );
    }

    #[test]
    fn empty_metadata_yields_no_packages() {
        let metadata = Metadata {
            packages: Vec::new(),
            workspace_members: Vec::new(),
        };
        assert!(workspace_bin_packages(&metadata).is_empty());
    }

    #[test]
    fn yields_no_packages_when_only_lib_and_xtask_remain() {
        const ONLY_LIB_AND_XTASK: &str = r#"{
            "packages": [
                {
                    "id": "luchta-types 0.1.0 (path+file:///repo/crates/luchta-types)",
                    "name": "luchta-types",
                    "manifest_path": "/repo/crates/luchta-types/Cargo.toml",
                    "targets": [{"kind": ["lib"]}]
                },
                {
                    "id": "xtask 0.1.0 (path+file:///repo/xtask)",
                    "name": "xtask",
                    "manifest_path": "/repo/xtask/Cargo.toml",
                    "targets": [{"kind": ["bin"]}]
                }
            ],
            "workspace_members": [
                "luchta-types 0.1.0 (path+file:///repo/crates/luchta-types)",
                "xtask 0.1.0 (path+file:///repo/xtask)"
            ]
        }"#;
        let metadata: Metadata = serde_json::from_str(ONLY_LIB_AND_XTASK).expect("metadata parses");
        assert!(workspace_bin_packages(&metadata).is_empty());
    }

    #[test]
    fn exit_code_label_formats_code_and_signal() {
        assert_eq!(exit_code_label(Some(2)), "2");
        assert_eq!(exit_code_label(None), "signal");
    }

    #[test]
    fn go_target_mapping_covers_supported_linux_host_variant() {
        assert_eq!(
            go_target_for_rust_triple("x86_64-unknown-linux-gnu"),
            Ok(GoTarget {
                goos: "linux",
                goarch: "amd64"
            })
        );
        assert_eq!(
            go_target_for_rust_triple("aarch64-unknown-linux-musl"),
            Ok(GoTarget {
                goos: "linux",
                goarch: "arm64"
            })
        );
    }

    #[test]
    fn go_target_mapping_covers_windows_variants() {
        assert_eq!(
            go_target_for_rust_triple("x86_64-pc-windows-msvc"),
            Ok(GoTarget {
                goos: "windows",
                goarch: "amd64"
            })
        );
        assert_eq!(
            go_target_for_rust_triple("i686-pc-windows-msvc"),
            Ok(GoTarget {
                goos: "windows",
                goarch: "386"
            })
        );
    }

    #[test]
    fn worker_binary_name_adds_windows_suffix() {
        assert_eq!(
            worker_binary_name("windows"),
            OsStr::new("luchta-tsc-worker.exe")
        );
        assert_eq!(worker_binary_name("linux"), OsStr::new("luchta-tsc-worker"));
    }

    #[test]
    fn resolve_out_dir_joins_relative_path_to_current_dir() {
        assert_eq!(
            resolve_out_dir(Path::new("/repo"), Path::new("target/testrel")),
            PathBuf::from("/repo/target/testrel")
        );
    }

    #[test]
    fn resolve_out_dir_preserves_absolute_path() {
        assert_eq!(
            resolve_out_dir(Path::new("/repo"), Path::new("/tmp/abs-out")),
            PathBuf::from("/tmp/abs-out")
        );
    }

    #[test]
    fn cargo_install_bin_dir_prefers_install_root() {
        let env = env_map(&[
            ("CARGO_INSTALL_ROOT", "/x/install"),
            ("CARGO_HOME", "/x/cargo-home"),
            ("HOME", "/x/home"),
        ]);
        assert_eq!(
            cargo_install_bin_dir_from_env(&env),
            Ok(PathBuf::from("/x/install/bin"))
        );
    }

    #[test]
    fn cargo_install_bin_dir_falls_back_to_cargo_home() {
        let env = env_map(&[("CARGO_HOME", "/x/cargo-home"), ("HOME", "/x/home")]);
        assert_eq!(
            cargo_install_bin_dir_from_env(&env),
            Ok(PathBuf::from("/x/cargo-home/bin"))
        );
    }

    #[test]
    fn cargo_install_bin_dir_falls_back_to_home_dot_cargo_bin() {
        let env = env_map(&[("HOME", "/x/home")]);
        assert_eq!(
            cargo_install_bin_dir_from_env(&env),
            Ok(PathBuf::from("/x/home/.cargo/bin"))
        );
    }

    #[test]
    fn cargo_install_bin_dir_accepts_userprofile_fallback() {
        let env = env_map(&[("USERPROFILE", "C:/Users/tester")]);
        assert_eq!(
            cargo_install_bin_dir_from_env(&env),
            Ok(PathBuf::from("C:/Users/tester/.cargo/bin"))
        );
    }

    #[test]
    fn cargo_install_bin_dir_errors_when_no_root_env_present() {
        let env = HashMap::new();
        assert_eq!(
            cargo_install_bin_dir_from_env(&env),
            Err(
                "failed to determine cargo install root: set CARGO_INSTALL_ROOT, CARGO_HOME, HOME, or USERPROFILE"
                    .to_string()
            )
        );
    }

    #[test]
    fn cargo_install_bin_dir_ignores_empty_cargo_install_root() {
        let env = env_map(&[("CARGO_INSTALL_ROOT", ""), ("CARGO_HOME", "/x/cargo-home")]);
        assert_eq!(
            cargo_install_bin_dir_from_env(&env),
            Ok(PathBuf::from("/x/cargo-home/bin"))
        );
    }

    #[test]
    fn unsupported_target_lists_supported_triples() {
        let error = go_target_for_rust_triple("foo-bar").expect_err("target rejected");
        assert!(error.contains("unsupported target `foo-bar`"));
        for target in supported_target_triples() {
            assert!(error.contains(target), "missing {target} in {error}");
        }
    }
}
