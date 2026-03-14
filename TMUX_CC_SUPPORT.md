# tmux -CC Control Mode Support for Alacritty

## Overview

This feature adds tmux control mode (`tmux -CC`) integration to Alacritty, allowing each tmux window/pane to be rendered as a native Alacritty window. It supports both local sessions via a CLI flag and in-band detection for remote servers (like iTerm2's tmux integration).

**Platform support: Linux and WSL only.** This feature uses Unix-specific APIs (Unix socketpairs, Unix domain sockets, signal handling) and is gated behind `#[cfg(unix)]`. It does not compile or run on native Windows.

Branch: `feature/tmux-cc-support`
Remote: https://github.com/evilsquid888/alacritty

**Status: Tested and working.** The `--tmux` flag opens a window, renders a shell prompt with colors, accepts keyboard input, and executes commands. Verified on X11 with tmux 3.4.

---

## How to Use

### Local session (--tmux flag)

```bash
# Start or attach to a tmux session named "mysession"
alacritty --tmux mysession

# Reconnect later to the same session
alacritty --tmux mysession

# Use default session name "alacritty"
alacritty --tmux alacritty
```

The `--tmux` flag spawns `tmux -CC new-session -A -s <name>`, which:
- Attaches to an existing session if one with that name exists (restoring all windows)
- Creates a new session if none exists
- The session persists after Alacritty exits

### Remote server (in-band detection)

No special flags needed. Works through any SSH connection:

```bash
# 1. Open a normal Alacritty terminal
# 2. SSH into your server
ssh user@remote-server

# 3. Start tmux in control mode on the remote
tmux -CC                    # new session
tmux -CC attach             # attach to existing session
tmux -CC new-session -A -s mywork   # create or attach by name
```

Alacritty detects the tmux DCS escape sequence (`\x1bP1000p`) in the terminal output and automatically switches to tmux integration mode. Each tmux pane becomes a native Alacritty window.

---

## Architecture

### Data Flow

```
tmux process (stdout)
  -> Controller (parses %output, decodes octal-escaped data)
  -> Unix socketpair (controller end -> write)
  -> PTY event loop (reads from socketpair, feeds to VTE parser)
  -> Terminal grid update -> Display render

User keyboard input
  -> PTY event loop (writes to socketpair)
  -> Controller (reads, formats as `send-keys` hex command)
  -> tmux process (stdin)
```

### Key Components

#### Protocol Parser (`alacritty_terminal/src/tmux/protocol.rs`)
- Parses tmux control mode notifications: `%output`, `%window-add`, `%window-close`, `%window-renamed`, `%begin`/`%end`, `%exit`, etc.
- Decodes octal-escaped output data (e.g. `\012` -> newline, `\\` -> backslash)
- Encodes user input as hex key sequences for `send-keys`
- Parses `list-panes` response format

#### Virtual PTY (`alacritty_terminal/src/tmux/pty.rs`)
- `TmuxPty`: implements `EventedPty` and `EventedReadWrite` using a Unix socketpair
- One end is used by the PTY event loop (reads pane output, writes user input)
- Other end (`PaneHandle`) is used by the controller (writes pane output, reads user input)
- Supports resize commands via an mpsc channel back to the controller
- Exit signaling via a separate socketpair

#### Local Controller (`alacritty/src/tmux.rs`)
- `TmuxSession`: manages a locally spawned `tmux -CC` process
- Runs on a dedicated thread
- Parses control protocol from tmux stdout
- Routes `%output` data to the correct pane's socketpair
- Reads user input from all pane socketpairs and sends as `send-keys`
- Handles window add/close/rename events
- Queries existing panes on startup with `list-panes`

#### In-Band Controller (`alacritty/src/tmux_inband.rs`)
- `InBandTmuxState`: manages tmux detected via DCS in an existing terminal
- Processes notification lines received as `TmuxCCNotification` events
- Creates `TmuxPty`/`PaneHandle` pairs for discovered panes
- Sends commands back to tmux through the source window's PTY notifier
- Handles the initial handshake and pane discovery

#### Event Loop Integration (`alacritty_terminal/src/event_loop.rs`)
- Scans raw PTY bytes for the tmux DCS header (`\x1bP1000p`) before VTE parsing
- When detected, switches to `tmux_cc_mode`: line-buffers bytes and emits `TmuxCCNotification` events instead of feeding to the VTE parser
- In tmux mode, skips the terminal lock entirely (no VTE parsing needed)

#### Event Handling (`alacritty/src/event.rs`)
- `TmuxEvent` variants: `PaneReady`, `WindowClosed`, `WindowRenamed`, `SessionExit`
- `TmuxCCNotification` terminal event for in-band mode
- Window creation via `WindowContext::initial_with_tmux_pty` and `WindowContext::with_tmux_pty`
- Source window hidden in in-band mode

---

## Files Changed

### New Files

| File | Lines | Purpose |
|------|-------|---------|
| `alacritty_terminal/src/tmux/mod.rs` | 11 | Module root, exports |
| `alacritty_terminal/src/tmux/protocol.rs` | 230 | tmux control mode protocol parser and tests |
| `alacritty_terminal/src/tmux/pty.rs` | 184 | TmuxPty virtual PTY via Unix socketpair |
| `alacritty/src/tmux.rs` | 340 | Local tmux session controller |
| `alacritty/src/tmux_inband.rs` | 300 | In-band tmux detection and management |

### Modified Files

| File | Change |
|------|--------|
| `alacritty_terminal/src/lib.rs` | Added `pub mod tmux` |
| `alacritty_terminal/src/event.rs` | Added `TmuxCCNotification` event variant |
| `alacritty_terminal/src/event_loop.rs` | DCS detection, tmux line parsing mode |
| `alacritty/src/cli.rs` | Added `--tmux <SESSION>` CLI flag |
| `alacritty/src/main.rs` | Added `mod tmux` and `mod tmux_inband` |
| `alacritty/src/event.rs` | `TmuxEvent` handling, tmux window creation, in-band line processing |
| `alacritty/src/window_context.rs` | `initial_with_tmux_pty`, `with_tmux_pty`, `notifier()` accessor |

---

## Tests

All 187 existing tests pass with no regressions:
- 141 unit tests (including 9 new tmux protocol tests)
- 45 reference tests
- 1 doctest

Run tests with:
```bash
cargo test -p alacritty_terminal
```

---

## Building

```bash
# Install dependencies (Ubuntu/Debian)
sudo apt-get install pkg-config libfontconfig1-dev libfreetype6-dev \
    libxcb-xfixes0-dev libxkbcommon-dev cmake python3

# Build
cargo build --release

# Run with tmux support
./target/release/alacritty --tmux mysession
```

---

## tmux Control Mode Protocol Reference

When tmux runs in control mode (`-CC`), it communicates via a text protocol on stdout/stdin:

### Notifications (tmux -> client)
- `%output %<pane_id> <octal-escaped-data>` - terminal output from a pane
- `%window-add @<window_id>` - new window created
- `%window-close @<window_id>` - window closed
- `%window-renamed @<window_id> <name>` - window title changed
- `%begin <time> <num> <flags>` - start of command response
- `%end <time> <num> <flags>` - successful end of response
- `%error <time> <num> <flags>` - error end of response
- `%exit [reason]` - tmux client exiting

### Commands (client -> tmux)
- `send-keys -t %<pane_id> 0xHH ...` - send hex-encoded keys to pane
- `resize-pane -t %<pane_id> -x <cols> -y <rows>` - resize pane
- `list-panes -s -F '#{pane_id} ...'` - query existing panes
- `new-window` / `kill-window` - window management

### DCS Header
tmux sends `\x1bP1000p` (ESC P 1000 p) as a Device Control String to identify the start of control mode. This is what Alacritty detects for in-band mode.
