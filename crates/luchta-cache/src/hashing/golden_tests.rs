//! Build expected bytes from the wire layout, not by running a second Serde encoder.
//! Postcard uses unsigned varints for lengths, `u32` fields and enum indices;
//! string lengths count UTF-8 bytes. `DependsOn` serializes as a config string.

use super::tests::sample_task_definition;
use super::{env_bytes, env_hash, pkg_dep_bytes, pkg_dep_hash, task_spec_bytes, task_spec_hash};
use crate::serialization::encode;
use luchta_types::{EnvSpec, TaskDefinition};
use std::collections::BTreeMap;

// In field order: command and worker (Some), weight (300), dependency strings,
// cache enabled, input and output lists. The nonce tag/value follows this prefix.
const TASK_PREFIX: &[u8] = b"\x01\x0epnpm run build\x01\x04node\xac\x02\
    \x05\x06^build\x07^^check\x04lint\x07ui#lint\x08#prepare\
    \x01\x02\x0bsrc/**/*.ts\x0cpackage.json\x01\x07dist/**";
const NONCE: &str = "global=ci";

fn golden_task() -> TaskDefinition {
    let mut task = sample_task_definition();
    task.weight = 300;
    task.depends_on = ["^build", "^^check", "lint", "ui#lint", "#prepare"]
        .into_iter()
        .map(|dependency| dependency.parse().unwrap())
        .collect();
    task
}

#[test]
fn task_spec_bytes_and_digests_match_postcard_goldens() {
    let task = golden_task();
    let cases = [
        (
            None,
            &b"\x00"[..],
            "55cb90106a342295546846cd5122ad8753a55f86c9ee66e8f3e83268f0253128",
        ),
        (
            Some(NONCE),
            &b"\x01\x09global=ci"[..],
            "65433a7fadc39ce167824f153e38f8a2f5ffccf23524e58ee933a38265514452",
        ),
    ];
    for (nonce, suffix, digest) in cases {
        let expected = [TASK_PREFIX, suffix].concat();
        for _ in 0..2 {
            assert_eq!(task_spec_bytes(&task, nonce).unwrap(), expected);
            assert_digest(task_spec_hash(&task, nonce), digest);
        }
    }
}

#[test]
fn task_spec_extension_digests_pin_existing_framing() {
    let mut task = golden_task();
    task.cache_files = vec![".eslintcache".into(), ".babel-cache/**".into()];
    assert_eq!(
        encode(&task.cache_files).unwrap(),
        b"\x02\x0c.eslintcache\x0f.babel-cache/**"
    );
    // Each extension tag is literal ASCII. The cache-files tag precedes the
    // base bytes and cache-file bytes, each with a u64 little-endian length.
    // The tool-version tag and length-prefixed UTF-8 bytes follow that preimage
    // (or the unframed base bytes when there are no cache files).
    assert_digest(
        task_spec_hash(&task, Some(NONCE)),
        "08c81e13c1a1d46f13510bdd163a25d121ccd6cb1d6d3a98fd47176f9717fb28",
    );
    task.tool_version = Some("swc_core=80.0.0".into());
    assert_digest(
        task_spec_hash(&task, Some(NONCE)),
        "b4b52900ea8ae3640dff7f0aa227e98edb14226b667c33ee5ee1d79adb0f27f7",
    );
    task.cache_files.clear();
    assert_digest(
        task_spec_hash(&task, Some(NONCE)),
        "b4ac93f666cf418f6460d4ad185b2afda1058822a1be38b4c88b4dc03064dc6d",
    );
}

#[test]
fn env_bytes_and_digest_are_golden_regardless_of_insertion_order() {
    let entries = [
        ("ABSENT", env_spec(None, None, true)),
        ("EXPLICIT", env_spec(Some("café"), None, true)),
        ("FALLBACK", env_spec(None, Some("test"), true)),
        ("INHERITED", env_spec(None, None, true)),
        ("SECRET", env_spec(Some("ignored"), None, false)),
    ];
    let sorted = env_in_insertion_order(entries.clone());
    let reversed = env_in_insertion_order(entries.into_iter().rev());
    // Four entries; each name is followed by the enum index (Present=0,
    // Absent=1) and a string for Present, including the inherited empty value.
    let expected = b"\x04\x06ABSENT\x01\x08EXPLICIT\x00\x05caf\xc3\xa9\
        \x08FALLBACK\x00\x04test\x09INHERITED\x00\x00";
    for env in [&sorted, &reversed, &sorted] {
        let resolve = |name: &str| (name == "INHERITED").then(String::new);
        assert_eq!(env_bytes(env, resolve).unwrap(), expected);
        assert_digest(
            env_hash(env, resolve),
            "de747add5399af4cc62f256278a28e774fa1460c85f5e95cd9e1c002fba49881",
        );
    }
}

fn env_in_insertion_order<'a>(
    entries: impl IntoIterator<Item = (&'a str, EnvSpec)>,
) -> BTreeMap<String, EnvSpec> {
    let mut env = BTreeMap::new();
    for (name, spec) in entries {
        env.insert(name.to_owned(), spec);
    }
    env
}

fn env_spec(value: Option<&str>, default: Option<&str>, input: bool) -> EnvSpec {
    EnvSpec {
        value: value.map(str::to_owned),
        default: default.map(str::to_owned),
        input,
    }
}

#[test]
fn pkg_dep_bytes_and_digest_are_golden_after_sorting_and_deduplication() {
    let sorted = [
        ("@ui/core", "1.3.0"),
        ("react", "18.2.0"),
        ("vite", "5.4.0"),
    ];
    let scrambled = [sorted[2], sorted[1], sorted[0], sorted[1]];
    // Three tuples; each writes its two strings without a tuple length prefix.
    let expected = b"\x03\x08@ui/core\x051.3.0\x05react\x0618.2.0\x04vite\x055.4.0";
    for input in [&sorted[..], &scrambled, &sorted] {
        let pairs: Vec<_> = input
            .iter()
            .map(|&(name, version)| (name.into(), version.into()))
            .collect();
        assert_eq!(pkg_dep_bytes(&pairs).unwrap(), expected);
        assert_digest(
            pkg_dep_hash(&pairs),
            "73a1e6d8a40148da293de7cfb36d27e0cdd35dc63c733a414208899ce3af7b1d",
        );
    }
}

fn assert_digest(actual: [u8; 32], expected: &str) {
    assert_eq!(blake3::Hash::from(actual).to_hex().as_str(), expected);
}
