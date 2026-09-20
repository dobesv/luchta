//! Build script to extract oxc_formatter git rev for tool_version.
//!
//! The oxc crates are git dependencies from the oxc-project/oxc repo.
//! We use oxc_formatter as the representative crate; all oxc_* crates
//! share the same git rev.

fn main() {
    // oxc_formatter is the main formatting crate
    let (_version, git_rev) = luchta_lockfile_version::emit_and_read("oxc_formatter");

    // Git dep - use the SHA from the source field
    let tool_version = match git_rev {
        Some(rev) => format!("oxc_formatter={rev}"),
        None => panic!("oxc_formatter must be a git dependency with a rev"),
    };
    println!("cargo::rustc-env=LUCHTA_TOOL_VERSION={}", tool_version);
}
