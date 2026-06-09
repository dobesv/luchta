//! Workspace discovery abstractions.

use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use serde_json::{Map, Value};
use walkdir::WalkDir;

use crate::{PackageNode, WorkspaceError};

/// Abstraction for loading workspace package metadata from a repository root.
pub trait WorkspaceDiscovery: Send + Sync + std::fmt::Debug {
    /// Discovers packages that participate in current workspace.
    fn discover(&self) -> Result<Vec<PackageNode>, WorkspaceError>;
}

/// Yarn workspace discovery backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YarnWorkspace {
    root: PathBuf,
}

impl YarnWorkspace {
    /// Creates Yarn workspace discovery backend rooted at provided path.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns configured workspace root.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }
}

#[derive(Debug, Deserialize)]
struct PackageJson {
    name: Option<String>,
    #[serde(default)]
    scripts: Option<Map<String, Value>>,
    #[serde(default)]
    workspaces: Option<WorkspacesField>,
}

impl PackageJson {
    fn has_scripts(&self) -> bool {
        self.scripts
            .as_ref()
            .is_some_and(|scripts| !scripts.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum WorkspacesField {
    Array(Vec<String>),
    Object { packages: Vec<String> },
}

impl WorkspacesField {
    fn packages(self) -> Vec<String> {
        match self {
            Self::Array(packages) => packages,
            Self::Object { packages } => packages,
        }
    }
}

impl YarnWorkspace {
    /// Walks the workspace tree collecting directories that match a workspace
    /// glob and contain a `package.json`.
    fn collect_package_paths(
        &self,
        matcher: &GlobSet,
    ) -> Result<BTreeSet<PathBuf>, WorkspaceError> {
        let mut package_paths = BTreeSet::new();

        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !is_node_modules(entry.path()))
        {
            let entry = entry.map_err(|error| WorkspaceError::Discovery(error.to_string()))?;
            if let Some(path) = self.matched_package_dir(&entry, matcher)? {
                package_paths.insert(path);
            }
        }

        Ok(package_paths)
    }

    /// Returns the package directory for `entry` when it is a workspace-matched
    /// directory containing a `package.json`, otherwise `None`.
    fn matched_package_dir(
        &self,
        entry: &walkdir::DirEntry,
        matcher: &GlobSet,
    ) -> Result<Option<PathBuf>, WorkspaceError> {
        if !entry.file_type().is_dir() {
            return Ok(None);
        }

        let path = entry.path();
        if path == self.root {
            return Ok(None);
        }

        let relative_path = path
            .strip_prefix(&self.root)
            .map_err(|error| WorkspaceError::Discovery(error.to_string()))?;

        if !matcher.is_match(relative_path) {
            return Ok(None);
        }

        if path.join("package.json").is_file() {
            Ok(Some(path.to_path_buf()))
        } else {
            Ok(None)
        }
    }
}

impl WorkspaceDiscovery for YarnWorkspace {
    fn discover(&self) -> Result<Vec<PackageNode>, WorkspaceError> {
        let root_package_path = self.root.join("package.json");
        let root_package = read_package_json(&root_package_path)?;
        let workspace_globs = root_package
            .workspaces
            .as_ref()
            .ok_or_else(|| {
                WorkspaceError::Discovery("root package.json missing workspaces field".into())
            })?
            .clone()
            .packages();

        let matcher = build_globset(&workspace_globs)?;
        let package_paths = self.collect_package_paths(&matcher)?;

        let mut packages = Vec::new();
        if root_package.has_scripts() {
            packages.push(package_node_from_package_json(
                root_package,
                self.root.clone(),
            )?);
        }

        for package_path in package_paths {
            let package_json = read_package_json(&package_path.join("package.json"))?;
            packages.push(package_node_from_package_json(package_json, package_path)?);
        }

        Ok(packages)
    }
}

fn read_package_json(path: &Path) -> Result<PackageJson, WorkspaceError> {
    let contents = fs::read_to_string(path).map_err(|error| {
        WorkspaceError::Discovery(format!("failed to read {}: {error}", path.display()))
    })?;

    serde_json::from_str(&contents).map_err(|error| {
        WorkspaceError::Discovery(format!("failed to parse {}: {error}", path.display()))
    })
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, WorkspaceError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| {
            WorkspaceError::Discovery(format!("invalid workspace glob '{pattern}': {error}"))
        })?;
        builder.add(glob);
    }

    builder
        .build()
        .map_err(|error| WorkspaceError::Discovery(format!("invalid workspace globs: {error}")))
}

fn package_node_from_package_json(
    package_json: PackageJson,
    package_path: PathBuf,
) -> Result<PackageNode, WorkspaceError> {
    let name = package_json
        .name
        .ok_or_else(|| {
            WorkspaceError::Discovery(format!(
                "package.json at {} missing name field",
                package_path.display()
            ))
        })?
        .into();

    Ok(PackageNode::new(name, package_path))
}

fn is_node_modules(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name == "node_modules",
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use super::{WorkspaceDiscovery, YarnWorkspace};

    #[test]
    fn discovers_workspace_packages_from_array_glob() {
        let temp_dir = tempdir().expect("create temp dir");
        write_json(
            temp_dir.path().join("package.json"),
            r#"{
                "name": "root",
                "private": true,
                "workspaces": ["packages/*"]
            }"#,
        );
        write_package(
            temp_dir.path().join("packages/app/package.json"),
            "@repo/app",
        );
        write_package(
            temp_dir.path().join("packages/utils/package.json"),
            "@repo/utils",
        );
        write_package(
            temp_dir.path().join("packages/web/package.json"),
            "@repo/web",
        );

        let workspace = YarnWorkspace::new(temp_dir.path());
        let mut packages = workspace.discover().expect("discover packages");
        packages.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));

        let discovered: Vec<_> = packages
            .into_iter()
            .map(|package| package.name.to_string())
            .collect();

        assert_eq!(discovered, vec!["@repo/app", "@repo/utils", "@repo/web"]);
    }

    #[test]
    fn ignores_node_modules_packages() {
        let temp_dir = tempdir().expect("create temp dir");
        write_json(
            temp_dir.path().join("package.json"),
            r#"{
                "name": "root",
                "private": true,
                "workspaces": ["packages/*", "node_modules/*"]
            }"#,
        );
        write_package(
            temp_dir.path().join("packages/real/package.json"),
            "@repo/real",
        );
        write_package(
            temp_dir.path().join("node_modules/stray/package.json"),
            "stray",
        );

        let workspace = YarnWorkspace::new(temp_dir.path());
        let packages = workspace.discover().expect("discover packages");

        let discovered: Vec<_> = packages
            .into_iter()
            .map(|package| package.name.to_string())
            .collect();

        assert_eq!(discovered, vec!["@repo/real"]);
    }

    #[test]
    fn includes_root_package_when_scripts_present() {
        let temp_dir = tempdir().expect("create temp dir");
        write_json(
            temp_dir.path().join("package.json"),
            r#"{
                "name": "root",
                "private": true,
                "scripts": { "build": "echo root" },
                "workspaces": { "packages": ["packages/*"] }
            }"#,
        );
        write_package(
            temp_dir.path().join("packages/app/package.json"),
            "@repo/app",
        );

        let workspace = YarnWorkspace::new(temp_dir.path());
        let mut packages = workspace.discover().expect("discover packages");
        packages.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));

        let discovered: Vec<_> = packages
            .into_iter()
            .map(|package| package.name.to_string())
            .collect();

        assert_eq!(discovered, vec!["@repo/app", "root"]);
    }

    #[test]
    fn excludes_root_package_when_scripts_missing() {
        let temp_dir = tempdir().expect("create temp dir");
        write_json(
            temp_dir.path().join("package.json"),
            r#"{
                "name": "root",
                "private": true,
                "workspaces": { "packages": ["packages/*"] }
            }"#,
        );
        write_package(
            temp_dir.path().join("packages/app/package.json"),
            "@repo/app",
        );

        let workspace = YarnWorkspace::new(temp_dir.path());
        let packages = workspace.discover().expect("discover packages");

        let discovered: Vec<_> = packages
            .into_iter()
            .map(|package| package.name.to_string())
            .collect();

        assert_eq!(discovered, vec!["@repo/app"]);
    }

    fn write_package(path: impl AsRef<Path>, name: &str) {
        write_json(
            path,
            &format!(
                r#"{{
                    "name": "{name}",
                    "scripts": {{ "build": "echo build" }}
                }}"#
            ),
        );
    }

    fn write_json(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().expect("parent dir")).expect("create parent dir");
        fs::write(path, contents).expect("write json");
    }
}
