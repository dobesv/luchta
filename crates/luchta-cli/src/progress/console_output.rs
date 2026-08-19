use std::{
    io::{IsTerminal, Write},
    sync::{Arc, Mutex},
};

use terminal_size::Width;
use unicode_width::UnicodeWidthChar;

use crate::cli::OutputMode;

const CLEAR_CURRENT_LINE: &str = "\r\x1b[2K";
const ANSI_RESET: &str = "\x1b[0m";
const FALLBACK_TERMINAL_COLUMNS: usize = 80;

/// Run-scoped console output synchronized with the interactive status line.
///
/// A live status has no trailing newline, so every ordinary write must clear it
/// first. Clones share the same mutex and visibility state, allowing task
/// runners, cache helpers, and the progress timer to write without splicing
/// their output into one another.
#[derive(Clone, Debug)]
pub(crate) struct ProgressOutput {
    live: bool,
    state: Arc<Mutex<InteractiveStatusState>>,
}

impl ProgressOutput {
    pub(super) fn detect(mode: OutputMode) -> Self {
        let terminal_supports_live_status = std::io::stderr().is_terminal()
            && match std::env::var("TERM") {
                Ok(term) => term != "dumb",
                Err(_) => true,
            };
        Self::new(mode == OutputMode::Default && terminal_supports_live_status)
    }

    pub(crate) fn new(live: bool) -> Self {
        Self {
            live,
            state: Arc::new(Mutex::new(InteractiveStatusState::default())),
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        self.live
    }

    pub(crate) fn progress_line(&self, line: &str) {
        if !self.live {
            eprintln!("{line}");
            return;
        }

        self.write_live_stderr(|state, stderr| {
            state.render(stderr, line, usable_terminal_columns())
        });
    }

    pub(crate) fn terminal_width(&self) -> Option<usize> {
        self.live.then(usable_terminal_columns)
    }

    pub(crate) fn clear_progress(&self) {
        if !self.live {
            return;
        }

        self.write_live_stderr(|state, stderr| state.clear(stderr));
    }

    pub(crate) fn stdout_line(&self, line: &str) {
        if !self.live {
            println!("{line}");
            return;
        }

        let mut state = self.lock_state();
        self.clear_locked(&mut state);
        let mut stdout = std::io::stdout().lock();
        expect_write(
            "stdout",
            writeln!(stdout, "{line}").and_then(|()| stdout.flush()),
        );
    }

    pub(crate) fn stderr_line(&self, line: &str) {
        if !self.live {
            eprintln!("{line}");
            return;
        }

        self.write_live_stderr(|state, stderr| {
            state
                .clear(stderr)
                .and_then(|()| writeln!(stderr, "{line}"))
                .and_then(|()| stderr.flush())
        });
    }

    pub(crate) fn stderr_block(&self, text: &str) {
        if !self.live {
            eprint!("{text}");
            return;
        }

        self.write_live_stderr(|state, stderr| {
            state.clear(stderr).and_then(|()| {
                stderr.write_all(text.as_bytes())?;
                if !text.ends_with('\n') {
                    stderr.write_all(b"\n")?;
                }
                stderr.flush()
            })
        });
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, InteractiveStatusState> {
        self.state
            .lock()
            .expect("interactive progress output mutex poisoned")
    }

    fn clear_locked(&self, state: &mut InteractiveStatusState) {
        let mut stderr = std::io::stderr().lock();
        expect_write("stderr", state.clear(&mut stderr));
    }

    fn write_live_stderr(
        &self,
        operation: impl FnOnce(
            &mut InteractiveStatusState,
            &mut std::io::StderrLock<'_>,
        ) -> std::io::Result<()>,
    ) {
        let mut state = self.lock_state();
        let mut stderr = std::io::stderr().lock();
        expect_write("stderr", operation(&mut state, &mut stderr));
    }
}

fn expect_write(destination: &str, result: std::io::Result<()>) {
    if let Err(error) = result {
        panic!("failed printing to {destination}: {error}");
    }
}

#[derive(Debug, Default)]
pub(super) struct InteractiveStatusState {
    visible: bool,
}

impl InteractiveStatusState {
    pub(super) fn render(
        &mut self,
        writer: &mut impl Write,
        line: &str,
        max_width: usize,
    ) -> std::io::Result<()> {
        writer.write_all(CLEAR_CURRENT_LINE.as_bytes())?;
        writer.write_all(truncate_ansi(line, max_width).as_bytes())?;
        writer.flush()?;
        self.visible = true;
        Ok(())
    }

    pub(super) fn clear(&mut self, writer: &mut impl Write) -> std::io::Result<()> {
        if !self.visible {
            return Ok(());
        }

        writer.write_all(CLEAR_CURRENT_LINE.as_bytes())?;
        writer.flush()?;
        self.visible = false;
        Ok(())
    }
}

fn usable_terminal_columns() -> usize {
    terminal_size::terminal_size_of(std::io::stderr())
        .map(|(Width(columns), _)| usize::from(columns))
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(FALLBACK_TERMINAL_COLUMNS)
        // Avoid the terminal's delayed-wrap state in the final column.
        .saturating_sub(1)
        .max(1)
}

fn ansi_sequence_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(..2) != Some(b"\x1b[") {
        return None;
    }
    bytes[2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|offset| offset + 3)
}

pub(super) fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut remaining = text;
    while !remaining.is_empty() {
        if let Some(sequence_len) = ansi_sequence_len(remaining) {
            remaining = &remaining[sequence_len..];
            continue;
        }
        let ch = remaining
            .chars()
            .next()
            .expect("non-empty text has a character");
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
        remaining = &remaining[ch.len_utf8()..];
    }
    width
}

pub(super) fn truncate_ansi(text: &str, max_width: usize) -> String {
    if visible_width(text) <= max_width {
        return text.to_owned();
    }

    let content_width = max_width.saturating_sub(1);
    let mut output = String::new();
    let mut width = 0;
    let mut remaining = text;
    while !remaining.is_empty() {
        if let Some(sequence_len) = ansi_sequence_len(remaining) {
            output.push_str(&remaining[..sequence_len]);
            remaining = &remaining[sequence_len..];
            continue;
        }
        let ch = remaining
            .chars()
            .next()
            .expect("non-empty text has a character");
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > content_width {
            break;
        }
        output.push(ch);
        width += char_width;
        remaining = &remaining[ch.len_utf8()..];
    }
    output.push('…');
    output.push_str(ANSI_RESET);
    output
}
