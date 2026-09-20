//! Build-time helper to read dependency versions from `Cargo.lock`.
//!
//! This crate provides utilities for extracting package version and git revision
//! information from a workspace's `Cargo.lock` file. Designed for use in `build.rs`
//! scripts to embed dependency metadata into the build.
//!
//! # Example
//!
//! ```ignore
//! // In build.rs:
//! fn main() {
//!     let (version, git_rev) = luchta_lockfile_version::emit_and_read("oxc_allocator");
//!     println!("cargo::rustc-env=OXC_VERSION={}", version);
//!     if let Some(rev) = git_rev {
//!         println!("cargo::rustc-env=OXC_GIT_REV={}", rev);
//!     }
//! }
//! ```

use std::env;
use std::fs;
use std::path::PathBuf;

/// Locate the workspace `Cargo.lock` by walking `CARGO_MANIFEST_DIR` ancestors.
///
/// Starting from the `CARGO_MANIFEST_DIR` environment variable (set by Cargo for
/// the crate being built), walks upward until finding a `Cargo.lock` file.
///
/// # Panics
///
/// Panics if `CARGO_MANIFEST_DIR` is not set or if no `Cargo.lock` is found
/// in any ancestor directory.
pub fn locate_lockfile() -> PathBuf {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");
    let mut path = PathBuf::from(manifest_dir);

    loop {
        let lockfile = path.join("Cargo.lock");
        if lockfile.exists() {
            return lockfile;
        }
        if !path.pop() {
            panic!(
                "Could not find Cargo.lock in any ancestor of CARGO_MANIFEST_DIR. \
                 Are you running this from within a Cargo workspace?"
            );
        }
    }
}

/// Return the resolved `version` for a `[[package]]` by name.
///
/// Parses the `Cargo.lock` content and returns the `version` field for the
/// first `[[package]]` entry matching the given name.
///
/// Returns `None` if no package with that name is found.
///
/// # Example
///
/// ```
/// let lock = r#"
/// [[package]]
/// name = "serde"
/// version = "1.0.203"
/// source = "registry+https://github.com/rust-lang/crates.io-index"
/// "#;
/// let version = luchta_lockfile_version::package_version(lock, "serde");
/// assert_eq!(version, Some("1.0.203".to_string()));
/// ```
pub fn package_version(lock: &str, name: &str) -> Option<String> {
    parse_package(lock, name).map(|(v, _)| v)
}

/// For a git dep, return the 40/64-char SHA after `#` in `source`; else `None`.
///
/// For packages with a `source` field starting with `git+`, extracts the git
/// revision SHA from the URL fragment (the part after `#`). Returns `None`
/// for registry packages or packages without a `source` field.
///
/// # Example
///
/// ```
/// let lock = r#"
/// [[package]]
/// name = "oxc_allocator"
/// version = "0.150.0"
/// source = "git+https://github.com/oxc-project/oxc.git?rev=abc#0123456789abcdef0123456789abcdef01234567"
/// "#;
/// let rev = luchta_lockfile_version::package_git_rev(lock, "oxc_allocator");
/// assert_eq!(rev, Some("0123456789abcdef0123456789abcdef01234567".to_string()));
///
/// // For a registry package, returns None
/// let lock2 = r#"
/// [[package]]
/// name = "serde"
/// version = "1.0.203"
/// source = "registry+https://github.com/rust-lang/crates.io-index"
/// "#;
/// let rev2 = luchta_lockfile_version::package_git_rev(lock2, "serde");
/// assert_eq!(rev2, None);
/// ```
pub fn package_git_rev(lock: &str, name: &str) -> Option<String> {
    parse_package(lock, name).and_then(|(_, source)| {
        source.and_then(|s| {
            if s.starts_with("git+") {
                s.rfind('#').map(|pos| s[pos + 1..].to_string())
            } else {
                None
            }
        })
    })
}

/// Convenience for `build.rs`: emit rerun-if-changed directives and return version info.
///
/// Prints `cargo::rerun-if-changed` directives for both `build.rs` and the lockfile,
/// then returns the `(version, git_rev)` tuple for the named package.
///
/// # Panics
///
/// Panics with a clear error message if:
/// - The `Cargo.lock` file cannot be read
/// - No package with the given name is found
/// - The package has no version field
///
/// # Output
///
/// Prints the following directives to stdout:
/// ```text
/// cargo::rerun-if-changed=build.rs
/// cargo::rerun-if-changed=<lockfile path>
/// ```
///
/// Note: When any `rerun-if-changed` directive is emitted, Cargo disables its default
/// file-watching behavior, so `build.rs` must be explicitly listed.
pub fn emit_and_read(name: &str) -> (String, Option<String>) {
    let lockfile = locate_lockfile();

    // Emit rerun-if-changed directives
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={}", lockfile.display());

    let lock_content = fs::read_to_string(&lockfile)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", lockfile.display(), e));

    let (version, source) = parse_package(&lock_content, name)
        .unwrap_or_else(|| panic!("Package '{}' not found in {}", name, lockfile.display()));

    let git_rev = source.and_then(|s| {
        if s.starts_with("git+") {
            s.rfind('#').map(|pos| s[pos + 1..].to_string())
        } else {
            None
        }
    });

    (version, git_rev)
}

/// Parse a package entry from Cargo.lock content.
///
/// Returns `Some((version, source))` where:
/// - `version` is the package's version string
/// - `source` is `Some(source_string)` if a source field exists, `None` otherwise
///
/// Returns `None` if no matching package is found.
fn parse_package(lock: &str, name: &str) -> Option<(String, Option<String>)> {
    // Split on [[package]] sections
    for section in lock.split("[[package]]").skip(1) {
        // Check if this section matches the target name
        if let Some(pkg_name) = extract_field(section, "name") {
            if pkg_name == name {
                let version = extract_field(section, "version")?;
                let source = extract_field(section, "source");
                return Some((version, source));
            }
        }
    }
    None
}

/// Extract a quoted field value from a package section.
///
/// Looks for a line like `field = "value"` and returns `Some("value")`.
/// Returns `None` if the field is not found.
fn extract_field(section: &str, field: &str) -> Option<String> {
    let prefix = format!("{} = \"", field);
    for line in section.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            if let Some(end) = rest.find('"') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lockfile() -> String {
        r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "serde"
version = "1.0.203"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "sha256..."
dependencies = []

[[package]]
name = "oxc_allocator"
version = "0.150.0"
source = "git+https://github.com/oxc-project/oxc.git?rev=7bf68f70c20f5329251f19be95076f6eab4fe396#7bf68f70c20f5329251f19be95076f6eab4fe396"
dependencies = []

[[package]]
name = "tokio"
version = "1.40.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "e8b0b..."
dependencies = []
"#.to_string()
    }

    #[test]
    fn test_package_version_crates_io() {
        let lock = sample_lockfile();
        let version = package_version(&lock, "serde");
        assert_eq!(version, Some("1.0.203".to_string()));
    }

    #[test]
    fn test_package_git_rev_crates_io_returns_none() {
        let lock = sample_lockfile();
        let rev = package_git_rev(&lock, "serde");
        assert_eq!(rev, None);
    }

    #[test]
    fn test_package_git_rev_crates_io_tokio_returns_none() {
        let lock = sample_lockfile();
        let rev = package_git_rev(&lock, "tokio");
        assert_eq!(rev, None);
    }

    #[test]
    fn test_package_version_git_dep() {
        let lock = sample_lockfile();
        let version = package_version(&lock, "oxc_allocator");
        assert_eq!(version, Some("0.150.0".to_string()));
    }

    #[test]
    fn test_package_git_rev_git_dep() {
        let lock = sample_lockfile();
        let rev = package_git_rev(&lock, "oxc_allocator");
        assert_eq!(
            rev,
            Some("7bf68f70c20f5329251f19be95076f6eab4fe396".to_string())
        );
    }

    #[test]
    fn test_package_both_version_and_git_rev() {
        let lock = sample_lockfile();
        let (v, rev) = parse_package(&lock, "oxc_allocator").unwrap();
        assert_eq!(v, "0.150.0");
        assert!(rev.is_some());
        assert!(rev.unwrap().starts_with("git+"));
    }

    #[test]
    fn test_missing_package_returns_none() {
        let lock = sample_lockfile();
        let version = package_version(&lock, "nonexistent");
        assert_eq!(version, None);

        let rev = package_git_rev(&lock, "nonexistent");
        assert_eq!(rev, None);

        let result = parse_package(&lock, "nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn test_package_without_source_field() {
        let lock = r#"
[[package]]
name = "local_package"
version = "0.1.0"
"#;
        let version = package_version(lock, "local_package");
        assert_eq!(version, Some("0.1.0".to_string()));

        let rev = package_git_rev(lock, "local_package");
        assert_eq!(rev, None);
    }

    #[test]
    fn test_extract_field_basic() {
        let section = r#"
name = "test"
version = "1.2.3"
source = "test-source"
"#;
        assert_eq!(extract_field(section, "name"), Some("test".to_string()));
        assert_eq!(extract_field(section, "version"), Some("1.2.3".to_string()));
        assert_eq!(
            extract_field(section, "source"),
            Some("test-source".to_string())
        );
        assert_eq!(extract_field(section, "missing"), None);
    }

    #[test]
    fn test_parse_package_with_whitespace() {
        let lock = r#"
[[package]]
    name = "indented"
    version = "2.0.0"
"#;
        let version = package_version(lock, "indented");
        assert_eq!(version, Some("2.0.0".to_string()));
    }

    #[test]
    fn test_empty_lockfile_returns_none() {
        let version = package_version("", "any");
        assert_eq!(version, None);
    }

    #[test]
    fn test_lockfile_with_only_version_header() {
        let lock = r#"# This file is automatically @generated by Cargo.
version = 4
"#;
        let version = package_version(lock, "serde");
        assert_eq!(version, None);
    }
}
