/// Runs `process` over `items` in parallel, chunking work across
/// `available_parallelism()` threads via `std::thread::scope` (no external
/// thread pool). Returns the per-item outcomes flattened in input order (chunk
/// order is preserved on merge). Empty input returns `Ok(vec![])` without
/// spawning any threads. If any worker thread panics, returns
/// `Err(panic_message.to_owned())`.
pub fn process_items_in_parallel<T, I, F>(
    items: &[I],
    panic_message: &str,
    process: F,
) -> Result<Vec<T>, String>
where
    T: Send,
    I: Sync,
    F: Fn(&I) -> T + Sync,
{
    if items.is_empty() {
        return Ok(vec![]);
    }

    let threads = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .min(items.len().max(1));
    let chunk_size = items.len().max(1).div_ceil(threads);

    std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for chunk in items.chunks(chunk_size) {
            let process = &process;
            jobs.push(scope.spawn(move || chunk.iter().map(process).collect::<Vec<T>>()));
        }

        let mut merged = Vec::with_capacity(items.len());
        for job in jobs {
            let chunk_outcomes = job.join().map_err(|_| panic_message.to_owned())?;
            merged.extend(chunk_outcomes);
        }
        Ok(merged)
    })
}

#[cfg(test)]
mod tests {
    use super::process_items_in_parallel;

    #[test]
    fn merges_multi_chunk_results_in_input_order() {
        let items: Vec<usize> = (0..10_000).collect();
        let expected: Vec<String> = items.iter().map(|item| format!("item-{item}")).collect();

        let actual = process_items_in_parallel(&items, "panic", |item| format!("item-{item}"))
            .expect("parallel processing succeeds");

        assert_eq!(actual, expected);
    }

    #[test]
    fn returns_panic_message_when_worker_panics() {
        // A single panicking item must surface as the provided panic message.
        // Note: no cross-thread synchronization here — the number of spawned
        // threads depends on `available_parallelism()`, so a test that blocks
        // until N threads run concurrently would deadlock on low-core hosts.
        let items = vec![0usize, 1, 2, 3];

        let result = process_items_in_parallel(&items, "worker panicked", |item| {
            assert_ne!(*item, 2, "boom");
            item + 1
        });

        assert_eq!(result, Err("worker panicked".to_owned()));
    }

    #[test]
    fn returns_empty_vec_without_spawning_for_empty_input() {
        let items: Vec<usize> = Vec::new();

        let result = process_items_in_parallel(&items, "panic", |_| unreachable!("no items"))
            .expect("empty input succeeds");

        assert!(result.is_empty());
    }
}
