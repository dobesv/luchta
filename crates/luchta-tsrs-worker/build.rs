//! Extract upstream and fork tsrs git revisions for cache invalidation.

fn main() {
    let (_, compiler_rev) = luchta_lockfile_version::emit_and_read("tsrs_compiler");
    let (_, core_rev) = luchta_lockfile_version::emit_and_read("tsrs_core");
    let compiler_rev = compiler_rev.expect("tsrs_compiler must be a git dependency with a rev");
    let core_rev = core_rev.expect("tsrs_core must be a git dependency with a rev");
    println!(
        "cargo::rustc-env=LUCHTA_TOOL_VERSION=tsrs_compiler={compiler_rev},tsrs_core={core_rev}"
    );
}
