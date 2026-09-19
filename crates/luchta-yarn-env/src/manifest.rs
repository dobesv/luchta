use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::DirectExecError;

#[derive(Debug, Clone)]
pub struct ManifestFiles {
    pub pnp_cjs: PathBuf,
    pub pnp_data: Option<PathBuf>,
    pub pnp_loader: Option<PathBuf>,
}

/// Size and mtime of every manifest file; equal fingerprints mean the parsed
/// manifest can be reused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Fingerprint(Vec<(PathBuf, u64, Option<SystemTime>)>);

pub fn locate(project_root: &Path) -> Result<ManifestFiles, DirectExecError> {
    let pnp_cjs = project_root.join(".pnp.cjs");
    if !pnp_cjs.is_file() {
        return Err(DirectExecError::NoPnpManifest {
            project_root: project_root.to_path_buf(),
        });
    }
    let optional = |name: &str| {
        let path = project_root.join(name);
        path.is_file().then_some(path)
    };
    Ok(ManifestFiles {
        pnp_cjs,
        pnp_data: optional(".pnp.data.json"),
        pnp_loader: optional(".pnp.loader.mjs"),
    })
}

pub fn fingerprint(files: &ManifestFiles) -> Result<Fingerprint, DirectExecError> {
    let mut entries = Vec::with_capacity(3);
    for path in [
        Some(&files.pnp_cjs),
        files.pnp_data.as_ref(),
        files.pnp_loader.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let metadata = std::fs::metadata(path).map_err(|source| {
            DirectExecError::io(format!("reading metadata of {}", path.display()), source)
        })?;
        entries.push((path.clone(), metadata.len(), metadata.modified().ok()));
    }
    Ok(Fingerprint(entries))
}

pub fn load(files: &ManifestFiles) -> Result<pnp::Manifest, DirectExecError> {
    let parse_error = |path: &Path, message: String| DirectExecError::ManifestParse {
        path: path.to_path_buf(),
        message,
    };
    match &files.pnp_data {
        Some(data_path) => {
            let raw = std::fs::read_to_string(data_path).map_err(|source| {
                DirectExecError::io(format!("reading {}", data_path.display()), source)
            })?;
            let mut manifest: pnp::Manifest = serde_json::from_str(&raw)
                .map_err(|error| parse_error(data_path, error.to_string()))?;
            pnp::init_pnp_manifest(&mut manifest, &files.pnp_cjs);
            Ok(manifest)
        }
        None => pnp::load_pnp_manifest(&files.pnp_cjs)
            .map_err(|error| parse_error(&files.pnp_cjs, error.to_string())),
    }
}

#[derive(Default)]
pub struct ManifestCache {
    loaded: Option<(Fingerprint, ManifestFiles, pnp::Manifest)>,
}

impl ManifestCache {
    /// Return the parsed manifest for `project_root`, reparsing only when the
    /// manifest files changed since the last call.
    pub fn get_or_load(
        &mut self,
        project_root: &Path,
    ) -> Result<(&pnp::Manifest, &ManifestFiles, Fingerprint), DirectExecError> {
        let files = locate(project_root)?;
        let current = fingerprint(&files)?;
        let stale = self
            .loaded
            .as_ref()
            .is_none_or(|(cached, _, _)| *cached != current);
        if stale {
            let manifest = load(&files)?;
            self.loaded = Some((current.clone(), files, manifest));
        }
        let (_, files, manifest) = self.loaded.as_ref().expect("populated above");
        Ok((manifest, files, current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::{FixtureOptions, PnpFixture};
    use crate::DirectExecError;

    #[test]
    fn locate_requires_pnp_cjs() {
        let temp = tempfile::tempdir().unwrap();
        let error = locate(temp.path()).unwrap_err();
        assert!(
            matches!(error, DirectExecError::NoPnpManifest { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("--no-direct"));
    }

    #[test]
    fn locate_reports_split_data_and_loader() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(
            temp.path(),
            FixtureOptions {
                with_loader: true,
                ..FixtureOptions::default()
            },
        );
        let files = locate(&fixture.root).unwrap();
        assert_eq!(files.pnp_data, Some(fixture.root.join(".pnp.data.json")));
        assert_eq!(files.pnp_loader, Some(fixture.root.join(".pnp.loader.mjs")));
    }

    #[test]
    fn locate_reports_inline_layout() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(
            temp.path(),
            FixtureOptions {
                inline_manifest: true,
                ..FixtureOptions::default()
            },
        );
        let files = locate(&fixture.root).unwrap();
        assert_eq!(files.pnp_data, None);
    }

    #[test]
    fn load_handles_split_and_inline_manifests() {
        for inline in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let fixture = PnpFixture::write(
                temp.path(),
                FixtureOptions {
                    inline_manifest: inline,
                    ..FixtureOptions::default()
                },
            );
            let files = locate(&fixture.root).unwrap();
            assert_eq!(files.pnp_data.is_some(), !inline);
            let manifest = load(&files).unwrap();
            assert!(pnp::find_locator(&manifest, &fixture.app_dir.join("package.json")).is_some());
        }
    }

    #[test]
    fn load_reports_parse_errors() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(temp.path(), FixtureOptions::default());
        std::fs::write(fixture.root.join(".pnp.data.json"), "{not json").unwrap();
        let error = load(&locate(&fixture.root).unwrap()).unwrap_err();
        assert!(
            matches!(error, DirectExecError::ManifestParse { .. }),
            "{error}"
        );
    }

    /// Shared body for `cache_reuses_until_*_changes`: writes a fixture
    /// (split or inline per `inline_manifest`), confirms `ManifestCache`
    /// reuses the parsed manifest across two calls, then appends a newline
    /// to `changed_file` (relative to the fixture root) and confirms the
    /// fingerprint changes.
    fn assert_cache_invalidated_by_touching(inline_manifest: bool, changed_file: &str) {
        let temp = tempfile::tempdir().unwrap();
        let fixture = PnpFixture::write(
            temp.path(),
            FixtureOptions {
                inline_manifest,
                ..FixtureOptions::default()
            },
        );
        let mut cache = ManifestCache::default();
        let first = cache.get_or_load(&fixture.root).unwrap().2;
        let second = cache.get_or_load(&fixture.root).unwrap().2;
        assert_eq!(first, second);

        let path = fixture.root.join(changed_file);
        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.push('\n');
        std::fs::write(&path, contents).unwrap();
        let third = cache.get_or_load(&fixture.root).unwrap().2;
        assert_ne!(first, third, "size change must produce a new fingerprint");
    }

    #[test]
    fn cache_reuses_until_files_change() {
        assert_cache_invalidated_by_touching(false, ".pnp.data.json");
    }

    #[test]
    fn cache_reuses_until_inline_manifest_changes() {
        assert_cache_invalidated_by_touching(true, ".pnp.cjs");
    }
}
