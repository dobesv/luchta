use std::error::Error;
use std::fmt::{self, Display, Formatter};

use miette::Diagnostic;

/// Silent sentinel used for task-run failures that already emitted per-task logs
/// and a final summary. Main maps this to exit code 1 without extra stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Diagnostic)]
pub struct TasksFailed;

impl Display for TasksFailed {
    fn fmt(&self, _f: &mut Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl Error for TasksFailed {}
