use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Result;

const TMP_SUFFIX: &str = ".tmp";
static TMP_PATH_NONCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    streaming_atomic_write(path, |file| {
        io::Write::write_all(file, contents)?;
        Ok(())
    })
}

pub(crate) fn streaming_atomic_write<F>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    let tmp_path = tmp_path(path);
    let mut file = File::create(&tmp_path)?;
    let write_result = write(&mut file);
    if let Err(err) = write_result {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp_path, path)?;
    sync_parent_dir(path)?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .expect("cache files always have file names")
        .to_string_lossy();
    let pid = std::process::id();
    let nonce = TMP_PATH_NONCE.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!("{name}.{pid}.{nonce}{TMP_SUFFIX}"))
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn atomic_write_writes_target_contents_without_leftover_tmp_files() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("snapshot.bin");

        atomic_write(&path, b"shared-cache-data").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"shared-cache-data");
        assert_eq!(tmp_files_in(temp_dir.path()), Vec::<PathBuf>::new());
    }

    #[test]
    fn atomic_write_replaces_target_without_visible_partial_contents() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("blob.bin");
        let old = vec![b'a'; 32 * 1024];
        let new = vec![b'b'; 32 * 1024];
        fs::write(&path, &old).unwrap();

        let start = Arc::new(Barrier::new(2));
        let done = Arc::new(Barrier::new(2));
        let writer_path = path.clone();
        let writer_new = new.clone();
        let writer_start = Arc::clone(&start);
        let writer_done = Arc::clone(&done);

        let writer = thread::spawn(move || {
            writer_start.wait();
            atomic_write(&writer_path, &writer_new).unwrap();
            writer_done.wait();
        });

        start.wait();
        for _ in 0..2_000 {
            let observed = fs::read(&path).unwrap();
            assert!(observed == old || observed == new);
        }
        done.wait();
        writer.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), new);
    }

    #[test]
    fn streaming_atomic_write_removes_tmp_file_when_writer_fails() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("broken.bin");

        let err = streaming_atomic_write(&path, |_file| Err(io::Error::other("boom"))).unwrap_err();

        assert_eq!(err.to_string(), "failed to access cache filesystem: boom");
        assert!(!path.exists());
        assert_eq!(tmp_files_in(temp_dir.path()), Vec::<PathBuf>::new());
    }

    fn tmp_files_in(dir: &Path) -> Vec<PathBuf> {
        let mut paths = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(TMP_SUFFIX))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }
}
