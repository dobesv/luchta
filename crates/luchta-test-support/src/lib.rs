/// Assert the current test runs under `cargo nextest`, panicking with guidance
/// otherwise. For tests that mutate process-global state (cwd, env vars, temp
/// dirs) and need nextest's per-test process isolation.
#[track_caller]
pub fn require_nextest() {
    if std::env::var_os("NEXTEST").is_none() {
        panic!(
            "\n\nThis test must be run with cargo-nextest, not `cargo test`.\n\
             It mutates process-global state and needs nextest's per-test process\n\
             isolation; `cargo test` shares one process across tests and produces\n\
             spurious failures.\n\n\
             Run instead:\n\tcargo nextest run --workspace\n\n\
             See AGENTS.md for the verification pipeline.\n"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nextest_runs_tests_with_the_hermetic_environment_wrapper() {
        require_nextest();

        assert_eq!(
            std::env::var("LUCHTA_TEST_HERMETIC_WRAPPER").as_deref(),
            Ok("1"),
            "the repository nextest wrapper did not run"
        );
        assert_eq!(
            std::env::var_os("LUCHTA_HERMETIC_TEST_CANARY"),
            None,
            "the nextest wrapper leaked a non-allowlisted variable"
        );
    }

    /// The wrapper must keep rustup's toolchain selection: tests that build
    /// other crates with cargo otherwise let a dependency's own
    /// `rust-toolchain.toml` pick the compiler.
    #[cfg(unix)]
    #[test]
    fn hermetic_wrapper_keeps_rustup_toolchain_and_strips_the_rest() {
        let wrapper = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/nextest-hermetic.sh");
        let output = std::process::Command::new("sh")
            .arg(&wrapper)
            .arg("env")
            .env("RUSTUP_TOOLCHAIN", "probe-toolchain")
            .env("RUSTUP_HOME", "/probe/rustup")
            .env("LUCHTA_HERMETIC_LEAK_PROBE", "leaked")
            .output()
            .expect("run the hermetic wrapper");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let env = String::from_utf8_lossy(&output.stdout);
        assert!(
            env.lines().any(|l| l == "RUSTUP_TOOLCHAIN=probe-toolchain"),
            "{env}"
        );
        assert!(
            env.lines().any(|l| l == "RUSTUP_HOME=/probe/rustup"),
            "{env}"
        );
        assert!(!env.contains("LUCHTA_HERMETIC_LEAK_PROBE"), "{env}");
    }
}
