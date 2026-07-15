//! A single-line progress spinner for long-running subprocesses.
//!
//! Full command output belongs in a log file, not the terminal: flooding stderr
//! with streamed script logs (e.g. pre-push hooks) grows scrollback and provokes
//! redraw glitches in multiplexers like zellij. Instead, show one animated line
//! that rewrites itself in place, then a single final status line.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(120);

/// A running spinner. Call [`Spinner::stop_ok`] / [`Spinner::stop_fail`] to end it.
pub struct Spinner {
    label: String,
    tty: bool,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner on stderr with `label`. On a non-TTY stderr it prints the
    /// label once and animates nothing.
    pub fn start(label: impl Into<String>) -> Spinner {
        let label = label.into();
        let tty = std::io::stderr().is_terminal();
        let stop = Arc::new(AtomicBool::new(false));

        let handle = if tty {
            let label = label.clone();
            let stop = Arc::clone(&stop);
            Some(thread::spawn(move || {
                let started = Instant::now();
                let mut frame = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let secs = started.elapsed().as_secs();
                    let mut err = std::io::stderr();
                    // \r to column 0, \x1b[K clears to end of line — rewrites in place.
                    let _ = write!(err, "\r\x1b[K{} {label} {secs}s", FRAMES[frame]);
                    let _ = err.flush();
                    frame = (frame + 1) % FRAMES.len();
                    thread::sleep(TICK);
                }
            }))
        } else {
            eprintln!("scrutiny: {label}…");
            None
        };

        Spinner {
            label,
            tty,
            stop,
            handle,
        }
    }

    /// Stop the spinner and print a success line.
    pub fn stop_ok(self, msg: impl AsRef<str>) {
        self.finish(format!("✓ {}", msg.as_ref()));
    }

    /// Stop the spinner and print a failure line.
    pub fn stop_fail(self, msg: impl AsRef<str>) {
        self.finish(format!("✗ {}", msg.as_ref()));
    }

    fn finish(mut self, line: String) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if self.tty {
            // Clear the animated line before printing the final status.
            eprint!("\r\x1b[K");
        }
        eprintln!("{line}");
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // Guard against an early return that skips stop_ok/stop_fail: end the
        // thread and clear the line so we never leave a dangling spinner.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
            if self.tty {
                eprint!("\r\x1b[K");
                let _ = std::io::stderr().flush();
            }
        }
        let _ = &self.label;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_tty_start_stop_does_not_panic() {
        // Under `cargo test`, stderr is not a TTY: no thread is spawned and
        // start/stop are plain prints. Just assert the lifecycle is sound.
        let sp = Spinner::start("git push — running pre-push hooks");
        assert!(sp.handle.is_none());
        sp.stop_ok("push complete");

        let sp = Spinner::start("git push");
        sp.stop_fail("push failed (exit 1)");
    }
}
