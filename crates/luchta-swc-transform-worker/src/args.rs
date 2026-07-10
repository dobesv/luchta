#![cfg(feature = "swc")]

use std::path::{Path, PathBuf};

use serde_json::{Map, Number, Value};
use swc_core::base::config::Options;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfigFile {
    pub path: PathBuf,
    pub input_pattern: ConfigFileInputPattern,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFileInputPattern {
    PackageRelative(PathBuf),
    WorkspaceRootRelative(PathBuf),
    FallbackGlob(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwcArgs {
    pub swcrc: bool,
    pub config_file: Option<ResolvedConfigFile>,
    pub env_name: Option<String>,
    pub out_dir: PathBuf,
    pub source_maps: Option<Value>,
    pub config: Value,
}

impl Default for SwcArgs {
    fn default() -> Self {
        Self {
            swcrc: true,
            config_file: None,
            env_name: None,
            out_dir: PathBuf::from("dist/js"),
            source_maps: None,
            config: base_config_json(),
        }
    }
}

impl SwcArgs {
    pub fn parse(command: &str, cwd: Option<&Path>) -> Result<Self, Vec<String>> {
        let tokens = split_argv(command)?;
        let mut args = Self::default();
        let mut index = 0;

        while index < tokens.len() {
            let token = &tokens[index];
            match token.as_str() {
                "--no-swcrc" => {
                    args.swcrc = false;
                    index += 1;
                }
                "--config-file" => {
                    let Some(value) = tokens.get(index + 1) else {
                        return Err(vec!["missing value for --config-file".to_owned()]);
                    };
                    args.config_file = Some(resolve_config_file(value, cwd));
                    index += 2;
                }
                value if value.starts_with("--config-file=") => {
                    let Some((_, path)) = value.split_once('=') else {
                        unreachable!();
                    };
                    args.config_file = Some(resolve_config_file(path, cwd));
                    index += 1;
                }
                "--env-name" => {
                    let Some(value) = tokens.get(index + 1) else {
                        return Err(vec!["missing value for --env-name".to_owned()]);
                    };
                    args.env_name = Some(value.clone());
                    index += 2;
                }
                value if value.starts_with("--env-name=") => {
                    let Some((_, env_name)) = value.split_once('=') else {
                        unreachable!();
                    };
                    args.env_name = Some(env_name.to_owned());
                    index += 1;
                }
                "-d" | "--out-dir" => {
                    let Some(value) = tokens.get(index + 1) else {
                        return Err(vec![format!("missing value for {token}")]);
                    };
                    args.out_dir = PathBuf::from(value);
                    index += 2;
                }
                value if value.starts_with("--out-dir=") => {
                    let Some((_, out_dir)) = value.split_once('=') else {
                        unreachable!();
                    };
                    args.out_dir = PathBuf::from(out_dir);
                    index += 1;
                }
                "--source-maps" => {
                    let Some(value) = tokens.get(index + 1) else {
                        return Err(vec!["missing value for --source-maps".to_owned()]);
                    };
                    args.source_maps = Some(normalize_source_maps(value));
                    index += 2;
                }
                value if value.starts_with("--source-maps=") => {
                    let Some((_, source_maps)) = value.split_once('=') else {
                        unreachable!();
                    };
                    args.source_maps = Some(normalize_source_maps(source_maps));
                    index += 1;
                }
                "-C" | "--config" => {
                    let Some(value) = tokens.get(index + 1) else {
                        return Err(vec![format!("missing value for {token}")]);
                    };
                    apply_config_entry(&mut args.config, value)?;
                    index += 2;
                }
                value if value.starts_with("--config=") => {
                    let Some((_, config)) = value.split_once('=') else {
                        unreachable!();
                    };
                    apply_config_entry(&mut args.config, config)?;
                    index += 1;
                }
                _ => {
                    index += 1;
                }
            }
        }

        Ok(args)
    }

    pub fn add_config_file_input(
        &self,
        cwd: &Path,
        inputs: &mut std::collections::BTreeSet<String>,
    ) {
        let Some(config_file) = &self.config_file else {
            return;
        };

        match &config_file.input_pattern {
            ConfigFileInputPattern::PackageRelative(relative) => {
                inputs.insert(normalize_path(relative));
            }
            ConfigFileInputPattern::WorkspaceRootRelative(relative) => {
                inputs.insert(format!("#{}", normalize_path(relative)));
            }
            ConfigFileInputPattern::FallbackGlob(pattern) => {
                inputs.insert(pattern.clone());
            }
        }

        if let Ok(relative) = config_file.path.strip_prefix(cwd) {
            inputs.insert(normalize_path(relative));
        }
    }

    pub fn out_root(&self, cwd: &Path) -> PathBuf {
        cwd.join(&self.out_dir)
    }

    pub fn build_options(&self, abs_path: &Path) -> Result<Options, Vec<String>> {
        let mut value = self.config.clone();
        apply_fixups(&mut value, self.swcrc, self.config_file.is_some());

        let mut object = value
            .as_object()
            .cloned()
            .ok_or_else(|| vec!["swc options config should be an object".to_owned()])?;
        object.insert("swcrc".to_owned(), Value::Bool(self.swcrc));
        object.insert(
            "configFile".to_owned(),
            match &self.config_file {
                Some(config_file) => Value::String(config_file.path.to_string_lossy().into_owned()),
                None => Value::Bool(false),
            },
        );
        object.insert(
            "filename".to_owned(),
            Value::String(abs_path.to_string_lossy().into_owned()),
        );
        if let Some(env_name) = &self.env_name {
            object.insert("envName".to_owned(), Value::String(env_name.clone()));
        }
        if let Some(source_maps) = &self.source_maps {
            object.insert("sourceMaps".to_owned(), source_maps.clone());
        } else {
            object.insert("sourceMaps".to_owned(), Value::Bool(true));
        }

        let mut options = serde_json::from_value::<Options>(Value::Object(object))
            .map_err(|error| vec![error.to_string()])?;
        options.runtime_options = swc_core::base::config::RuntimeOptions::default();
        Ok(options)
    }
}

fn apply_fixups(config: &mut Value, swcrc: bool, has_config_file: bool) {
    let has_env = config.get("env").is_some();
    let has_target = config
        .get("jsc")
        .and_then(Value::as_object)
        .is_some_and(|jsc| jsc.contains_key("target"));

    if has_env {
        if let Some(jsc) = config.get_mut("jsc").and_then(Value::as_object_mut) {
            jsc.remove("target");
        }
        return;
    }

    if has_target || swcrc || has_config_file {
        return;
    }

    merge_values(
        config,
        &serde_json::json!({
            "jsc": {
                "target": "es2022"
            }
        }),
    );
}

fn apply_config_entry(config: &mut Value, entry: &str) -> Result<(), Vec<String>> {
    let Some((key, value)) = entry.split_once('=') else {
        return Err(vec![format!("invalid -C/--config entry: {entry}")]);
    };
    let mut nested = Value::Object(Map::new());
    insert_nested_value(
        &mut nested,
        &key.split('.').collect::<Vec<_>>(),
        coerce_value(value),
    );
    merge_values(config, &nested);
    Ok(())
}

fn insert_nested_value(target: &mut Value, path: &[&str], value: Value) {
    if path.is_empty() {
        *target = value;
        return;
    }

    let object = target
        .as_object_mut()
        .expect("nested config target should always be object");
    if path.len() == 1 {
        object.insert(path[0].to_owned(), value);
        return;
    }

    let child = object
        .entry(path[0].to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !child.is_object() {
        *child = Value::Object(Map::new());
    }
    insert_nested_value(child, &path[1..], value);
}

fn merge_values(target: &mut Value, incoming: &Value) {
    match (target, incoming) {
        (Value::Object(target_obj), Value::Object(incoming_obj)) => {
            for (key, value) in incoming_obj {
                match target_obj.get_mut(key) {
                    Some(existing) => merge_values(existing, value),
                    None => {
                        target_obj.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, incoming) => *target = incoming.clone(),
    }
}

fn base_config_json() -> Value {
    serde_json::json!({
        "jsc": {
            "parser": {
                "syntax": "typescript",
                "tsx": true
            }
        }
    })
}

fn coerce_value(value: &str) -> Value {
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => {
            if let Ok(int_value) = value.parse::<i64>() {
                return Value::Number(Number::from(int_value));
            }
            if value.contains('.') {
                if let Ok(float_value) = value.parse::<f64>() {
                    if let Some(number) = Number::from_f64(float_value) {
                        return Value::Number(number);
                    }
                }
            }
            Value::String(value.to_owned())
        }
    }
}

fn normalize_source_maps(value: &str) -> Value {
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "both" | "inline" => Value::Bool(true),
        other => Value::String(other.to_owned()),
    }
}

fn resolve_config_file(value: &str, cwd: Option<&Path>) -> ResolvedConfigFile {
    if let Some(root_relative) = value.strip_prefix('#') {
        let relative = PathBuf::from(root_relative);
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        return ResolvedConfigFile {
            path: workspace_root.join(&relative),
            input_pattern: ConfigFileInputPattern::WorkspaceRootRelative(relative),
        };
    }

    let path = PathBuf::from(value);
    if path.is_absolute() {
        return ResolvedConfigFile {
            input_pattern: fallback_pattern_for(&path),
            path,
        };
    }

    let resolved = cwd.map_or_else(|| path.clone(), |cwd| cwd.join(&path));
    ResolvedConfigFile {
        path: resolved,
        input_pattern: ConfigFileInputPattern::PackageRelative(path),
    }
}

fn fallback_pattern_for(path: &Path) -> ConfigFileInputPattern {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "config.json".to_owned(), ToOwned::to_owned);
    ConfigFileInputPattern::FallbackGlob(format!("#**/{file_name}"))
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn split_argv(input: &str) -> Result<Vec<String>, Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for ch in input.chars() {
        match quote {
            Some(active) if ch == active => quote = None,
            Some(_) => current.push(ch),
            None if matches!(ch, '\'' | '"') => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }

    if let Some(active) = quote {
        return Err(vec![format!("unterminated quote {active}")]);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(all(test, feature = "swc"))]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{ConfigFileInputPattern, SwcArgs};

    #[test]
    fn parses_cli_flags_and_quotes() {
        let args = SwcArgs::parse(
            "--no-swcrc --config-file configs/swc.json --env-name node -d 'dist/node build' --source-maps both -C module.type=commonjs",
            Some(Path::new("/repo/pkg")),
        )
        .expect("parse args");

        assert!(!args.swcrc);
        assert_eq!(
            args.config_file.as_ref().map(|config| config.path.clone()),
            Some(Path::new("/repo/pkg/configs/swc.json").to_path_buf())
        );
        assert_eq!(args.env_name.as_deref(), Some("node"));
        assert_eq!(args.out_dir, Path::new("dist/node build"));
        assert_eq!(args.source_maps, Some(json!(true)));
        assert_eq!(args.config["module"]["type"], json!("commonjs"));
    }

    #[test]
    fn repeated_config_flags_deep_merge_and_override() {
        let args = SwcArgs::parse(
            "-C jsc.transform.react.runtime=classic -C jsc.transform.react.runtime=automatic -C module.type=es6",
            None,
        )
        .expect("parse args");

        assert_eq!(
            args.config["jsc"]["transform"]["react"]["runtime"],
            json!("automatic")
        );
        assert_eq!(args.config["module"]["type"], json!("es6"));
    }

    #[test]
    fn config_values_are_coerced_to_json_scalars() {
        let args = SwcArgs::parse(
            "-C env.mode=entry -C env.coreJs=3.3 -C jsc.loose=true -C custom.name=node18",
            None,
        )
        .expect("parse args");

        assert_eq!(args.config["env"]["mode"], json!("entry"));
        assert_eq!(args.config["env"]["coreJs"], json!(3.3));
        assert_eq!(args.config["jsc"]["loose"], json!(true));
        assert_eq!(args.config["custom"]["name"], json!("node18"));
    }

    #[test]
    fn build_options_drops_target_when_env_present() {
        let args = SwcArgs::parse(
            "-C env.mode=entry -C env.targets=ie 11 -C jsc.target=es5",
            None,
        )
        .expect("parse args");
        let options = args
            .build_options(Path::new("/repo/pkg/src/index.ts"))
            .expect("build options");

        assert!(options.config.env.is_some());
        assert!(options.config.jsc.target.is_none());
    }

    #[test]
    fn build_options_defaults_target_without_swcrc_or_config_file() {
        let args = SwcArgs::parse("--no-swcrc", None).expect("parse args");
        let options = args
            .build_options(Path::new("/repo/pkg/src/index.ts"))
            .expect("build options");

        assert_eq!(
            options.config.jsc.target,
            Some(swc_core::ecma::ast::EsVersion::Es2022)
        );
    }

    #[test]
    fn out_dir_defaults_and_config_file_input_are_resolved() {
        let args = SwcArgs::parse(
            "--config-file ../shared/.swcrc",
            Some(Path::new("/repo/pkg")),
        )
        .expect("parse args");
        assert_eq!(args.out_dir, Path::new("dist/js"));
        assert_eq!(
            args.config_file.as_ref().map(|config| config.path.clone()),
            Some(Path::new("/repo/pkg/../shared/.swcrc").to_path_buf())
        );
    }

    #[test]
    fn config_file_hash_form_resolves_to_workspace_root_input() {
        let args = SwcArgs::parse(
            "--config-file '#swc.node.json'",
            Some(Path::new("packages/foo")),
        )
        .expect("parse args");
        let config_file = args.config_file.as_ref().expect("config file");
        assert!(matches!(
            config_file.input_pattern,
            ConfigFileInputPattern::WorkspaceRootRelative(ref path) if path == Path::new("swc.node.json")
        ));

        let mut inputs = std::collections::BTreeSet::new();
        args.add_config_file_input(Path::new("packages/foo"), &mut inputs);
        assert!(inputs.contains("#swc.node.json"));
    }

    #[test]
    fn config_file_package_relative_form_stays_cwd_relative() {
        let args = SwcArgs::parse(
            "--config-file swc.node.json",
            Some(Path::new("packages/foo")),
        )
        .expect("parse args");
        let config_file = args.config_file.as_ref().expect("config file");
        assert!(matches!(
            config_file.input_pattern,
            ConfigFileInputPattern::PackageRelative(ref path) if path == Path::new("swc.node.json")
        ));
        assert_eq!(config_file.path, Path::new("packages/foo/swc.node.json"));

        let mut inputs = std::collections::BTreeSet::new();
        args.add_config_file_input(Path::new("packages/foo"), &mut inputs);
        assert!(inputs.contains("swc.node.json"));
    }

    #[test]
    fn absolute_outside_cwd_keeps_workspace_glob_fallback() {
        let args = SwcArgs {
            config_file: Some(super::ResolvedConfigFile {
                path: Path::new("/repo/swc.node.json").to_path_buf(),
                input_pattern: ConfigFileInputPattern::FallbackGlob("#**/swc.node.json".to_owned()),
            }),
            ..SwcArgs::default()
        };

        let mut inputs = std::collections::BTreeSet::new();
        args.add_config_file_input(Path::new("/repo/packages/foo"), &mut inputs);
        assert!(inputs.contains("#**/swc.node.json"));
    }

    #[test]
    fn inline_source_maps_are_normalized_to_external_maps() {
        let args = SwcArgs::parse("--source-maps inline", None).expect("parse args");
        let options = args
            .build_options(Path::new("/repo/pkg/src/index.ts"))
            .expect("build options");

        assert!(matches!(
            options.source_maps,
            Some(swc_core::base::config::SourceMapsConfig::Bool(true))
        ));
    }
}
