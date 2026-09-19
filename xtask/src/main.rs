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
    /// Print shipped release binary names, one per line.
    ListReleaseBins,
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
        XtaskCommand::ListReleaseBins => list_release_bins(),
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

/// Vendored patches applied to `vendor/typescript`, in required order:
/// `upstream-pnp.patch` first (PR #63919, mechanically regenerated — never
/// hand-edited), then `luchta.patch` (our own additive packages and small
/// upstream tweaks) on top of it. `luchta.patch`'s hunks are anchored to blobs
/// that the upstream patch produces, so it cannot apply first.
const PATCHES_IN_ORDER: &[&str] = &["patches/upstream-pnp.patch", "patches/luchta.patch"];

fn build_worker_to(repo_root: &Path, target: &str, out_dir: &Path) -> Result<PathBuf, String> {
    let vendor_dir = repo_root.join("vendor/typescript");
    ensure_vendor_submodule_initialized(&vendor_dir)?;

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

    let patches = patch_paths(repo_root);
    let output_path = out_dir.join(worker_binary_name(go_target.goos));
    let go_module_dir = vendor_dir.join("tsc");

    reset_vendor_worktree(&vendor_dir)?;
    apply_patches_and_build(
        &vendor_dir,
        &patches,
        &go_module_dir,
        &output_path,
        go_target,
    )?;
    Ok(output_path)
}

/// Applies every patch and then builds the Go worker, bracketing both steps
/// in a single reset of `vendor_dir` afterward — success or failure.
///
/// Patch application and the build used to be reset separately (the build
/// alone was wrapped), which meant a failure partway through
/// `apply_patches` — e.g. `upstream-pnp.patch` applies but `luchta.patch`
/// then fails — returned early and left the submodule holding the first
/// patch's changes uncommitted. That dirty worktree then confused the next
/// `build-worker` run (and anyone poking at `vendor/typescript` by hand).
/// Putting apply and build in one fallible section with one unconditional
/// reset after it, mirroring how the build alone used to be handled, closes
/// that gap: whatever fails, the reset still runs and the submodule ends up
/// clean.
fn apply_patches_and_build(
    vendor_dir: &Path,
    patches: &[(&'static str, PathBuf)],
    go_module_dir: &Path,
    output_path: &Path,
    go_target: GoTarget,
) -> Result<(), String> {
    let build_result = apply_patches(vendor_dir, patches)
        .and_then(|()| go_build_worker(go_module_dir, output_path, go_target));
    let reset_result = reset_vendor_worktree(vendor_dir);

    build_result?;
    reset_result?;
    Ok(())
}

/// Resolves `PATCHES_IN_ORDER` to full paths under `repo_root`, paired with
/// their repo-relative display name for diagnostics. Kept separate from
/// `apply_patches` so the ordering itself — load-bearing, since
/// `luchta.patch` cannot apply before `upstream-pnp.patch` — is unit
/// testable without shelling out to git.
fn patch_paths(repo_root: &Path) -> Vec<(&'static str, PathBuf)> {
    PATCHES_IN_ORDER
        .iter()
        .map(|&relative| (relative, repo_root.join(relative)))
        .collect()
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

fn ensure_vendor_submodule_initialized(vendor_dir: &Path) -> Result<(), String> {
    if vendor_dir.join(".git").exists() {
        Ok(())
    } else {
        Err("vendor/typescript not initialized — run: git submodule update --init".to_string())
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

/// Builds a `git` command scoped to `vendor_dir` via `-C`, with the
/// environment variables that can override `-C` stripped.
///
/// Git itself sets `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, and
/// `GIT_COMMON_DIR` on child processes in ordinary situations (running from
/// a hook, for instance) — this isn't only a deliberate-misuse concern. Any
/// of them take precedence over `-C` and can point git at a different
/// repository, worktree, or index than the one we just named.
/// `reset_vendor_worktree` runs `checkout .` and `clean -fd`, so under the
/// wrong worktree that silently deletes files that were never meant to be
/// touched. Stripping these here makes `-C vendor_dir` authoritative for
/// every git invocation in this file, without touching any other inherited
/// environment variable.
fn git_command(vendor_dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .arg("-C")
        .arg(vendor_dir);
    command
}

/// Applies every patch in `patches`, in order, to `vendor_dir`. Stops at the
/// first one that fails and names it — that diagnostic is what makes patch
/// staleness obvious, so it must say *which* patch broke, not just that
/// something did.
fn apply_patches(vendor_dir: &Path, patches: &[(&'static str, PathBuf)]) -> Result<(), String> {
    for (relative, patch_path) in patches {
        apply_one_patch(vendor_dir, patch_path, relative)?;
    }
    Ok(())
}

fn apply_one_patch(vendor_dir: &Path, patch_path: &Path, patch_label: &str) -> Result<(), String> {
    let check_status = run_command(
        git_command(vendor_dir)
            .arg("apply")
            .arg("--check")
            .arg(patch_path),
        &format!("failed to run git apply --check for {patch_label}"),
    )?;

    if !check_status.success() {
        return Err(format!(
            "{patch_label} does not apply to vendor/typescript — rebase needed"
        ));
    }

    let apply_status = run_command(
        git_command(vendor_dir).arg("apply").arg(patch_path),
        &format!("failed to run git apply for {patch_label}"),
    )?;

    if apply_status.success() {
        Ok(())
    } else {
        Err(format!(
            "git apply exited with {} while applying {patch_label}",
            exit_code_label(apply_status.code())
        ))
    }
}

fn reset_vendor_worktree(vendor_dir: &Path) -> Result<(), String> {
    let checkout_status = run_command(
        git_command(vendor_dir).arg("checkout").arg("."),
        "failed to run git checkout .",
    )?;

    if !checkout_status.success() {
        return Err(format!(
            "git checkout . exited with {}",
            exit_code_label(checkout_status.code())
        ));
    }

    let clean_status = run_command(
        git_command(vendor_dir).arg("clean").arg("-fd"),
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
    go_module_dir: &Path,
    output_path: &Path,
    go_target: GoTarget,
) -> Result<(), String> {
    let status = Command::new("go")
        .current_dir(go_module_dir)
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

fn list_release_bins() -> ExitCode {
    match try_list_release_bins() {
        Ok(bins) => {
            for bin in bins {
                println!("{bin}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn try_list_release_bins() -> Result<Vec<String>, String> {
    let metadata =
        workspace_metadata().map_err(|error| format!("failed to load cargo metadata: {error}"))?;
    Ok(workspace_release_bin_names(&metadata))
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
    copy_atomically(&built_path, &installed_path)?;

    Ok(installed_path)
}

/// Copies `source` to `destination` atomically: the file is copied into a
/// temporary path in `destination`'s own directory and then renamed over
/// `destination`. This avoids `ETXTBSY` ("Text file busy") when `destination`
/// is a binary that is currently running — unlike an in-place copy, a rename
/// only swaps the directory entry, so a running process keeps executing the
/// old (now unlinked) inode while the new binary takes its place for the next
/// invocation. The temporary file lives alongside `destination` rather than
/// in a system temp directory because `rename` only works within a single
/// filesystem. `std::fs::copy` preserves the source's permission bits
/// (including the executable bit), so no explicit `chmod` is needed. The
/// temporary file is removed if the rename fails.
fn copy_atomically(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "failed to copy {} to {}: destination has no parent directory",
            source.display(),
            destination.display()
        )
    })?;
    let file_name = destination.file_name().ok_or_else(|| {
        format!(
            "failed to copy {} to {}: destination has no final component",
            source.display(),
            destination.display()
        )
    })?;
    let temp_path = parent.join(format!(".{}.tmp", file_name.to_string_lossy()));

    std::fs::copy(source, &temp_path).map_err(|error| {
        format!(
            "failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    if let Err(error) = std::fs::rename(&temp_path, destination) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        ));
    }

    Ok(())
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

fn workspace_release_bin_names(metadata: &Metadata) -> Vec<String> {
    let mut bins: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .flat_map(|package| package.targets.iter())
        .filter(|target| target.is_bin())
        .map(|target| target.name.clone())
        .filter(|name| name == "luchta" || name.starts_with("luchta-"))
        .collect();

    bins.push("luchta-tsc-worker".to_string());
    bins.sort();
    bins.dedup();
    bins
}

fn crate_dir(manifest_path: &Path) -> Option<PathBuf> {
    manifest_path.parent().map(Path::to_path_buf)
}

fn cargo_install(crate_dir: &Path) -> Result<(), String> {
    let status = Command::new(cargo_bin())
        .arg("install")
        .arg("--locked")
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
    name: String,
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
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    /// Cargo metadata JSON modelling workspace packages with bin targets,
    /// including `luchta` CLI target, multi-bin package with one shippable bin
    /// plus one mock helper, lib-only crate, xtask itself, and non-member bin
    /// dependency.
    ///
    /// Workspace packages appear out of alphabetical order so sort-order tests
    /// exercise actual sorting.
    const SAMPLE_METADATA: &str = r#"{
        "packages": [
            {
                "id": "zebra-tool 0.1.0 (path+file:///repo/crates/zebra-tool)",
                "name": "zebra-tool",
                "manifest_path": "/repo/crates/zebra-tool/Cargo.toml",
                "targets": [{"name": "zebra-tool", "kind": ["bin"]}]
            },
            {
                "id": "luchta-cli 0.1.0 (path+file:///repo/crates/luchta-cli)",
                "name": "luchta-cli",
                "manifest_path": "/repo/crates/luchta-cli/Cargo.toml",
                "targets": [
                    {"name": "luchta", "kind": ["bin"]},
                    {"name": "luchta_cli", "kind": ["lib"]}
                ]
            },
            {
                "id": "luchta-yarn-worker 0.1.0 (path+file:///repo/crates/luchta-yarn-worker)",
                "name": "luchta-yarn-worker",
                "manifest_path": "/repo/crates/luchta-yarn-worker/Cargo.toml",
                "targets": [{"name": "luchta-yarn-worker", "kind": ["bin"]}]
            },
            {
                "id": "luchta-worker-watcher 0.1.0 (path+file:///repo/crates/luchta-worker-watcher)",
                "name": "luchta-worker-watcher",
                "manifest_path": "/repo/crates/luchta-worker-watcher/Cargo.toml",
                "targets": [
                    {"name": "luchta-worker-watcher", "kind": ["bin"]},
                    {"name": "mock-worker-delegate", "kind": ["bin"]}
                ]
            },
            {
                "id": "luchta-bash-worker 0.1.0 (path+file:///repo/crates/luchta-bash-worker)",
                "name": "luchta-bash-worker",
                "manifest_path": "/repo/crates/luchta-bash-worker/Cargo.toml",
                "targets": [{"name": "luchta-bash-worker", "kind": ["bin"]}]
            },
            {
                "id": "luchta-types 0.1.0 (path+file:///repo/crates/luchta-types)",
                "name": "luchta-types",
                "manifest_path": "/repo/crates/luchta-types/Cargo.toml",
                "targets": [{"name": "luchta_types", "kind": ["lib"]}]
            },
            {
                "id": "xtask 0.1.0 (path+file:///repo/xtask)",
                "name": "xtask",
                "manifest_path": "/repo/xtask/Cargo.toml",
                "targets": [{"name": "xtask", "kind": ["bin"]}]
            },
            {
                "id": "some-dep 1.0.0 (registry+https://example.com)",
                "name": "some-dep",
                "manifest_path": "/cache/some-dep/Cargo.toml",
                "targets": [{"name": "some-dep", "kind": ["bin"]}]
            }
        ],
        "workspace_members": [
            "zebra-tool 0.1.0 (path+file:///repo/crates/zebra-tool)",
            "luchta-cli 0.1.0 (path+file:///repo/crates/luchta-cli)",
            "luchta-yarn-worker 0.1.0 (path+file:///repo/crates/luchta-yarn-worker)",
            "luchta-worker-watcher 0.1.0 (path+file:///repo/crates/luchta-worker-watcher)",
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

    fn workspace_manifests(repo_root: &Path) -> Vec<PathBuf> {
        let mut manifests = vec![
            repo_root.join("Cargo.toml"),
            repo_root.join("xtask/Cargo.toml"),
        ];
        let crates_dir = repo_root.join("crates");
        for entry in std::fs::read_dir(&crates_dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", crates_dir.display()))
        {
            let entry = entry.expect("failed to read crates directory entry");
            let manifest = entry.path().join("Cargo.toml");
            if manifest.is_file() {
                manifests.push(manifest);
            }
        }
        manifests.sort();
        manifests
    }

    fn collect_oxc_git_revs(
        value: &toml::Value,
        manifest: &Path,
        repo_root: &Path,
        revisions: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        match value {
            toml::Value::Table(table) => {
                // Match both canonical forms of the oxc repo URL (with or without
                // the `.git` suffix, tolerating a trailing slash) so a pin written
                // in a different-but-equivalent form can't slip past the guard.
                if table
                    .get("git")
                    .and_then(toml::Value::as_str)
                    .map(|git| git.trim_end_matches('/').trim_end_matches(".git"))
                    == Some("https://github.com/oxc-project/oxc")
                {
                    let rev = table
                        .get("rev")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_else(|| {
                            panic!(
                                "oxc git dependency in {} has no string `rev`",
                                manifest.display()
                            )
                        });
                    let relative_manifest = manifest.strip_prefix(repo_root).unwrap_or(manifest);
                    revisions
                        .entry(rev.to_string())
                        .or_default()
                        .insert(relative_manifest.display().to_string());
                }
                for nested in table.values() {
                    collect_oxc_git_revs(nested, manifest, repo_root, revisions);
                }
            }
            toml::Value::Array(array) => {
                for nested in array {
                    collect_oxc_git_revs(nested, manifest, repo_root, revisions);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn oxc_git_dependencies_share_one_revision() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask manifest directory has a parent");
        let mut revisions = BTreeMap::<String, BTreeSet<String>>::new();

        for manifest in workspace_manifests(repo_root) {
            let contents = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest.display()));
            let document = toml::from_str::<toml::Value>(&contents)
                .unwrap_or_else(|error| panic!("failed to parse {}: {error}", manifest.display()));
            collect_oxc_git_revs(&document, &manifest, repo_root, &mut revisions);
        }

        let revisions_by_file = revisions
            .iter()
            .map(|(rev, manifests)| {
                format!(
                    "  {rev}: {}",
                    manifests.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            revisions.len(),
            1,
            "expected exactly one oxc git dependency rev across workspace Cargo.toml files; found:\n{revisions_by_file}"
        );
    }

    #[test]
    fn collect_oxc_git_revs_matches_url_variants() {
        // The `.git`-suffixed form, the bare form, and a trailing-slash form all
        // point at the same repo, so all three must count toward the shared rev.
        let manifest = toml::from_str::<toml::Value>(
            r#"
            [workspace.dependencies]
            oxc_a = { git = "https://github.com/oxc-project/oxc.git", rev = "abc" }
            oxc_b = { git = "https://github.com/oxc-project/oxc", rev = "abc" }
            oxc_c = { git = "https://github.com/oxc-project/oxc/", rev = "abc" }
            other = { git = "https://github.com/other/repo.git", rev = "zzz" }
            "#,
        )
        .expect("parse");
        let mut revisions = BTreeMap::<String, BTreeSet<String>>::new();
        collect_oxc_git_revs(
            &manifest,
            Path::new("Cargo.toml"),
            Path::new(""),
            &mut revisions,
        );

        assert_eq!(
            revisions.keys().cloned().collect::<Vec<_>>(),
            vec!["abc".to_string()],
            "all oxc URL forms must fold into one rev and non-oxc repos must be ignored"
        );
    }

    #[test]
    fn target_is_bin_detects_bin_kind() {
        assert!(Target {
            name: "example-bin".to_string(),
            kind: vec!["bin".to_string()]
        }
        .is_bin());
        assert!(Target {
            name: "example-mixed".to_string(),
            kind: vec!["lib".to_string(), "bin".to_string()]
        }
        .is_bin());
        assert!(!Target {
            name: "example-lib".to_string(),
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
                "luchta-worker-watcher",
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
    fn selects_release_bin_target_names_and_appends_go_worker() {
        let names = workspace_release_bin_names(&sample());
        assert_eq!(
            names,
            vec![
                "luchta",
                "luchta-bash-worker",
                "luchta-tsc-worker",
                "luchta-worker-watcher",
                "luchta-yarn-worker"
            ]
        );
    }

    #[test]
    fn release_bin_names_exclude_mock_delegate_xtask_and_unmatched_bins() {
        let names = workspace_release_bin_names(&sample());
        assert!(!names.contains(&"mock-worker-delegate".to_string()));
        assert!(!names.contains(&"xtask".to_string()));
        assert!(!names.contains(&"zebra-tool".to_string()));
        assert!(!names.contains(&"luchta-cli".to_string()));
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
                    "targets": [{"name": "luchta_types", "kind": ["lib"]}]
                },
                {
                    "id": "xtask 0.1.0 (path+file:///repo/xtask)",
                    "name": "xtask",
                    "manifest_path": "/repo/xtask/Cargo.toml",
                    "targets": [{"name": "xtask", "kind": ["bin"]}]
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
    fn patches_apply_upstream_pnp_before_luchta() {
        // Load-bearing order: luchta.patch's hunks are anchored to blobs that
        // upstream-pnp.patch produces, so it cannot apply first.
        let patches = patch_paths(Path::new("/repo"));
        let relative_names: Vec<_> = patches.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            relative_names,
            vec!["patches/upstream-pnp.patch", "patches/luchta.patch"]
        );
    }

    #[test]
    fn patch_paths_resolves_against_repo_root() {
        let patches = patch_paths(Path::new("/repo"));
        let full_paths: Vec<_> = patches.iter().map(|(_, path)| path.clone()).collect();
        assert_eq!(
            full_paths,
            vec![
                PathBuf::from("/repo/patches/upstream-pnp.patch"),
                PathBuf::from("/repo/patches/luchta.patch"),
            ]
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

    #[test]
    fn git_command_strips_repo_location_env_vars() {
        // GIT_DIR, GIT_WORK_TREE, GIT_INDEX_FILE, and GIT_COMMON_DIR all
        // override -C when set, and git sets some of these itself on child
        // processes (e.g. from a hook). get_envs() reports env_remove'd keys
        // as present with value None, which is how we assert they're
        // explicitly stripped rather than merely never set.
        let command = git_command(Path::new("/repo/vendor/typescript"));
        let envs: HashMap<_, _> = command.get_envs().collect();
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            assert_eq!(
                envs.get(OsStr::new(var)),
                Some(&None),
                "{var} should be explicitly removed from the git command's environment"
            );
        }
    }

    /// Runs `git` with `args` in `dir`, panicking with stderr on failure.
    /// Test-only plumbing for the fixture below, not a path production code
    /// takes.
    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git spawns");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Builds a throwaway git repo with one committed file plus two
    /// standalone patches against it. Both patches are diffed against the
    /// same committed base (so each applies cleanly to a pristine checkout
    /// on its own), but the second patch's context still assumes the
    /// original line 2 — so once the first patch has already rewritten it
    /// on disk, the second no longer applies. That mirrors the real failure
    /// mode under test: a second patch, anchored to content the first patch
    /// changes, fails mid-sequence and must not leave the worktree dirty.
    fn two_patch_fixture(temp: &tempfile::TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let repo_dir = temp.path().join("repo");
        std::fs::create_dir(&repo_dir).expect("create repo dir");
        git(&repo_dir, &["init", "-q", "-b", "main"]);
        git(&repo_dir, &["config", "user.email", "test@example.com"]);
        git(&repo_dir, &["config", "user.name", "Test"]);

        let file_path = repo_dir.join("foo.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\n").expect("write foo.txt");
        git(&repo_dir, &["add", "foo.txt"]);
        git(&repo_dir, &["commit", "-q", "-m", "init"]);

        git(&repo_dir, &["checkout", "-q", "-b", "feature-1"]);
        std::fs::write(&file_path, "line1\nPATCHED-A\nline3\n").expect("write feature-1");
        git(&repo_dir, &["commit", "-q", "-am", "feature-1"]);
        let patch1 = temp.path().join("patch1.patch");
        let diff1 = Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["diff", "main", "feature-1", "--", "foo.txt"])
            .output()
            .expect("git diff feature-1");
        std::fs::write(&patch1, &diff1.stdout).expect("write patch1");

        git(&repo_dir, &["checkout", "-q", "main"]);
        git(&repo_dir, &["checkout", "-q", "-b", "feature-2"]);
        std::fs::write(&file_path, "line1\nPATCHED-B\nline3\n").expect("write feature-2");
        git(&repo_dir, &["commit", "-q", "-am", "feature-2"]);
        let patch2 = temp.path().join("patch2.patch");
        let diff2 = Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["diff", "main", "feature-2", "--", "foo.txt"])
            .output()
            .expect("git diff feature-2");
        std::fs::write(&patch2, &diff2.stdout).expect("write patch2");

        git(&repo_dir, &["checkout", "-q", "main"]);

        (repo_dir, patch1, patch2)
    }

    #[test]
    fn apply_patches_and_build_resets_worktree_after_second_patch_fails() {
        // Regression test for a real failure: apply_patches used to return
        // early on the second patch's error, and reset_vendor_worktree only
        // ran around the build step, so the first patch's change was left
        // uncommitted in the submodule. apply_patches_and_build now brackets
        // both apply_patches and the build in one fallible section with a
        // single unconditional reset after it.
        let temp = tempfile::TempDir::new().expect("tempdir");
        let (repo_dir, patch1, patch2) = two_patch_fixture(&temp);
        let patches: Vec<(&'static str, PathBuf)> =
            vec![("patch1.patch", patch1), ("patch2.patch", patch2)];

        // go_module_dir/output_path/go_target are never touched: the second
        // patch fails inside apply_patches before go_build_worker would run,
        // which is what lets this test exercise the reset without needing
        // `go` installed.
        let result = apply_patches_and_build(
            &repo_dir,
            &patches,
            Path::new("unused-go-module-dir"),
            Path::new("unused-output-path"),
            GoTarget {
                goos: "linux",
                goarch: "amd64",
            },
        );

        let error = result.expect_err("second patch's stale context must fail to apply");
        assert!(
            error.contains("patch2.patch"),
            "error should name the failing patch: {error}"
        );

        let status = Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(
            status.stdout.is_empty(),
            "vendor worktree must be clean after a failed second patch, got: {}",
            String::from_utf8_lossy(&status.stdout)
        );

        let contents = std::fs::read_to_string(repo_dir.join("foo.txt")).expect("read foo.txt");
        assert_eq!(
            contents, "line1\nline2\nline3\n",
            "first patch's change must be reverted by the reset, not left dangling"
        );
    }

    #[test]
    fn copy_atomically_replaces_existing_destination_and_leaves_no_temp_file() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("source-bin");
        std::fs::write(&source, "new contents").expect("write source");
        let destination = temp.path().join("dest-bin");
        std::fs::write(&destination, "old contents").expect("seed destination");

        copy_atomically(&source, &destination).expect("copy");

        assert_eq!(
            std::fs::read_to_string(&destination).expect("read"),
            "new contents"
        );
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary file left behind");
    }

    #[cfg(unix)]
    #[test]
    fn copy_atomically_preserves_executable_permission_bit() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("source-bin");
        std::fs::write(&source, "#!/bin/sh\n").expect("write source");
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755))
            .expect("chmod source");
        let destination = temp.path().join("dest-bin");

        copy_atomically(&source, &destination).expect("copy");

        let mode = std::fs::metadata(&destination)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bit did not survive copy");
    }

    #[cfg(unix)]
    #[test]
    fn copy_atomically_replaces_destination_inode_rather_than_writing_in_place() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("source-bin");
        std::fs::write(&source, "new contents").expect("write source");
        let destination = temp.path().join("dest-bin");
        std::fs::write(&destination, "old contents").expect("seed destination");
        let original_inode = std::fs::metadata(&destination)
            .expect("stat destination")
            .ino();

        copy_atomically(&source, &destination).expect("copy");

        let replaced_inode = std::fs::metadata(&destination)
            .expect("stat destination")
            .ino();
        assert_ne!(
            original_inode, replaced_inode,
            "destination must be a new inode: an in-place copy reuses it, which is what fails with ETXTBSY against a running binary"
        );
    }
}
