use std::path::{Path, PathBuf};

use crate::bins::Binary;
use crate::DirectExecError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimSpec {
    pub name: String,
    pub exec: String,
    pub leading_args: Vec<String>,
}

impl ShimSpec {
    fn new(name: &str, exec: &Path, leading_args: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            exec: exec.to_string_lossy().into_owned(),
            leading_args: leading_args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[cfg(unix)]
    fn contents(&self) -> String {
        shim_script(&self.exec, &self.leading_args)
    }
}

/// The exact POSIX wrapper yarn's `makePathWrapper` writes.
pub fn shim_script(exec: &str, leading_args: &[String]) -> String {
    let mut script = format!("#!/bin/sh\nexec \"{exec}\"");
    for arg in leading_args {
        script.push(' ');
        script.push('\'');
        script.push_str(&arg.replace('\'', "'\"'\"'"));
        script.push('\'');
    }
    script.push_str(" \"$@\"\n");
    script
}

pub fn standard_shims(node: &Path, yarn: &Path) -> Vec<ShimSpec> {
    vec![
        ShimSpec::new("node", node, &[]),
        ShimSpec::new("yarn", yarn, &[]),
        ShimSpec::new("yarnpkg", yarn, &[]),
        ShimSpec::new("run", yarn, &["run"]),
        ShimSpec::new("node-gyp", yarn, &["run", "--top-level", "node-gyp"]),
    ]
}

pub fn binary_shims(node: &Path, binaries: &[Binary]) -> Vec<ShimSpec> {
    binaries
        .iter()
        .map(|binary| {
            let target = binary.path.to_string_lossy();
            if binary.is_node_script {
                ShimSpec::new(&binary.name, node, &[target.as_ref()])
            } else {
                ShimSpec::new(&binary.name, &binary.path, &[])
            }
        })
        .collect()
}

/// The per-user parent of every shim directory. Scoping it by euid means two
/// different users on a shared machine (or a shared `/tmp`) never contend for
/// the same directory name, so one user's shim dir can never masquerade as
/// another's.
#[cfg(unix)]
fn shim_parent_dir() -> PathBuf {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    std::env::temp_dir().join(format!("luchta-yarn-bin-{euid}"))
}

#[cfg(unix)]
fn shim_dir_for(shims: &[ShimSpec]) -> PathBuf {
    let mut hasher = blake3::Hasher::new();
    for shim in shims {
        hasher.update(shim.name.as_bytes());
        hasher.update(b"\0");
        hasher.update(shim.contents().as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize().to_hex();
    shim_parent_dir().join(&digest.as_str()[..24])
}

/// Refuse to reuse `target` unless it's owned by the current user and not
/// writable by group or other. A shared temp dir means anyone can try to
/// pre-create the content-addressed path ahead of us; without this check
/// we'd happily execute shims planted by another user.
#[cfg(unix)]
fn check_shim_dir_ownership(target: &Path) -> Result<(), DirectExecError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(target).map_err(|source| {
        DirectExecError::io(
            format!("reusing shim directory {}", target.display()),
            source,
        )
    })?;
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid || metadata.mode() & 0o022 != 0 {
        return Err(DirectExecError::io(
            format!("reusing shim directory {}", target.display()),
            std::io::Error::other("not owned by the current user or writable by others"),
        ));
    }
    Ok(())
}

/// Write the shims into a content-addressed directory under the OS temp dir
/// and return it. Existing directories are reused, after an ownership check;
/// creation is atomic via a temporary sibling and rename, so concurrent
/// workers never see a partial dir.
#[cfg(unix)]
pub fn materialize_shim_dir(shims: &[ShimSpec]) -> Result<PathBuf, DirectExecError> {
    use std::fs::DirBuilder;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let target = shim_dir_for(shims);
    if target.is_dir() {
        check_shim_dir_ownership(&target)?;
        return Ok(target);
    }
    let parent = target.parent().expect("shim dir has a parent");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|source| DirectExecError::io(format!("creating {}", parent.display()), source))?;
    check_shim_dir_ownership(parent)?;

    let staging = parent.join(format!(
        ".{}-{}-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("shims"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&staging)
        .map_err(|source| DirectExecError::io(format!("creating {}", staging.display()), source))?;
    // Set the mode explicitly rather than relying on umask: a permissive
    // umask (e.g. 002) would otherwise leave the group-write bit set, which
    // fails this function's own ownership check the next time it's reused.
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
        .map_err(|source| DirectExecError::io(format!("chmod {}", staging.display()), source))?;
    for shim in shims {
        let path = staging.join(&shim.name);
        std::fs::write(&path, shim.contents()).map_err(|source| {
            DirectExecError::io(format!("writing shim {}", path.display()), source)
        })?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|source| DirectExecError::io(format!("chmod {}", path.display()), source))?;
    }
    match std::fs::rename(&staging, &target) {
        Ok(()) => Ok(target),
        Err(_) if target.is_dir() => {
            let _ = std::fs::remove_dir_all(&staging);
            check_shim_dir_ownership(&target)?;
            Ok(target)
        }
        Err(source) => Err(DirectExecError::io(
            format!("renaming {} to {}", staging.display(), target.display()),
            source,
        )),
    }
}

#[cfg(not(unix))]
pub fn materialize_shim_dir(_shims: &[ShimSpec]) -> Result<PathBuf, DirectExecError> {
    Err(DirectExecError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::bins::Binary;

    #[test]
    fn shim_script_matches_yarn_template() {
        assert_eq!(
            shim_script(
                "/usr/bin/node",
                &["/repo/yarn.cjs".to_owned(), "run".to_owned()]
            ),
            "#!/bin/sh\nexec \"/usr/bin/node\" '/repo/yarn.cjs' 'run' \"$@\"\n"
        );
        assert_eq!(
            shim_script("/usr/bin/node", &[]),
            "#!/bin/sh\nexec \"/usr/bin/node\" \"$@\"\n"
        );
    }

    #[test]
    fn leading_args_escape_single_quotes() {
        assert_eq!(
            shim_script("/bin/x", &["it's".to_owned()]),
            "#!/bin/sh\nexec \"/bin/x\" 'it'\"'\"'s' \"$@\"\n"
        );
    }

    #[test]
    fn standard_shims_cover_yarn_entry_points() {
        let shims = standard_shims(Path::new("/usr/bin/node"), Path::new("/usr/bin/yarn"));
        let names: Vec<&str> = shims.iter().map(|shim| shim.name.as_str()).collect();
        assert_eq!(names, vec!["node", "yarn", "yarnpkg", "run", "node-gyp"]);
        let run = &shims[3];
        assert_eq!(run.exec, "/usr/bin/yarn");
        assert_eq!(run.leading_args, vec!["run".to_owned()]);
        let gyp = &shims[4];
        assert_eq!(
            gyp.leading_args,
            vec![
                "run".to_owned(),
                "--top-level".to_owned(),
                "node-gyp".to_owned()
            ]
        );
    }

    #[test]
    fn binary_shims_route_scripts_through_node() {
        let binaries = vec![
            Binary {
                name: "tool".into(),
                path: "/repo/tool.js".into(),
                is_node_script: true,
            },
            Binary {
                name: "native".into(),
                path: "/repo/native".into(),
                is_node_script: false,
            },
        ];
        let shims = binary_shims(Path::new("/usr/bin/node"), &binaries);
        assert_eq!(shims[0].exec, "/usr/bin/node");
        assert_eq!(shims[0].leading_args, vec!["/repo/tool.js".to_owned()]);
        assert_eq!(shims[1].exec, "/repo/native");
        assert!(shims[1].leading_args.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn materialize_is_content_addressed_and_executable() {
        use std::os::unix::fs::PermissionsExt;

        let shims = standard_shims(Path::new("/usr/bin/node"), Path::new("/usr/bin/yarn"));
        let first = materialize_shim_dir(&shims).unwrap();
        let second = materialize_shim_dir(&shims).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with(shim_parent_dir()));
        let mode = std::fs::metadata(first.join("node"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_to_string(first.join("run")).unwrap(),
            "#!/bin/sh\nexec \"/usr/bin/yarn\" 'run' \"$@\"\n"
        );

        let other = standard_shims(Path::new("/opt/node"), Path::new("/usr/bin/yarn"));
        assert_ne!(materialize_shim_dir(&other).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_shim_dir_is_rejected_on_reuse() {
        use std::os::unix::fs::PermissionsExt;

        // A shim set unique to this test so its content hash doesn't collide
        // with any other test's shim directory.
        let shims = standard_shims(
            Path::new("/usr/bin/node-ownership-test"),
            Path::new("/usr/bin/yarn-ownership-test"),
        );
        let target = materialize_shim_dir(&shims).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777)).unwrap();

        let error = materialize_shim_dir(&shims).unwrap_err();
        assert!(matches!(error, DirectExecError::Io { .. }), "{error}");

        // Don't leave a world-writable directory behind for a later run to
        // trip over.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }
}
