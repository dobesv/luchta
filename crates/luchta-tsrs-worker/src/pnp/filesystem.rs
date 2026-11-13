use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use luchta_yarn_env::bins::{new_zip_cache, read_virtual_file, ZipCache};
use pnp::fs::{FileType, VPath, VPathInfo, ZipCache as _};
use pnp::{Manifest, PackageInformation, Resolution};
use tsrs_core::tspath;
use tsrs_vfs::{osvfs, Entries, FileInfo, FileMode, FS};

/// Read-only zip and PnP package view over the OS filesystem. Writes to ordinary
/// output paths still go to the OS; writes into archives are rejected.
pub struct PnpFs {
    manifest: Option<Arc<Manifest>>,
    zips: ZipCache,
}

impl PnpFs {
    pub fn new(manifest: Option<Arc<Manifest>>) -> Self {
        Self {
            manifest,
            zips: new_zip_cache(),
        }
    }

    fn package(&self, path: &Path) -> Option<&PackageInformation> {
        let manifest = self.manifest.as_ref()?;
        // Relative paths (including drive-relative C:) have no PnP issuer.
        // find_locator requires an absolute path; leave these to the OS view.
        if !path.is_absolute() {
            return None;
        }
        let locator = pnp::find_locator(manifest, &path.join("package.json"))?;
        manifest
            .package_registry_data
            .get(&locator.name)?
            .get(&locator.reference)
    }

    // Only node_modules segments *inside* an owning package are synthetic. A
    // zip's own node_modules/pkg prefix is a real archive member, not a lookup.
    fn mapped_path(&self, path: &str) -> Option<String> {
        let path = tspath::normalize_path(&tspath::normalize_slashes(path));
        let Some((issuer, request)) = path.rsplit_once("/node_modules/") else {
            return Some(path);
        };
        let Some(owner) = self.package(Path::new(&path)) else {
            return Some(path);
        };
        if issuer.len()
            < owner
                .package_location
                .to_string_lossy()
                .trim_end_matches('/')
                .len()
        {
            return Some(path);
        }
        // Keep the separator so / and C:/ remain absolute issuers at the root.
        let issuer = self.mapped_path(&path[..issuer.len() + 1])?;
        let manifest = self.manifest.as_ref()?;
        match pnp::resolve_to_unqualified_via_manifest(
            manifest,
            request,
            &Path::new(&issuer).join("package.json"),
        )
        .ok()?
        {
            Resolution::Resolved(root, subpath) => {
                let resolved = subpath.map_or(root.clone(), |subpath| root.join(subpath));
                Some(tspath::normalize_slashes(&resolved.to_string_lossy()))
            }
            Resolution::Skipped => Some(path),
        }
    }

    fn dependency_entries(&self, path: &str) -> Option<Entries> {
        let path = tspath::normalize_path(&tspath::normalize_slashes(path));
        let (issuer, scope) = if let Some(issuer) = path.strip_suffix("/node_modules") {
            (issuer, "")
        } else {
            let (issuer, scope) = path.rsplit_once("/node_modules/")?;
            if !scope.starts_with('@') || scope.contains('/') {
                return None;
            }
            (issuer, scope)
        };
        // Keep the separator so / and C:/ remain absolute issuers at the root.
        let issuer = self.mapped_path(&path[..issuer.len() + 1])?;
        let package = self.package(Path::new(&issuer))?;
        let mut directories = Vec::new();
        for name in package.package_dependencies.keys() {
            let entry = if scope.is_empty() {
                name.split('/').next().unwrap()
            } else {
                let Some(entry) = name.strip_prefix(&format!("{scope}/")) else {
                    continue;
                };
                entry
            };
            directories.push(entry.to_owned());
        }
        directories.sort();
        directories.dedup();
        Some(Entries {
            directories,
            ..Default::default()
        })
    }

    fn is_kind(&self, path: &str, kind: FileType) -> bool {
        let Some(mapped) = self.mapped_path(path) else {
            return false;
        };
        match VPath::from(Path::new(&mapped)) {
            Ok(VPath::Zip(info)) => self.zip_kind(&info).is_some_and(|actual| actual == kind),
            Ok(VPath::Virtual(info)) => native_is_kind(&info.physical_base_path(), kind),
            Ok(VPath::Native(path)) => native_is_kind(&path, kind),
            Err(_) => false,
        }
    }

    fn virtual_stat(&self, path: &str) -> Option<FileInfo> {
        let mapped = self.mapped_path(path)?;
        match VPath::from(Path::new(&mapped)).ok()? {
            VPath::Zip(info) => {
                let inner = info.zip_path.trim_end_matches('/');
                let kind = self.zip_kind(&info)?;
                let size = if kind == FileType::File {
                    self.zips.read(info.physical_base_path(), inner).ok()?.len() as i64
                } else {
                    0
                };
                Some(FileInfo {
                    name: tspath::get_base_file_name(&mapped).to_string(),
                    size,
                    mode: if kind == FileType::Directory {
                        FileMode::Dir
                    } else {
                        FileMode::None
                    },
                    mod_time: osvfs::fs()
                        .stat(&info.physical_base_path().to_string_lossy())
                        .and_then(|s| s.mod_time),
                })
            }
            VPath::Virtual(info) => osvfs::fs().stat(&info.physical_base_path().to_string_lossy()),
            VPath::Native(path) => osvfs::fs().stat(&path.to_string_lossy()),
        }
    }

    fn zip_kind(&self, info: &pnp::fs::ZipInfo) -> Option<FileType> {
        let inner = info.zip_path.trim_end_matches('/');
        self.zips
            .act(info.physical_base_path(), |zip| {
                if inner.is_empty() {
                    Some(FileType::Directory)
                } else {
                    zip.file_type(inner).ok()
                }
            })
            .ok()
            .flatten()
    }

    fn zip_entries(&self, info: &pnp::fs::ZipInfo) -> Entries {
        let inner = info.zip_path.trim_end_matches('/');
        let prefix = if inner.is_empty() {
            String::new()
        } else {
            format!("{inner}/")
        };
        self.zips
            .act(info.physical_base_path(), |zip| {
                let immediate = |name: &str| {
                    let child = name.strip_prefix(&prefix)?.trim_end_matches('/');
                    (!child.is_empty() && !child.contains('/')).then(|| child.to_owned())
                };
                let mut files: Vec<_> = zip
                    .files
                    .keys()
                    .filter_map(|name| immediate(name))
                    .collect();
                let mut directories: Vec<_> =
                    zip.dirs.iter().filter_map(|name| immediate(name)).collect();
                files.sort();
                directories.sort();
                Entries {
                    files,
                    directories,
                    symlinks: Some(Default::default()),
                }
            })
            .unwrap_or_default()
    }

    fn writable_path(&self, path: &str) -> Result<PathBuf, String> {
        let mapped = self
            .mapped_path(path)
            .ok_or_else(|| format!("cannot resolve path: {path}"))?;
        match VPath::from(Path::new(&mapped)).map_err(|error| error.to_string())? {
            VPath::Zip(_) => Err(format!("cannot modify zip archive member: {path}")),
            VPath::Virtual(info) => Ok(info.physical_base_path()),
            VPath::Native(path) => Ok(path),
        }
    }
}

impl FS for PnpFs {
    fn use_case_sensitive_file_names(&self) -> bool {
        osvfs::fs().use_case_sensitive_file_names()
    }

    fn file_exists(&self, path: &str) -> bool {
        self.is_kind(path, FileType::File)
    }

    fn read_file(&self, path: &str) -> Option<String> {
        let path = self.mapped_path(path)?;
        match VPath::from(Path::new(&path)).ok()? {
            VPath::Native(native) => osvfs::fs().read_file(&native.to_string_lossy()),
            VPath::Virtual(info) => {
                osvfs::fs().read_file(&info.physical_base_path().to_string_lossy())
            }
            VPath::Zip(_) => {
                let bytes = read_virtual_file(Path::new(&path), &self.zips).ok()?;
                // Match the OS reader's UTF-8/UTF-16 BOM handling in archives.
                Some(tsrs_vfs::internal::decode_bytes(bytes))
            }
        }
    }

    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        osvfs::fs().write_file(&self.writable_path(path)?.to_string_lossy(), data)
    }

    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        osvfs::fs().append_file(&self.writable_path(path)?.to_string_lossy(), data)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        osvfs::fs().remove(&self.writable_path(path)?.to_string_lossy())
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        osvfs::fs().chtimes(&self.writable_path(path)?.to_string_lossy(), a_time, m_time)
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.dependency_entries(path).is_some() || self.is_kind(path, FileType::Directory)
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        if !self.is_kind(path, FileType::Directory) {
            if let Some(entries) = self.dependency_entries(path) {
                return entries;
            }
        }
        let Some(mapped) = self.mapped_path(path) else {
            return Entries::default();
        };
        match VPath::from(Path::new(&mapped)) {
            Ok(VPath::Zip(info)) => self.zip_entries(&info),
            Ok(VPath::Virtual(info)) => {
                osvfs::fs().get_accessible_entries(&info.physical_base_path().to_string_lossy())
            }
            Ok(VPath::Native(path)) => osvfs::fs().get_accessible_entries(&path.to_string_lossy()),
            Err(_) => Entries::default(),
        }
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        self.virtual_stat(path).or_else(|| {
            self.dependency_entries(path).map(|_| FileInfo {
                name: tspath::get_base_file_name(path).to_string(),
                size: 0,
                mode: FileMode::Dir,
                mod_time: None,
            })
        })
    }

    fn realpath(&self, path: &str) -> String {
        let Some(mapped) = self.mapped_path(path) else {
            return path.to_owned();
        };
        // Preserve virtual package identity: stripping __virtual__ merges peer
        // environments. VPath only devirtualizes when reading the physical data.
        match VPath::from(Path::new(&mapped)) {
            Ok(VPath::Native(path)) => osvfs::fs().realpath(&path.to_string_lossy()),
            _ => mapped,
        }
    }
}

fn native_is_kind(path: &Path, kind: FileType) -> bool {
    let path = path.to_string_lossy();
    match kind {
        FileType::File => osvfs::fs().file_exists(&path),
        FileType::Directory => osvfs::fs().directory_exists(&path),
    }
}
