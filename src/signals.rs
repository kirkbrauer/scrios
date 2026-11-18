//! What stops the process from outside: SIGINT and SIGTERM on unix; on Windows the console's Ctrl-C and Ctrl-Break.
//! Windows has no SIGTERM: a process that is ended with `taskkill /f` or `Stop-Process`, or by closing its console
//! window, is not told and ends at once.

use std::io;

/// What the messages call these: the keys or the signals that a person or a supervisor uses to stop `scrios`.
pub(crate) const NAMES: &str = if cfg!(windows) { "Ctrl-C" } else { "SIGINT or SIGTERM" };

/// The listeners for the signals that mean "stop".
#[derive(Debug)]
pub(crate) struct StopSignals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    brk: tokio::signal::windows::CtrlBreak,
}

impl StopSignals {
    /// Starts listening. Needs a running tokio runtime on unix.
    ///
    /// # Errors
    ///
    /// The operating system refused the handler.
    #[cfg(unix)]
    pub(crate) fn install() -> io::Result<StopSignals> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(StopSignals { term: signal(SignalKind::terminate())?, int: signal(SignalKind::interrupt())? })
    }

    /// Starts listening.
    ///
    /// # Errors
    ///
    /// The operating system refused the handler.
    #[cfg(windows)]
    pub(crate) fn install() -> io::Result<StopSignals> {
        use tokio::signal::windows::{ctrl_break, ctrl_c};
        Ok(StopSignals { interrupt: ctrl_c()?, brk: ctrl_break()? })
    }

    /// Completes when one of them arrives (and again for the next one). Safe to cancel.
    #[cfg(unix)]
    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.int.recv() => {}
        }
    }

    /// Completes when one of them arrives (and again for the next one). Safe to cancel.
    #[cfg(windows)]
    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.brk.recv() => {}
        }
    }
}

// Only on Windows: tokio leaves a unix signal handler installed for good, which would make a test process deaf to
// the Ctrl-C of whoever runs the tests.
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_handlers_can_be_installed() {
        // Nothing sends a Ctrl-C here: the point is that installing works on this platform and that waiting is
        // cancellable (a timeout drops the future).
        let mut signals = StopSignals::install().unwrap();
        let waited = tokio::time::timeout(std::time::Duration::from_millis(20), signals.recv()).await;
        assert!(waited.is_err(), "no signal was sent");
        assert!(!NAMES.is_empty());
    }
}
