use std::path::{Path, PathBuf};

use pnp::fs::{VPath, VPathInfo, ZipCache as _};
use pnp::{Manifest, PackageDependency, PackageInformation, PackageLocator};
use serde::Deserialize;

use crate::DirectExecError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binary {
    pub name: String,
    pub path: PathBuf,
    pub is_node_script: bool,
}

pub type ZipCache = pnp::fs::LruZipCache<Vec<u8>>;

pub fn new_zip_cache() -> ZipCache {
    pnp::fs::LruZipCache::new(64, pnp::fs::open_zip_via_read_p)
}

/// Read a file that may live inside a zip archive or a `__virtual__` folder.
pub fn read_virtual_file(path: &Path, zips: &ZipCache) -> std::io::Result<Vec<u8>> {
    match VPath::from(path)? {
        VPath::Zip(info) => {
            let inner = info.zip_path.trim_end_matches('/').to_owned();
            zips.read(info.physical_base_path(), inner)
        }
        VPath::Virtual(info) => std::fs::read(info.physical_base_path()),
        VPath::Native(native) => std::fs::read(native),
    }
}

#[derive(Deserialize)]
struct BinManifest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    bin: Option<BinField>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BinField {
    Single(String),
    Map(std::collections::BTreeMap<String, String>),
}

/// Binaries reachable from `workspace_dir`: the workspace itself first, then
/// its direct dependencies in name order (the order yarn writes them into
/// the manifest); on a bin name collision the later entry wins.
pub fn accessible_binaries(
    manifest: &Manifest,
    workspace_dir: &Path,
    zips: &ZipCache,
) -> Result<Vec<Binary>, DirectExecError> {
    let (locator, workspace) = find_workspace(manifest, workspace_dir)?;
    let dependency_locators = dependency_locators_excluding(workspace, locator);

    let mut binaries: Vec<Binary> = Vec::new();
    extend_binaries(&mut binaries, locator, workspace, zips);
    for candidate in &dependency_locators {
        if let Some(info) = lookup(manifest, candidate) {
            extend_binaries(&mut binaries, candidate, info, zips);
        }
    }
    Ok(binaries)
}

/// Locates `workspace_dir`'s own manifest entry, erroring with the same
/// `DirectExecError` variant whether the path isn't a workspace at all or
/// the registry lookup for it comes up empty.
fn find_workspace<'a>(
    manifest: &'a Manifest,
    workspace_dir: &Path,
) -> Result<(&'a PackageLocator, &'a PackageInformation), DirectExecError> {
    let not_in_manifest = || DirectExecError::WorkspaceNotInManifest {
        workspace_dir: workspace_dir.to_path_buf(),
    };
    // Look up via the package.json path: the location trie stores directory
    // locations and `get_ancestor_value` matches strict ancestors reliably.
    let locator = pnp::find_locator(manifest, &workspace_dir.join("package.json"))
        .filter(|locator| locator.reference.starts_with("workspace:"))
        .ok_or_else(not_in_manifest)?;
    let workspace = lookup(manifest, locator).ok_or_else(not_in_manifest)?;
    Ok((locator, workspace))
}

/// `workspace`'s direct dependencies, in the name order yarn writes them
/// into the manifest, excluding `workspace` itself (a package can depend on
/// its own name via a self-reference).
fn dependency_locators_excluding(
    workspace: &PackageInformation,
    workspace_locator: &PackageLocator,
) -> Vec<PackageLocator> {
    // `package_dependencies` comes out of an `FxHashMap`, whose iteration
    // order is not stable across runs. Sorting by name reproduces yarn's own
    // manifest order, since it writes `packageDependencies` sorted by name.
    let sorted_dependencies: std::collections::BTreeMap<&str, &Option<PackageDependency>> =
        workspace
            .package_dependencies
            .iter()
            .map(|(name, dependency)| (name.as_str(), dependency))
            .collect();

    sorted_dependencies
        .into_iter()
        .filter_map(|(name, dependency)| resolved_dependency_locator(name, dependency.as_ref()))
        .filter(|resolved| resolved != workspace_locator)
        .collect()
}

fn resolved_dependency_locator(
    name: &str,
    dependency: Option<&PackageDependency>,
) -> Option<PackageLocator> {
    Some(match dependency? {
        PackageDependency::Reference(reference) => PackageLocator {
            name: name.to_owned(),
            reference: reference.clone(),
        },
        PackageDependency::Alias(alias_name, reference) => PackageLocator {
            name: alias_name.clone(),
            reference: reference.clone(),
        },
    })
}

/// Append `locator`'s binaries to `binaries`, letting later entries overwrite
/// earlier ones with the same name (matching yarn's `Map.set` overwrite).
/// Unreadable or binless packages are skipped, not errors.
fn extend_binaries(
    binaries: &mut Vec<Binary>,
    locator: &PackageLocator,
    info: &PackageInformation,
    zips: &ZipCache,
) {
    let Some(found) = package_binaries(locator, info, zips) else {
        return;
    };
    for binary in found {
        binaries.retain(|existing| existing.name != binary.name);
        binaries.push(binary);
    }
}

/// `pnp::get_package` panics on unknown locators; this is the total version.
fn lookup<'a>(manifest: &'a Manifest, locator: &PackageLocator) -> Option<&'a PackageInformation> {
    manifest
        .package_registry_data
        .get(&locator.name)?
        .get(&locator.reference)
}

fn package_binaries(
    locator: &PackageLocator,
    info: &PackageInformation,
    zips: &ZipCache,
) -> Option<Vec<Binary>> {
    let package_json = info.package_location.join("package.json");
    let raw = read_virtual_file(&package_json, zips).ok()?;
    let parsed: BinManifest = serde_json::from_slice(&raw).ok()?;
    let bin = parsed.bin?;
    let package_name = parsed.name.unwrap_or_else(|| locator.name.clone());
    let entries: Vec<(String, String)> = match bin {
        BinField::Single(path) => vec![(unscoped(&package_name).to_owned(), path)],
        BinField::Map(map) => map.into_iter().collect(),
    };
    Some(
        entries
            .into_iter()
            .filter(|(name, _)| is_safe_bin_name(name))
            .map(|(name, relative)| {
                let path = info.package_location.join(relative);
                let is_node_script = is_node_script(&path, zips);
                Binary {
                    name,
                    path,
                    is_node_script,
                }
            })
            .collect(),
    )
}

/// Yarn's `toFilename` rejects a bin name that is empty, `.`, `..`, or
/// contains a path separator; those would let a malicious `bin` map write a
/// shim outside the shim directory. Reject the same names here rather than
/// writing them to disk.
fn is_safe_bin_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

fn unscoped(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Yarn's `isNodeScript`: known script extensions are scripts, `.exe`/`.bin`
/// are not, anything else is inspected for ELF, Mach-O, or MZ magic bytes.
fn is_node_script(path: &Path, zips: &ZipCache) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("js" | "cjs" | "mjs" | "jsx" | "ts" | "tsx") => return true,
        Some("exe" | "bin") => return false,
        _ => {}
    }
    let Ok(bytes) = read_virtual_file(path, zips) else {
        return true;
    };
    let Some(magic) = bytes.get(0..4) else {
        return true;
    };
    let is_binary = magic == [0xCA, 0xFE, 0xBA, 0xBE]
        || magic == [0xCF, 0xFA, 0xED, 0xFE]
        || magic == [0x7F, b'E', b'L', b'F']
        || magic[0..2] == *b"MZ";
    !is_binary
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::manifest;
    use crate::test_fixture::{FixtureOptions, PnpFixture};

    fn bins_for_app(fixture: &PnpFixture) -> BTreeMap<String, Binary> {
        let files = manifest::locate(&fixture.root).unwrap();
        let manifest = manifest::load(&files).unwrap();
        accessible_binaries(&manifest, &fixture.app_dir, &new_zip_cache())
            .unwrap()
            .into_iter()
            .map(|binary| (binary.name.clone(), binary))
            .collect()
    }

    #[test]
    fn collects_self_and_direct_dependency_bins() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());

        let files = manifest::locate(&fixture.root).unwrap();
        let manifest = manifest::load(&files).unwrap();
        let raw = accessible_binaries(&manifest, &fixture.app_dir, &new_zip_cache()).unwrap();
        // Dependency order follows the dependency *name* sorted (yarn's own
        // manifest order): "@scope/tool" < "left-pad" < "native-thing".
        let raw_names: Vec<&str> = raw.iter().map(|binary| binary.name.as_str()).collect();
        assert_eq!(
            raw_names,
            vec!["app", "tool", "tool-extra", "left-pad", "native-thing"]
        );

        let bins = bins_for_app(&fixture);
        let names: Vec<&String> = bins.keys().collect();
        assert_eq!(
            names,
            vec!["app", "left-pad", "native-thing", "tool", "tool-extra"]
        );
    }

    #[test]
    fn string_bin_uses_unscoped_package_name_and_workspace_path() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        let bins = bins_for_app(&fixture);
        assert_eq!(bins["app"].path, fixture.app_dir.join("app-cli.js"));
        assert!(bins["app"].is_node_script);
    }

    #[test]
    fn zip_bin_keeps_virtual_path_in_shim_target() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        let bins = bins_for_app(&fixture);
        assert_eq!(
            bins["left-pad"].path,
            fixture
                .root
                .join(".yarn/cache/left-pad-npm-1.3.0-abc-10.zip/node_modules/left-pad/bin/cli.js")
        );
        assert_eq!(
            bins["tool"].path,
            fixture.root.join(".yarn/__virtual__/@scope-tool-virtual-deadbeef/0/cache/@scope-tool-npm-2.0.0-def-10.zip/node_modules/@scope/tool/cli.js")
        );
        assert!(bins["tool-extra"].is_node_script);
    }

    #[test]
    fn native_binary_is_detected_by_magic_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        let bins = bins_for_app(&fixture);
        assert!(!bins["native-thing"].is_node_script);
        assert!(bins["native-thing"]
            .path
            .ends_with("native-thing/bin/native"));
    }

    #[test]
    fn missing_zip_and_unresolved_peer_are_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        let bins = bins_for_app(&fixture);
        assert!(!bins.contains_key("not-installed"));
        assert!(!bins.contains_key("react"));
    }

    #[test]
    fn unsafe_bin_names_are_rejected() {
        assert!(!is_safe_bin_name(""));
        assert!(!is_safe_bin_name("."));
        assert!(!is_safe_bin_name(".."));
        assert!(!is_safe_bin_name("../../x"));
        assert!(!is_safe_bin_name("a/b"));
        assert!(!is_safe_bin_name("a\\b"));
        assert!(is_safe_bin_name("tool-extra"));
    }

    #[test]
    fn unknown_workspace_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        let files = manifest::locate(&fixture.root).unwrap();
        let manifest = manifest::load(&files).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let error = accessible_binaries(&manifest, outside.path(), &new_zip_cache()).unwrap_err();
        assert!(
            matches!(error, DirectExecError::WorkspaceNotInManifest { .. }),
            "{error}"
        );
    }
}
