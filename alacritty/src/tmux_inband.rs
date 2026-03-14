//! In-band tmux control mode manager.
//!
//! When a user runs `tmux -CC` inside an existing Alacritty terminal,
//! the DCS sequence `\033P1000p` is detected by the VTE parser. The PTY
//! event loop switches to line-based parsing and emits
//! [`TmuxCCNotification`] events. This module processes those notifications
//! and manages the resulting pane windows.

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};

use log::{debug, error, info, warn};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use alacritty_terminal::event_loop::Notifier;
use alacritty_terminal::event::Notify;
use alacritty_terminal::tmux::protocol::{self, Notification};
use alacritty_terminal::tmux::pty::TmuxPtyCmd;
use alacritty_terminal::tmux::{TmuxPty, PaneHandle, create_pane_pair};

use crate::event::{Event, EventType};
use crate::tmux::TmuxEvent;

/// Manages in-band tmux control mode state.
///
/// Created when a DCS `\033P1000p` is detected in a terminal window's output.
/// The source window's PTY remains connected to the tmux process — we send
/// commands by writing to its [`Notifier`].
pub struct InBandTmuxState {
    /// The Alacritty WindowId of the window where tmux -CC was started.
    pub source_window_id: WindowId,

    /// Pane handles keyed by tmux pane id (e.g. `%0`).
    pane_handles: HashMap<String, PaneHandle>,

    /// Maps tmux pane id → tmux window id.
    pane_to_window: HashMap<String, String>,

    /// Maps tmux window id → Alacritty WindowId.
    pub window_map: HashMap<String, WindowId>,

    /// Maps Alacritty WindowId → tmux window id.
    pub reverse_window_map: HashMap<WindowId, String>,

    /// Channel to receive TmuxPty instances for new panes.
    pty_rx: Receiver<(TmuxPty, String)>,
    pty_tx: Sender<(TmuxPty, String)>,

    /// Channel sender for TmuxPtyCmd (resize events from pane windows).
    cmd_tx: Sender<TmuxPtyCmd>,
    cmd_rx: Option<Receiver<TmuxPtyCmd>>,

    /// Whether we've completed the initial handshake.
    handshake_done: bool,

    /// Whether we're currently inside a %begin/%end response block.
    in_response: bool,

    /// Accumulates response lines between %begin and %end.
    response_lines: Vec<String>,

    /// Whether we've queried initial panes.
    initial_query_sent: bool,

    /// Event proxy for sending events to the main loop.
    event_proxy: EventLoopProxy<Event>,
}

impl InBandTmuxState {
    pub fn new(source_window_id: WindowId, event_proxy: EventLoopProxy<Event>) -> Self {
        let (pty_tx, pty_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();

        InBandTmuxState {
            source_window_id,
            pane_handles: HashMap::new(),
            pane_to_window: HashMap::new(),
            window_map: HashMap::new(),
            reverse_window_map: HashMap::new(),
            pty_rx,
            pty_tx,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            handshake_done: false,
            in_response: false,
            response_lines: Vec::new(),
            initial_query_sent: false,
            event_proxy,
        }
    }

    /// Process a tmux control mode notification line.
    ///
    /// `notifier` is the Notifier for the source window's PTY — used to
    /// send commands to the tmux process.
    pub fn process_line(&mut self, line: &str, notifier: &Notifier) {
        let notification = protocol::parse_line(line);

        match notification {
            Notification::Begin { .. } => {
                self.in_response = true;
                self.response_lines.clear();
            },
            Notification::End { .. } => {
                self.in_response = false;
                if !self.handshake_done {
                    self.handshake_done = true;
                    info!("tmux in-band handshake complete");

                    // Query existing panes.
                    self.send_command(notifier, "list-panes -s -F '#{pane_id} #{window_id} #{pane_active} #{pane_width} #{pane_height} #{window_name}'");
                    self.initial_query_sent = true;
                } else if self.initial_query_sent {
                    // Process the response as pane info.
                    self.process_pane_list_response();
                    self.initial_query_sent = false;
                }
            },
            Notification::Error { .. } => {
                self.in_response = false;
                if !self.handshake_done {
                    error!("tmux returned error during handshake");
                }
            },
            Notification::ResponseLine(ref data) if self.in_response => {
                self.response_lines.push(data.clone());
            },

            Notification::Output { ref pane_id, ref data } => {
                if let Some(handle) = self.pane_handles.get_mut(pane_id) {
                    let mut written = 0;
                    while written < data.len() {
                        match handle.file.write(&data[written..]) {
                            Ok(0) => break,
                            Ok(n) => written += n,
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::yield_now();
                            },
                            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(err) => {
                                warn!("Error writing output to pane {pane_id}: {err}");
                                break;
                            },
                        }
                    }
                }
            },

            Notification::WindowAdd { ref window_id } => {
                info!("tmux window added: {window_id}");
                // Query panes for this window.
                let cmd = format!(
                    "list-panes -t {window_id} -F '#{{pane_id}} #{{window_id}} #{{pane_active}} #{{pane_width}} #{{pane_height}} #{{window_name}}'"
                );
                self.send_command(notifier, &cmd);
                self.initial_query_sent = true;
            },

            Notification::WindowClose { ref window_id } => {
                info!("tmux window closed: {window_id}");
                let panes_to_remove: Vec<String> = self
                    .pane_to_window
                    .iter()
                    .filter(|(_, wid)| *wid == window_id)
                    .map(|(pid, _)| pid.clone())
                    .collect();

                for pane_id in &panes_to_remove {
                    if let Some(mut handle) = self.pane_handles.remove(pane_id) {
                        let _ = handle.exit_signal.write(&[1]);
                    }
                    self.pane_to_window.remove(pane_id);
                }

                let _ = self.event_proxy.send_event(Event::new(
                    EventType::TmuxEvent(TmuxEvent::WindowClosed {
                        window_id: window_id.clone(),
                    }),
                    None,
                ));
            },

            Notification::WindowRenamed { ref window_id, ref name } => {
                let _ = self.event_proxy.send_event(Event::new(
                    EventType::TmuxEvent(TmuxEvent::WindowRenamed {
                        window_id: window_id.clone(),
                        name: name.clone(),
                    }),
                    None,
                ));
            },

            Notification::Exit { .. } => {
                info!("tmux session exited (in-band)");
                // Signal all panes to exit.
                for (_, handle) in &mut self.pane_handles {
                    let _ = handle.exit_signal.write(&[1]);
                }
                let _ = self.event_proxy.send_event(Event::new(
                    EventType::TmuxEvent(TmuxEvent::SessionExit),
                    None,
                ));
            },

            _ => {},
        }

        // Drain resize commands from pane windows.
        if let Some(ref cmd_rx) = self.cmd_rx {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    TmuxPtyCmd::Resize { ref pane_id, size } => {
                        let resize_cmd = format!(
                            "resize-pane -t {} -x {} -y {}",
                            pane_id, size.num_cols, size.num_lines,
                        );
                        self.send_command(notifier, &resize_cmd);
                    },
                }
            }
        }

        // Drain user input from all pane handles and send as keys.
        self.drain_pane_inputs(notifier);
    }

    /// Process a list-panes response accumulated in `response_lines`.
    fn process_pane_list_response(&mut self) {
        let lines = std::mem::take(&mut self.response_lines);
        for line in &lines {
            let cleaned = line.trim_matches('\'');
            if let Some(pane) = protocol::parse_pane_info(cleaned) {
                if self.pane_handles.contains_key(&pane.pane_id) {
                    continue;
                }

                match create_pane_pair(
                    pane.pane_id.clone(),
                    pane.window_id.clone(),
                    self.cmd_tx.clone(),
                ) {
                    Ok((pty, handle)) => {
                        self.pane_to_window
                            .insert(pane.pane_id.clone(), pane.window_id.clone());
                        self.pane_handles.insert(pane.pane_id.clone(), handle);
                        let _ = self.pty_tx.send((pty, pane.window_name.clone()));

                        let _ = self.event_proxy.send_event(Event::new(
                            EventType::TmuxEvent(TmuxEvent::PaneReady {
                                pane_id: pane.pane_id.clone(),
                                window_id: pane.window_id.clone(),
                                title: pane.window_name.clone(),
                            }),
                            None,
                        ));
                    },
                    Err(err) => {
                        error!("Failed to create pane pair: {err}");
                    },
                }
            }
        }
    }

    /// Send a tmux command through the source window's PTY.
    fn send_command(&self, notifier: &Notifier, cmd: &str) {
        debug!("tmux cmd: {cmd}");
        let mut bytes = cmd.as_bytes().to_vec();
        bytes.push(b'\n');
        notifier.notify(bytes);
    }

    /// Read user input from pane handles and forward as send-keys commands.
    fn drain_pane_inputs(&mut self, notifier: &Notifier) {
        let mut buf = [0u8; 4096];
        let mut commands = Vec::new();

        for (pane_id, handle) in self.pane_handles.iter_mut() {
            loop {
                match std::io::Read::read(&mut handle.file, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let keys = protocol::encode_keys(&buf[..n]);
                        commands.push(format!("send-keys -t {pane_id} {keys}"));
                    },
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }

        for cmd in &commands {
            self.send_command(notifier, cmd);
        }
    }

    /// Try to receive the next ready TmuxPty (non-blocking).
    pub fn try_recv_pty(&self) -> Option<(TmuxPty, String)> {
        self.pty_rx.try_recv().ok()
    }

    /// Register mapping between tmux window and Alacritty window.
    pub fn register_window(&mut self, tmux_window_id: String, alacritty_window_id: WindowId) {
        self.reverse_window_map.insert(alacritty_window_id, tmux_window_id.clone());
        self.window_map.insert(tmux_window_id, alacritty_window_id);
    }

    /// Look up the Alacritty WindowId for a tmux window.
    pub fn get_alacritty_window(&self, tmux_window_id: &str) -> Option<WindowId> {
        self.window_map.get(tmux_window_id).copied()
    }
}
