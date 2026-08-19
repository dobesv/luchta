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
}
