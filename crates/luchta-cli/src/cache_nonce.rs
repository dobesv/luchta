use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

// Percent-encode controls plus `%`, `&`, and `=` so values can never produce
// cache-nonce delimiters or ambiguous pre-encoded sequences.
const CACHE_NONCE_ENCODE_SET: &AsciiSet = &CONTROLS.add(b'%').add(b'&').add(b'=');

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheNonceScopes<'a> {
    pub env: Option<&'a str>,
    pub global: Option<&'a str>,
    pub worker: Option<&'a str>,
    pub task: Option<&'a str>,
    pub worker_runtime: Option<&'a str>,
}

impl<'a> CacheNonceScopes<'a> {
    pub fn for_task(
        env: Option<&'a str>,
        global: Option<&'a str>,
        task_def: &'a luchta_types::TaskDefinition,
        workers: &'a std::collections::HashMap<String, luchta_types::WorkerDefinition>,
        worker_runtime: Option<&'a str>,
    ) -> Self {
        let worker = task_def
            .worker
            .as_deref()
            .and_then(|worker_name| workers.get(worker_name))
            .and_then(|definition| definition.cache.as_ref())
            .and_then(|cache| cache.cache_nonce.as_deref());
        let task = task_def
            .cache
            .as_ref()
            .and_then(|cache| cache.cache_nonce.as_deref());

        Self {
            env,
            global,
            worker,
            task,
            worker_runtime,
        }
    }
}

impl CacheNonceScopes<'_> {
    pub fn resolve(self) -> Option<String> {
        let mut parts = Vec::new();

        for (key, value) in [
            ("env", self.env),
            ("global", self.global),
            ("worker", self.worker),
            ("task", self.task),
            ("workerNonce", self.worker_runtime),
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
}

#[cfg(test)]
mod tests {
    use super::CacheNonceScopes;

    const ENV_ONLY: CacheNonceScopes<'static> = CacheNonceScopes {
        env: Some("env-value"),
        global: None,
        worker: None,
        task: None,
        worker_runtime: None,
    };

    const GLOBAL_ONLY: CacheNonceScopes<'static> = CacheNonceScopes {
        env: None,
        global: Some("global-value"),
        worker: None,
        task: None,
        worker_runtime: None,
    };

    const WORKER_ONLY: CacheNonceScopes<'static> = CacheNonceScopes {
        env: None,
        global: None,
        worker: Some("worker-value"),
        task: None,
        worker_runtime: None,
    };

    const TASK_ONLY: CacheNonceScopes<'static> = CacheNonceScopes {
        env: None,
        global: None,
        worker: None,
        task: Some("task-value"),
        worker_runtime: None,
    };

    const WORKER_RUNTIME_ONLY: CacheNonceScopes<'static> = CacheNonceScopes {
        env: None,
        global: None,
        worker: None,
        task: None,
        worker_runtime: Some("worker-runtime-value"),
    };

    #[test]
    fn returns_none_when_all_sources_absent() {
        assert_eq!(CacheNonceScopes::default().resolve(), None);
    }

    #[test]
    fn resolves_single_source_nonces() {
        for (scopes, expected) in [
            (ENV_ONLY, "env=env-value"),
            (GLOBAL_ONLY, "global=global-value"),
            (WORKER_ONLY, "worker=worker-value"),
            (TASK_ONLY, "task=task-value"),
            (WORKER_RUNTIME_ONLY, "workerNonce=worker-runtime-value"),
        ] {
            assert_eq!(scopes.resolve(), Some(expected.to_string()));
        }
    }

    #[test]
    fn resolves_all_sources_in_fixed_order() {
        assert_eq!(
            CacheNonceScopes {
                env: Some("one"),
                global: Some("two"),
                worker: Some("three"),
                task: Some("four"),
                worker_runtime: Some("five"),
            }
            .resolve(),
            Some("env=one&global=two&worker=three&task=four&workerNonce=five".to_string())
        );
        assert_eq!(
            CacheNonceScopes {
                env: None,
                global: Some("two"),
                worker: Some("three"),
                task: Some("four"),
                worker_runtime: None,
            }
            .resolve(),
            Some("global=two&worker=three&task=four".to_string())
        );
        assert_eq!(
            CacheNonceScopes {
                env: Some("one"),
                global: None,
                worker: Some("three"),
                task: Some("four"),
                worker_runtime: None,
            }
            .resolve(),
            Some("env=one&worker=three&task=four".to_string())
        );
    }

    #[test]
    fn percent_encoding_keeps_values_unambiguous() {
        assert_eq!(
            CacheNonceScopes {
                env: Some("a&b"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            Some("env=a%26b".to_string())
        );
        assert_eq!(
            CacheNonceScopes {
                env: Some("a=b"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            Some("env=a%3Db".to_string())
        );
        assert_ne!(
            CacheNonceScopes {
                env: Some("a&b"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            CacheNonceScopes {
                env: Some("a"),
                global: Some("b"),
                ..CacheNonceScopes::default()
            }
            .resolve()
        );
        assert_ne!(
            CacheNonceScopes {
                env: Some("a=b"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            CacheNonceScopes {
                env: Some("a"),
                global: Some("b"),
                ..CacheNonceScopes::default()
            }
            .resolve()
        );
        assert_ne!(
            CacheNonceScopes {
                env: Some("a&b"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            CacheNonceScopes {
                env: Some("a=b"),
                ..CacheNonceScopes::default()
            }
            .resolve()
        );
    }

    #[test]
    fn percent_encoding_escapes_percent_signs() {
        assert_eq!(
            CacheNonceScopes {
                env: Some("already%encoded"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            Some("env=already%25encoded".to_string())
        );
    }

    #[test]
    fn resolve_cache_nonce_is_deterministic() {
        let first = CacheNonceScopes {
            env: Some("env=value"),
            global: Some("global&value"),
            worker: Some("worker%value"),
            task: Some("task\nvalue"),
            worker_runtime: Some("worker-runtime=value"),
        }
        .resolve();
        let second = CacheNonceScopes {
            env: Some("env=value"),
            global: Some("global&value"),
            worker: Some("worker%value"),
            task: Some("task\nvalue"),
            worker_runtime: Some("worker-runtime=value"),
        }
        .resolve();

        assert_eq!(first, second);
    }

    #[test]
    fn resolve_cache_nonce_combines_scopes_instead_of_overriding() {
        let env_only = CacheNonceScopes {
            env: Some("env-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();
        let global_only = CacheNonceScopes {
            global: Some("global-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();
        let worker_only = CacheNonceScopes {
            worker: Some("worker-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();
        let task_only = CacheNonceScopes {
            task: Some("task-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();
        let worker_runtime_only = CacheNonceScopes {
            worker_runtime: Some("worker-runtime-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();
        let combined = CacheNonceScopes {
            env: Some("env-a"),
            global: Some("global-a"),
            worker: Some("worker-a"),
            task: Some("task-a"),
            worker_runtime: Some("worker-runtime-a"),
        }
        .resolve();

        assert_eq!(
            combined.as_deref(),
            Some("env=env-a&global=global-a&worker=worker-a&task=task-a&workerNonce=worker-runtime-a")
        );
        assert_ne!(combined, env_only);
        assert_ne!(combined, global_only);
        assert_ne!(combined, worker_only);
        assert_ne!(combined, task_only);
        assert_ne!(combined, worker_runtime_only);
    }

    #[test]
    fn resolve_cache_nonce_worker_scope_is_sparse_and_targeted() {
        let worker_a_task = CacheNonceScopes {
            env: Some("env-a"),
            global: Some("global-a"),
            worker: Some("worker-a"),
            task: Some("task-a"),
            worker_runtime: None,
        }
        .resolve();
        let worker_b_task = CacheNonceScopes {
            env: Some("env-a"),
            global: Some("global-a"),
            task: Some("task-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();
        let no_worker_task = CacheNonceScopes {
            env: Some("env-a"),
            global: Some("global-a"),
            task: Some("task-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();
        let dangling_worker_task = CacheNonceScopes {
            env: Some("env-a"),
            global: Some("global-a"),
            task: Some("task-a"),
            ..CacheNonceScopes::default()
        }
        .resolve();

        assert_ne!(worker_a_task, worker_b_task);
        assert_eq!(worker_b_task, no_worker_task);
        assert_eq!(no_worker_task, dangling_worker_task);
        assert_eq!(
            no_worker_task.as_deref(),
            Some("env=env-a&global=global-a&task=task-a")
        );
    }

    #[test]
    fn resolve_cache_nonce_with_only_worker_runtime_scope() {
        assert_eq!(
            CacheNonceScopes {
                worker_runtime: Some("1.2.3"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            Some("workerNonce=1.2.3".to_string())
        );
    }

    #[test]
    fn resolve_cache_nonce_with_all_five_scopes_orders_worker_runtime_last() {
        assert_eq!(
            CacheNonceScopes {
                env: Some("env-value"),
                global: Some("global-value"),
                worker: Some("worker-value"),
                task: Some("task-value"),
                worker_runtime: Some("1.2.3"),
            }
            .resolve(),
            Some(
                "env=env-value&global=global-value&worker=worker-value&task=task-value&workerNonce=1.2.3"
                    .to_string()
            )
        );
    }

    #[test]
    fn resolve_cache_nonce_omits_worker_runtime_scope_when_absent() {
        assert_eq!(
            CacheNonceScopes {
                env: Some("env-value"),
                task: Some("task-value"),
                ..CacheNonceScopes::default()
            }
            .resolve(),
            Some("env=env-value&task=task-value".to_string())
        );
    }
}
