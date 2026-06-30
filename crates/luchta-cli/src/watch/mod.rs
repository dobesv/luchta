//! Watch mode session management.
//!
//! Provides a long-lived session for watch mode that can execute multiple
//! build cycles without restarting workers.

pub mod driver;
pub mod session;
pub mod watcher;
