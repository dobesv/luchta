use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::{input_key_hex, Snapshot};

const CACHE_FILE_SCOPE_DOMAIN: &[u8] = b"luchta:cache-file-scope:v1";
const MAX_OBSERVATIONS_PER_SCOPE: usize = 8;

/// One immutable advisory cache-file state observed after a successful task.
/// `state_blob_hash = None` is a tombstone: the qualifying run produced no
/// matching files, so an older state must not be resurrected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFileObservation {
    pub scope_hash: [u8; 32],
    pub upstream_outputs_hash: [u8; 32],
    pub state_blob_hash: Option<[u8; 32]>,
    pub state_bytes: u64,
    pub cached_at_unix_ms: u64,
}

pub(super) fn merge_cache_file_observations(
    snapshot: &mut Snapshot,
    observations: Vec<CacheFileObservation>,
) -> bool {
    let mut changed = false;
    for observation in observations {
        let key = input_key_hex(observation.scope_hash);
        let candidates = snapshot.cache_files.entry(key).or_default();

        if let Some(existing) = candidates.iter_mut().find(|existing| {
            existing.upstream_outputs_hash == observation.upstream_outputs_hash
                && existing.state_blob_hash == observation.state_blob_hash
        }) {
            if observation.cached_at_unix_ms > existing.cached_at_unix_ms {
                *existing = observation;
                changed = true;
            }
        } else {
            candidates.push(observation);
            changed = true;
        }

        candidates.sort_unstable_by(observation_newest_first);
        if candidates.len() > MAX_OBSERVATIONS_PER_SCOPE {
            candidates.truncate(MAX_OBSERVATIONS_PER_SCOPE);
            changed = true;
        }
    }
    changed
}

fn observation_newest_first(left: &CacheFileObservation, right: &CacheFileObservation) -> Ordering {
    right
        .cached_at_unix_ms
        .cmp(&left.cached_at_unix_ms)
        .then_with(|| right.upstream_outputs_hash.cmp(&left.upstream_outputs_hash))
        .then_with(|| {
            right
                .state_blob_hash
                .is_none()
                .cmp(&left.state_blob_hash.is_none())
        })
        .then_with(|| right.state_blob_hash.cmp(&left.state_blob_hash))
}

/// Coarse advisory-state scope. It deliberately excludes resolved task input
/// contents and upstream task outputs so a changed source state can still
/// receive useful warm state. Declared task inputs remain represented through
/// `task_spec_hash`, as do `cacheFiles` and the resolved nonce.
#[must_use]
pub fn derive_cache_file_scope(
    task_id: &str,
    task_spec_hash: [u8; 32],
    env_hash: [u8; 32],
    pkg_dep_hash: [u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CACHE_FILE_SCOPE_DOMAIN);
    hasher.update(&(task_id.len() as u64).to_le_bytes());
    hasher.update(task_id.as_bytes());
    hasher.update(&task_spec_hash);
    hasher.update(&env_hash);
    hasher.update(&pkg_dep_hash);
    *hasher.finalize().as_bytes()
}

/// Select exactly one cache-file observation. A matching upstream-output
/// state outranks every mismatch; within either class newest wins, followed by
/// deterministic hashes. A selected tombstone is returned as-is so callers
/// run cold without falling back to older candidates.
#[must_use]
pub fn select_cache_file_observation(
    candidates: &[CacheFileObservation],
    current_upstream_outputs_hash: [u8; 32],
) -> Option<&CacheFileObservation> {
    candidates.iter().max_by(|left, right| {
        let left_matches = left.upstream_outputs_hash == current_upstream_outputs_hash;
        let right_matches = right.upstream_outputs_hash == current_upstream_outputs_hash;
        left_matches
            .cmp(&right_matches)
            .then_with(|| left.cached_at_unix_ms.cmp(&right.cached_at_unix_ms))
            .then_with(|| left.upstream_outputs_hash.cmp(&right.upstream_outputs_hash))
            .then_with(|| {
                left.state_blob_hash
                    .is_none()
                    .cmp(&right.state_blob_hash.is_none())
            })
            .then_with(|| left.state_blob_hash.cmp(&right.state_blob_hash))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_changes_with_each_coarse_component() {
        let baseline = derive_cache_file_scope("pkg#lint", [1; 32], [2; 32], [3; 32]);
        let changed = [
            derive_cache_file_scope("other#lint", [1; 32], [2; 32], [3; 32]),
            derive_cache_file_scope("pkg#lint", [9; 32], [2; 32], [3; 32]),
            derive_cache_file_scope("pkg#lint", [1; 32], [9; 32], [3; 32]),
            derive_cache_file_scope("pkg#lint", [1; 32], [2; 32], [9; 32]),
        ];

        for candidate in changed {
            assert_ne!(baseline, candidate);
        }
    }

    #[test]
    fn ranking_prefers_upstream_match_then_recency() {
        let candidates = observations_with_newer_mismatch();

        assert_eq!(
            select_cache_file_observation(&candidates, [4; 32])
                .unwrap()
                .state_blob_hash,
            Some([1; 32]),
            "an older upstream match outranks a newer mismatch"
        );
        assert_eq!(
            select_cache_file_observation(&candidates, [9; 32])
                .unwrap()
                .state_blob_hash,
            Some([2; 32]),
            "without a match the newest candidate wins"
        );
    }

    #[test]
    fn newest_tombstone_prevents_resurrection() {
        let upstream_outputs_hash = [4; 32];
        let mut candidates = observations_with_newer_mismatch();
        candidates[1].upstream_outputs_hash = upstream_outputs_hash;
        candidates[1].state_blob_hash = None;

        assert!(
            select_cache_file_observation(&candidates, upstream_outputs_hash)
                .unwrap()
                .state_blob_hash
                .is_none()
        );
    }

    fn observations_with_newer_mismatch() -> Vec<CacheFileObservation> {
        vec![
            CacheFileObservation {
                scope_hash: [1; 32],
                upstream_outputs_hash: [4; 32],
                state_blob_hash: Some([1; 32]),
                state_bytes: 10,
                cached_at_unix_ms: 100,
            },
            CacheFileObservation {
                scope_hash: [1; 32],
                upstream_outputs_hash: [8; 32],
                state_blob_hash: Some([2; 32]),
                state_bytes: 20,
                cached_at_unix_ms: 200,
            },
        ]
    }
}
