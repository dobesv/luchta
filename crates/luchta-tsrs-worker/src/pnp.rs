//! PnP filesystem shared by config parsing and program construction.
//!
//! tsrs resolves config `extends` before its module-resolver hook runs. Present
//! PnP dependencies as synthetic node_modules entries so its own resolver handles
//! exports, typesVersions and config inheritance in both phases. `realpath` maps
//! successful lookups back to package locations, including virtual zip paths.

mod filesystem;
mod resolver;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use luchta_yarn_env::manifest::{self, Fingerprint};
use tsrs_compiler::CreateModuleResolver;
use tsrs_tsoptions::ParseConfigHost;
use tsrs_vfs::{bundled, FS};

pub use filesystem::PnpFs;

struct CachedManifest {
    fingerprint: Fingerprint,
    manifest: Arc<::pnp::Manifest>,
}

static MANIFEST_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedManifest>>> = OnceLock::new();

/// Reuse parsed manifests across jobs until path, size or mtime changes. The
/// shared yarn-env loader handles both inline CJS and split JSON manifests.
pub fn cached_manifest(cwd: &Path) -> Result<Option<Arc<::pnp::Manifest>>, String> {
    // Use tsrs's canonical spelling (not Windows' verbatim \\?\ prefix), so
    // manifest trie keys and paths supplied by the compiler use the same form.
    let cwd = tsrs_vfs::osvfs::fs().realpath(&cwd.to_string_lossy());
    let Some(path) = ::pnp::find_closest_pnp_manifest_path(Path::new(&cwd)) else {
        return Ok(None);
    };
    let root = path.parent().expect("manifest has a parent");
    let files = manifest::locate(root).map_err(|error| error.to_string())?;
    let fingerprint = manifest::fingerprint(&files).map_err(|error| error.to_string())?;
    // Loading may panic, but entries are inserted only after a complete load.
    let mut cache = MANIFEST_CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = cache.get(root) {
        if entry.fingerprint == fingerprint {
            return Ok(Some(Arc::clone(&entry.manifest)));
        }
    }
    let manifest = Arc::new(manifest::load(&files).map_err(|error| error.to_string())?);
    cache.insert(
        root.to_path_buf(),
        CachedManifest {
            fingerprint,
            manifest: Arc::clone(&manifest),
        },
    );
    Ok(Some(manifest))
}

/// Use the same instance for ParseConfigHost and CompilerHost. Using only the
/// program hook leaves bare config `extends` unresolved.
pub struct CompilerSystem {
    cwd: String,
    fs: Arc<dyn FS>,
    manifest: Option<Arc<::pnp::Manifest>>,
}

impl CompilerSystem {
    pub fn new(cwd: &Path) -> Result<Self, String> {
        let cwd = tsrs_vfs::osvfs::fs().realpath(&cwd.to_string_lossy());
        let manifest = cached_manifest(Path::new(&cwd))?;
        let fs = Arc::new(bundled::wrap_fs(PnpFs::new(manifest.clone())));
        Ok(Self { cwd, fs, manifest })
    }

    pub fn filesystem(&self) -> Arc<dyn FS> {
        Arc::clone(&self.fs)
    }

    /// Keep tsrs's resolution rules and caches; PnpFs supplies package locations.
    pub fn module_resolver(&self) -> Option<CreateModuleResolver> {
        self.manifest.as_ref().map(|manifest| {
            let manifest = Arc::clone(manifest);
            Arc::new(move |options| {
                Box::new(resolver::PnpResolver::new(options, Arc::clone(&manifest)))
                    as Box<dyn tsrs_module::Resolver>
            }) as CreateModuleResolver
        })
    }
}

impl ParseConfigHost for CompilerSystem {
    fn fs(&self) -> &dyn FS {
        self.fs.as_ref()
    }

    fn get_current_directory(&self) -> &str {
        &self.cwd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luchta_test_support::require_nextest;
    use luchta_yarn_env::test_fixture::PnpFixture;

    #[test]
    fn pnp_manifest_cache_recovers_from_poisoning() {
        require_nextest();
        let temp = tempfile::tempdir().unwrap();
        let root = tsrs_vfs::osvfs::fs().realpath(&temp.path().to_string_lossy());
        let fixture = PnpFixture::write(Path::new(&root), Default::default());
        let first = cached_manifest(&fixture.app_dir).unwrap().unwrap();
        let cache = MANIFEST_CACHE.get().unwrap();
        let panic = std::panic::catch_unwind(|| {
            let _guard = cache.lock().unwrap();
            panic!("manifest loader panic");
        });
        assert!(panic.is_err());
        assert!(cache.is_poisoned());
        let recovered = cached_manifest(&fixture.app_dir).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &recovered));

        // A different PnP root must still load and cache after the panic.
        let second_root = Path::new(&root).join("second");
        let second = PnpFixture::write(&second_root, Default::default());
        let loaded = cached_manifest(&second.app_dir).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first, &loaded));
        assert!(Arc::ptr_eq(
            &loaded,
            &cached_manifest(&second.app_dir).unwrap().unwrap()
        ));
    }
}
