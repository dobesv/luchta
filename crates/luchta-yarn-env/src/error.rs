use std::path::PathBuf;

use thiserror::Error;

const HINT: &str = "Pass --no-direct to luchta-yarn-worker to run this task through yarn instead.";

/// Why direct execution could not be prepared for a task. Every message is
/// user-facing: it becomes the task's failure output.
#[derive(Debug, Error)]
pub enum DirectExecError {
    #[error("direct yarn execution needs `.pnp.cjs` in {project_root} (worker working directory); none found, so this is not a Yarn Plug'n'Play project or the worker is not running from the project root. {HINT}")]
    NoPnpManifest { project_root: PathBuf },

    #[error("direct yarn execution could not parse the PnP manifest {path}: {message}. {HINT}")]
    ManifestParse { path: PathBuf, message: String },

    #[error("direct yarn execution could not find a workspace at {workspace_dir} in the PnP manifest. {HINT}")]
    WorkspaceNotInManifest { workspace_dir: PathBuf },

    #[error("direct yarn execution could not read {path}: {message}. {HINT}")]
    PackageJson { path: PathBuf, message: String },

    #[error("direct yarn execution: script `{script}` is not declared in {package_json}. {HINT}")]
    ScriptNotFound {
        script: String,
        package_json: PathBuf,
    },

    #[error("direct yarn execution needs `{name}` on PATH and did not find it. {HINT}")]
    ToolNotFound { name: &'static str },

    #[error("direct yarn execution is not supported on Windows. {HINT}")]
    UnsupportedPlatform,

    #[error("direct yarn execution failed while {context}: {source}. {HINT}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl DirectExecError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
