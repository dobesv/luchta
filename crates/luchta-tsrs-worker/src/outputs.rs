//! Clean stale declarations before each config's emit and report emitted paths.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use tsrs_core::{tspath, CompilerOptions};
use tsrs_tsoptions::ParsedCommandLine;

/// Call once per parsed candidate config, before constructing/emitting its program.
/// `cwd` must be the package's absolute working directory. Config parsing must use
/// the job's `CompilerSystem`, so inherited output paths and source roots agree.
/// `emitted` contains successful writes from earlier candidates in this run.
pub fn clean_outputs(
    cwd: &Path,
    config: &ParsedCommandLine,
    emitted: &[String],
) -> Result<(), String> {
    let options = config
        .compiler_options()
        .ok_or("parsed config has no compiler options")?;
    if options.no_emit.is_true() {
        return Ok(());
    }
    // Parsed roots include authored declarations. Declarations reached only by
    // imports aren't protected: cleanup runs before the program graph exists.
    let protected = config
        .file_names()
        .iter()
        .chain(emitted)
        .map(|path| protected_path(&cwd.join(path)))
        .collect();
    clean_output_dir(
        cwd,
        &clean_dir_of(cwd, &options),
        Path::new(&config.common_source_directory()),
        &protected,
    )
}

fn protected_path(path: &Path) -> String {
    // Cleanup walks a canonical root; match symlinked config/output paths too.
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = tspath::normalize_path(&tspath::normalize_slashes(&path.to_string_lossy()));
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path
    }
}

fn clean_dir_of(cwd: &Path, options: &CompilerOptions) -> PathBuf {
    if !options.declaration_dir.is_empty() {
        return cwd.join(&options.declaration_dir);
    }
    if !options.out_dir.is_empty() {
        return cwd.join(&options.out_dir);
    }
    cwd.join("dist/types")
}

/// Return sorted, forward-slash paths relative to `cwd`. Keep duplicates, as the
/// Go worker does when multiple configs emit the same file. No filesystem access.
pub fn resolve_outputs(cwd: &Path, outputs: &[String]) -> Vec<String> {
    let cwd = tspath::normalize_slashes(&cwd.to_string_lossy());
    let comparison = tspath::ComparePathsOptions {
        use_case_sensitive_file_names: !cfg!(windows),
        ..Default::default()
    };
    let mut outputs: Vec<_> = outputs
        .iter()
        .map(|output| relativize_output(&cwd, output, &comparison))
        .collect();
    outputs.sort();
    outputs
}

fn relativize_output(cwd: &str, output: &str, comparison: &tspath::ComparePathsOptions) -> String {
    let output = tspath::normalize_slashes(output);
    // filepath.Rel falls back to the original path when only one is absolute.
    if (tspath::get_root_length(cwd) > 0) != (tspath::get_root_length(&output) > 0) {
        return output;
    }
    let relative = tspath::get_relative_path_from_directory(cwd, &output, comparison);
    if relative.is_empty() {
        return ".".to_owned();
    }
    relative
}

fn clean_output_dir(
    cwd: &Path,
    clean_dir: &Path,
    source_root: &Path,
    protected: &HashSet<String>,
) -> Result<(), String> {
    let cwd = fs::canonicalize(cwd)
        .map_err(|error| format!("resolve package directory {cwd:?}: {error}"))?;
    let (clean_dir, exists) = resolve_clean_dir(clean_dir)?;
    ensure_local_clean_dir(&cwd, &clean_dir)?;
    if !exists {
        return Ok(());
    }
    clean_directory(&clean_dir, &clean_dir, source_root, protected)
}

/// Resolve the closest existing ancestor too: a missing output directory can
/// still escape through an intermediate symlink, and must fail before emitting.
fn resolve_clean_dir(clean_dir: &Path) -> Result<(PathBuf, bool), String> {
    let mut ancestor = clean_dir;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    append_component(&mut resolved, *component);
                }
                return Ok((resolved, suffix.is_empty()));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("resolve output directory {ancestor:?}: {error}"));
            }
        }
        let component = ancestor.components().next_back().ok_or_else(|| {
            format!("resolve output directory {clean_dir:?}: no existing ancestor")
        })?;
        suffix.push(component);
        ancestor = ancestor.parent().ok_or_else(|| {
            format!("resolve output directory {clean_dir:?}: no existing ancestor")
        })?;
    }
}

fn append_component(path: &mut PathBuf, component: Component<'_>) {
    if component == Component::ParentDir {
        path.pop();
    } else {
        path.push(component.as_os_str());
    }
}

fn ensure_local_clean_dir(cwd: &Path, clean_dir: &Path) -> Result<(), String> {
    if !clean_dir.starts_with(cwd) {
        return Err(format!(
            "refusing to clean output directory {clean_dir:?} outside package {cwd:?}"
        ));
    }
    Ok(())
}

fn clean_entry(
    root: &Path,
    path: &Path,
    source_root: &Path,
    protected: &HashSet<String>,
) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if vanished_child(root, path, &error) => return Ok(()),
        Err(error) => return Err(format!("inspect output {path:?}: {error}")),
    };
    if metadata.is_dir() {
        return clean_directory(root, path, source_root, protected);
    }
    remove_stale_declaration(root, path, source_root, protected)
}

fn clean_directory(
    root: &Path,
    path: &Path,
    source_root: &Path,
    protected: &HashSet<String>,
) -> Result<(), String> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if vanished_child(root, path, &error) => return Ok(()),
        Err(error) => return Err(format!("read output directory {path:?}: {error}")),
    };
    // Like Go WalkDir, don't follow directory symlinks below the cleanup root.
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("read output entry in {path:?}: {error}")),
        };
        clean_entry(root, &entry.path(), source_root, protected)?;
    }
    Ok(())
}

fn vanished_child(root: &Path, path: &Path, error: &io::Error) -> bool {
    path != root && error.kind() == io::ErrorKind::NotFound
}

fn remove_stale_declaration(
    root: &Path,
    path: &Path,
    source_root: &Path,
    protected: &HashSet<String>,
) -> Result<(), String> {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let declaration = file_name.strip_suffix(".map").unwrap_or(&file_name);
    if !tspath::file_extension_is_one_of(declaration, tspath::SUPPORTED_DECLARATION_EXTENSIONS) {
        return Ok(());
    }
    if protected.contains(&protected_path(path))
        || (file_name.ends_with(".map")
            && protected.contains(&protected_path(&path.with_file_name(declaration))))
    {
        return Ok(());
    }
    let extension = tspath::get_declaration_file_extension(declaration);
    let stem = &declaration[..declaration.len() - extension.len()];
    let relative_dir = path.parent().unwrap().strip_prefix(root).unwrap();
    for extension in tspath::get_possible_original_input_extension_for_extension(declaration) {
        let source = source_root
            .join(relative_dir)
            .join(format!("{stem}{extension}"));
        if source_file_exists(&source)? {
            return Ok(());
        }
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if vanished_child(root, path, &error) => Ok(()),
        Err(error) => Err(format!("remove stale declaration {path:?}: {error}")),
    }
}

fn source_file_exists(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("stat source {path:?}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnp::CompilerSystem;
    use luchta_test_support::require_nextest;
    use serde_json::json;
    use tsrs_tsoptions::{get_parsed_command_line_of_config_file, ParseConfigHost};

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn parse(cwd: &Path, options: serde_json::Value) -> ParsedCommandLine {
        write(
            &cwd.join("tsconfig.json"),
            &json!({"compilerOptions": options, "include": ["src/**/*"]}).to_string(),
        );
        parse_file(cwd)
    }

    fn parse_file(cwd: &Path) -> ParsedCommandLine {
        let system = CompilerSystem::new(cwd).unwrap();
        let config_path = format!("{}/tsconfig.json", system.get_current_directory());
        let (config, errors) =
            get_parsed_command_line_of_config_file(&config_path, None, None, &system, None);
        assert!(errors.is_empty(), "config read diagnostics: {errors:?}");
        let config = config.unwrap();
        assert!(
            config.errors.is_empty(),
            "config diagnostics: {:?}",
            config.errors
        );
        config
    }

    fn clean(cwd: &Path) -> Result<(), String> {
        clean_output_dir(
            cwd,
            &cwd.join("dist/types"),
            &cwd.join("src"),
            &HashSet::new(),
        )
    }

    fn declaration_pair(dir: &Path, name: &str) {
        write(&dir.join(name), "export declare const value: number;");
        write(&dir.join(format!("{name}.map")), "{}");
    }

    fn assert_pair(dir: &Path, name: &str, exists: bool) {
        assert_eq!(dir.join(name).exists(), exists, "{name}");
        assert_eq!(
            dir.join(format!("{name}.map")).exists(),
            exists,
            "{name}.map"
        );
    }

    // Go: TestRelativizeOutputs.
    #[test]
    fn outputs_relativize_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        let outputs = ["dist/a.js", "dist/types/a.d.ts"]
            .map(|name| cwd.join(name).to_string_lossy().into_owned());
        assert_eq!(
            resolve_outputs(cwd, &outputs),
            ["dist/a.js", "dist/types/a.d.ts"]
        );
    }

    // Go: TestCleanOutputsRemovesStaleDts. Also cover nested paths and TSX.
    #[test]
    fn outputs_clean_removes_stale_dts() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        let dir = cwd.join("dist/types");
        write(&cwd.join("src/keep.ts"), "export const x = 1;");
        write(&cwd.join("src/nested/view.tsx"), "export const view = 1;");
        for name in [
            "keep.d.ts",
            "gone.d.ts",
            "nested/view.d.ts",
            "nested/gone.d.ts",
        ] {
            declaration_pair(&dir, name);
        }
        write(&dir.join("unrelated.js"), "keep");
        write(&dir.join("unrelated.js.map"), "keep");
        write(&dir.join("unrelated.d.css.ts"), "keep");
        clean(cwd).unwrap();
        for name in ["keep.d.ts", "nested/view.d.ts"] {
            assert_pair(&dir, name, true);
        }
        for name in ["gone.d.ts", "nested/gone.d.ts"] {
            assert_pair(&dir, name, false);
        }
        for name in ["unrelated.js", "unrelated.js.map", "unrelated.d.css.ts"] {
            assert!(dir.join(name).exists(), "{name}");
        }
    }

    // Go: TestCleanOutputsPreservesAllowJSSources.
    #[test]
    fn outputs_clean_preserves_allow_js_sources() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        let dir = cwd.join("dist/types");
        for extension in ["js", "jsx"] {
            write(
                &cwd.join(format!("src/from-{extension}.{extension}")),
                "export const x = 1;",
            );
            declaration_pair(&dir, &format!("from-{extension}.d.ts"));
        }
        clean(cwd).unwrap();
        for extension in ["js", "jsx"] {
            assert_pair(&dir, &format!("from-{extension}.d.ts"), true);
        }
    }

    // Go: TestCleanOutputsHandlesModuleDeclarationExtensions.
    #[test]
    fn outputs_clean_handles_module_declaration_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        let dir = cwd.join("dist/types");
        for (source, declaration) in [
            ("mts", "d.mts"),
            ("mjs", "d.mts"),
            ("cts", "d.cts"),
            ("cjs", "d.cts"),
        ] {
            write(
                &cwd.join(format!("src/live-{source}.{source}")),
                "export const x = 1;",
            );
            declaration_pair(&dir, &format!("live-{source}.{declaration}"));
        }
        for name in ["gone.d.mts", "gone.d.cts"] {
            declaration_pair(&dir, name);
        }
        clean(cwd).unwrap();
        for name in [
            "live-mts.d.mts",
            "live-mjs.d.mts",
            "live-cts.d.cts",
            "live-cjs.d.cts",
        ] {
            assert_pair(&dir, name, true);
        }
        for name in ["gone.d.mts", "gone.d.cts"] {
            assert_pair(&dir, name, false);
        }
    }

    // Go: TestCleanOutputsSkipsWhenNoEmit.
    #[test]
    fn outputs_clean_skips_when_no_emit() {
        require_nextest();
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        write(&cwd.join("src/keep.ts"), "export const x = 1;");
        let dir = cwd.join("dist/types");
        declaration_pair(&dir, "gone.d.ts");
        let config = parse(cwd, json!({"noEmit": true, "outDir": "../outside"}));
        clean_outputs(cwd, &config, &[]).unwrap();
        clean_outputs(&cwd.join("missing-package"), &config, &[]).unwrap();
        assert_pair(&dir, "gone.d.ts", true);
    }

    // Go: TestCleanOutputsRejectsOutsideMissingDirectory.
    #[test]
    fn outputs_clean_rejects_outside_missing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("packages/pkg");
        fs::create_dir_all(&cwd).unwrap();
        let error = clean_output_dir(
            &cwd,
            &temp.path().join("dist"),
            &cwd.join("src"),
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(error.contains("outside package"), "{error}");
    }

    // Go: TestCleanOutputsRejectsIntermediateSymlinkEscape.
    // Windows symlink creation requires privileges unavailable on some CI hosts.
    #[cfg(unix)]
    #[test]
    fn outputs_clean_rejects_intermediate_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("pkg");
        let outside = temp.path().join("outside");
        fs::create_dir_all(cwd.join("build")).unwrap();
        declaration_pair(&outside.join("types"), "gone.d.ts");
        std::os::unix::fs::symlink(&outside, cwd.join("build/link")).unwrap();
        let error = clean_output_dir(
            &cwd,
            &cwd.join("build/link/types"),
            &cwd.join("src"),
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(error.contains("outside package"), "{error}");
        assert_pair(&outside.join("types"), "gone.d.ts", true);
        let error = clean_output_dir(
            &cwd,
            &cwd.join("build/link/missing/types"),
            &cwd.join("src"),
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(error.contains("outside package"), "{error}");
    }

    // Go: TestCleanOutputsDoesNotDeleteWhenSourceStatFails.
    #[test]
    fn outputs_clean_does_not_delete_when_source_stat_fails() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        write(&cwd.join("src/nested"), "not a directory");
        let dir = cwd.join("dist/types");
        declaration_pair(&dir, "nested/gone.d.ts");
        let error = clean(cwd).unwrap_err();
        assert!(error.contains("stat source"), "{error}");
        assert_pair(&dir, "nested/gone.d.ts", true);
    }

    #[test]
    fn outputs_clean_uses_config_directory_precedence_and_source_root() {
        require_nextest();
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        write(&cwd.join("src/keep.ts"), "export const x = 1;");
        for (mut options, expected) in [
            (
                json!({"declaration": true, "declarationDir": "declarations", "outDir": "out"}),
                "declarations",
            ),
            (json!({"outDir": "out"}), "out"),
            (json!({}), "dist/types"),
        ] {
            for dir in ["declarations", "out", "dist/types"] {
                declaration_pair(&cwd.join(dir), "keep.d.ts");
                declaration_pair(&cwd.join(dir), "gone.d.ts");
            }
            options["rootDir"] = json!("src");
            let config = parse(cwd, options);
            clean_outputs(cwd, &config, &[]).unwrap();
            for dir in ["declarations", "out", "dist/types"] {
                assert_pair(&cwd.join(dir), "keep.d.ts", true);
                assert_pair(&cwd.join(dir), "gone.d.ts", dir != expected);
            }
        }
    }

    #[test]
    fn outputs_clean_uses_parsed_default_source_root() {
        require_nextest();
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        write(&cwd.join("src/keep.ts"), "export const x = 1;");
        let dir = cwd.join("dist/types");
        declaration_pair(&dir, "src/keep.d.ts");
        declaration_pair(&dir, "src/gone.d.ts");
        let config = parse(cwd, json!({}));
        clean_outputs(cwd, &config, &[]).unwrap();
        assert_pair(&dir, "src/keep.d.ts", true);
        assert_pair(&dir, "src/gone.d.ts", false);
    }

    #[test]
    fn outputs_clean_uses_inherited_directories_relative_to_base_config() {
        require_nextest();
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        write(&cwd.join("src/keep.ts"), "export const x = 1;");
        write(
            &cwd.join("tsconfig.json"),
            r#"{"extends":"./config/base.json","include":["src/**/*"]}"#,
        );
        write(
            &cwd.join("config/base.json"),
            r#"{"compilerOptions":{"declaration":true,"declarationDir":"../types","rootDir":"../src"}}"#,
        );
        let dir = cwd.join("types");
        declaration_pair(&dir, "keep.d.ts");
        declaration_pair(&dir, "gone.d.ts");
        let config = parse_file(cwd);
        clean_outputs(cwd, &config, &[]).unwrap();
        assert_pair(&dir, "keep.d.ts", true);
        assert_pair(&dir, "gone.d.ts", false);
    }

    #[test]
    fn outputs_clean_rejects_file_as_output_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("types.d.ts");
        write(&path, "not a directory");
        let error = clean_output_dir(
            temp.path(),
            &path,
            &temp.path().join("src"),
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(error.contains("read output directory"), "{error}");
        assert!(path.exists());
    }

    #[test]
    fn outputs_clean_missing_local_dir_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        clean(temp.path()).unwrap();
        assert!(!temp.path().join("dist").exists());
    }

    #[test]
    fn outputs_clean_rejects_existing_outside_directory() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("pkg");
        let outside = temp.path().join("pkg-sibling");
        fs::create_dir_all(&cwd).unwrap();
        declaration_pair(&outside, "gone.d.ts");
        let error =
            clean_output_dir(&cwd, &outside, &cwd.join("src"), &HashSet::new()).unwrap_err();
        assert!(error.contains("outside package"), "{error}");
        assert_pair(&outside, "gone.d.ts", true);
    }

    #[test]
    fn outputs_clean_missing_paths_cannot_hide_parent_escape() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("pkg");
        fs::create_dir_all(&cwd).unwrap();
        let error = clean_output_dir(
            &cwd,
            &cwd.join("missing/../../outside"),
            &cwd.join("src"),
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(error.contains("outside package"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn outputs_clean_does_not_follow_nested_or_dangling_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("pkg");
        let dir = cwd.join("dist/types");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&dir).unwrap();
        declaration_pair(&outside, "gone.d.ts");
        std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("missing"), dir.join("dangling")).unwrap();
        clean(&cwd).unwrap();
        assert_pair(&outside, "gone.d.ts", true);
        assert!(fs::symlink_metadata(dir.join("dangling")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn outputs_clean_allows_symlinks_within_package() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("pkg");
        let dir = cwd.join("declarations");
        declaration_pair(&dir, "gone.d.ts");
        std::os::unix::fs::symlink(&dir, cwd.join("types")).unwrap();
        std::os::unix::fs::symlink(&cwd, temp.path().join("package-link")).unwrap();
        clean_output_dir(
            &temp.path().join("package-link"),
            &cwd.join("types"),
            &cwd.join("src"),
            &HashSet::new(),
        )
        .unwrap();
        assert_pair(&dir, "gone.d.ts", false);
    }

    #[test]
    fn outputs_resolve_sorts_preserves_duplicates_and_handles_outside_paths() {
        let outputs = [
            "/repo/pkg/z.d.ts",
            "/repo/other/a.d.ts",
            "/repo/pkg/a.d.ts",
            "/repo/pkg/a.d.ts",
            "relative.d.ts",
            "/repo/pkg",
        ]
        .map(str::to_owned);
        assert_eq!(
            resolve_outputs(Path::new("/repo/pkg"), &outputs),
            [
                ".",
                "../other/a.d.ts",
                "a.d.ts",
                "a.d.ts",
                "relative.d.ts",
                "z.d.ts"
            ]
        );
    }

    #[test]
    fn outputs_resolve_normalizes_windows_slashes_and_preserves_other_drives() {
        let outputs = [
            r"C:\repo\pkg\dist\b.d.ts",
            r"C:\repo\pkg\dist\a.d.ts",
            r"D:\other\a.d.ts",
        ]
        .map(str::to_owned);
        assert_eq!(
            resolve_outputs(Path::new(r"C:\repo\pkg"), &outputs),
            ["D:/other/a.d.ts", "dist/a.d.ts", "dist/b.d.ts"]
        );
    }
}
