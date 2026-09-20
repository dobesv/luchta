//! Build script to extract swc_core and styled_components versions for tool_version.
//!
//! The tool_version string is sorted `name=version` pairs joined by `,`.

fn main() {
    // swc worker depends on both swc_core and styled_components - both affect output
    let (swc_core_version, _swc_core_rev) = luchta_lockfile_version::emit_and_read("swc_core");
    let (styled_components_version, _styled_components_rev) =
        luchta_lockfile_version::emit_and_read("styled_components");

    // Sort by name for stable output
    let mut parts = [
        format!("styled_components={styled_components_version}"),
        format!("swc_core={swc_core_version}"),
    ];
    parts.sort();

    let tool_version = parts.join(",");
    println!("cargo::rustc-env=LUCHTA_TOOL_VERSION={}", tool_version);
}
