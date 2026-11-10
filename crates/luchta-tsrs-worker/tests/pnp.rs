use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use luchta_test_support::require_nextest;
use luchta_tsrs_worker::pnp::{cached_manifest, CompilerSystem};
use luchta_yarn_env::test_fixture::{FixtureOptions, PnpFixture};
use tsrs_compiler::{
    free_unshared_program, get_diagnostics_of_any_program, new_cached_fs_compiler_host,
    new_program, Context, ProgramOptions,
};
use tsrs_core::{tspath, P};
use tsrs_tsoptions::{get_parsed_command_line_of_config_file, ParseConfigHost, ParsedCommandLine};
use tsrs_vfs::bundled;

fn path(path: &Path) -> String {
    tspath::normalize_slashes(&path.to_string_lossy())
}

fn fixture(root: &Path, inline_manifest: bool) -> PnpFixture {
    let root = tsrs_vfs::osvfs::fs().realpath(&path(root));
    PnpFixture::write(
        Path::new(&root),
        FixtureOptions {
            inline_manifest,
            ..Default::default()
        },
    )
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn append_zip(zip: &Path, entries: &[(&str, &str)]) {
    let mut all = zip_entries(zip);
    all.extend(
        entries
            .iter()
            .map(|(name, text)| ((*name).to_owned(), text.as_bytes().to_vec())),
    );
    let mut writer = zip::ZipWriter::new(fs::File::create(zip).unwrap());
    for (name, bytes) in all {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
}

fn parse(system: &CompilerSystem) -> ParsedCommandLine {
    let config_path = format!("{}/tsconfig.json", system.get_current_directory());
    let (config, errors) =
        get_parsed_command_line_of_config_file(&config_path, None, None, system, None);
    assert!(errors.is_empty(), "config read diagnostics: {errors:?}");
    let config = config.expect("parsed config");
    assert!(
        config.errors.is_empty(),
        "config diagnostics: {:?}",
        config.errors
    );
    config
}

// Go: TestCompilerFSNoPnp.
#[test]
fn pnp_compiler_fs_no_pnp() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let system = CompilerSystem::new(temp.path()).unwrap();
    assert!(system.module_resolver().is_none());
    let file = path(&temp.path().join("index.ts"));
    system
        .fs()
        .write_file(&file, "export const x = 1;\n")
        .unwrap();
    system.fs().append_file(&file, "// appended\n").unwrap();
    assert!(system.fs().file_exists(&file));
    assert_eq!(
        system.fs().read_file(&file).unwrap(),
        fs::read_to_string(&file).unwrap()
    );
    assert!(system.fs().directory_exists(&path(temp.path())));
    assert!(system
        .fs()
        .get_accessible_entries(&path(temp.path()))
        .files
        .contains(&"index.ts".into()));
    system.fs().remove(&file).unwrap();
    assert!(!system.fs().file_exists(&file));
}

// Go: TestCompilerFSDetectsPnp. Both forms must work without executing CJS.
#[test]
fn pnp_compiler_fs_detects_pnp() {
    require_nextest();
    for inline in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let fixture = fixture(temp.path(), inline);
        let system = CompilerSystem::new(&fixture.app_dir).unwrap();
        assert!(system.module_resolver().is_some());
        let member = path(
            &fixture
                .left_pad_zip
                .join("node_modules/left-pad/package.json"),
        );
        assert!(system.fs().file_exists(&member));
        assert!(system.fs().read_file(&member).unwrap().contains("left-pad"));
        let synthetic = path(&fixture.app_dir.join("node_modules/left-pad/package.json"));
        assert_eq!(system.fs().realpath(&synthetic), member);
    }
}

fn assert_os_view(system: &CompilerSystem, directory: &Path) {
    let directory = path(directory);
    let os = tsrs_vfs::osvfs::fs();
    assert_eq!(
        system.fs().directory_exists(&directory),
        os.directory_exists(&directory),
        "{directory}"
    );
    let actual = system.fs().get_accessible_entries(&directory);
    let expected = os.get_accessible_entries(&directory);
    assert_eq!(actual.files, expected.files, "{directory}");
    assert_eq!(actual.directories, expected.directories, "{directory}");
    assert_eq!(
        system.fs().stat(&directory).map(|info| info.mode),
        os.stat(&directory).map(|info| info.mode),
        "{directory}"
    );
    assert_eq!(system.fs().realpath(&directory), os.realpath(&directory));
    let member = format!("{directory}/left-pad/package.json");
    assert_eq!(system.fs().file_exists(&member), os.file_exists(&member));
    assert_eq!(system.fs().read_file(&member), os.read_file(&member));
}

#[test]
fn pnp_filesystem_root_node_modules_uses_os_fallback() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    // Native root keeps this regression valid for Windows drive roots too.
    let root = fixture.root.ancestors().last().unwrap();
    assert_os_view(&system, &root.join("node_modules"));
}

#[test]
fn pnp_filesystem_root_scoped_node_modules_uses_os_fallback() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    let root = fixture.root.ancestors().last().unwrap();
    assert_os_view(&system, &root.join("node_modules/@types"));
}

#[test]
fn pnp_above_project_and_sibling_paths_use_os_fallback() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(&temp.path().join("project"), false);
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    let sibling = temp.path().join("sibling/node_modules");
    write(
        &sibling.join("left-pad/package.json"),
        r#"{"name":"os-only"}"#,
    );
    fs::create_dir_all(sibling.join("@types/os-only")).unwrap();
    for ancestor in fixture.root.parent().unwrap().ancestors() {
        assert_os_view(&system, &ancestor.join("node_modules"));
        assert_os_view(&system, &ancestor.join("node_modules/@types"));
    }
    assert_os_view(&system, &sibling);
    assert_os_view(&system, &sibling.join("@types"));
}

#[test]
fn pnp_failed_resolution_reports_diagnostics_without_panicking() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    write(
        &fixture.app_dir.join("index.ts"),
        "import {x} from 'missing-dependency'; export const value = x;\n",
    );
    write(
        &fixture.app_dir.join("tsconfig.json"),
        r#"{"compilerOptions":{"module":"NodeNext","noEmit":true,"types":["missing-type"]},"files":["index.ts"]}"#,
    );
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    compile(&system, |program| {
        let semantic = program.get_semantic_diagnostics(&Context::default(), None);
        assert!(
            semantic.iter().any(|diag| diag.code() == 2307),
            "{semantic:?}"
        );
        let options = program.get_program_diagnostics();
        assert!(
            options.iter().any(|diag| diag.code() == 2688),
            "{options:?}"
        );
    });
}

// Go: TestInitPnpApiCachedReusesParsedManifest.
#[test]
fn pnp_cached_reuses_parsed_manifest() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let first = cached_manifest(&fixture.app_dir).unwrap().unwrap();
    let second = cached_manifest(&fixture.root).unwrap().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let app = fixture.app_dir.clone();
            std::thread::spawn(move || cached_manifest(&app).unwrap().unwrap())
        })
        .collect();
    for thread in threads {
        assert!(Arc::ptr_eq(&first, &thread.join().unwrap()));
    }
}

// Go: TestInitPnpApiCachedReparsesWhenManifestChanges. Also check split data,
// size-only invalidation and removal rather than only touching the CJS file.
#[test]
fn pnp_cached_reparses_when_manifest_changes() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let first = cached_manifest(&fixture.root).unwrap().unwrap();
    let cjs = fixture.root.join(".pnp.cjs");
    let future = SystemTime::now() + Duration::from_secs(2);
    fs::File::open(&cjs).unwrap().set_modified(future).unwrap();
    let second = cached_manifest(&fixture.root).unwrap().unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    let data = fixture.root.join(".pnp.data.json");
    let modified = fs::metadata(&data).unwrap().modified().unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&data)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    fs::File::open(&data)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let third = cached_manifest(&fixture.root).unwrap().unwrap();
    assert!(!Arc::ptr_eq(&second, &third));
    fs::remove_file(&cjs).unwrap();
    assert!(cached_manifest(&fixture.root).unwrap().is_none());
}

// Go: TestInitPnpApiCachedReturnsNilWithoutManifest.
#[test]
fn pnp_cached_returns_none_without_manifest() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    assert!(cached_manifest(temp.path()).unwrap().is_none());
}

fn compile(system: &CompilerSystem, check: impl FnOnce(&'static tsrs_compiler::Program)) {
    let config = parse(system);
    let host = new_cached_fs_compiler_host(
        system.get_current_directory(),
        system.filesystem(),
        &bundled::lib_path(),
        None,
        None,
        None,
    );
    let mut options = ProgramOptions::new(P::new(config), host);
    options.create_module_resolver = system.module_resolver();
    options.checker_regions = true;
    options.checker_diagnostic_regions = true;
    options.regions = Some(Arc::default());
    let program = new_program(options);
    let result = catch_unwind(AssertUnwindSafe(|| check(program)));
    // SAFETY: check's diagnostics and source-file pointers have gone out of scope.
    // This standalone program doesn't share arenas with another program.
    unsafe {
        free_unshared_program(program);
    }
    if let Err(panic) = result {
        resume_unwind(panic);
    }
}

fn assert_clean(program: &'static tsrs_compiler::Program) {
    let diagnostics = get_diagnostics_of_any_program(
        &Context::default(),
        program,
        None,
        false,
        &mut |ctx, file| program.get_bind_diagnostics(ctx, file),
        &mut |ctx, file| program.get_semantic_diagnostics(ctx, file),
    );
    assert!(
        diagnostics.is_empty(),
        "program diagnostics: {diagnostics:?}"
    );
}

#[test]
fn pnp_compiles_imports_from_zipped_and_virtual_packages() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    append_zip(
        &fixture.left_pad_zip,
        &[(
            "node_modules/left-pad/index.d.ts",
            "export declare const answer: number;\n",
        )],
    );
    append_zip(
        &fixture.scope_tool_zip,
        &[(
            "node_modules/@scope/tool/index.d.ts",
            "export declare const tool: string;\n",
        )],
    );
    write(&fixture.app_dir.join("index.ts"), "import {answer} from 'left-pad'; import {tool} from '@scope/tool'; export const result: string = tool + answer;\n");
    write(
        &fixture.app_dir.join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"module":"NodeNext","noEmit":true},"files":["index.ts"]}"#,
    );
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    compile(&system, |program| {
        assert_clean(program);
        let files: Vec<_> = program
            .source_files()
            .iter()
            .map(|file| file.file_name())
            .collect();
        assert!(
            files
                .iter()
                .any(|file| file.contains(".zip/node_modules/left-pad/index.d.ts")),
            "{files:?}"
        );
        assert!(
            files
                .iter()
                .any(|file| file.contains("/__virtual__/")
                    && file.ends_with("/@scope/tool/index.d.ts")),
            "{files:?}"
        );
    });
}

// Go: TestResolveInputsBareExtendsUnderPnp. Same inherited, nonlocal include
// assertion; fixture's native-thing is an unplugged package rather than sibling.
#[test]
fn pnp_resolve_inputs_bare_extends_under_pnp() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let base = fixture
        .root
        .join(".yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing");
    write(
        &base.join("strict.json"),
        r#"{"include":["shared/**/*.ts"],"compilerOptions":{"strict":true}}"#,
    );
    write(
        &base.join("shared/existing.ts"),
        "export const shared = true;\n",
    );
    write(
        &fixture.app_dir.join("tsconfig.json"),
        r#"{"extends":"native-thing/strict.json","compilerOptions":{"outDir":"dist"}}"#,
    );
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    let parsed = parse(&system);
    assert!(parsed.compiler_options().unwrap().strict.is_true());
    let (include, _) = parsed.get_matched_include_spec(&path(&base.join("shared/existing.ts")));
    assert!(
        include.starts_with("../") && include.ends_with("/native-thing/shared/**/*.ts"),
        "{include:?}"
    );
    assert_eq!(
        parsed.extended_source_files(),
        vec![path(&base.join("strict.json"))]
    );
    assert_eq!(
        parsed.file_names(),
        vec![path(&base.join("shared/existing.ts"))]
    );
}

#[test]
fn pnp_zip_filesystem_lists_stats_and_rejects_writes() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    let directory = path(&fixture.left_pad_zip.join("node_modules/left-pad"));
    assert!(system.fs().directory_exists(&directory));
    let entries = system.fs().get_accessible_entries(&directory);
    assert_eq!(entries.files, ["package.json"]);
    assert_eq!(entries.directories, ["bin"]);
    let file = format!("{directory}/bin/cli.js");
    let stat = system.fs().stat(&file).unwrap();
    assert!(!stat.is_dir());
    assert_eq!(
        stat.size,
        system.fs().read_file(&file).unwrap().len() as i64
    );
    assert!(system.fs().stat(&directory).unwrap().is_dir());
    assert!(system.fs().write_file(&file, "changed").is_err());
    assert!(system.fs().remove(&directory).is_err());
    assert!(!system.fs().file_exists(&format!("{directory}/missing.ts")));
}

fn zip_entries(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    (0..archive.len())
        .map(|index| {
            let mut file = archive.by_index(index).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            (file.name().to_owned(), bytes)
        })
        .collect()
}

#[test]
fn pnp_resolves_zipped_package_exports_and_config_extends() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    append_zip(
        &fixture.left_pad_zip,
        &[
            (
                "node_modules/left-pad/package.json",
                r#"{"name":"left-pad","version":"1.3.0","exports":{".":{"types":"./types/main.d.ts"},"./strict.json":"./strict.json"}}"#,
            ),
            (
                "node_modules/left-pad/types/main.d.ts",
                "export declare const answer: 42;\n",
            ),
            (
                "node_modules/left-pad/strict.json",
                r#"{"compilerOptions":{"strict":true,"module":"NodeNext"}}"#,
            ),
        ],
    );
    write(
        &fixture.app_dir.join("index.ts"),
        "import {answer} from 'left-pad'; export const result: 42 = answer;\n",
    );
    write(
        &fixture.app_dir.join("tsconfig.json"),
        r#"{"extends":"left-pad/strict.json","compilerOptions":{"noEmit":true},"files":["index.ts"]}"#,
    );
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    let parsed = parse(&system);
    assert_eq!(
        parsed.extended_source_files(),
        [path(
            &fixture
                .left_pad_zip
                .join("node_modules/left-pad/strict.json")
        )]
    );
    compile(&system, |program| {
        assert_clean(program);
        assert!(program.source_files().iter().any(|file| file
            .file_name()
            .ends_with(".zip/node_modules/left-pad/types/main.d.ts")));
    });
}

#[test]
fn pnp_resolves_automatic_and_explicit_type_directives() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let manifest_path = fixture.root.join(".pnp.data.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    // Add an alias to a package already built by PnpFixture, not a new manifest.
    data["packageRegistryData"][2][1][0][1]["packageDependencies"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!([
            "@types/example",
            ["native-thing", "npm:1.0.0"]
        ]));
    fs::write(&manifest_path, serde_json::to_vec(&data).unwrap()).unwrap();
    let base = fixture
        .root
        .join(".yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing");
    write(
        &base.join("index.d.ts"),
        "declare const fixtureGlobal: string;\n",
    );
    write(
        &fixture.app_dir.join("index.ts"),
        "/// <reference types=\"example\" />\nexport const result: string = fixtureGlobal;\n",
    );
    write(
        &fixture.app_dir.join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"module":"NodeNext","noEmit":true},"files":["index.ts"]}"#,
    );
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    assert_eq!(
        system
            .fs()
            .get_accessible_entries(&path(&fixture.app_dir.join("node_modules/@types")))
            .directories,
        ["example"]
    );
    compile(&system, assert_clean);
}

#[test]
fn pnp_plain_os_fallback_compiles_node_modules() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("node_modules/plain/index.d.ts"),
        "export declare const answer: number;\n",
    );
    write(
        &temp.path().join("index.ts"),
        "import {answer} from 'plain'; export const result: number = answer;\n",
    );
    write(
        &temp.path().join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"module":"NodeNext","noEmit":true},"files":["index.ts"]}"#,
    );
    let system = CompilerSystem::new(temp.path()).unwrap();
    assert!(system.module_resolver().is_none());
    compile(&system, assert_clean);
}

#[test]
fn pnp_invalid_manifest_does_not_fall_back_or_poison_cache() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let original = fs::read(fixture.root.join(".pnp.data.json")).unwrap();
    assert!(cached_manifest(&fixture.app_dir).unwrap().is_some());
    fs::write(fixture.root.join(".pnp.data.json"), b"not json").unwrap();
    assert!(CompilerSystem::new(&fixture.app_dir).is_err());
    fs::write(fixture.root.join(".pnp.data.json"), original).unwrap();
    assert!(CompilerSystem::new(&fixture.app_dir).is_ok());
}

#[test]
fn pnp_does_not_leak_undeclared_ancestor_dependencies() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let manifest_path = fixture.root.join(".pnp.data.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    data["enableTopLevelFallback"] = serde_json::json!(false);
    data["packageRegistryData"][1][1][0][1]["packageDependencies"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(["hidden", ["native-thing", "npm:1.0.0"]]));
    fs::write(&manifest_path, serde_json::to_vec(&data).unwrap()).unwrap();
    let base = fixture
        .root
        .join(".yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing");
    write(
        &base.join("index.d.ts"),
        "export declare const hidden: number;\n",
    );
    write(
        &fixture.app_dir.join("index.ts"),
        "import {hidden} from 'hidden'; export {hidden};\n",
    );
    write(
        &fixture.app_dir.join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"module":"NodeNext","noEmit":true},"files":["index.ts"]}"#,
    );
    let system = CompilerSystem::new(&fixture.app_dir).unwrap();
    compile(&system, |program| {
        let diagnostics = program.get_semantic_diagnostics(&Context::default(), None);
        assert!(!diagnostics.is_empty(), "undeclared import must fail");
    });
}

#[test]
fn pnp_transitive_zip_imports_with_preserve_symlinks() {
    require_nextest();
    let temp = tempfile::tempdir().unwrap();
    let fixture = fixture(temp.path(), false);
    let manifest_path = fixture.root.join(".pnp.data.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    data["packageRegistryData"][3][1][0][1]["packageDependencies"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(["native-thing", "npm:1.0.0"]));
    fs::write(&manifest_path, serde_json::to_vec(&data).unwrap()).unwrap();
    let base = fixture
        .root
        .join(".yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing");
    write(
        &base.join("index.d.ts"),
        "export declare const answer: 42;\n",
    );
    append_zip(
        &fixture.left_pad_zip,
        &[(
            "node_modules/left-pad/index.d.ts",
            "export {answer} from 'native-thing';\n",
        )],
    );
    write(
        &fixture.app_dir.join("index.ts"),
        "import {answer} from 'left-pad'; export const result: 42 = answer;\n",
    );
    for preserve in [false, true] {
        write(
            &fixture.app_dir.join("tsconfig.json"),
            &format!(
                r#"{{"compilerOptions":{{"strict":true,"module":"NodeNext","noEmit":true,"preserveSymlinks":{preserve}}},"files":["index.ts"]}}"#
            ),
        );
        let system = CompilerSystem::new(&fixture.app_dir).unwrap();
        compile(&system, assert_clean);
    }
}
