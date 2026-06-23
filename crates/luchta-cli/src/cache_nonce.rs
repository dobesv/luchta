use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

// Percent-encode controls plus `%`, `&`, and `=` so values can never produce
// cache-nonce delimiters or ambiguous pre-encoded sequences.
const CACHE_NONCE_ENCODE_SET: &AsciiSet = &CONTROLS.add(b'%').add(b'&').add(b'=');

pub fn resolve_cache_nonce(
    env_nonce: Option<&str>,
    global_nonce: Option<&str>,
    worker_nonce: Option<&str>,
    task_nonce: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();

    for (key, value) in [
        ("env", env_nonce),
        ("global", global_nonce),
        ("worker", worker_nonce),
        ("task", task_nonce),
    ] {
        if let Some(value) = value {
            parts.push(format!(
                "{key}={}",
                utf8_percent_encode(value, CACHE_NONCE_ENCODE_SET)
            ));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_cache_nonce;

    #[test]
    fn returns_none_when_all_sources_absent() {
        assert_eq!(resolve_cache_nonce(None, None, None, None), None);
    }

    #[test]
    fn resolves_single_source_nonces() {
        for (sources, expected) in [
            ((Some("env-value"), None, None, None), "env=env-value"),
            (
                (None, Some("global-value"), None, None),
                "global=global-value",
            ),
            (
                (None, None, Some("worker-value"), None),
                "worker=worker-value",
            ),
            ((None, None, None, Some("task-value")), "task=task-value"),
        ] {
            assert_eq!(
                resolve_cache_nonce(sources.0, sources.1, sources.2, sources.3),
                Some(expected.to_string())
            );
        }
    }

    #[test]
    fn resolves_all_sources_in_fixed_order() {
        assert_eq!(
            resolve_cache_nonce(Some("one"), Some("two"), Some("three"), Some("four")),
            Some("env=one&global=two&worker=three&task=four".to_string())
        );
        assert_eq!(
            resolve_cache_nonce(None, Some("two"), Some("three"), Some("four")),
            Some("global=two&worker=three&task=four".to_string())
        );
        assert_eq!(
            resolve_cache_nonce(Some("one"), None, Some("three"), Some("four")),
            Some("env=one&worker=three&task=four".to_string())
        );
    }

    #[test]
    fn percent_encoding_keeps_values_unambiguous() {
        assert_eq!(
            resolve_cache_nonce(Some("a&b"), None, None, None),
            Some("env=a%26b".to_string())
        );
        assert_eq!(
            resolve_cache_nonce(Some("a=b"), None, None, None),
            Some("env=a%3Db".to_string())
        );
        assert_ne!(
            resolve_cache_nonce(Some("a&b"), None, None, None),
            resolve_cache_nonce(Some("a"), Some("b"), None, None)
        );
        assert_ne!(
            resolve_cache_nonce(Some("a=b"), None, None, None),
            resolve_cache_nonce(Some("a"), Some("b"), None, None)
        );
        assert_ne!(
            resolve_cache_nonce(Some("a&b"), None, None, None),
            resolve_cache_nonce(Some("a=b"), None, None, None)
        );
    }

    #[test]
    fn percent_encoding_escapes_percent_signs() {
        assert_eq!(
            resolve_cache_nonce(Some("already%encoded"), None, None, None),
            Some("env=already%25encoded".to_string())
        );
    }

    #[test]
    fn resolve_cache_nonce_is_deterministic() {
        let first = resolve_cache_nonce(
            Some("env=value"),
            Some("global&value"),
            Some("worker%value"),
            Some("task\nvalue"),
        );
        let second = resolve_cache_nonce(
            Some("env=value"),
            Some("global&value"),
            Some("worker%value"),
            Some("task\nvalue"),
        );

        assert_eq!(first, second);
    }

    #[test]
    fn resolve_cache_nonce_combines_scopes_instead_of_overriding() {
        let env_only = resolve_cache_nonce(Some("env-a"), None, None, None);
        let global_only = resolve_cache_nonce(None, Some("global-a"), None, None);
        let worker_only = resolve_cache_nonce(None, None, Some("worker-a"), None);
        let task_only = resolve_cache_nonce(None, None, None, Some("task-a"));
        let combined = resolve_cache_nonce(
            Some("env-a"),
            Some("global-a"),
            Some("worker-a"),
            Some("task-a"),
        );

        assert_eq!(
            combined.as_deref(),
            Some("env=env-a&global=global-a&worker=worker-a&task=task-a")
        );
        assert_ne!(combined, env_only);
        assert_ne!(combined, global_only);
        assert_ne!(combined, worker_only);
        assert_ne!(combined, task_only);
    }

    #[test]
    fn resolve_cache_nonce_worker_scope_is_sparse_and_targeted() {
        let worker_a_task = resolve_cache_nonce(
            Some("env-a"),
            Some("global-a"),
            Some("worker-a"),
            Some("task-a"),
        );
        let worker_b_task =
            resolve_cache_nonce(Some("env-a"), Some("global-a"), None, Some("task-a"));
        let no_worker_task =
            resolve_cache_nonce(Some("env-a"), Some("global-a"), None, Some("task-a"));
        let dangling_worker_task =
            resolve_cache_nonce(Some("env-a"), Some("global-a"), None, Some("task-a"));

        assert_ne!(worker_a_task, worker_b_task);
        assert_eq!(worker_b_task, no_worker_task);
        assert_eq!(no_worker_task, dangling_worker_task);
        assert_eq!(
            no_worker_task.as_deref(),
            Some("env=env-a&global=global-a&task=task-a")
        );
    }
}
