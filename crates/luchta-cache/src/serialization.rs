//! Shared bincode configuration for the cache crate.
//!
//! Both the hashing path (`hashing.rs`) and the storage path (`store.rs`) must
//! serialize with byte-for-byte identical settings; if they drifted (e.g. a
//! different int-encoding mode) hashes and stored records would silently
//! diverge. Keeping a single source of truth here removes that risk.

/// Canonical bincode configuration used for cache hashing and record storage.
pub(crate) fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard().with_fixed_int_encoding()
}
