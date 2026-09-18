//! Shared bincode configuration for the cache crate.
//!
//! Both the hashing path (`hashing.rs`) and the storage path (`store.rs`) must
//! serialize with byte-for-byte identical settings; if they drifted (e.g. a
//! different int-encoding mode) hashes and stored records would silently
//! diverge. Keeping a single source of truth here removes that risk.
//!
//! `bincode` itself is frozen at 2.0.1 and will not move: the 3.0.0 release on
//! crates.io is a tombstone (its entire source is a `compile_error!`),
//! published only because crates.io has no way to mark a crate archived — see
//! the project's README for the "unmaintained, doxxing incident" notice.
//! Because this module feeds `hashing.rs`'s output into the cache-key hash,
//! swapping the serialization library would change every existing cache key,
//! so it needs a dedicated migration plan, not a routine dependency bump.
//! Candidate successors, in rough order of fit: `wincode` (bincode-compatible
//! wire format), `postcard`, `rkyv`. The golden-byte tests below exist to gate
//! that future migration — they pin bincode 2.0.1's exact output today so a
//! successor's compatibility (or lack of it) is provable, not assumed.

/// Canonical bincode configuration used for cache hashing and record storage.
pub(crate) fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard().with_fixed_int_encoding()
}

#[cfg(test)]
mod tests {
    use super::bincode_config;
    use serde::Serialize;

    /// Mirrors the shape of a cache hash input: borrowed strs, an Option, a
    /// bool, a numeric field and a sequence. If bincode ever encodes any of
    /// these differently, cache keys change and every existing cache entry
    /// misses.
    #[derive(Serialize)]
    struct Sample<'a> {
        command: Option<&'a str>,
        worker: Option<&'a str>,
        weight: u32,
        depends_on: &'a [&'a str],
        cache_enabled: bool,
    }

    fn sample() -> Sample<'static> {
        Sample {
            command: Some("build"),
            worker: None,
            weight: 3,
            depends_on: &["^build", "lint"],
            cache_enabled: true,
        }
    }

    #[test]
    fn golden_bytes_for_fixed_int_config() {
        let encoded = bincode::serde::encode_to_vec(sample(), bincode_config()).unwrap();
        assert_eq!(
            hex(&encoded),
            "0105000000000000006275696c640003000000020000000000000006000000000000005e6275696c6404000000000000006c696e7401"
        );
    }

    #[test]
    fn golden_bytes_for_varint_config() {
        let config = bincode::config::standard();
        let encoded = bincode::serde::encode_to_vec(sample(), config).unwrap();
        assert_eq!(
            hex(&encoded),
            "01056275696c64000302065e6275696c64046c696e7401"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
