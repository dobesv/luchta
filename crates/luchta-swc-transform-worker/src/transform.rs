#![cfg(feature = "swc")]

use std::path::{Path, PathBuf};

use serde_json::Value;
use swc_core::base::config::Options;
use swc_core::base::{try_with_handler, Compiler, HandlerOpts};
use swc_core::common::errors::ColorConfig;
use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, Globals, SourceMap, GLOBALS};

use crate::args::SwcArgs;

pub struct TransformSuccess {
    pub code: String,
    pub source_map_json: Option<String>,
}

pub fn transform_source(
    args: &SwcArgs,
    abs_path: &Path,
    source: &str,
    source_map_source_path: &Path,
    source_mapping_url: &str,
) -> Result<TransformSuccess, Vec<String>> {
    let cm: Lrc<SourceMap> = Default::default();
    let compiler = Compiler::new(cm.clone());
    let fm = cm.new_source_file(
        FileName::Real(abs_path.to_path_buf()).into(),
        source.to_string(),
    );
    let opts = args.build_options(abs_path)?;
    let output = run_transform(compiler, cm, fm, &opts)?;

    let source_map_json = output
        .map
        .map(|source_map_json| rewrite_source_map_sources(&source_map_json, source_map_source_path))
        .transpose()?;

    let mut code = output.code;
    if source_map_json.is_some() {
        code.push_str("\n//# sourceMappingURL=");
        code.push_str(source_mapping_url);
        code.push('\n');
    }

    Ok(TransformSuccess {
        code,
        source_map_json,
    })
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
        && is_transformable(path)
}

fn rewrite_source_map_sources(
    source_map_json: &str,
    source_map_source_path: &Path,
) -> Result<String, Vec<String>> {
    let mut source_map: Value =
        serde_json::from_str(source_map_json).map_err(|error| vec![error.to_string()])?;
    let normalized_source = normalize_path(source_map_source_path);
    let Some(source_map_object) = source_map.as_object_mut() else {
        return Err(vec!["source map json should be an object".to_owned()]);
    };
    source_map_object.insert("sources".to_owned(), serde_json::json!([normalized_source]));
    serde_json::to_string(&source_map).map_err(|error| vec![error.to_string()])
}

fn run_transform(
    compiler: Compiler,
    cm: Lrc<SourceMap>,
    fm: Lrc<swc_core::common::SourceFile>,
    opts: &Options,
) -> Result<swc_core::base::TransformOutput, Vec<String>> {
    let globals = Globals::default();
    GLOBALS
        .set(&globals, || {
            try_with_handler(
                cm,
                HandlerOpts {
                    color: ColorConfig::Never,
                    ..Default::default()
                },
                |handler| compiler.process_js_file(fm, handler, opts),
            )
        })
        .map_err(|error| vec![error.to_string()])
}

#[cfg(all(test, feature = "swc"))]
mod tests {
    use std::path::Path;

    use assert_fs::prelude::*;
    use serde_json::Value;

    use crate::args::SwcArgs;

    use super::{
        is_transformable, output_path_for, relative_source_map_source_path, should_skip,
        source_map_output_path, source_mapping_url, transform_source,
    };

    #[test]
    fn transforms_typescript_to_javascript_with_source_map() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        let source = temp.child("src/index.ts");
        source
            .write_str("const value: number = 1;\nexport const x = value;\n")
            .expect("write source");

        let out = transform_source(
            &SwcArgs::default(),
            source.path(),
            "const value: number = 1;\nexport const x = value;\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("transform succeeds");
        assert!(out.code.contains("value = 1"));
        assert!(out.code.contains("export"));
        assert!(out.code.contains("x = value"));
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
    fn honors_swcrc_target_when_present() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        temp.child(".swcrc")
            .write_str("{\"jsc\":{\"target\":\"es5\"}}")
            .expect("write .swcrc");
        let source = temp.child("src/index.ts");
        source
            .write_str("const squared = (n: number) => n ** 2;\n")
            .expect("write source");

        let out = transform_source(
            &SwcArgs::default(),
            source.path(),
            "const squared = (n: number) => n ** 2;\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(out.code.contains("var squared = function squared(n)"));
        assert!(out.code.contains("Math.pow(n, 2)"));
    }

    #[test]
    fn no_swcrc_flag_ignores_sibling_swcrc() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        temp.child(".swcrc")
            .write_str("{\"jsc\":{\"target\":\"es5\"}}")
            .expect("write .swcrc");
        let source = temp.child("src/index.ts");
        source
            .write_str("const squared = (n: number) => n ** 2;\n")
            .expect("write source");

        let args = SwcArgs::parse("--no-swcrc", Some(temp.path())).expect("parse args");
        let out = transform_source(
            &args,
            source.path(),
            "const squared = (n: number) => n ** 2;\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(out.code.contains("const squared = (n)=>n ** 2;"));
        assert!(!out.code.contains("Math.pow(n, 2)"));
    }

    #[test]
    fn config_can_switch_module_output_types() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        let source = temp.child("src/index.ts");
        source
            .write_str("export const value: number = 1;\n")
            .expect("write source");

        let cjs_args =
            SwcArgs::parse("-C module.type=commonjs", Some(temp.path())).expect("parse args");
        let cjs = transform_source(
            &cjs_args,
            source.path(),
            "export const value: number = 1;\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("cjs transform succeeds");
        assert!(cjs.code.contains("Object.defineProperty(exports"));

        let esm_args = SwcArgs::parse("-C module.type=es6", Some(temp.path())).expect("parse args");
        let esm = transform_source(
            &esm_args,
            source.path(),
            "export const value: number = 1;\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("esm transform succeeds");
        assert!(esm.code.contains("export"));
        assert!(esm.code.contains("value = 1"));
    }

    #[test]
    fn config_can_enable_react_automatic_runtime() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        let source = temp.child("src/index.tsx");
        source
            .write_str("export const App = () => <div />;\n")
            .expect("write source");

        let args = SwcArgs::parse(
            "-C jsc.transform.react.runtime=automatic",
            Some(temp.path()),
        )
        .expect("parse args");
        let out = transform_source(
            &args,
            source.path(),
            "export const App = () => <div />;\n",
            Path::new("src/index.tsx"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(out.code.contains("react/jsx-runtime"));
        assert!(out.code.contains("_jsx"));
    }

    #[test]
    fn env_config_downlevels_without_target_conflict() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        let source = temp.child("src/index.ts");
        source
            .write_str("const values = [1, 2, 3];\nconst merged = [...values];\n")
            .expect("write source");

        temp.child("env.swcrc")
            .write_str(
                "{\"env\":{\"mode\":\"entry\",\"coreJs\":3.30,\"targets\":{\"chrome\":\"40\"}}}",
            )
            .expect("write env config");
        let args = SwcArgs::parse("--no-swcrc --config-file env.swcrc", Some(temp.path()))
            .expect("parse args");
        let out = transform_source(
            &args,
            source.path(),
            "const values = [1, 2, 3];\nconst merged = [...values];\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(!out.code.contains("env` and `jsc.target`"));
        assert!(out.code.contains("_to_consumable_array") || out.code.contains("concat("));
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
        assert!(is_transformable(Path::new("file.jsx")));
        assert!(is_transformable(Path::new("file.tsx")));
        assert!(is_transformable(Path::new("file.mjs")));
        assert!(is_transformable(Path::new("file.cjs")));
        assert!(is_transformable(Path::new("file.mts")));
        assert!(is_transformable(Path::new("file.cts")));
        assert!(!is_transformable(Path::new("file.css")));
    }

    #[test]
    fn skips_babel_parity_test_story_files() {
        // Original extensions
        assert!(should_skip(Path::new("src/button.stories.tsx")));
        assert!(should_skip(Path::new("src/foo.unitTest.ts")));
        assert!(should_skip(Path::new("src/foo.intTest.ts")));
        assert!(should_skip(Path::new("src/foo.e2eTest.ts")));
        assert!(!should_skip(Path::new("src/foo.spec.ts")));

        // Newly-covered extensions (mjs, cjs, mts, cts)
        assert!(should_skip(Path::new("src/button.stories.mts")));
        assert!(should_skip(Path::new("src/foo.unitTest.cts")));
        assert!(should_skip(Path::new("src/foo.intTest.mjs")));
        assert!(should_skip(Path::new("src/foo.e2eTest.cjs")));

        // Non-test files with new extensions should NOT be skipped
        assert!(!should_skip(Path::new("src/foo.mts")));
        assert!(!should_skip(Path::new("src/foo.cts")));
        assert!(!should_skip(Path::new("src/foo.mjs")));
        assert!(!should_skip(Path::new("src/foo.cjs")));
    }

    #[test]
    fn source_maps_disabled_no_url_appended() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        let source = temp.child("src/index.ts");
        source
            .write_str("export const value: number = 1;\n")
            .expect("write source");

        let args = SwcArgs::parse("--no-swcrc --source-maps false", Some(temp.path()))
            .expect("parse args");
        let out = transform_source(
            &args,
            source.path(),
            "export const value: number = 1;\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(
            !out.code.contains("sourceMappingURL"),
            "when source maps disabled, code should NOT contain sourceMappingURL"
        );
        assert!(
            out.source_map_json.is_none(),
            "when source maps disabled, source_map_json should be None"
        );
    }

    #[test]
    fn source_maps_enabled_url_appended() {
        let temp = assert_fs::TempDir::new().expect("temp dir");
        let source = temp.child("src/index.ts");
        source
            .write_str("export const value: number = 1;\n")
            .expect("write source");

        let args =
            SwcArgs::parse("--no-swcrc --source-maps true", Some(temp.path())).expect("parse args");
        let out = transform_source(
            &args,
            source.path(),
            "export const value: number = 1;\n",
            Path::new("src/index.ts"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(
            out.code.ends_with("\n//# sourceMappingURL=index.js.map\n"),
            "when source maps enabled, code should end with sourceMappingURL comment"
        );
        assert!(
            out.source_map_json.is_some(),
            "when source maps enabled, source_map_json should be Some"
        );
    }

    const REACT_COMPONENT_SOURCE: &str =
        "export function Foo({ items }: { items: number[] }) {\n  return <ul>{items.map((n) => <li key={n}>{n}</li>)}</ul>;\n}\n";

    #[test]
    fn react_compiler_memoizes_components_when_enabled() {
        // With `jsc.transform.reactCompiler` enabled, the SWC worker must run the
        // React Compiler transform (issue #264). The compiler rewrites components
        // to allocate a memoization cache from `react/compiler-runtime`.
        let temp = assert_fs::TempDir::new().expect("temp dir");
        temp.child(".swcrc")
            .write_str(
                r#"{"jsc":{"parser":{"syntax":"typescript","tsx":true},"transform":{"reactCompiler":true}}}"#,
            )
            .expect("write .swcrc");
        let source = temp.child("src/index.tsx");
        source
            .write_str(REACT_COMPONENT_SOURCE)
            .expect("write source");

        let args = SwcArgs::parse("", Some(temp.path())).expect("parse args");
        let out = transform_source(
            &args,
            source.path(),
            REACT_COMPONENT_SOURCE,
            Path::new("src/index.tsx"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(
            out.code.contains("react/compiler-runtime"),
            "React Compiler output should import the compiler runtime, got:\n{}",
            out.code
        );
        assert!(
            out.code.contains("_c("),
            "React Compiler output should allocate a memoization cache via _c(), got:\n{}",
            out.code
        );
    }

    #[test]
    fn react_compiler_not_applied_when_disabled() {
        // Without the `reactCompiler` flag the output must be a plain JSX->JS
        // transform with no compiler-runtime memoization cache.
        let temp = assert_fs::TempDir::new().expect("temp dir");
        temp.child(".swcrc")
            .write_str(r#"{"jsc":{"parser":{"syntax":"typescript","tsx":true}}}"#)
            .expect("write .swcrc");
        let source = temp.child("src/index.tsx");
        source
            .write_str(REACT_COMPONENT_SOURCE)
            .expect("write source");

        let args = SwcArgs::parse("", Some(temp.path())).expect("parse args");
        let out = transform_source(
            &args,
            source.path(),
            REACT_COMPONENT_SOURCE,
            Path::new("src/index.tsx"),
            "index.js.map",
        )
        .expect("transform succeeds");

        assert!(
            !out.code.contains("react/compiler-runtime"),
            "React Compiler must not run when disabled, got:\n{}",
            out.code
        );
        assert!(
            !out.code.contains("_c("),
            "React Compiler must not allocate a memoization cache when disabled, got:\n{}",
            out.code
        );
    }
}
