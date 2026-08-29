use super::SharedCache;

impl SharedCache {
    /// Discard optional queued uploads immediately, used by interrupt and
    /// watch cancellation paths where responsiveness takes precedence.
    pub fn discard_remote_work(&self) {
        #[cfg(unix)]
        if let Some(remote) = &self.remote {
            remote.discard_push_queue();
        }
    }

    /// Drain accepted artifact and index jobs within the configured total
    /// cycle budget while leaving the worker pool reusable for watch cycles.
    pub fn finish_remote_cycle(&self) {
        #[cfg(unix)]
        if let Some(remote) = &self.remote {
            remote.drain_push_queue();
        }
    }

    /// Emit and reset one concise cycle aggregate when explicitly enabled.
    pub fn report_cycle_stats(&self) {
        if std::env::var("LUCHTA_SHARED_CACHE_STATS").as_deref() == Ok("1") {
            eprintln!("{}", self.stats.take_diagnostic_line());
        }
    }
}
