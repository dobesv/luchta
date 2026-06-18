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

pub fn write_blob(
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

    if existing_files.is_empty() {
        return Ok(BlobWriteResult::NoOutputs);
    }

    streaming_atomic_write(&blob_path, |file| {
        let writer = BufWriter::new(file);
        let encoder = zstd::Encoder::new(writer, ZSTD_LEVEL)?;
        let mut tar = Builder::new(encoder);

        for (rel_path, absolute_path, metadata) in &existing_files {
            let mut input = File::open(absolute_path)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(metadata.len());
            header.set_mode(tar_entry_mode(metadata));
            header.set_cksum();
            tar.append_data(&mut header, rel_path, &mut input)?;
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

        let result = write_blob(
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

        let result = write_blob(
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

        let result = write_blob(&paths, &outputs_hash, &package_dir, &rel_paths, 9).unwrap();

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

        let first = write_blob(&paths, &outputs_hash, &package_dir, &rel_paths, 1_024).unwrap();
        assert_eq!(first, BlobWriteResult::Written);

        let blob_path = blob_path(&paths, &outputs_hash);
        let first_bytes = fs::read(&blob_path).unwrap();
        let first_mtime = fs::metadata(&blob_path).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(20));

        let second = write_blob(&paths, &outputs_hash, &package_dir, &rel_paths, 1_024).unwrap();
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

        let empty = write_blob(&paths, &[13_u8; 32], &package_dir, &[], 1_024).unwrap();
        assert_eq!(empty, BlobWriteResult::NoOutputs);

        let missing = write_blob(
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

        let write_result = write_blob(
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

/// Container for meta files extracted from a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaFiles {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub record: Vec<u8>,
}

/// Write blob with embedded meta files.
///
/// Creates a tar.zst archive containing:
/// - All output files from `rel_output_paths`
/// - `.luchta-meta/stdout.log`
/// - `.luchta-meta/stderr.log`
/// - `.luchta-meta/meta.bincode`
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
    let meta_bytes = meta.stdout.len() + meta.stderr.len() + meta.record.len();
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
    let compressed = match File::open(&blob_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(BlobReadResultWithMeta::Missing)
        }
        Err(error) => return Err(error),
    };

    let staging_dir = tempfile::Builder::new()
        .prefix("blob-restore-meta-")
        .tempdir_in(package_dir)?;

    match extract_blob_with_meta_to_staging(compressed, package_dir, staging_dir.path()) {
        Ok(Some(meta)) => Ok(BlobReadResultWithMeta::Restored(StagedRestore {
            meta,
            staging_dir,
            package_dir: package_dir.to_path_buf(),
        })),
        Ok(None) => Ok(BlobReadResultWithMeta::Corrupt),
        Err(RestoreError::Corrupt) => Ok(BlobReadResultWithMeta::Corrupt),
        Err(RestoreError::Io(error)) => Err(error),
    }
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
    /// Move all non-meta files from staging into the package directory.
    /// After this call, the staging directory is cleaned up.
    pub fn commit(self) -> io::Result<()> {
        move_non_meta_files(self.staging_dir.path(), &self.package_dir)?;
        self.staging_dir.close()?;
        Ok(())
    }

    /// Discard this restore without modifying the package directory.
    /// The staging directory is cleaned up.
    pub fn discard(self) -> io::Result<()> {
        self.staging_dir.close()
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

fn extract_blob_with_meta_to_staging(
    compressed: File,
    package_dir: &Path,
    staging_dir: &Path,
) -> Result<Option<MetaFiles>, RestoreError> {
    let decoder = zstd::Decoder::new(compressed).map_err(|_| RestoreError::Corrupt)?;
    let mut archive = Archive::new(decoder);
    let entries = archive.entries().map_err(|_| RestoreError::Corrupt)?;

    let mut meta_stdout = None;
    let mut meta_stderr = None;
    let mut meta_record = None;

    for entry in entries {
        let mut entry = entry.map_err(|_| RestoreError::Corrupt)?;
        let entry_path = entry.path().map_err(|_| RestoreError::Corrupt)?;
        let entry_path = entry_path.into_owned();

        // Check for meta files
        let path_str = entry_path.to_string_lossy();
        let meta_prefix = format!("{}/", META_DIR_NAME);

        if path_str.starts_with(&meta_prefix) {
            let file_name = entry_path
                .file_name()
                .ok_or(RestoreError::Corrupt)?
                .to_string_lossy();

            // Extract meta file contents into memory
            let mut contents = Vec::new();
            entry
                .read_to_end(&mut contents)
                .map_err(|_| RestoreError::Corrupt)?;

            match file_name.as_ref() {
                META_STDOUT_FILE_NAME => meta_stdout = Some(contents),
                META_STDERR_FILE_NAME => meta_stderr = Some(contents),
                META_RECORD_FILE_NAME => meta_record = Some(contents),
                _ => {} // Unknown meta file, ignore
            }
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

    // Meta files are optional - we only need at least one present
    // If no meta files, return Corrupt
    if meta_stdout.is_none() && meta_stderr.is_none() && meta_record.is_none() {
        // No meta files found, blob may be from an older version
        // Return None to indicate corrupt/missing meta
        return Ok(None);
    }

    Ok(Some(MetaFiles {
        stdout: meta_stdout.unwrap_or_default(),
        stderr: meta_stderr.unwrap_or_default(),
        record: meta_record.unwrap_or_default(),
    }))
}

fn move_non_meta_files(from_dir: &Path, to_dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip .luchta-meta directory
        if name_str == META_DIR_NAME {
            continue;
        }

        let source_path = entry.path();
        let target_path = to_dir.join(&name);
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            fs::create_dir_all(&target_path)?;
            move_non_meta_files(&source_path, &target_path)?;
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
