use super::*;

fn directory_names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn init_repository(path: &Path) {
    gix::init(path).unwrap();
    // Input resolution reads the index even when every candidate is untracked.
    assert!(std::process::Command::new("git")
        .args(["read-tree", "--empty"])
        .current_dir(path)
        .status()
        .unwrap()
        .success());
}

#[test]
fn empty_restore_leaves_package_unchanged() {
    let repo = tempfile::tempdir().unwrap();
    let package = repo.path().join("package");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("source.ts"), b"source").unwrap();
    let before = directory_names(&package);

    for commit in [true, false] {
        let staged = StagedRestore::empty(&package).unwrap();
        assert!(staged.staging_dir.is_none());
        assert!(staged.relative_file_paths().unwrap().is_empty());
        assert_eq!(
            staged.cache_files_hash().unwrap(),
            crate::combined_cache_files_hash(&[])
        );
        assert!(!staged.is_package_relative_staging_path(Path::new("source.ts")));
        assert_eq!(directory_names(&package), before);
        if commit {
            assert!(staged.commit().unwrap().is_empty());
        } else {
            staged.discard().unwrap();
        }
        assert_eq!(directory_names(&package), before);
        assert_eq!(fs::read(package.join("source.ts")).unwrap(), b"source");
    }
    assert!(!repo.path().join(".luchta").exists());
}

#[test]
fn empty_restore_needs_no_existing_package_directory() {
    let repo = tempfile::tempdir().unwrap();
    let missing = repo.path().join("missing");
    let staged = StagedRestore::empty(&missing).unwrap();
    assert!(staged.relative_file_paths().unwrap().is_empty());
    assert!(staged.commit().unwrap().is_empty());
    StagedRestore::empty(&missing).unwrap().discard().unwrap();
    assert!(!missing.exists());
}

#[test]
fn outputs_stage_under_repository_tmp_until_commit_or_discard() {
    let repo = tempfile::tempdir().unwrap();
    let paths = super::super::open_shared_paths(&repo.path().join("shared")).unwrap();
    let source = repo.path().join("source");
    let package = repo.path().join("package");
    let staging_root = repo.path().join(".luchta/tmp");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&package).unwrap();
    fs::write(source.join("out.js"), b"cached").unwrap();
    fs::write(package.join("out.js"), b"live").unwrap();
    let before = directory_names(&package);
    let hash = [42; 32];
    write_outputs_blob(&paths, &hash, &source, &[PathBuf::from("out.js")], 100).unwrap();

    for commit in [false, true] {
        let BlobReadResultWithMeta::Restored(staged) =
            restore_outputs_staged(&paths, &hash, &package, &staging_root).unwrap()
        else {
            panic!("outputs should stage");
        };
        let staging_path = staged.staging_dir.as_ref().unwrap().path().to_path_buf();
        assert_eq!(staging_path.parent(), Some(staging_root.as_path()));
        assert!(staging_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("blob-restore-meta-"));
        assert_eq!(directory_names(&staging_root).len(), 1);
        assert_eq!(directory_names(&package), before);
        assert_eq!(fs::read(package.join("out.js")).unwrap(), b"live");
        assert_eq!(
            staged.relative_file_paths().unwrap(),
            [PathBuf::from("out.js")]
        );
        if commit {
            assert_eq!(staged.commit().unwrap(), [package.join("out.js")]);
        } else {
            staged.discard().unwrap();
        }
        assert!(!staging_path.exists());
        assert!(directory_names(&staging_root).is_empty());
        assert_eq!(directory_names(&package), before);
    }
    assert_eq!(fs::read(package.join("out.js")).unwrap(), b"cached");
}

#[test]
fn staging_root_creation_failure_falls_back_to_package() {
    let repo = tempfile::tempdir().unwrap();
    let package = repo.path().join("package");
    fs::create_dir(&package).unwrap();
    fs::write(repo.path().join(".luchta"), b"blocked").unwrap();
    for prefix in ["blob-restore-", "blob-restore-meta-"] {
        let staging =
            create_restore_staging_dir(&repo.path().join(".luchta/tmp"), &package, prefix).unwrap();
        assert_eq!(staging.path().parent(), Some(package.as_path()));
        assert!(staging
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(prefix));
        staging.close().unwrap();
        assert!(directory_names(&package).is_empty());
    }
    assert_eq!(fs::read(repo.path().join(".luchta")).unwrap(), b"blocked");
}

#[test]
fn legacy_restore_staging_uses_repository_tmp() {
    let repo = tempfile::tempdir().unwrap();
    let package = repo.path().join("package");
    fs::create_dir(&package).unwrap();
    let staging_root = repo.path().join(".luchta/tmp");
    let staging = create_restore_staging_dir(&staging_root, &package, "blob-restore-").unwrap();
    assert_eq!(staging.path().parent(), Some(staging_root.as_path()));
    assert!(directory_names(&package).is_empty());
    fs::write(staging.path().join("out.js"), b"cached").unwrap();
    apply_staging_dir(staging, &package).unwrap();
    assert_eq!(
        directory_names(&package),
        [std::ffi::OsString::from("out.js")]
    );
    assert!(directory_names(&staging_root).is_empty());
}

// /dev/shm gives Linux CI an unprivileged second filesystem. Other platforms
// still exercise staging selection and normal commits above.
#[cfg(target_os = "linux")]
fn second_filesystem(package: &Path) -> Option<TempDir> {
    use std::os::unix::fs::MetadataExt;

    let temp = match tempfile::tempdir_in("/dev/shm") {
        Ok(temp) => temp,
        Err(error) => {
            eprintln!("skipping cross-device test: /dev/shm is unavailable: {error}");
            return None;
        }
    };
    if fs::metadata(temp.path()).unwrap().dev() == fs::metadata(package).unwrap().dev() {
        eprintln!("skipping cross-device test: /dev/shm and package share a device");
        return None;
    }
    Some(temp)
}

#[test]
#[cfg(target_os = "linux")]
fn different_device_stages_in_package() {
    let package = tempfile::tempdir().unwrap();
    let Some(other_device) = second_filesystem(package.path()) else {
        return;
    };
    let staging_root = other_device.path().join(".luchta/tmp");
    let staging =
        create_restore_staging_dir(&staging_root, package.path(), "blob-restore-meta-").unwrap();
    assert_eq!(staging.path().parent(), Some(package.path()));
    assert!(directory_names(&staging_root).is_empty());
    fs::write(staging.path().join("out.js"), b"cached").unwrap();
    let staged = StagedRestore {
        meta: StagedRestore::empty(package.path()).unwrap().meta,
        staging_dir: Some(staging),
        package_dir: package.path().to_path_buf(),
    };
    staged.commit().unwrap();
    assert_eq!(fs::read(package.path().join("out.js")).unwrap(), b"cached");
    assert_eq!(
        directory_names(package.path()),
        [std::ffi::OsString::from("out.js")]
    );
}

#[test]
#[cfg(target_os = "linux")]
fn rename_time_cross_device_fallback_commits_all_files() {
    use std::os::unix::fs::PermissionsExt;

    for legacy in [false, true] {
        let package = tempfile::tempdir().unwrap();
        let Some(other_device) = second_filesystem(package.path()) else {
            return;
        };
        let staging = tempfile::tempdir_in(other_device.path()).unwrap();
        fs::create_dir(staging.path().join("dist")).unwrap();
        fs::write(staging.path().join("dist/a.js"), b"cached a").unwrap();
        fs::write(staging.path().join("dist/b.js"), b"cached b").unwrap();
        fs::set_permissions(
            staging.path().join("dist/a.js"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::create_dir_all(package.path().join("dist")).unwrap();
        fs::write(package.path().join("dist/a.js"), b"live").unwrap();
        if legacy {
            apply_staging_dir(staging, package.path()).unwrap();
        } else {
            let staged = StagedRestore {
                meta: StagedRestore::empty(package.path()).unwrap().meta,
                staging_dir: Some(staging),
                package_dir: package.path().to_path_buf(),
            };
            assert_eq!(staged.commit().unwrap().len(), 2);
        }
        assert_eq!(
            fs::read(package.path().join("dist/a.js")).unwrap(),
            b"cached a"
        );
        assert_eq!(
            fs::read(package.path().join("dist/b.js")).unwrap(),
            b"cached b"
        );
        assert_eq!(
            fs::metadata(package.path().join("dist/a.js"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            directory_names(&package.path().join("dist")),
            [
                std::ffi::OsString::from("a.js"),
                std::ffi::OsString::from("b.js")
            ]
        );
        assert!(directory_names(other_device.path()).is_empty());
    }
}

#[test]
fn repository_staging_is_ignored_with_external_local_cache() {
    let repo = tempfile::tempdir().unwrap();
    let external_cache = tempfile::tempdir().unwrap();
    crate::store::Cache::open(external_cache.path()).unwrap();
    let root = repo.path().canonicalize().unwrap();
    init_repository(&root);
    fs::write(root.join("source.ts"), b"source").unwrap();
    assert!(!root.join(".luchta/.gitignore").exists());

    let staging_root = root.join(".luchta/tmp");
    let staging = create_restore_staging_dir(&staging_root, &root, "blob-restore-meta-").unwrap();
    fs::write(staging.path().join("live-staged.ts"), b"cached").unwrap();
    assert_eq!(
        fs::read_to_string(root.join(".luchta/.gitignore")).unwrap(),
        crate::store::GITIGNORE_CONTENTS
    );
    let inputs = crate::resolve_inputs(&root, &["**/*".to_owned()]).unwrap();
    assert_eq!(
        inputs
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["source.ts"]
    );
    staging.close().unwrap();
    assert!(directory_names(&staging_root).is_empty());
}

#[test]
fn staging_ignore_preserves_existing_user_rules() {
    for existing in ["# user rules\n/cache/", "# user rules\n!tmp/\n", "*\n"] {
        let repo = tempfile::tempdir().unwrap();
        let staging_root = repo.path().join(".luchta/tmp");
        fs::create_dir_all(staging_root.parent().unwrap()).unwrap();
        let ignore_path = repo.path().join(".luchta/.gitignore");
        fs::write(&ignore_path, existing).unwrap();
        let staging =
            create_restore_staging_dir(&staging_root, repo.path(), "blob-restore-").unwrap();
        init_repository(repo.path());
        fs::write(staging.path().join("live-staged.ts"), b"cached").unwrap();
        let inputs = crate::resolve_inputs(repo.path(), &["**/*".to_owned()]).unwrap();
        assert!(inputs
            .iter()
            .all(|entry| !entry.path.starts_with(".luchta/tmp/")));
        let updated = fs::read_to_string(&ignore_path).unwrap();
        assert!(updated.starts_with(existing));
        if existing != crate::store::GITIGNORE_CONTENTS {
            assert!(updated.ends_with("\n/tmp/\n"));
        } else {
            assert_eq!(updated, existing);
        }
        staging.close().unwrap();
        create_restore_staging_dir(&staging_root, repo.path(), "blob-restore-")
            .unwrap()
            .close()
            .unwrap();
        assert_eq!(fs::read_to_string(&ignore_path).unwrap(), updated);
    }
}

#[test]
fn staging_path_membership_handles_root_package_and_fallback() {
    for fallback in [false, true] {
        let repo = tempfile::tempdir().unwrap();
        let staging_root = repo.path().join(".luchta/tmp");
        if fallback {
            fs::write(repo.path().join(".luchta"), b"blocked").unwrap();
        }
        let staging =
            create_restore_staging_dir(&staging_root, repo.path(), "blob-restore-meta-").unwrap();
        let relative_dir = staging
            .path()
            .strip_prefix(repo.path())
            .unwrap()
            .to_path_buf();
        let relative_file = relative_dir.join("cache.bin");
        fs::write(staging.path().join("cache.bin"), b"cached").unwrap();
        let mut staged = StagedRestore::empty(repo.path()).unwrap();
        staged.staging_dir = Some(staging);
        assert!(staged.is_package_relative_staging_path(&relative_dir));
        assert!(staged.is_package_relative_staging_path(&relative_file));
        assert!(!staged.is_package_relative_staging_path(Path::new("cache.bin")));
        assert!(!staged.is_package_relative_staging_path(Path::new("source.ts")));
        staged.discard().unwrap();
        assert!(!repo.path().join(relative_dir).exists());
    }
}

#[test]
fn staging_path_membership_rejects_paths_in_nested_package() {
    let repo = tempfile::tempdir().unwrap();
    let package = repo.path().join("package");
    fs::create_dir(&package).unwrap();
    let staging = create_restore_staging_dir(
        &repo.path().join(".luchta/tmp"),
        &package,
        "blob-restore-meta-",
    )
    .unwrap();
    let mut staged = StagedRestore::empty(&package).unwrap();
    staged.staging_dir = Some(staging);
    assert!(!staged.is_package_relative_staging_path(Path::new("cache.bin")));
    assert!(!staged.is_package_relative_staging_path(Path::new(".luchta/tmp/cache.bin")));
    staged.discard().unwrap();
}

#[test]
fn staging_ignore_handles_non_utf8_and_trailing_whitespace() {
    for (existing, needs_rule) in [
        (b"# user comment: \xff\n/cache/".as_slice(), true),
        (b"# user comment: \xff\n/tmp/\n\n \t\r\n".as_slice(), false),
        (b"/tmp/ \t\r\n\n".as_slice(), false),
        (b"/tmp/\n\n!tmp/\n\n".as_slice(), true),
    ] {
        let repo = tempfile::tempdir().unwrap();
        let staging_root = repo.path().join(".luchta/tmp");
        fs::create_dir_all(staging_root.parent().unwrap()).unwrap();
        let ignore_path = repo.path().join(".luchta/.gitignore");
        fs::write(&ignore_path, existing).unwrap();
        let mut expected = existing.to_vec();
        if needs_rule {
            if !existing.ends_with(b"\n") {
                expected.push(b'\n');
            }
            expected.extend_from_slice(b"/tmp/\n");
        }
        for _ in 0..2 {
            let staging =
                create_restore_staging_dir(&staging_root, repo.path(), "blob-restore-").unwrap();
            assert_eq!(staging.path().parent(), Some(staging_root.as_path()));
            assert_eq!(fs::read(&ignore_path).unwrap(), expected);
            staging.close().unwrap();
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn cross_device_cleanup_failures_do_not_fail_committed_restore() {
    use std::os::unix::fs::PermissionsExt;

    for legacy in [false, true] {
        let package = tempfile::tempdir().unwrap();
        let Some(other_device) = second_filesystem(package.path()) else {
            return;
        };
        let staging = tempfile::tempdir_in(other_device.path()).unwrap();
        let staging_path = staging.path().to_path_buf();
        let source_dir = staging_path.join("dist");
        fs::create_dir(&source_dir).unwrap();
        fs::write(source_dir.join("a.js"), b"cached a").unwrap();
        fs::write(source_dir.join("b.js"), b"cached b").unwrap();
        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let probe = fs::remove_file(source_dir.join("a.js"));
        if probe.is_ok() {
            fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "skipping cleanup failure test: user can remove files from read-only directories"
            );
            return;
        }
        assert_eq!(probe.unwrap_err().kind(), io::ErrorKind::PermissionDenied);

        let result = if legacy {
            apply_staging_dir(staging, package.path()).map(|()| Vec::new())
        } else {
            let mut staged = StagedRestore::empty(package.path()).unwrap();
            staged.staging_dir = Some(staging);
            staged.commit()
        };
        // Restore permissions before asserting, even if the restore failed.
        if source_dir.exists() {
            fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::remove_dir_all(&staging_path).unwrap();
        }
        let mut written = result.expect("cleanup must not fail a committed restore");
        if !legacy {
            written.sort();
            assert_eq!(
                written,
                [
                    package.path().join("dist/a.js"),
                    package.path().join("dist/b.js")
                ]
            );
        }
        assert_eq!(
            fs::read(package.path().join("dist/a.js")).unwrap(),
            b"cached a"
        );
        assert_eq!(
            fs::read(package.path().join("dist/b.js")).unwrap(),
            b"cached b"
        );
        assert_eq!(
            directory_names(&package.path().join("dist")),
            [
                std::ffi::OsString::from("a.js"),
                std::ffi::OsString::from("b.js")
            ]
        );
        assert!(directory_names(other_device.path()).is_empty());
    }
}
