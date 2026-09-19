//! Builder for a small synthetic Yarn Berry PnP project used by tests in this
//! workspace. Enabled with the `test-fixture` feature.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

#[derive(Debug, Clone, Default)]
pub struct FixtureOptions {
    /// Write an inlined `.pnp.cjs` (RAW_RUNTIME_STATE) instead of `.pnp.cjs` +
    /// `.pnp.data.json`.
    pub inline_manifest: bool,
    /// Also write `.pnp.loader.mjs`.
    pub with_loader: bool,
    /// Contents for `<root>/.env.yarn`.
    pub env_file: Option<&'static str>,
}

#[derive(Debug)]
pub struct PnpFixture {
    pub root: PathBuf,
    pub app_dir: PathBuf,
    pub left_pad_zip: PathBuf,
    pub scope_tool_zip: PathBuf,
}

impl PnpFixture {
    pub fn write(root: &Path, options: FixtureOptions) -> Self {
        let root = root.to_path_buf();
        let app_dir = write_workspace_packages(&root);
        let (left_pad_zip, scope_tool_zip) = write_dependency_zips(&root);
        write_native_thing(&root);
        write_manifest(&root, &options);

        Self {
            root,
            app_dir,
            left_pad_zip,
            scope_tool_zip,
        }
    }
}

/// Writes the root `package.json` and the `@fixture/app` workspace package,
/// returning the app package's directory.
fn write_workspace_packages(root: &Path) -> PathBuf {
    let app_dir = root.join("packages/app");
    fs::create_dir_all(&app_dir).unwrap();

    write_json(
        &root.join("package.json"),
        &json!({
            "name": "root",
            "version": "1.0.0",
            "packageManager": "yarn@4.18.0",
            "workspaces": ["packages/*"]
        }),
    );
    write_json(
        &app_dir.join("package.json"),
        &json!({
            "name": "@fixture/app",
            "version": "1.2.3",
            "bin": "app-cli.js",
            "scripts": {
                "build": "echo building",
                "args": "printf '%s\\n' \"$@\"",
                "env": "env"
            }
        }),
    );
    fs::write(app_dir.join("app-cli.js"), "console.log('app')\n").unwrap();
    app_dir
}

/// Writes the cached zip archives for the `left-pad` and `@scope/tool`
/// dependencies, returning their paths.
fn write_dependency_zips(root: &Path) -> (PathBuf, PathBuf) {
    let cache = root.join(".yarn/cache");
    fs::create_dir_all(&cache).unwrap();

    let left_pad_zip = cache.join("left-pad-npm-1.3.0-abc-10.zip");
    write_zip(
        &left_pad_zip,
        &[
            (
                "node_modules/left-pad/package.json",
                r#"{"name":"left-pad","version":"1.3.0","bin":"bin/cli.js"}"#.as_bytes(),
            ),
            (
                "node_modules/left-pad/bin/cli.js",
                b"console.log('left-pad')\n",
            ),
        ],
    );
    let scope_tool_zip = cache.join("@scope-tool-npm-2.0.0-def-10.zip");
    write_zip(
        &scope_tool_zip,
        &[
            (
                "node_modules/@scope/tool/package.json",
                r#"{"name":"@scope/tool","version":"2.0.0","bin":{"tool":"cli.js","tool-extra":"extra.js"}}"#.as_bytes(),
            ),
            ("node_modules/@scope/tool/cli.js", b"console.log('tool')\n"),
            ("node_modules/@scope/tool/extra.js", b"console.log('extra')\n"),
        ],
    );
    (left_pad_zip, scope_tool_zip)
}

/// Writes the unplugged (non-zip) `native-thing` dependency, whose binary is
/// an ELF-magic-prefixed file rather than a node script.
fn write_native_thing(root: &Path) {
    let native_dir =
        root.join(".yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing");
    fs::create_dir_all(native_dir.join("bin")).unwrap();
    write_json(
        &native_dir.join("package.json"),
        &json!({"name": "native-thing", "version": "1.0.0", "bin": "bin/native"}),
    );
    fs::write(
        native_dir.join("bin/native"),
        [0x7F, b'E', b'L', b'F', 0, 0],
    )
    .unwrap();
}

/// Writes the PnP manifest (inline or split), plus the optional loader and
/// `.env.yarn` files `options` requests.
fn write_manifest(root: &Path, options: &FixtureOptions) {
    let data = manifest_data();
    if options.inline_manifest {
        let raw = serde_json::to_string(&data).unwrap();
        let escaped = raw.replace('\\', "\\\\").replace('\'', "\\'");
        fs::write(
            root.join(".pnp.cjs"),
            format!("#!/usr/bin/env node\nconst RAW_RUNTIME_STATE =\n'{escaped}';\n"),
        )
        .unwrap();
    } else {
        fs::write(
            root.join(".pnp.cjs"),
            "#!/usr/bin/env node\nfunction $$SETUP_STATE(hydrateRuntimeState, basePath) {\n  const pnpDataFilepath = path.resolve(__dirname, '.pnp.data.json');\n  return hydrateRuntimeState(JSON.parse(fs.readFileSync(pnpDataFilepath, 'utf8')), {basePath: basePath || __dirname});\n}\n",
        )
        .unwrap();
        write_json(&root.join(".pnp.data.json"), &data);
    }
    if options.with_loader {
        fs::write(root.join(".pnp.loader.mjs"), "export {};\n").unwrap();
    }
    if let Some(env_file) = options.env_file {
        fs::write(root.join(".env.yarn"), env_file).unwrap();
    }
}

const VIRTUAL_TOOL: &str = "virtual:deadbeef#npm:2.0.0";

fn manifest_data() -> serde_json::Value {
    let app_deps = json!([
        ["@fixture/app", "workspace:packages/app"],
        ["left-pad", "npm:1.3.0"],
        ["@scope/tool", VIRTUAL_TOOL],
        ["native-thing", "npm:1.0.0"],
        ["not-installed", "npm:9.9.9"],
        ["react", null]
    ]);
    json!({
        "__info": [],
        "dependencyTreeRoots": [
            {"name": "root", "reference": "workspace:."},
            {"name": "@fixture/app", "reference": "workspace:packages/app"}
        ],
        "enableTopLevelFallback": true,
        "ignorePatternData": null,
        "fallbackExclusionList": [],
        "fallbackPool": [],
        "packageRegistryData": [
            [null, [[null, {"packageLocation": "./", "packageDependencies": [["root", "workspace:."]], "linkType": "SOFT"}]]],
            ["root", [["workspace:.", {"packageLocation": "./", "packageDependencies": [["root", "workspace:."]], "linkType": "SOFT"}]]],
            ["@fixture/app", [["workspace:packages/app", {"packageLocation": "./packages/app/", "packageDependencies": app_deps, "linkType": "SOFT"}]]],
            ["left-pad", [["npm:1.3.0", {"packageLocation": "./.yarn/cache/left-pad-npm-1.3.0-abc-10.zip/node_modules/left-pad/", "packageDependencies": [["left-pad", "npm:1.3.0"]], "linkType": "HARD"}]]],
            ["@scope/tool", [
                ["npm:2.0.0", {"packageLocation": "./.yarn/cache/@scope-tool-npm-2.0.0-def-10.zip/node_modules/@scope/tool/", "packageDependencies": [["@scope/tool", "npm:2.0.0"]], "linkType": "HARD"}],
                [VIRTUAL_TOOL, {"packageLocation": "./.yarn/__virtual__/@scope-tool-virtual-deadbeef/0/cache/@scope-tool-npm-2.0.0-def-10.zip/node_modules/@scope/tool/", "packageDependencies": [["@scope/tool", VIRTUAL_TOOL], ["react", null]], "packagePeers": ["react"], "linkType": "HARD"}]
            ]],
            ["native-thing", [["npm:1.0.0", {"packageLocation": "./.yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing/", "packageDependencies": [["native-thing", "npm:1.0.0"]], "linkType": "HARD"}]]],
            ["not-installed", [["npm:9.9.9", {"packageLocation": "./.yarn/cache/not-installed-npm-9.9.9-000-10.zip/node_modules/not-installed/", "packageDependencies": [["not-installed", "npm:9.9.9"]], "linkType": "HARD"}]]]
        ]
    })
}

fn write_json(path: &Path, value: &serde_json::Value) {
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = fs::File::create(path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, contents) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(contents).unwrap();
    }
    writer.finish().unwrap();
}

#[cfg(test)]
mod tests {
    use super::{FixtureOptions, PnpFixture};

    #[test]
    fn split_manifest_deserializes_into_pnp_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        let raw = std::fs::read_to_string(fixture.root.join(".pnp.data.json")).unwrap();
        let mut manifest: pnp::Manifest = serde_json::from_str(&raw).unwrap();
        pnp::init_pnp_manifest(&mut manifest, &fixture.root.join(".pnp.cjs"));
        let locator = pnp::find_locator(&manifest, &fixture.app_dir.join("package.json"))
            .expect("app is in the trie");
        assert_eq!(locator.name, "@fixture/app");
        assert_eq!(locator.reference, "workspace:packages/app");
    }

    #[test]
    fn inline_manifest_loads_with_pnp() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(
            temp.path(),
            FixtureOptions {
                inline_manifest: true,
                ..FixtureOptions::default()
            },
        );
        let manifest = pnp::load_pnp_manifest(&fixture.root.join(".pnp.cjs")).unwrap();
        assert!(manifest.package_registry_data.contains_key("left-pad"));
    }

    #[test]
    fn zip_entries_are_readable_through_pnp() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        let zip = pnp::fs::open_zip_via_read(&fixture.left_pad_zip).unwrap();
        let text = zip
            .read_to_string("node_modules/left-pad/package.json")
            .unwrap();
        assert!(text.contains("\"bin\""));
    }
}
