//! Build script to extract oxc_linter git rev for tool_version.
//!
//! The oxc crates are git dependencies from the oxc-project/oxc repo.
//! We use oxc_linter as the representative crate; all oxc_* crates
//! share the same git rev.

fn main() {
    // oxc_linter is the main linting crate
    let (_version, git_rev) = luchta_lockfile_version::emit_and_read("oxc_linter");

    // Git dep - use the SHA from the source field
    let tool_version = match git_rev {
        Some(rev) => format!("oxc_linter={rev}"),
        None => panic!("oxc_linter must be a git dependency with a rev"),
    };
    println!("cargo::rustc-env=LUCHTA_TOOL_VERSION={}", tool_version);
}
