use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::{RemoteFileDownload, RemoteFileUpload, RemoteSync};
use crate::shared::{
    blob_path, cache_file_blob_path, hex_hash, rclone, SharedCachePaths, CACHE_FILES_DIR_NAME,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteBlobRole {
    Outputs,
    CacheFiles,
}

struct RemoteBlobArtifact {
    remote_fs: String,
    file_name: String,
    local_path: PathBuf,
    role: RemoteBlobRole,
}

#[derive(Debug)]
pub(crate) struct OwnedCacheFileBlob {
    pub(crate) paths: Arc<SharedCachePaths>,
    pub(crate) state_hash: [u8; 32],
}

impl RemoteSync {
    pub(crate) fn enqueue_cache_file_blob(&self, push: OwnedCacheFileBlob) {
        self.enqueue_push(super::PushMsg::CacheFileBlob(push));
    }

    fn cache_files_fs(&self) -> String {
        format!(
            "{}/{CACHE_FILES_DIR_NAME}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }

    pub(crate) fn pull_blob(
        &self,
        paths: &SharedCachePaths,
        outputs_hash: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        self.pull_content_blob(self.remote_blob_artifact(
            paths,
            outputs_hash,
            RemoteBlobRole::Outputs,
        ))
    }

    pub(crate) fn pull_cache_file_blob(
        &self,
        paths: &SharedCachePaths,
        state_hash: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        self.pull_content_blob(self.remote_blob_artifact(
            paths,
            state_hash,
            RemoteBlobRole::CacheFiles,
        ))
    }

    fn pull_content_blob(&self, artifact: RemoteBlobArtifact) -> Result<(), rclone::RcloneError> {
        if self.is_disabled() || artifact.local_path.exists() {
            return Ok(());
        }
        self.record_blob_get(artifact.role);
        let Some((result, elapsed)) = self.remote_operation(|timeout| {
            self.copy_remote_file_down(RemoteFileDownload {
                src_fs: &artifact.remote_fs,
                src_remote: &artifact.file_name,
                local_path: &artifact.local_path,
                timeout,
            })
        }) else {
            return Ok(());
        };
        self.state
            .stats
            .download_latency_ms
            .fetch_add(elapsed.as_millis() as u64, Ordering::AcqRel);
        match result {
            Ok(()) => {
                self.record_remote_success();
                self.state.stats.download_bytes.fetch_add(
                    fs::metadata(&artifact.local_path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                    Ordering::AcqRel,
                );
                Ok(())
            }
            Err(error) => {
                self.record_remote_error(&error);
                Err(error)
            }
        }
    }

    fn record_blob_get(&self, role: RemoteBlobRole) {
        let counter = match role {
            RemoteBlobRole::Outputs => &self.state.stats.blob_gets,
            RemoteBlobRole::CacheFiles => &self.state.stats.cache_file_blob_gets,
        };
        counter.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn push_blob_if_missing(&self, paths: &SharedCachePaths, outputs_hash: &[u8; 32]) {
        self.push_content_blob_if_missing(self.remote_blob_artifact(
            paths,
            outputs_hash,
            RemoteBlobRole::Outputs,
        ));
    }

    pub(super) fn push_cache_file_blob_if_missing(
        &self,
        paths: &SharedCachePaths,
        state_hash: &[u8; 32],
    ) {
        self.push_content_blob_if_missing(self.remote_blob_artifact(
            paths,
            state_hash,
            RemoteBlobRole::CacheFiles,
        ));
    }

    fn push_content_blob_if_missing(&self, artifact: RemoteBlobArtifact) {
        let Some((preflight, _)) = self.remote_operation(|timeout| {
            self.rclone
                .stat(&artifact.remote_fs, &artifact.file_name, timeout)
        }) else {
            return;
        };
        match preflight {
            Ok(Some(_)) => {
                self.record_remote_success();
                return;
            }
            Ok(None) => self.record_remote_success(),
            Err(error) => {
                self.record_remote_error(&error);
                eprintln!(
                    "warn: shared cache upload preflight failed for blob={}: {error}",
                    artifact.file_name
                );
                return;
            }
        }

        let bytes = fs::metadata(&artifact.local_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let Some((result, elapsed)) = self.remote_operation(|timeout| {
            self.copy_local_file_up(RemoteFileUpload {
                local_path: &artifact.local_path,
                dst_fs: &artifact.remote_fs,
                dst_remote: &artifact.file_name,
                timeout,
            })
        }) else {
            return;
        };
        self.record_upload_stats(bytes, elapsed, result.is_ok());
        if let Err(error) = result {
            self.record_remote_error(&error);
            eprintln!(
                "warn: shared cache artifact upload failed for blob={}: {error}",
                artifact.file_name
            );
        } else {
            self.record_blob_upload(artifact.role);
            self.record_remote_success();
        }
    }

    fn record_blob_upload(&self, role: RemoteBlobRole) {
        if role == RemoteBlobRole::CacheFiles {
            self.state
                .stats
                .cache_file_blob_uploads
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    fn remote_blob_artifact(
        &self,
        paths: &SharedCachePaths,
        hash: &[u8; 32],
        role: RemoteBlobRole,
    ) -> RemoteBlobArtifact {
        let file_name = format!("{}.tar.zst", hex_hash(*hash));
        let (remote_fs, local_path) = match role {
            RemoteBlobRole::Outputs => (self.blobs_fs(), blob_path(paths, hash)),
            RemoteBlobRole::CacheFiles => {
                (self.cache_files_fs(), cache_file_blob_path(paths, hash))
            }
        };
        RemoteBlobArtifact {
            remote_fs,
            file_name,
            local_path,
            role,
        }
    }
}
