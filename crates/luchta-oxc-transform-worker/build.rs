//! Build script to extract oxc_transformer git rev for tool_version.
//!
//! The oxc crates are git dependencies from the oxc-project/oxc repo.
//! We use oxc_transformer as the representative crate; all oxc_* crates
//! share the same git rev.

fn main() {
    // oxc_transformer is the main transform crate
    let (_version, git_rev) = luchta_lockfile_version::emit_and_read("oxc_transformer");

    // Git dep - use the SHA from the source field
    let tool_version = match git_rev {
        Some(rev) => format!("oxc_transformer={rev}"),
        None => panic!("oxc_transformer must be a git dependency with a rev"),
    };
    println!("cargo::rustc-env=LUCHTA_TOOL_VERSION={}", tool_version);
}
