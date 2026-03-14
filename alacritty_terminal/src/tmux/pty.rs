//! Virtual PTY backed by Unix socketpair for tmux control mode.
//!
//! Instead of a real pseudo-terminal connected to a shell, a `TmuxPty`
//! communicates with the [`TmuxController`] via a Unix socketpair. The
//! controller feeds decoded `%output` data into one end, and reads user
//! keystrokes from the other.

use std::io::{self, ErrorKind, Read};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use polling::{Event, PollMode, Poller};

use crate::event::{OnResize, WindowSize};
use crate::tty::{ChildEvent, EventedPty, EventedReadWrite};

// Re-use the same token constants as the regular PTY.
pub(crate) const PTY_READ_WRITE_TOKEN: usize = 0;
pub(crate) const PTY_CHILD_EVENT_TOKEN: usize = 1;

/// Message from a TmuxPty back to the controller.
#[derive(Debug)]
pub enum TmuxPtyCmd {
    /// The window was resized.
    Resize { pane_id: String, size: WindowSize },
}

/// Virtual PTY for a single tmux pane.
///
/// This is the "PTY event loop side" of the socketpair. The other end
/// is held by the [`TmuxController`].
pub struct TmuxPty {
    /// Socketpair endpoint — reads receive pane output, writes send user input.
    pub(crate) file: UnixStream,

    /// Signaled (one byte written) when the pane exits.
    pub(crate) exit_signal: UnixStream,

    /// Channel to send commands (e.g. resize) back to the controller.
    pub(crate) cmd_tx: Sender<TmuxPtyCmd>,

    /// tmux pane identifier (e.g. `%0`).
    pub(crate) pane_id: String,
}

impl EventedReadWrite for TmuxPty {
    type Reader = UnixStream;
    type Writer = UnixStream;

    #[inline]
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        unsafe {
            poll.add_with_mode(&self.file, interest, poll_opts)?;
        }
        unsafe {
            poll.add_with_mode(
                &self.exit_signal,
                Event::readable(PTY_CHILD_EVENT_TOKEN),
                PollMode::Level,
            )
        }
    }

    #[inline]
    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        poll.modify_with_mode(&self.file, interest, poll_opts)?;
        poll.modify_with_mode(
            &self.exit_signal,
            Event::readable(PTY_CHILD_EVENT_TOKEN),
            PollMode::Level,
        )
    }

    #[inline]
    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        poll.delete(&self.file)?;
        poll.delete(&self.exit_signal)
    }

    #[inline]
    fn reader(&mut self) -> &mut UnixStream {
        &mut self.file
    }

    #[inline]
    fn writer(&mut self) -> &mut UnixStream {
        &mut self.file
    }
}

impl EventedPty for TmuxPty {
    #[inline]
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        let mut buf = [0u8; 1];
        match self.exit_signal.read(&mut buf) {
            Ok(_) => Some(ChildEvent::Exited(None)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => None,
            Err(_) => None,
        }
    }
}

impl OnResize for TmuxPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        let _ = self.cmd_tx.send(TmuxPtyCmd::Resize {
            pane_id: self.pane_id.clone(),
            size: window_size,
        });
    }
}

/// The controller's handle to a pane.
///
/// Held by [`TmuxController`]; the paired [`TmuxPty`] is used by the
/// PTY event loop.
pub struct PaneHandle {
    /// Socketpair endpoint — writes send pane output, reads receive user input.
    pub file: UnixStream,

    /// Write one byte here to signal pane exit.
    pub exit_signal: UnixStream,

    /// Associated tmux window (e.g. `@0`).
    pub window_id: String,

    /// This pane's identifier (e.g. `%0`).
    pub pane_id: String,
}

impl PaneHandle {
    /// Raw fd for polling.
    pub fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

/// Create a paired (TmuxPty, PaneHandle) connected via Unix socketpair.
///
/// * `pane_id`   — tmux pane id, e.g. `%0`
/// * `window_id` — tmux window id, e.g. `@0`
/// * `cmd_tx`    — channel for TmuxPty to send commands back to the controller
pub fn create_pane_pair(
    pane_id: String,
    window_id: String,
    cmd_tx: Sender<TmuxPtyCmd>,
) -> io::Result<(TmuxPty, PaneHandle)> {
    let (pty_end, controller_end) = UnixStream::pair()?;
    let (exit_rx, exit_tx) = UnixStream::pair()?;

    pty_end.set_nonblocking(true)?;
    controller_end.set_nonblocking(true)?;
    exit_rx.set_nonblocking(true)?;

    let pty = TmuxPty {
        file: pty_end,
        exit_signal: exit_rx,
        cmd_tx,
        pane_id: pane_id.clone(),
    };

    let handle = PaneHandle {
        file: controller_end,
        exit_signal: exit_tx,
        window_id,
        pane_id,
    };

    Ok((pty, handle))
}
