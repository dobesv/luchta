use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use tar::{Archive, Builder};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::atomicio::streaming_atomic_write;
use super::SharedCachePaths;

/// Default zstd compression level for shared-cache blobs.
///
/// Level 3 keeps CPU cost modest while still shrinking typical build outputs
/// well enough for cross-project blob reuse.
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobWriteResult {
    Written,
    AlreadyExists,
    SkippedTooLarge { bytes: u64 },
    NoOutputs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobReadResult {
    Restored,
    Missing,
    Corrupt,
}

pub fn restore_blob(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
) -> io::Result<BlobReadResult> {
    let blob_path = blob_path(paths, outputs_hash);
    let compressed = match File::open(&blob_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(BlobReadResult::Missing)
        }
        Err(error) => return Err(error),
    };

    let staging_dir = tempfile::Builder::new()
        .prefix("blob-restore-")
        .tempdir_in(package_dir)?;

    // No half-extracted poisoning: unpack whole archive into isolated temp dir
    // first, then move validated files into package tree only after full success.
    match extract_blob_to_staging(compressed, package_dir, staging_dir.path()) {
        Ok(()) => apply_staging_dir(staging_dir, package_dir).map(|()| BlobReadResult::Restored),
        Err(RestoreError::Corrupt) => Ok(BlobReadResult::Corrupt),
        Err(RestoreError::Io(error)) => Err(error),
    }
}

fn extract_blob_to_staging(
    compressed: File,
    package_dir: &Path,
    staging_dir: &Path,
) -> Result<(), RestoreError> {
    let decoder = zstd::Decoder::new(compressed).map_err(|_| RestoreError::Corrupt)?;
    let mut archive = Archive::new(decoder);
    let entries = archive.entries().map_err(|_| RestoreError::Corrupt)?;

    for entry in entries {
        let mut entry = entry.map_err(|_| RestoreError::Corrupt)?;
        let entry_path = entry.path().map_err(|_| RestoreError::Corrupt)?;
        let relative_path = validate_entry_path(package_dir, &entry_path)?;
        let target_path = staging_dir.join(&relative_path);

        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target_path)?;
            continue;
        }

        if !entry.header().entry_type().is_file() {
            return Err(RestoreError::Corrupt);
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut output = File::create(&target_path)?;
        io::copy(&mut entry, &mut output).map_err(|_| RestoreError::Corrupt)?;

        #[cfg(unix)]
        {
            let mode = entry.header().mode().map_err(|_| RestoreError::Corrupt)?;
            fs::set_permissions(&target_path, fs::Permissions::from_mode(mode))?;
        }
    }

    Ok(())
}

fn apply_staging_dir(staging_dir: TempDir, package_dir: &Path) -> io::Result<()> {
    move_tree(staging_dir.path(), package_dir)?;
    staging_dir.close()
}

fn move_tree(from_dir: &Path, to_dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from_dir)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = to_dir.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            fs::create_dir_all(&target_path)?;
            move_tree(&source_path, &target_path)?;
            fs::remove_dir(&source_path)?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::rename(&source_path, &target_path)?;
    }

    Ok(())
}

fn validate_entry_path(package_dir: &Path, entry_path: &Path) -> Result<PathBuf, RestoreError> {
    let relative_path = lexical_normalize(entry_path);

    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(RestoreError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "shared blob entry escapes package directory: {}",
                entry_path.display()
            ),
        )));
    }

    let normalized_package_dir = lexical_normalize(package_dir);
    let destination = lexical_normalize(&normalized_package_dir.join(&relative_path));
    if !path_starts_with(&destination, &normalized_package_dir) {
        return Err(RestoreError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "shared blob entry escapes package directory: {}",
                entry_path.display()
            ),
        )));
    }

    Ok(relative_path)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                components.clear();
                components.push(Component::Prefix(prefix));
            }
            Component::RootDir => {
                components.clear();
                components.push(Component::RootDir);
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                if matches!(components.last(), Some(Component::Normal(_))) {
                    components.pop();
                    continue;
                }
                components.push(Component::ParentDir);
            }
            Component::Normal(_) => components.push(component),
        }
    }

    components.iter().collect()
}

fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    let path_components: Vec<_> = path.components().collect();
    let prefix_components: Vec<_> = prefix.components().collect();

    if prefix_components.len() > path_components.len() {
        return false;
    }

    path_components
        .iter()
        .take(prefix_components.len())
        .eq(prefix_components.iter())
}

enum RestoreError {
    Corrupt,
    Io(io::Error),
}

impl From<io::Error> for RestoreError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

fn blob_path(paths: &SharedCachePaths, outputs_hash: &[u8; 32]) -> PathBuf {
    paths.blobs_dir.join(format!(
        "{}.tar.zst",
        blake3::Hash::from(*outputs_hash).to_hex()
    ))
}

/// Write an outputs-only blob.
///
/// The archive contains nothing but the task's output files. Per-task meta
/// lives in `entries/<input_key>` — see `shared/entry_meta.rs` and issue #278.
pub fn write_outputs_blob(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
    rel_output_paths: &[PathBuf],
    size_cap_bytes: u64,
) -> io::Result<BlobWriteResult> {
    let blob_path = paths.blobs_dir.join(format!(
        "{}.tar.zst",
        blake3::Hash::from(*outputs_hash).to_hex()
    ));

    write_file_blob(&blob_path, package_dir, rel_output_paths, size_cap_bytes)
}

pub(crate) fn write_file_blob(
    blob_path: &Path,
    package_dir: &Path,
    relative_paths: &[PathBuf],
    size_cap_bytes: u64,
) -> io::Result<BlobWriteResult> {
    if blob_path.exists() {
        return Ok(BlobWriteResult::AlreadyExists);
    }

    let existing_files =
        match collect_blob_source_files(package_dir, relative_paths, size_cap_bytes)? {
            Ok(files) => files,
            Err(bytes) => return Ok(BlobWriteResult::SkippedTooLarge { bytes }),
        };
    if existing_files.is_empty() {
        return Ok(BlobWriteResult::NoOutputs);
    }

    streaming_atomic_write(blob_path, |file| {
        let writer = BufWriter::new(file);
        let encoder = zstd::Encoder::new(writer, ZSTD_LEVEL)?;
        let mut tar = Builder::new(encoder);

        for source in &existing_files {
            let mut input = File::open(&source.absolute_path)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(source.metadata.len());
            header.set_mode(tar_entry_mode(&source.metadata));
            header.set_cksum();
            tar.append_data(&mut header, &source.relative_path, &mut input)?;
        }

        tar.finish()?;
        let encoder = tar.into_inner()?;
        let mut writer = encoder.finish()?;
        writer.flush()?;
        Ok(())
    })
    .map_err(io::Error::other)?;

    Ok(BlobWriteResult::Written)
}

struct BlobSourceFile {
    relative_path: PathBuf,
    absolute_path: PathBuf,
    metadata: Metadata,
}

fn collect_blob_source_files(
    package_dir: &Path,
    relative_paths: &[PathBuf],
    size_cap_bytes: u64,
) -> io::Result<Result<Vec<BlobSourceFile>, u64>> {
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    for relative_path in relative_paths {
        let absolute_path = package_dir.join(relative_path);
        let metadata = match fs::metadata(&absolute_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            continue;
        }
        total_bytes = total_bytes.saturating_add(metadata.len());
        if total_bytes > size_cap_bytes {
            return Ok(Err(total_bytes));
        }
        files.push(BlobSourceFile {
            relative_path: relative_path.clone(),
            absolute_path,
            metadata,
        });
    }
    Ok(Ok(files))
}

fn tar_entry_mode(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o777
    }

    #[cfg(not(unix))]
    {
        let _ = metadata;
        0o644
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    use super::*;
    use crate::shared::open_shared_paths;

    #[test]
    fn write_blob_writes_package_relative_tar_entries() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist/nested")).unwrap();
        fs::write(package_dir.join("dist/app.js"), b"console.log('hi');").unwrap();
        fs::write(package_dir.join("dist/nested/chunk.js"), b"chunk").unwrap();

        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();
        let outputs_hash = [7_u8; 32];
        let rel_paths = vec![
            PathBuf::from("dist/app.js"),
            PathBuf::from("dist/nested/chunk.js"),
        ];

        let result = write_outputs_blob(
            &paths,
            &outputs_hash,
            &package_dir,
            &rel_paths,
            1_024 * 1_024,
        )
        .unwrap();

        assert_eq!(result, BlobWriteResult::Written);

        let blob_path = blob_path(&paths, &outputs_hash);
        assert!(blob_path.exists());

        let entries = list_entries(&blob_path).unwrap();
        assert_eq!(entries, rel_paths);
        for entry in entries {
            assert!(entry.is_relative());
            assert!(!entry.to_string_lossy().starts_with('/'));
            assert!(!entry.starts_with(&package_dir));
        }
    }

    #[test]
    #[cfg(unix)]
    fn write_blob_preserves_executable_mode_in_tar_header() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let script_path = package_dir.join("bin/tool.sh");
        fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        fs::write(&script_path, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();
        let outputs_hash = [8_u8; 32];
        let rel_paths = vec![PathBuf::from("bin/tool.sh")];

        let result = write_outputs_blob(
            &paths,
            &outputs_hash,
            &package_dir,
            &rel_paths,
            1_024 * 1_024,
        )
        .unwrap();

        assert_eq!(result, BlobWriteResult::Written);

        let blob_path = blob_path(&paths, &outputs_hash);
        let entries = read_entry_summaries(&blob_path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, rel_paths[0]);
        assert_ne!(entries[0].1 & 0o111, 0);
    }

    #[test]
    fn write_blob_skips_when_outputs_exceed_size_cap() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        fs::write(package_dir.join("a.txt"), b"12345").unwrap();
        fs::write(package_dir.join("b.txt"), b"67890").unwrap();

        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();
        let outputs_hash = [9_u8; 32];
        let rel_paths = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];

        let result =
            write_outputs_blob(&paths, &outputs_hash, &package_dir, &rel_paths, 9).unwrap();

        assert_eq!(result, BlobWriteResult::SkippedTooLarge { bytes: 10 });
        assert_eq!(read_dir_paths(&paths.blobs_dir), Vec::<PathBuf>::new());
    }

    #[test]
    fn write_blob_deduplicates_existing_blob() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        fs::write(package_dir.join("out.txt"), b"first version").unwrap();

        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();
        let outputs_hash = [11_u8; 32];
        let rel_paths = vec![PathBuf::from("out.txt")];

        let first =
            write_outputs_blob(&paths, &outputs_hash, &package_dir, &rel_paths, 1_024).unwrap();
        assert_eq!(first, BlobWriteResult::Written);

        let blob_path = blob_path(&paths, &outputs_hash);
        let first_bytes = fs::read(&blob_path).unwrap();
        let first_mtime = fs::metadata(&blob_path).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(20));

        let second =
            write_outputs_blob(&paths, &outputs_hash, &package_dir, &rel_paths, 1_024).unwrap();
        assert_eq!(second, BlobWriteResult::AlreadyExists);
        assert_eq!(fs::read(&blob_path).unwrap(), first_bytes);
        assert_eq!(
            fs::metadata(&blob_path).unwrap().modified().unwrap(),
            first_mtime
        );
    }

    #[test]
    fn write_blob_returns_no_outputs_when_list_empty_or_missing() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();

        let empty = write_outputs_blob(&paths, &[13_u8; 32], &package_dir, &[], 1_024).unwrap();
        assert_eq!(empty, BlobWriteResult::NoOutputs);

        let missing = write_outputs_blob(
            &paths,
            &[14_u8; 32],
            &package_dir,
            &[PathBuf::from("missing.txt")],
            1_024,
        )
        .unwrap();
        assert_eq!(missing, BlobWriteResult::NoOutputs);
        assert_eq!(read_dir_paths(&paths.blobs_dir), Vec::<PathBuf>::new());
    }

    #[test]
    fn write_outputs_blob_creates_no_file_when_there_are_no_outputs() {
        let temp = tempdir().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let package_dir = temp.path().join("pkg");
        std::fs::create_dir_all(&package_dir).unwrap();

        let outputs_hash = crate::resolve::combined_outputs_hash(&[]);
        let result = write_outputs_blob(&paths, &outputs_hash, &package_dir, &[], 1_024).unwrap();

        assert_eq!(result, BlobWriteResult::NoOutputs);
        assert!(
            !blob_path(&paths, &outputs_hash).exists(),
            "no blob file should be created for an empty output set"
        );
    }

    #[test]
    fn write_outputs_blob_omits_meta_dir_from_archive() {
        let temp = tempdir().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let package_dir = temp.path().join("pkg");
        std::fs::create_dir_all(package_dir.join("dist")).unwrap();
        std::fs::write(package_dir.join("dist/main.js"), "console.log(1);").unwrap();

        let outputs_hash = [21_u8; 32];
        let result = write_outputs_blob(
            &paths,
            &outputs_hash,
            &package_dir,
            &[PathBuf::from("dist/main.js")],
            1_000_000,
        )
        .unwrap();
        assert_eq!(result, BlobWriteResult::Written);

        let entries = list_entries(&blob_path(&paths, &outputs_hash)).unwrap();
        assert_eq!(entries, vec![PathBuf::from("dist/main.js")]);
    }

    fn list_entries(blob_path: &Path) -> io::Result<Vec<PathBuf>> {
        let compressed = File::open(blob_path)?;
        let decoder = zstd::Decoder::new(compressed)?;
        let mut archive = tar::Archive::new(decoder);
        let mut entries = Vec::new();
        for entry in archive.entries()? {
            let entry = entry?;
            entries.push(entry.path()?.into_owned());
        }
        Ok(entries)
    }

    fn read_entry_summaries(blob_path: &Path) -> io::Result<Vec<(PathBuf, u32)>> {
        let compressed = File::open(blob_path)?;
        let decoder = zstd::Decoder::new(compressed)?;
        let mut archive = tar::Archive::new(decoder);
        let mut entries = Vec::new();
        for entry in archive.entries()? {
            let entry = entry?;
            let path = entry.path()?.into_owned();
            let mode = entry.header().mode()?;
            entries.push((path, mode));
        }
        Ok(entries)
    }

    #[test]
    fn restore_blob_round_trips_files_and_bytes() {
        let temp_dir = tempdir().unwrap();
        let source_package_dir = temp_dir.path().join("pkg-src");
        fs::create_dir_all(source_package_dir.join("dist/bin")).unwrap();
        fs::write(
            source_package_dir.join("dist/out.txt"),
            b"hello shared cache
",
        )
        .unwrap();
        fs::write(
            source_package_dir.join("dist/bin/tool.sh"),
            b"#!/bin/sh
echo hi
",
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            source_package_dir.join("dist/bin/tool.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();
        let outputs_hash = [21_u8; 32];
        let rel_paths = vec![
            PathBuf::from("dist/out.txt"),
            PathBuf::from("dist/bin/tool.sh"),
        ];

        let write_result = write_outputs_blob(
            &paths,
            &outputs_hash,
            &source_package_dir,
            &rel_paths,
            1_024 * 1_024,
        )
        .unwrap();
        assert_eq!(write_result, BlobWriteResult::Written);

        let restore_package_dir = temp_dir.path().join("pkg-restore");
        fs::create_dir_all(&restore_package_dir).unwrap();

        let restore_result = restore_blob(&paths, &outputs_hash, &restore_package_dir).unwrap();
        assert_eq!(restore_result, BlobReadResult::Restored);
        assert_eq!(
            fs::read(restore_package_dir.join("dist/out.txt")).unwrap(),
            fs::read(source_package_dir.join("dist/out.txt")).unwrap()
        );
        assert_eq!(
            fs::read(restore_package_dir.join("dist/bin/tool.sh")).unwrap(),
            fs::read(source_package_dir.join("dist/bin/tool.sh")).unwrap()
        );
        #[cfg(unix)]
        {
            let mode = fs::metadata(restore_package_dir.join("dist/bin/tool.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
        }
    }

    #[test]
    fn restore_blob_returns_missing_for_absent_blob() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();

        let result = restore_blob(&paths, &[31_u8; 32], &package_dir).unwrap();

        assert_eq!(result, BlobReadResult::Missing);
        assert_eq!(read_tree_paths(&package_dir), Vec::<PathBuf>::new());
    }

    #[test]
    fn restore_blob_returns_corrupt_without_partial_poisoning() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();
        let outputs_hash = [41_u8; 32];

        fs::write(blob_path(&paths, &outputs_hash), b"not a zstd tar blob").unwrap();

        let result = restore_blob(&paths, &outputs_hash, &package_dir).unwrap();

        assert_eq!(result, BlobReadResult::Corrupt);
        assert_eq!(read_tree_paths(&package_dir), Vec::<PathBuf>::new());
    }

    #[test]
    fn restore_blob_hard_fails_on_escape_entry() {
        let temp_dir = tempdir().unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let cache_dir = temp_dir.path().join("shared-cache");
        let paths = open_shared_paths(&cache_dir).unwrap();
        let outputs_hash = [51_u8; 32];

        write_malicious_blob(
            &blob_path(&paths, &outputs_hash),
            Path::new("../evil.txt"),
            b"owned",
        );

        let blob_path = blob_path(&paths, &outputs_hash);
        let archive_paths = list_entries(&blob_path).unwrap();
        assert_eq!(archive_paths, vec![PathBuf::from("../evil.txt")]);

        let error =
            restore_blob(&paths, &outputs_hash, &package_dir).expect_err("escape must error");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!temp_dir.path().join("evil.txt").exists());
        assert_eq!(read_tree_paths(&package_dir), Vec::<PathBuf>::new());
    }

    fn blob_path(paths: &SharedCachePaths, outputs_hash: &[u8; 32]) -> PathBuf {
        paths.blobs_dir.join(format!(
            "{}.tar.zst",
            blake3::Hash::from(*outputs_hash).to_hex()
        ))
    }

    fn write_malicious_blob(blob_path: &Path, entry_path: &Path, contents: &[u8]) {
        let file = File::create(blob_path).unwrap();
        let mut encoder = zstd::Encoder::new(file, ZSTD_LEVEL).unwrap();
        let mut tar_bytes = [0_u8; 2048];

        let path_bytes = entry_path.as_os_str().as_encoded_bytes();
        assert!(path_bytes.len() <= 100);
        tar_bytes[..path_bytes.len()].copy_from_slice(path_bytes);
        tar_bytes[100..108].copy_from_slice(b"0000644 ");
        tar_bytes[108..116].copy_from_slice(b"0000000 ");
        tar_bytes[116..124].copy_from_slice(b"0000000 ");
        let size = format!("{:011o} ", contents.len());
        tar_bytes[124..136].copy_from_slice(size.as_bytes());
        tar_bytes[136..148].copy_from_slice(b"00000000000 ");
        tar_bytes[148..156].fill(b' ');
        tar_bytes[156] = b'0';
        tar_bytes[257..263].copy_from_slice(b"ustar ");
        tar_bytes[263..265].copy_from_slice(b"00");
        let checksum: u32 = tar_bytes[..512].iter().map(|byte| u32::from(*byte)).sum();
        let checksum = format!("{:06o}  ", checksum);
        tar_bytes[148..156].copy_from_slice(checksum.as_bytes());

        let data_start = 512;
        tar_bytes[data_start..data_start + contents.len()].copy_from_slice(contents);

        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap();
    }

    fn read_tree_paths(dir: &Path) -> Vec<PathBuf> {
        let mut entries = walkdir::WalkDir::new(dir)
            .min_depth(1)
            .into_iter()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .strip_prefix(dir)
                    .unwrap()
                    .to_path_buf()
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
        let mut entries = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}

// === Meta file handling for SharedCache ===

use super::super::shared::{
    META_DIR_NAME, META_RECORD_FILE_NAME, META_STDERR_FILE_NAME, META_STDOUT_FILE_NAME,
};
use std::io::{Cursor, Read};

use crate::store::ReportInput;

/// Container for meta files extracted from a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaFiles {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub record: Vec<u8>,
    pub reports: Vec<ReportInput>,
}

/// Write blob with embedded meta files.
///
/// Creates a tar.zst archive containing:
/// - All output files from `rel_output_paths`
/// - `.luchta-meta/stdout.log`
/// - `.luchta-meta/stderr.log`
/// - `.luchta-meta/meta.bincode`
/// - `.luchta-meta/reports/<filename>`
pub fn write_blob_with_meta(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
    rel_output_paths: &[PathBuf],
    size_cap_bytes: u64,
    meta: &MetaFiles,
) -> io::Result<BlobWriteResult> {
    let blob_path = paths.blobs_dir.join(format!(
        "{}.tar.zst",
        blake3::Hash::from(*outputs_hash).to_hex()
    ));

    if blob_path.exists() {
        return Ok(BlobWriteResult::AlreadyExists);
    }

    let mut existing_files = Vec::new();
    let mut total_bytes = 0_u64;

    for rel_path in rel_output_paths {
        let absolute_path = package_dir.join(rel_path);
        let metadata = match fs::metadata(&absolute_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };

        if !metadata.is_file() {
            continue;
        }

        total_bytes = total_bytes.saturating_add(metadata.len());
        if total_bytes > size_cap_bytes {
            return Ok(BlobWriteResult::SkippedTooLarge { bytes: total_bytes });
        }

        existing_files.push((rel_path.clone(), absolute_path, metadata));
    }

    // Add meta file sizes to cap check (must happen BEFORE the empty check)
    let report_bytes: usize = meta.reports.iter().map(|report| report.content.len()).sum();
    let meta_bytes = meta.stdout.len() + meta.stderr.len() + meta.record.len() + report_bytes;
    total_bytes = total_bytes.saturating_add(meta_bytes as u64);
    if total_bytes > size_cap_bytes {
        return Ok(BlobWriteResult::SkippedTooLarge { bytes: total_bytes });
    }

    // Note: We don't return NoOutputs here - even 0 output files with meta is stored.
    // The write code handles empty outputs correctly.

    streaming_atomic_write(&blob_path, |file| {
        let writer = BufWriter::new(file);
        let encoder = zstd::Encoder::new(writer, ZSTD_LEVEL)?;
        let mut tar = Builder::new(encoder);

        // Write output files
        for (rel_path, absolute_path, metadata) in &existing_files {
            let mut input = File::open(absolute_path)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(metadata.len());
            header.set_mode(tar_entry_mode(metadata));
            header.set_cksum();
            tar.append_data(&mut header, rel_path, &mut input)?;
        }

        // Write meta files
        let meta_dir = PathBuf::from(META_DIR_NAME);

        // stdout.log
        if !meta.stdout.is_empty() {
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.stdout.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(
                &mut header,
                meta_dir.join(META_STDOUT_FILE_NAME),
                Cursor::new(&meta.stdout),
            )?;
        }

        // stderr.log
        if !meta.stderr.is_empty() {
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.stderr.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(
                &mut header,
                meta_dir.join(META_STDERR_FILE_NAME),
                Cursor::new(&meta.stderr),
            )?;
        }

        // meta.bincode
        if !meta.record.is_empty() {
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.record.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(
                &mut header,
                meta_dir.join(META_RECORD_FILE_NAME),
                Cursor::new(&meta.record),
            )?;
        }

        for report in &meta.reports {
            let mut header = tar::Header::new_gnu();
            header.set_size(report.content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(
                &mut header,
                meta_dir.join("reports").join(&report.filename),
                Cursor::new(report.content.as_bytes()),
            )?;
        }

        tar.finish()?;
        let encoder = tar.into_inner()?;
        let mut writer = encoder.finish()?;
        writer.flush()?;
        Ok(())
    })
    .map_err(io::Error::other)?;

    Ok(BlobWriteResult::Written)
}

/// Restore blob and extract meta files separately.
///
/// Returns `BlobReadResultWithMeta::Restored(StagedRestore)` on success.
/// The `StagedRestore` contains the meta files and a staging directory with
/// the output files, but does NOT move them into `package_dir` until
/// `commit()` is called. This allows validation before restoration.
pub fn restore_blob_with_meta(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
) -> io::Result<BlobReadResultWithMeta<StagedRestore>> {
    let blob_path = blob_path(paths, outputs_hash);
    restore_file_blob_staged(&blob_path, package_dir)
}

pub(crate) fn restore_file_blob_staged(
    blob_path: &Path,
    package_dir: &Path,
) -> io::Result<BlobReadResultWithMeta<StagedRestore>> {
    restore_file_blob_staged_with_size_cap(blob_path, package_dir, None)
}

pub(crate) fn restore_file_blob_staged_with_size_cap(
    blob_path: &Path,
    package_dir: &Path,
    size_cap_bytes: Option<u64>,
) -> io::Result<BlobReadResultWithMeta<StagedRestore>> {
    let compressed = match File::open(blob_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(BlobReadResultWithMeta::Missing)
        }
        Err(error) => return Err(error),
    };

    let staging_dir = tempfile::Builder::new()
        .prefix("blob-restore-meta-")
        .tempdir_in(package_dir)?;

    match extract_blob_with_meta_to_staging(
        compressed,
        package_dir,
        staging_dir.path(),
        size_cap_bytes,
    ) {
        Ok(meta) => Ok(BlobReadResultWithMeta::Restored(StagedRestore {
            meta,
            staging_dir,
            package_dir: package_dir.to_path_buf(),
        })),
        Err(RestoreError::Corrupt) => Ok(BlobReadResultWithMeta::Corrupt),
        Err(RestoreError::Io(error)) => Err(error),
    }
}

/// Restore an outputs-only blob into a staging directory.
///
/// Blobs written by older clients still carry a `.luchta-meta/` directory.
/// Its contents are ignored here — `entries/<input_key>` is authoritative —
/// and `move_non_meta_files` filters it out on commit.
pub fn restore_outputs_staged(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
) -> io::Result<BlobReadResultWithMeta<StagedRestore>> {
    restore_blob_with_meta(paths, outputs_hash, package_dir)
}

/// A staged restore that holds extracted files in a temp directory.
///
/// Call `commit()` to move files into the package directory after validation.
/// Call `discard()` to abandon this restore without modifying the package dir.
/// If neither is called, the staging directory is cleaned up when dropped.
#[derive(Debug)]
pub struct StagedRestore {
    pub meta: MetaFiles,
    staging_dir: TempDir,
    package_dir: PathBuf,
}

impl StagedRestore {
    /// A staged restore holding no files. Commits to an empty path list.
    pub fn empty(package_dir: &Path) -> io::Result<Self> {
        let staging_dir = tempfile::Builder::new()
            .prefix("blob-restore-empty-")
            .tempdir_in(package_dir)?;
        Ok(Self {
            meta: MetaFiles {
                stdout: Vec::new(),
                stderr: Vec::new(),
                record: Vec::new(),
                reports: Vec::new(),
            },
            staging_dir,
            package_dir: package_dir.to_path_buf(),
        })
    }

    /// Move all non-meta files from staging into the package directory.
    /// Returns absolute destination paths written into the package directory.
    /// After this call, the staging directory is cleaned up.
    pub fn commit(self) -> io::Result<Vec<PathBuf>> {
        let written_paths = match move_non_meta_files(self.staging_dir.path(), &self.package_dir) {
            Ok(written_paths) => written_paths,
            Err(error) => return Err(io::Error::new(error.source.kind(), error.source)),
        };
        self.staging_dir.close()?;
        Ok(written_paths)
    }

    /// Discard this restore without modifying the package directory.
    /// The staging directory is cleaned up.
    pub fn discard(self) -> io::Result<()> {
        self.staging_dir.close()
    }

    pub(crate) fn relative_file_paths(&self) -> io::Result<Vec<PathBuf>> {
        let mut paths = walkdir::WalkDir::new(self.staging_dir.path())
            .min_depth(1)
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_type().is_file() => Some(Ok(entry)),
                Ok(_) => None,
                Err(error) => Some(Err(io::Error::other(error))),
            })
            .map(|entry| {
                let entry = entry?;
                entry
                    .path()
                    .strip_prefix(self.staging_dir.path())
                    .map(Path::to_path_buf)
                    .map_err(io::Error::other)
            })
            .collect::<io::Result<Vec<_>>>()?;
        paths.sort_unstable();
        Ok(paths)
    }

    pub(crate) fn cache_files_hash(&self) -> io::Result<[u8; 32]> {
        let entries = self
            .relative_file_paths()?
            .into_iter()
            .map(|path| {
                let absolute = self.staging_dir.path().join(&path);
                let metadata = fs::metadata(&absolute)?;
                Ok(crate::FileEntry {
                    path: path.to_string_lossy().replace('\\', "/"),
                    size: metadata.len(),
                    mtime_ns: 0,
                    hash: crate::blake3_file(&absolute)
                        .map_err(|error| io::Error::other(error.to_string()))?,
                    absent: false,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(crate::combined_cache_files_hash(&entries))
    }

    pub(crate) fn contains_package_relative_path(&self, path: &Path) -> bool {
        self.package_dir
            .join(path)
            .starts_with(self.staging_dir.path())
    }
}

/// Generic BlobReadResult that can carry a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobReadResultWithMeta<T = ()> {
    Restored(T),
    Missing,
    Corrupt,
}

impl Default for BlobReadResultWithMeta {
    fn default() -> Self {
        BlobReadResultWithMeta::Restored(())
    }
}

#[derive(Default)]
struct ExtractedMetaFiles {
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    record: Option<Vec<u8>>,
    reports: Vec<ReportInput>,
}

impl ExtractedMetaFiles {
    fn capture<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        entry_path: &Path,
    ) -> Result<bool, RestoreError> {
        let path = entry_path.to_string_lossy();
        if !path.starts_with(&format!("{META_DIR_NAME}/")) {
            return Ok(false);
        }

        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .map_err(|_| RestoreError::Corrupt)?;
        if path.starts_with(&format!("{META_DIR_NAME}/reports/")) {
            self.capture_report(entry_path, contents)?;
            return Ok(true);
        }

        let file_name = entry_path
            .file_name()
            .ok_or(RestoreError::Corrupt)?
            .to_string_lossy();
        match file_name.as_ref() {
            META_STDOUT_FILE_NAME => self.stdout = Some(contents),
            META_STDERR_FILE_NAME => self.stderr = Some(contents),
            META_RECORD_FILE_NAME => self.record = Some(contents),
            _ => {}
        }
        Ok(true)
    }

    fn capture_report(&mut self, entry_path: &Path, contents: Vec<u8>) -> Result<(), RestoreError> {
        let file_name = entry_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(RestoreError::Corrupt)?;
        if !crate::store::is_valid_report_filename(file_name) {
            return Err(RestoreError::Corrupt);
        }
        self.reports.push(ReportInput {
            filename: file_name.to_owned(),
            mime_type: String::new(),
            content: String::from_utf8_lossy(&contents).into_owned(),
        });
        Ok(())
    }

    fn into_meta_files(self) -> MetaFiles {
        MetaFiles {
            stdout: self.stdout.unwrap_or_default(),
            stderr: self.stderr.unwrap_or_default(),
            record: self.record.unwrap_or_default(),
            reports: self.reports,
        }
    }
}

fn extract_blob_with_meta_to_staging(
    compressed: File,
    package_dir: &Path,
    staging_dir: &Path,
    size_cap_bytes: Option<u64>,
) -> Result<MetaFiles, RestoreError> {
    let decoder = zstd::Decoder::new(compressed).map_err(|_| RestoreError::Corrupt)?;
    let mut archive = Archive::new(decoder);
    let entries = archive.entries().map_err(|_| RestoreError::Corrupt)?;

    let mut meta = ExtractedMetaFiles::default();
    let mut output_bytes = 0_u64;

    for entry in entries {
        let mut entry = entry.map_err(|_| RestoreError::Corrupt)?;
        let entry_path = entry.path().map_err(|_| RestoreError::Corrupt)?;
        let entry_path = entry_path.into_owned();

        if meta.capture(&mut entry, &entry_path)? {
            continue;
        }

        // Regular output file
        let relative_path = validate_entry_path(package_dir, &entry_path)?;
        let target_path = staging_dir.join(&relative_path);

        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target_path)?;
            continue;
        }

        if !entry.header().entry_type().is_file() {
            return Err(RestoreError::Corrupt);
        }

        let entry_size = entry.header().size().map_err(|_| RestoreError::Corrupt)?;
        add_extracted_output_bytes(&mut output_bytes, entry_size, size_cap_bytes)?;

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut output = File::create(&target_path)?;
        io::copy(&mut entry, &mut output).map_err(|_| RestoreError::Corrupt)?;

        #[cfg(unix)]
        {
            let mode = entry.header().mode().map_err(|_| RestoreError::Corrupt)?;
            fs::set_permissions(&target_path, fs::Permissions::from_mode(mode))?;
        }
    }

    // Outputs-only blobs (the current format, see write_outputs_blob) carry no
    // .luchta-meta at all — that is not corruption, just an entry whose record,
    // stdout, and stderr live in entries/<input_key> instead.
    Ok(meta.into_meta_files())
}

fn add_extracted_output_bytes(
    total_bytes: &mut u64,
    entry_bytes: u64,
    size_cap_bytes: Option<u64>,
) -> Result<(), RestoreError> {
    *total_bytes = total_bytes.saturating_add(entry_bytes);
    if size_cap_bytes.is_some_and(|cap| *total_bytes > cap) {
        return Err(RestoreError::Corrupt);
    }
    Ok(())
}

struct MoveOutputsError {
    source: io::Error,
    written_paths: Vec<PathBuf>,
}

impl std::fmt::Debug for MoveOutputsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoveOutputsError")
            .field("kind", &self.source.kind())
            .field("written_paths", &self.written_paths)
            .finish()
    }
}

impl From<io::Error> for MoveOutputsError {
    fn from(source: io::Error) -> Self {
        Self {
            source,
            written_paths: Vec::new(),
        }
    }
}

fn move_non_meta_files(from_dir: &Path, to_dir: &Path) -> Result<Vec<PathBuf>, MoveOutputsError> {
    let mut written_paths = Vec::new();

    for entry in fs::read_dir(from_dir).map_err(MoveOutputsError::from)? {
        let entry = entry.map_err(MoveOutputsError::from)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip .luchta-meta directory
        if name_str == META_DIR_NAME {
            continue;
        }

        let source_path = entry.path();
        let target_path = to_dir.join(&name);
        let file_type = entry.file_type().map_err(MoveOutputsError::from)?;

        if file_type.is_dir() {
            fs::create_dir_all(&target_path).map_err(|source| MoveOutputsError {
                source,
                written_paths: written_paths.clone(),
            })?;
            match move_non_meta_files(&source_path, &target_path) {
                Ok(child_paths) => written_paths.extend(child_paths),
                Err(mut error) => {
                    written_paths.append(&mut error.written_paths);
                    return Err(MoveOutputsError {
                        source: error.source,
                        written_paths,
                    });
                }
            }
            fs::remove_dir(&source_path).map_err(|source| MoveOutputsError {
                source,
                written_paths: written_paths.clone(),
            })?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|source| MoveOutputsError {
                source,
                written_paths: written_paths.clone(),
            })?;
        }

        fs::rename(&source_path, &target_path).map_err(|source| MoveOutputsError {
            source,
            written_paths: written_paths.clone(),
        })?;
        written_paths.push(target_path);
    }

    Ok(written_paths)
}

#[cfg(test)]
mod restore_tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn move_non_meta_files_returns_written_destination_paths() {
        let temp_dir = tempdir().expect("create temp dir");
        let staging_dir = temp_dir.path().join("staging");
        let package_dir = temp_dir.path().join("package");
        fs::create_dir_all(staging_dir.join("dist/nested")).expect("create staging dirs");
        fs::create_dir_all(staging_dir.join(META_DIR_NAME)).expect("create meta dir");
        fs::create_dir_all(&package_dir).expect("create package dir");
        fs::write(staging_dir.join("dist/app.js"), "app").expect("write app output");
        fs::write(staging_dir.join("dist/nested/chunk.js"), "chunk").expect("write chunk output");
        fs::write(staging_dir.join(META_DIR_NAME).join("stdout.log"), "meta")
            .expect("write meta file");

        let mut written_paths =
            move_non_meta_files(&staging_dir, &package_dir).expect("move outputs");
        written_paths.sort();

        let mut expected = vec![
            package_dir.join("dist/app.js"),
            package_dir.join("dist/nested/chunk.js"),
        ];
        expected.sort();

        assert_eq!(written_paths, expected);
        assert!(package_dir.join("dist/app.js").exists());
        assert!(package_dir.join("dist/nested/chunk.js").exists());
        assert!(staging_dir.join(META_DIR_NAME).join("stdout.log").exists());
    }

    #[test]
    fn move_non_meta_files_error_preserves_already_written_paths() {
        let temp_dir = tempdir().expect("create temp dir");
        let staging_dir = temp_dir.path().join("staging");
        let package_dir = temp_dir.path().join("package");
        fs::create_dir_all(staging_dir.join("a-dir")).expect("create staging dirs");
        fs::create_dir_all(staging_dir.join(META_DIR_NAME)).expect("create meta dir");
        fs::create_dir_all(&package_dir).expect("create package dir");
        fs::write(package_dir.join("a-dir"), "conflict").expect("create conflicting file");
        fs::write(staging_dir.join("a-file.js"), "app").expect("write app output");
        fs::write(staging_dir.join("a-dir/conflict.js"), "bad").expect("write conflicting output");

        let error = move_non_meta_files(&staging_dir, &package_dir).expect_err("move should fail");

        assert_eq!(error.source.kind(), io::ErrorKind::AlreadyExists);

        // `fs::read_dir` traversal order is unspecified, so `a-file.js` may or may
        // not have been moved before the conflicting `a-dir` entry failed. Assert
        // order-independently: the only file that can ever be reported is
        // `a-file.js`, and its presence in `written_paths` must exactly match its
        // presence on disk (only successfully-moved files are recorded).
        let a_file = package_dir.join("a-file.js");
        for written in &error.written_paths {
            assert_eq!(
                written, &a_file,
                "unexpected written path recorded: {written:?}"
            );
        }
        assert_eq!(
            error.written_paths.contains(&a_file),
            a_file.exists(),
            "written_paths must record exactly the files moved into the package dir"
        );
    }
}
