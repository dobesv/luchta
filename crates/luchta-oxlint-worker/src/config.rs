#![cfg(feature = "oxc")]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use oxc_linter::{ConfigStore, ConfigStoreBuilder, ExternalPluginStore, Oxlintrc};
use rustc_hash::{FxBuildHasher, FxHashMap};

const ROOT_CONFIG_FILENAMES: [&str; 2] = [".oxlintrc.json", ".oxlintrc.jsonc"];
const TS_CONFIG_FILENAME: &str = "oxlint.config.ts";
const SKIP_DIRS: [&str; 2] = [".git", "node_modules"];

#[derive(Debug)]
pub struct LoadedConfig {
    pub store: ConfigStore,
    pub root_config_path: Option<PathBuf>,
    pub saw_only_unsupported_ts_config: bool,
}

pub fn discover_config(cwd: &Path) -> Result<LoadedConfig, String> {
    let root_config_path = find_root_config_path(cwd);
    let saw_only_unsupported_ts_config =
        root_config_path.is_none() && find_root_ts_config_path(cwd).is_some();
    let oxlintrc = match root_config_path.as_deref() {
        Some(path) => Oxlintrc::from_file(path)
            .map_err(|error| format!("failed to load oxlint config {}: {error}", path.display()))?,
        None => Oxlintrc::default(),
    };
    let mut external_plugin_store = ExternalPluginStore::new(true);
    let base_config =
        ConfigStoreBuilder::from_oxlintrc(false, oxlintrc, None, &mut external_plugin_store, None)
            .map_err(|error| format!("failed to build oxlint config: {error}"))?
            .build(&mut external_plugin_store)
            .map_err(|error| format!("failed to finalize oxlint config: {error}"))?;
    let nested = FxHashMap::with_capacity_and_hasher(0, FxBuildHasher);
    Ok(LoadedConfig {
        store: ConfigStore::new(base_config, nested, external_plugin_store),
        root_config_path,
        saw_only_unsupported_ts_config,
    })
}

pub fn collect_target_files(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for entry in WalkBuilder::new(cwd)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_global(false)
        .follow_links(true)
        .build()
    {
        let entry =
            entry.map_err(|error| format!("failed to walk workspace for sources: {error}"))?;
        let path = entry.into_path();
        if path.is_dir() || should_skip_path(&path) || !is_js_ts_source(&path) {
            continue;
        }
        files.push(path);
    }
    files.sort();
    Ok(files)
}

fn find_root_config_path(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        for filename in ROOT_CONFIG_FILENAMES {
            let candidate = ancestor.join(filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_root_ts_config_path(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        let candidate = ancestor.join(TS_CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn should_skip_path(path: &Path) -> bool {
    path.components().any(|component| {
        let text = component.as_os_str().to_string_lossy();
        SKIP_DIRS.iter().any(|skip| text == *skip)
    })
}

fn is_js_ts_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts")
    )
}
