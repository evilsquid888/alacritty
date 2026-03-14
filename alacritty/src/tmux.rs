//! tmux control mode session manager.
//!
//! Spawns a `tmux -CC` process and manages the bidirectional communication
//! between tmux panes and Alacritty windows.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use log::{debug, error, info, warn};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use alacritty_terminal::tmux::protocol::{self, Notification, PaneInfo};
use alacritty_terminal::tmux::pty::{PaneHandle, TmuxPtyCmd};
use alacritty_terminal::tmux::{TmuxPty, create_pane_pair};

use crate::event::{Event, EventType};

/// Messages sent from the controller thread to the main event loop.
#[derive(Debug, Clone)]
pub enum TmuxEvent {
    /// A new pane was discovered or created; create an Alacritty window for it.
    PaneReady {
        pane_id: String,
        window_id: String,
        title: String,
    },
    /// A tmux window was closed; close the corresponding Alacritty window.
    WindowClosed { window_id: String },
    /// A tmux window was renamed.
    WindowRenamed { window_id: String, name: String },
    /// The tmux session has ended.
    SessionExit,
}

/// Manages the tmux -CC process and routes data between panes and windows.
pub struct TmuxSession {
    /// Channel to receive TmuxPty instances for new panes.
    pty_rx: Receiver<(TmuxPty, String)>,

    /// Mapping from tmux window_id to Alacritty WindowId.
    pub window_map: HashMap<String, WindowId>,

    /// Mapping from Alacritty WindowId to tmux window_id.
    pub reverse_window_map: HashMap<WindowId, String>,

}

impl TmuxSession {
    /// Start a new tmux control mode session.
    ///
    /// Spawns `tmux -CC new-session -A -s <name>` and begins parsing the
    /// control protocol on a background thread. Returns `TmuxPty` instances
    /// for any existing panes through the channel.
    pub fn start(
        session_name: String,
        event_proxy: EventLoopProxy<Event>,
    ) -> io::Result<Self> {
        let (pty_tx, pty_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel::<TmuxPtyCmd>();

        let proxy_clone = event_proxy.clone();
        let name_clone = session_name.clone();

        std::thread::Builder::new().name("tmux controller".into()).spawn(move || {
            if let Err(err) =
                run_controller(&name_clone, proxy_clone, pty_tx, cmd_tx, cmd_rx)
            {
                error!("tmux controller error: {err}");
            }
        })?;

        Ok(TmuxSession {
            pty_rx,
            window_map: HashMap::new(),
            reverse_window_map: HashMap::new(),
        })
    }

    /// Try to receive the next ready TmuxPty (non-blocking).
    ///
    /// Returns `Some((pty, title))` if a pane is ready, `None` otherwise.
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

/// Main controller loop — runs on a dedicated thread.
fn run_controller(
    session_name: &str,
    event_proxy: EventLoopProxy<Event>,
    pty_tx: Sender<(TmuxPty, String)>,
    cmd_tx: Sender<TmuxPtyCmd>,
    cmd_rx: Receiver<TmuxPtyCmd>,
) -> io::Result<()> {
    // Spawn tmux in control mode.
    info!("Starting tmux control mode session: {session_name}");
    let mut child = Command::new("tmux")
        .args(["-CC", "new-session", "-A", "-s", session_name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let tmux_stdout = child.stdout.take().unwrap();
    let mut tmux_stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(tmux_stdout);

    // Skip the initial DCS escape if present.
    skip_dcs_header(&mut reader)?;

    // Wait for the initial %begin/%end handshake.
    wait_initial_handshake(&mut reader)?;

    // Query existing panes.
    info!("Querying existing tmux panes...");
    let panes = query_panes(&mut reader, &mut tmux_stdin)?;
    info!("Found {} pane(s)", panes.len());

    // Pane state: maps pane_id -> PaneHandle.
    let mut pane_handles: HashMap<String, PaneHandle> = HashMap::new();
    // Track which window each pane belongs to.
    let mut pane_to_window: HashMap<String, String> = HashMap::new();

    // Create TmuxPty for each existing pane.
    for pane in &panes {
        let (pty, handle) =
            create_pane_pair(pane.pane_id.clone(), pane.window_id.clone(), cmd_tx.clone())?;

        pane_to_window.insert(pane.pane_id.clone(), pane.window_id.clone());
        pane_handles.insert(pane.pane_id.clone(), handle);

        // Send the PTY and title to the main thread for window creation.
        let _ = pty_tx.send((pty, pane.window_name.clone()));

        // Signal that a pane is ready.
        let _ = event_proxy.send_event(Event::new(
            EventType::TmuxEvent(TmuxEvent::PaneReady {
                pane_id: pane.pane_id.clone(),
                window_id: pane.window_id.clone(),
                title: pane.window_name.clone(),
            }),
            None,
        ));
    }

    // Enter the main I/O loop.
    //
    // We use a polling-based loop to multiplex:
    //   1. Reading lines from tmux stdout (for control protocol notifications).
    //   2. Reading user input from each pane's controller_end fd.
    //   3. Receiving TmuxPtyCmd messages (resize events).
    //
    // For simplicity, we use a blocking line-read approach for tmux stdout
    // and check pane inputs between lines.
    let mut line = String::new();
    let mut cmd_counter: u64 = 1;
    let mut in_response = false;
    let mut response_lines: Vec<String> = Vec::new();

    loop {
        line.clear();

        // Read one line from tmux. This blocks until data is available.
        match reader.read_line(&mut line) {
            Ok(0) => {
                info!("tmux stdout closed");
                break;
            },
            Ok(_) => {},
            Err(err) => {
                error!("Error reading from tmux: {err}");
                break;
            },
        }

        let trimmed = line.trim_end_matches('\n');
        debug!("tmux: {trimmed}");

        let notification = protocol::parse_line(trimmed);

        match notification {
            Notification::Output { ref pane_id, ref data } => {
                if let Some(handle) = pane_handles.get_mut(pane_id) {
                    // Write decoded output to the controller end of the socketpair.
                    // The PTY event loop will read it from the other end.
                    let mut written = 0;
                    while written < data.len() {
                        match handle.file.write(&data[written..]) {
                            Ok(0) => break,
                            Ok(n) => written += n,
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // Socket buffer full — spin briefly.
                                std::thread::yield_now();
                            },
                            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
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
                // Query panes for this new window.
                let new_panes =
                    query_window_panes(&mut reader, &mut tmux_stdin, window_id, &mut cmd_counter)?;

                for pane in &new_panes {
                    if pane_handles.contains_key(&pane.pane_id) {
                        continue;
                    }

                    let (pty, handle) = create_pane_pair(
                        pane.pane_id.clone(),
                        pane.window_id.clone(),
                        cmd_tx.clone(),
                    )?;

                    pane_to_window.insert(pane.pane_id.clone(), pane.window_id.clone());
                    pane_handles.insert(pane.pane_id.clone(), handle);

                    let _ = pty_tx.send((pty, pane.window_name.clone()));
                    let _ = event_proxy.send_event(Event::new(
                        EventType::TmuxEvent(TmuxEvent::PaneReady {
                            pane_id: pane.pane_id.clone(),
                            window_id: pane.window_id.clone(),
                            title: pane.window_name.clone(),
                        }),
                        None,
                    ));
                }
            },

            Notification::WindowClose { ref window_id } => {
                info!("tmux window closed: {window_id}");
                // Signal exit for all panes in this window.
                let panes_to_remove: Vec<String> = pane_to_window
                    .iter()
                    .filter(|(_, wid)| *wid == window_id)
                    .map(|(pid, _)| pid.clone())
                    .collect();

                for pane_id in &panes_to_remove {
                    if let Some(mut handle) = pane_handles.remove(pane_id) {
                        let _ = handle.exit_signal.write(&[1]);
                    }
                    pane_to_window.remove(pane_id);
                }

                let _ = event_proxy.send_event(Event::new(
                    EventType::TmuxEvent(TmuxEvent::WindowClosed {
                        window_id: window_id.clone(),
                    }),
                    None,
                ));
            },

            Notification::WindowRenamed { ref window_id, ref name } => {
                let _ = event_proxy.send_event(Event::new(
                    EventType::TmuxEvent(TmuxEvent::WindowRenamed {
                        window_id: window_id.clone(),
                        name: name.clone(),
                    }),
                    None,
                ));
            },

            Notification::Exit { .. } => {
                info!("tmux session exited");
                let _ = event_proxy.send_event(Event::new(
                    EventType::TmuxEvent(TmuxEvent::SessionExit),
                    None,
                ));
                break;
            },

            Notification::Begin { .. } => {
                in_response = true;
                response_lines.clear();
            },
            Notification::End { .. } | Notification::Error { .. } => {
                in_response = false;
            },
            Notification::ResponseLine(ref _line) if in_response => {
                // Ignore unsolicited response lines.
            },

            _ => {},
        }

        // Check for user input on all pane handles (non-blocking).
        drain_pane_inputs(&mut pane_handles, &mut tmux_stdin);

        // Check for resize commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                TmuxPtyCmd::Resize { ref pane_id, size } => {
                    if let Some(handle) = pane_handles.get(pane_id) {
                        let cmd = format!(
                            "resize-pane -t {} -x {} -y {}\n",
                            handle.pane_id, size.num_cols, size.num_lines,
                        );
                        let _ = tmux_stdin.write_all(cmd.as_bytes());
                        let _ = tmux_stdin.flush();
                    }
                },
            }
        }
    }

    // Signal all remaining panes to exit.
    for (_, handle) in &mut pane_handles {
        let _ = handle.exit_signal.write(&[1]);
    }

    let _ = event_proxy.send_event(Event::new(
        EventType::TmuxEvent(TmuxEvent::SessionExit),
        None,
    ));

    // Wait for the tmux process to finish.
    let _ = child.wait();

    Ok(())
}

/// Skip the DCS escape sequence at the start of tmux control mode.
///
/// tmux sends `\x1bP1000p` (or similar) as a protocol header.
fn skip_dcs_header(reader: &mut BufReader<impl Read>) -> io::Result<()> {
    // Peek at the first byte.
    let buf = reader.fill_buf()?;
    if buf.is_empty() {
        return Ok(());
    }

    // If it starts with ESC (0x1b), read until 'p' (end of DCS).
    if buf[0] == 0x1b {
        let mut byte = [0u8; 1];
        loop {
            reader.read_exact(&mut byte)?;
            if byte[0] == b'p' {
                break;
            }
        }
        // Consume the newline after the DCS.
        let mut line = String::new();
        reader.read_line(&mut line)?;
    }

    Ok(())
}

/// Wait for the initial `%begin`/`%end` handshake.
fn wait_initial_handshake(reader: &mut BufReader<impl Read>) -> io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "tmux closed during handshake"));
        }

        let trimmed = line.trim();
        match protocol::parse_line(trimmed) {
            Notification::End { .. } => {
                debug!("tmux handshake complete");
                return Ok(());
            },
            Notification::Error { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "tmux returned error during handshake",
                ));
            },
            _ => continue,
        }
    }
}

/// Query existing panes with `list-panes -s`.
fn query_panes(
    reader: &mut BufReader<impl Read>,
    stdin: &mut impl Write,
) -> io::Result<Vec<PaneInfo>> {
    // Send the list-panes command.
    let cmd =
        "list-panes -s -F '#{pane_id} #{window_id} #{pane_active} #{pane_width} #{pane_height} #{window_name}'\n";
    stdin.write_all(cmd.as_bytes())?;
    stdin.flush()?;

    // Read response between %begin and %end.
    let mut panes = Vec::new();
    let mut in_response = false;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        match protocol::parse_line(trimmed) {
            Notification::Begin { .. } => {
                in_response = true;
            },
            Notification::End { .. } => break,
            Notification::Error { .. } => {
                warn!("Error querying panes");
                break;
            },
            Notification::ResponseLine(ref data) if in_response => {
                // Remove surrounding quotes from -F format.
                let data = data.trim_matches('\'');
                if let Some(pane) = protocol::parse_pane_info(data) {
                    panes.push(pane);
                }
            },
            _ => {},
        }
    }

    Ok(panes)
}

/// Query panes for a specific window.
fn query_window_panes(
    reader: &mut BufReader<impl Read>,
    stdin: &mut impl Write,
    window_id: &str,
    cmd_counter: &mut u64,
) -> io::Result<Vec<PaneInfo>> {
    let cmd = format!(
        "list-panes -t {window_id} -F '#{{pane_id}} #{{window_id}} #{{pane_active}} #{{pane_width}} #{{pane_height}} #{{window_name}}'\n"
    );
    stdin.write_all(cmd.as_bytes())?;
    stdin.flush()?;
    *cmd_counter += 1;

    let mut panes = Vec::new();
    let mut in_response = false;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        match protocol::parse_line(trimmed) {
            Notification::Begin { .. } => {
                in_response = true;
            },
            Notification::End { .. } => break,
            Notification::Error { .. } => {
                warn!("Error querying panes for window {window_id}");
                break;
            },
            Notification::ResponseLine(ref data) if in_response => {
                let data = data.trim_matches('\'');
                if let Some(pane) = protocol::parse_pane_info(data) {
                    panes.push(pane);
                }
            },
            // Handle any output notifications that arrive between our command.
            Notification::Output { .. } => {
                // These will be picked up by the main loop later.
                // For now, we can't easily dispatch them without the pane handles.
            },
            _ => {},
        }
    }

    Ok(panes)
}

/// Non-blocking read of user input from all pane handles, forwarding to tmux stdin.
fn drain_pane_inputs(handles: &mut HashMap<String, PaneHandle>, tmux_stdin: &mut impl Write) {
    let mut buf = [0u8; 4096];

    for (pane_id, handle) in handles.iter_mut() {
        loop {
            match handle.file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Encode as hex keys and send to tmux.
                    let keys = protocol::encode_keys(&buf[..n]);
                    let cmd = format!("send-keys -t {pane_id} {keys}\n");
                    let _ = tmux_stdin.write_all(cmd.as_bytes());
                    let _ = tmux_stdin.flush();
                },
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
}
