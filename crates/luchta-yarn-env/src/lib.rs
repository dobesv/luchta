//! Compute the environment Yarn Berry injects when running a `package.json`
//! script, without launching yarn. See `luchta-yarn-worker` for the consumer.

pub mod bins;
#[cfg(all(test, unix))]
mod direct_job_tests;
pub mod env_file;
mod error;
pub mod manifest;
pub mod node_options;
pub mod script;
pub mod shims;
#[cfg(any(test, feature = "test-fixture"))]
pub mod test_fixture;
pub mod tools;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use error::DirectExecError;

/// A process ready to spawn: `program` with `args`, a fully built `env`, run
/// from the workspace directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectJob {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceKey {
    workspace_dir: PathBuf,
    manifest: manifest::Fingerprint,
    package_json: (u64, Option<SystemTime>),
    path_value: String,
}

#[derive(Debug, Clone)]
struct WorkspaceEnv {
    shim_dir: PathBuf,
    package_name: String,
    package_version: String,
    package_json: PathBuf,
    scripts: HashMap<String, String>,
    bash: PathBuf,
}

#[derive(Debug, Clone)]
struct Versions {
    yarn: String,
    node: String,
}

pub struct YarnEnvComputer {
    project_root: PathBuf,
    manifests: manifest::ManifestCache,
    zips: bins::ZipCache,
    workspaces: HashMap<PathBuf, (WorkspaceKey, WorkspaceEnv)>,
    versions: Option<Versions>,
}

#[derive(serde::Deserialize, Default)]
struct WorkspaceManifest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    scripts: HashMap<String, String>,
}

impl YarnEnvComputer {
    /// `project_root` is canonicalized so a symlink anywhere in it (e.g. a
    /// `/tmp` symlinked to `/private/tmp` on macOS, or a workspace checked
    /// out through a symlinked path) does not desync it from the physical
    /// paths `pnp::find_locator` compares against the manifest's trie keys,
    /// which are derived from the `.pnp.cjs` file's own (physical) location.
    /// Falls back to the given path if canonicalization fails (e.g. it
    /// doesn't exist yet), leaving the existing "no manifest" error paths
    /// intact for that case.
    pub fn new(project_root: PathBuf) -> Self {
        let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
        Self {
            project_root,
            manifests: manifest::ManifestCache::default(),
            zips: bins::new_zip_cache(),
            workspaces: HashMap::new(),
            versions: None,
        }
    }

    /// Prepare `bash -c "<script body> <extra args>"` with yarn's environment
    /// for the given workspace. `workspace_dir` may be relative to the project
    /// root.
    pub fn direct_job(
        &mut self,
        workspace_dir: &Path,
        script: &str,
        extra_args: &[String],
        base_env: &HashMap<String, String>,
    ) -> Result<DirectJob, DirectExecError> {
        if cfg!(windows) {
            return Err(DirectExecError::UnsupportedPlatform);
        }
        let workspace_dir = self.project_root.join(workspace_dir);
        // Canonicalize so a symlinked workspace path (e.g. reached through a
        // symlinked package directory) matches the physical paths the PnP
        // manifest lookup and INIT_CWD/PROJECT_CWD are keyed on. Falls back
        // to the joined path on error, so a genuinely missing directory still
        // surfaces the existing PackageJson/WorkspaceNotInManifest errors.
        let workspace_dir = std::fs::canonicalize(&workspace_dir).unwrap_or(workspace_dir);
        let path_value = base_env.get("PATH").cloned().unwrap_or_default();

        let (manifest_files, fingerprint) = {
            let (_, files, fingerprint) = self.manifests.get_or_load(&self.project_root)?;
            (files.clone(), fingerprint)
        };
        let workspace = self.workspace_env(&workspace_dir, fingerprint, &path_value)?;
        let body =
            workspace
                .scripts
                .get(script)
                .ok_or_else(|| DirectExecError::ScriptNotFound {
                    script: script.to_owned(),
                    package_json: workspace.package_json.clone(),
                })?;
        let versions = self.versions(&path_value)?;

        let mut env = base_env.clone();
        match std::fs::read_to_string(self.project_root.join(".env.yarn")) {
            Ok(contents) => {
                for (key, value) in env_file::parse_env_file(&contents, base_env) {
                    env.insert(key, value);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(DirectExecError::io("reading .env.yarn", error)),
        }
        let shim_dir = workspace.shim_dir.to_string_lossy().into_owned();
        let old_path = env.get("PATH").cloned().unwrap_or_default();
        env.insert(
            "PATH".into(),
            if old_path.is_empty() {
                // Defensive: callers are expected to always provide PATH.
                shim_dir.clone()
            } else {
                format!("{shim_dir}:{old_path}")
            },
        );
        env.insert("BERRY_BIN_FOLDER".into(), shim_dir.clone());
        env.insert("npm_execpath".into(), format!("{shim_dir}/yarn"));
        env.insert("npm_node_execpath".into(), format!("{shim_dir}/node"));
        env.insert(
            "INIT_CWD".into(),
            workspace_dir.to_string_lossy().into_owned(),
        );
        env.insert(
            "PROJECT_CWD".into(),
            self.project_root.to_string_lossy().into_owned(),
        );
        env.insert("npm_package_name".into(), workspace.package_name.clone());
        env.insert(
            "npm_package_version".into(),
            workspace.package_version.clone(),
        );
        env.insert(
            "npm_package_json".into(),
            workspace.package_json.to_string_lossy().into_owned(),
        );
        env.insert("npm_lifecycle_event".into(), script.to_owned());
        env.insert(
            "npm_config_user_agent".into(),
            tools::user_agent(&versions.yarn, &versions.node),
        );
        match node_options::merge_node_options(
            env.get("NODE_OPTIONS").map(String::as_str),
            &manifest_files.pnp_cjs,
            manifest_files.pnp_loader.as_deref(),
        ) {
            Some(value) => env.insert("NODE_OPTIONS".into(), value),
            // Defensive: merge_node_options always returns Some given a pnp_cjs path.
            None => env.remove("NODE_OPTIONS"),
        };

        Ok(DirectJob {
            program: workspace.bash.to_string_lossy().into_owned(),
            args: script::build_bash_args(body, extra_args),
            env,
        })
    }

    fn workspace_env(
        &mut self,
        workspace_dir: &Path,
        fingerprint: manifest::Fingerprint,
        path_value: &str,
    ) -> Result<WorkspaceEnv, DirectExecError> {
        let package_json = workspace_dir.join("package.json");
        let metadata =
            std::fs::metadata(&package_json).map_err(|source| DirectExecError::PackageJson {
                path: package_json.clone(),
                message: source.to_string(),
            })?;
        let key = WorkspaceKey {
            workspace_dir: workspace_dir.to_path_buf(),
            manifest: fingerprint,
            package_json: (metadata.len(), metadata.modified().ok()),
            path_value: path_value.to_owned(),
        };
        if let Some((cached_key, cached)) = self.workspaces.get(workspace_dir) {
            if *cached_key == key && cached.shim_dir.is_dir() {
                return Ok(cached.clone());
            }
        }

        let raw = std::fs::read_to_string(&package_json).map_err(|source| {
            DirectExecError::PackageJson {
                path: package_json.clone(),
                message: source.to_string(),
            }
        })?;
        let parsed: WorkspaceManifest =
            serde_json::from_str(&raw).map_err(|error| DirectExecError::PackageJson {
                path: package_json.clone(),
                message: error.to_string(),
            })?;

        let node = tools::resolve_on_path("node", path_value)
            .ok_or(DirectExecError::ToolNotFound { name: "node" })?;
        let yarn = tools::resolve_on_path("yarn", path_value)
            .ok_or(DirectExecError::ToolNotFound { name: "yarn" })?;
        let bash = tools::resolve_on_path("bash", path_value)
            .ok_or(DirectExecError::ToolNotFound { name: "bash" })?;

        let (manifest, _, _) = self.manifests.get_or_load(&self.project_root)?;
        let binaries = bins::accessible_binaries(manifest, workspace_dir, &self.zips)?;
        let mut shims = shims::standard_shims(&node, &yarn);
        shims.extend(shims::binary_shims(&node, &binaries));
        let shim_dir = shims::materialize_shim_dir(&shims)?;

        let env = WorkspaceEnv {
            shim_dir,
            package_name: parsed.name.unwrap_or_default(),
            package_version: parsed.version.unwrap_or_default(),
            package_json,
            scripts: parsed.scripts,
            bash,
        };
        self.workspaces
            .insert(workspace_dir.to_path_buf(), (key, env.clone()));
        Ok(env)
    }

    fn versions(&mut self, path_value: &str) -> Result<Versions, DirectExecError> {
        if let Some(versions) = &self.versions {
            return Ok(versions.clone());
        }
        let yarn = tools::yarn_version_from_project(&self.project_root).or_else(|| {
            let yarn = tools::resolve_on_path("yarn", path_value)?;
            tools::probe_version(&yarn, path_value)
        });
        let node = tools::resolve_on_path("node", path_value)
            .and_then(|node| tools::probe_version(&node, path_value));
        let versions = Versions {
            yarn: yarn.unwrap_or_else(|| "unknown".to_owned()),
            node: node.unwrap_or_else(|| "unknown".to_owned()),
        };
        self.versions = Some(versions.clone());
        Ok(versions)
    }
}
