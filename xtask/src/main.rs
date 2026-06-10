use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use serde::Deserialize;

/// Project automation tasks for the Luchta workspace.
#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Luchta project automation tasks", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Debug, Subcommand)]
enum XtaskCommand {
    /// Install all workspace binary crates via `cargo install --path`.
    Install,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        XtaskCommand::Install => install_bins(),
    }
}

fn install_bins() -> ExitCode {
    let metadata = match workspace_metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("failed to load cargo metadata: {error}");
            return ExitCode::FAILURE;
        }
    };

    let packages = workspace_bin_packages(&metadata);

    if packages.is_empty() {
        println!("No workspace binary crates found.");
        return ExitCode::SUCCESS;
    }

    let total = packages.len();
    println!("Installing {total} workspace binary crate(s)...");

    let mut installed = 0usize;

    for package in &packages {
        println!(
            "\n==> Installing {} from {}",
            package.name,
            package.crate_dir.display()
        );

        if let Err(error) = cargo_install(&package.crate_dir) {
            eprintln!("\nInstall failed for {}: {error}", package.name);
            eprintln!("Summary: installed {installed}/{total} crate(s).");
            return ExitCode::FAILURE;
        }

        installed += 1;
    }

    println!("\nSummary: installed {installed}/{total} crate(s).");
    ExitCode::SUCCESS
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
            "luchta-types 0.1.0 (path+file:///repo/crates/luchta-types)",
            "xtask 0.1.0 (path+file:///repo/xtask)"
        ]
    }"#;

    fn sample() -> Metadata {
        serde_json::from_str(SAMPLE_METADATA).expect("sample metadata parses")
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
            vec!["luchta-cli", "luchta-yarn-worker", "zebra-tool"]
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
}
