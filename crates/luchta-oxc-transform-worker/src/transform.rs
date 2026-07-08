#![cfg(feature = "oxc")]

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};

pub struct TransformSuccess {
    pub code: String,
    pub source_map_json: Option<String>,
}

pub fn transform_source(
    path: &Path,
    source: &str,
    target: &str,
    source_map_source_path: &Path,
    source_mapping_url: &str,
) -> Result<TransformSuccess, Vec<String>> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).map_err(|error| vec![error.to_string()])?;
    let parse_return = Parser::new(&allocator, source, source_type).parse();

    if !parse_return.diagnostics.is_empty() {
        return Err(parse_return
            .diagnostics
            .into_iter()
            .map(|diag| diag.to_string())
            .collect());
    }

    let mut program = parse_return.program;
    let semantic_return = SemanticBuilder::new().build(&program);
    if !semantic_return.diagnostics.is_empty() {
        return Err(semantic_return
            .diagnostics
            .into_iter()
            .map(|diag| diag.to_string())
            .collect());
    }

    let options = TransformOptions::from_target(target).map_err(|error| vec![error])?;
    let transform_return = Transformer::new(&allocator, path, &options)
        .build_with_scoping(semantic_return.semantic.into_scoping(), &mut program);

    if !transform_return.diagnostics.is_empty() {
        return Err(transform_return
            .diagnostics
            .into_iter()
            .map(|diag| diag.to_string())
            .collect());
    }

    let codegen_return = Codegen::new()
        .with_scoping(Some(transform_return.scoping))
        .with_options(CodegenOptions {
            source_map_path: Some(source_map_source_path.to_path_buf()),
            ..CodegenOptions::default()
        })
        .build(&program);

    let mut code = codegen_return.code;
    code.push_str("\n//# sourceMappingURL=");
    code.push_str(source_mapping_url);
    code.push('\n');

    let source_map_json = codegen_return.map.map(|map| map.to_json_string());

    Ok(TransformSuccess {
        code,
        source_map_json,
    })
}

pub fn derive_env_name(task_id: &str) -> String {
    let task_name = task_id.split_once('#').map_or(task_id, |(_, task)| task);
    task_name
        .strip_prefix("build:")
        .filter(|env| !env.is_empty())
        .unwrap_or("js")
        .to_owned()
}

pub fn resolve_target_env(env_name: &str) -> String {
    if TransformOptions::from_target(env_name).is_ok() {
        return env_name.to_owned();
    }

    // v1 mapping: pass through valid OXC targets/envs; otherwise use conservative fixed defaults.
    // Source maps emitted alongside transformed `.js` outputs.
    match env_name {
        "node" => "es2022",
        "browser" => "es2017",
        "js" => "es2022",
        _ => "es2022",
    }
    .to_owned()
}

pub fn output_path_for(
    src_root: &Path,
    out_root: &Path,
    source_path: &Path,
) -> Result<PathBuf, String> {
    let relative = source_path.strip_prefix(src_root).map_err(|error| {
        format!(
            "failed to derive relative path for {}: {error}",
            source_path.display()
        )
    })?;
    let mut out_path = out_root.join(relative);
    out_path.set_extension("js");
    Ok(out_path)
}

pub fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn source_map_output_path(output_path: &Path) -> PathBuf {
    let file_name = output_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("output path should have file name");
    output_path.with_file_name(format!("{file_name}.map"))
}

pub fn relative_source_map_source_path(cwd: &Path, source_path: &Path) -> PathBuf {
    source_path.strip_prefix(cwd).map_or_else(
        |_| PathBuf::from(normalize_path(source_path)),
        |relative| PathBuf::from(normalize_path(relative)),
    )
}

pub fn source_mapping_url(source_map_output_path: &Path) -> Result<String, String> {
    source_map_output_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            format!(
                "invalid source map output path: {}",
                source_map_output_path.display()
            )
        })
}

pub fn is_transformable(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts")
    )
}

pub fn should_skip(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    [".unitTest.", ".intTest.", ".e2eTest.", ".stories."]
        .iter()
        .any(|needle| name.contains(needle))
        && matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("js" | "jsx" | "ts" | "tsx")
        )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    use super::{
        derive_env_name, is_transformable, output_path_for, relative_source_map_source_path,
        resolve_target_env, should_skip, source_map_output_path, source_mapping_url,
        transform_source,
    };

    #[test]
    fn transforms_typescript_to_javascript_with_source_map() {
        let path = Path::new("src/index.ts");
        let out = transform_source(
            path,
            "const value: number = 1;\nexport const x = value;\n",
            "es2022",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("transform succeeds");
        assert!(out.code.contains("const value = 1;"));
        assert!(out.code.contains("export const x = value;"));
        assert!(out.code.ends_with("\n//# sourceMappingURL=index.js.map\n"));
        let source_map_json = out.source_map_json.expect("source map json");
        let source_map: Value =
            serde_json::from_str(&source_map_json).expect("valid source map json");
        assert_eq!(source_map["version"], 3);
        assert!(source_map["mappings"]
            .as_str()
            .is_some_and(|mappings| !mappings.is_empty()));
        assert_eq!(source_map["sources"], serde_json::json!(["src/index.ts"]));
    }

    #[test]
    fn derives_env_name_like_babel_worker() {
        assert_eq!(derive_env_name("pkg#build:node"), "node");
        assert_eq!(derive_env_name("pkg#build:browser"), "browser");
        assert_eq!(derive_env_name("pkg#build"), "js");
    }

    #[test]
    fn resolves_targets_with_fallbacks() {
        assert_eq!(resolve_target_env("es2020"), "es2020");
        assert_eq!(resolve_target_env("node"), "es2022");
        assert_eq!(resolve_target_env("browser"), "es2017");
        assert_eq!(resolve_target_env("weird"), "es2022");
    }

    #[test]
    fn output_path_rewrites_extension() {
        let path = output_path_for(
            Path::new("src"),
            Path::new("dist/js"),
            Path::new("src/nested/file.tsx"),
        )
        .expect("output path");
        assert_eq!(path, Path::new("dist/js/nested/file.js"));
    }

    #[test]
    fn source_map_paths_match_js_outputs_and_relative_sources() {
        assert_eq!(
            source_map_output_path(Path::new("dist/js/nested/file.js")),
            Path::new("dist/js/nested/file.js.map")
        );
        assert_eq!(
            relative_source_map_source_path(
                Path::new("/repo"),
                Path::new("/repo/src/nested/file.tsx")
            ),
            Path::new("src/nested/file.tsx")
        );
        assert_eq!(
            source_mapping_url(Path::new("dist/js/nested/file.js.map")).expect("basename"),
            "file.js.map"
        );
    }

    #[test]
    fn matches_transformable_extensions() {
        assert!(is_transformable(Path::new("file.ts")));
        assert!(is_transformable(Path::new("file.mjs")));
        assert!(!is_transformable(Path::new("file.css")));
    }

    #[test]
    fn skips_babel_parity_test_story_files() {
        assert!(should_skip(Path::new("src/button.stories.tsx")));
        assert!(should_skip(Path::new("src/foo.unitTest.ts")));
        assert!(!should_skip(Path::new("src/foo.spec.ts")));
    }
}
