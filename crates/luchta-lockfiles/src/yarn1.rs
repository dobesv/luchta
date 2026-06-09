use std::collections::BTreeMap;

use crate::{Lockfile, LockfileError, Package};

/// Stub Yarn v1 lockfile implementation.
///
/// Real parsing lands in Phase 2 Task 2.1. For now this type provides trait
/// shape needed by downstream crates.
#[derive(Debug, Default, Clone)]
pub struct Yarn1Lockfile;

impl Yarn1Lockfile {
    /// Creates empty Yarn v1 lockfile stub.
    pub fn new() -> Self {
        Self
    }
}

impl Lockfile for Yarn1Lockfile {
    fn resolve_package(
        &self,
        _workspace_path: &str,
        _name: &str,
        _version: &str,
    ) -> Result<Option<Package>, LockfileError> {
        Ok(None)
    }

    fn all_dependencies(&self, _key: &str) -> Result<BTreeMap<String, String>, LockfileError> {
        Ok(BTreeMap::new())
    }
}
