//! Build script to extract ast-grep-core version for tool_version.
//!
//! ast-grep-config and ast-grep-language share the same version; ast-grep-core
//! is sufficient and representative.

fn main() {
    // ast-grep-core is the core matching engine with pinned version
    let (version, _rev) = luchta_lockfile_version::emit_and_read("ast-grep-core");

    // Registry dep - use version directly
    let tool_version = format!("ast-grep-core={version}");
    println!("cargo::rustc-env=LUCHTA_TOOL_VERSION={}", tool_version);
}
