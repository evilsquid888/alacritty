//! tmux control mode protocol parser.
//!
//! Parses the structured text protocol that tmux sends when running in
//! control mode (`tmux -CC`). Each line from tmux stdout starting with `%`
//! is a notification.

/// A parsed tmux control mode notification.
#[derive(Debug, Clone)]
pub enum Notification {
    /// Output from a pane: `%output %<pane_id> <escaped_data>`.
    Output { pane_id: String, data: Vec<u8> },

    /// A window was added: `%window-add @<window_id>`.
    WindowAdd { window_id: String },

    /// A window was closed: `%window-close @<window_id>`.
    WindowClose { window_id: String },

    /// A window was renamed: `%window-renamed @<window_id> <name>`.
    WindowRenamed { window_id: String, name: String },

    /// A window's pane changed: `%window-pane-changed @<window_id> %<pane_id>`.
    WindowPaneChanged { window_id: String, pane_id: String },

    /// Layout changed: `%layout-change @<window_id> <layout> ...`.
    LayoutChange { window_id: String, layout: String },

    /// Session changed: `%session-changed $<id> <name>`.
    SessionChanged { session_id: String, name: String },

    /// Start of a command response: `%begin <time> <num> <flags>`.
    Begin { time: String, number: u64, flags: u64 },

    /// Successful end of command response: `%end <time> <num> <flags>`.
    End { time: String, number: u64, flags: u64 },

    /// Error end of command response: `%error <time> <num> <flags>`.
    Error { time: String, number: u64, flags: u64 },

    /// tmux is exiting: `%exit [reason]`.
    Exit { reason: Option<String> },

    /// A non-notification line (command response data between %begin/%end).
    ResponseLine(String),

    /// Unrecognized notification.
    Unknown(String),
}

/// Information about a tmux pane from `list-panes` output.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub window_id: String,
    pub width: u16,
    pub height: u16,
    pub window_name: String,
    pub active: bool,
}

/// Parse a single line from tmux control mode output.
pub fn parse_line(line: &str) -> Notification {
    if !line.starts_with('%') {
        return Notification::ResponseLine(line.to_string());
    }

    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    let cmd = parts[0];

    match cmd {
        "%output" if parts.len() >= 3 => {
            let pane_id = parts[1].to_string();
            let data = decode_output(parts[2]);
            Notification::Output { pane_id, data }
        },
        "%window-add" if parts.len() >= 2 => {
            Notification::WindowAdd { window_id: parts[1].to_string() }
        },
        "%window-close" if parts.len() >= 2 => {
            Notification::WindowClose { window_id: parts[1].to_string() }
        },
        "%window-renamed" if parts.len() >= 3 => Notification::WindowRenamed {
            window_id: parts[1].to_string(),
            name: parts[2].to_string(),
        },
        "%window-pane-changed" if parts.len() >= 3 => Notification::WindowPaneChanged {
            window_id: parts[1].to_string(),
            pane_id: parts[2].to_string(),
        },
        "%layout-change" if parts.len() >= 3 => Notification::LayoutChange {
            window_id: parts[1].to_string(),
            layout: parts[2].to_string(),
        },
        "%session-changed" if parts.len() >= 3 => Notification::SessionChanged {
            session_id: parts[1].to_string(),
            name: parts[2].to_string(),
        },
        "%begin" => parse_begin_end_error(line, |time, number, flags| Notification::Begin {
            time,
            number,
            flags,
        }),
        "%end" => {
            parse_begin_end_error(line, |time, number, flags| Notification::End {
                time,
                number,
                flags,
            })
        },
        "%error" => {
            parse_begin_end_error(line, |time, number, flags| Notification::Error {
                time,
                number,
                flags,
            })
        },
        "%exit" => {
            let reason = if parts.len() >= 2 {
                Some(parts[1..].join(" "))
            } else {
                None
            };
            Notification::Exit { reason }
        },
        _ => Notification::Unknown(line.to_string()),
    }
}

/// Parse `%begin`/`%end`/`%error` lines: `%cmd <time> <number> <flags>`.
fn parse_begin_end_error<F>(line: &str, f: F) -> Notification
where
    F: FnOnce(String, u64, u64) -> Notification,
{
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() >= 4 {
        let time = parts[1].to_string();
        let number = parts[2].parse().unwrap_or(0);
        let flags = parts[3].parse().unwrap_or(0);
        f(time, number, flags)
    } else {
        Notification::Unknown(line.to_string())
    }
}

/// Decode tmux control mode output encoding.
///
/// tmux encodes non-printable bytes as octal (`\ooo`) and backslashes as `\\`.
pub fn decode_output(encoded: &str) -> Vec<u8> {
    let bytes = encoded.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'\\' {
                result.push(b'\\');
                i += 2;
            } else if i + 3 < bytes.len()
                && bytes[i + 1].is_ascii_digit()
                && bytes[i + 2].is_ascii_digit()
                && bytes[i + 3].is_ascii_digit()
            {
                // Octal escape: \ooo
                let val = (bytes[i + 1] - b'0') as u16 * 64
                    + (bytes[i + 2] - b'0') as u16 * 8
                    + (bytes[i + 3] - b'0') as u16;
                result.push(val as u8);
                i += 4;
            } else {
                // Not a valid escape, pass through.
                result.push(bytes[i]);
                i += 1;
            }
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }

    result
}

/// Encode bytes for tmux `send-keys` in control mode.
///
/// Escapes special characters so they can be sent as hex keys.
pub fn encode_keys(data: &[u8]) -> String {
    let mut result = String::new();
    for &byte in data {
        // Send each byte as a hex key sequence.
        result.push_str(&format!("0x{:02X} ", byte));
    }
    result
}

/// Parse a line from `list-panes -s -F` output.
///
/// Expected format: `%<id> @<wid> <active> <width> <height> <name>`
pub fn parse_pane_info(line: &str) -> Option<PaneInfo> {
    let parts: Vec<&str> = line.splitn(6, ' ').collect();
    if parts.len() < 6 {
        return None;
    }

    Some(PaneInfo {
        pane_id: parts[0].to_string(),
        window_id: parts[1].to_string(),
        active: parts[2] == "1",
        width: parts[3].parse().ok()?,
        height: parts[4].parse().ok()?,
        window_name: parts[5].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_simple_text() {
        assert_eq!(decode_output("hello"), b"hello");
    }

    #[test]
    fn decode_octal_newline() {
        assert_eq!(decode_output("hello\\012world"), b"hello\nworld");
    }

    #[test]
    fn decode_escaped_backslash() {
        assert_eq!(decode_output("path\\\\file"), b"path\\file");
    }

    #[test]
    fn decode_mixed() {
        let decoded = decode_output("\\033[1mBold\\033[0m");
        assert_eq!(decoded, b"\x1b[1mBold\x1b[0m");
    }

    #[test]
    fn parse_output_notification() {
        let line = "%output %0 hello\\012world";
        match parse_line(line) {
            Notification::Output { pane_id, data } => {
                assert_eq!(pane_id, "%0");
                assert_eq!(data, b"hello\nworld");
            },
            other => panic!("Expected Output, got {:?}", other),
        }
    }

    #[test]
    fn parse_window_add() {
        match parse_line("%window-add @1") {
            Notification::WindowAdd { window_id } => assert_eq!(window_id, "@1"),
            other => panic!("Expected WindowAdd, got {:?}", other),
        }
    }

    #[test]
    fn parse_begin() {
        match parse_line("%begin 1234567890 1 0") {
            Notification::Begin { number, flags, .. } => {
                assert_eq!(number, 1);
                assert_eq!(flags, 0);
            },
            other => panic!("Expected Begin, got {:?}", other),
        }
    }

    #[test]
    fn parse_exit() {
        match parse_line("%exit") {
            Notification::Exit { reason } => assert_eq!(reason, None),
            other => panic!("Expected Exit, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_line() {
        match parse_line("some response data") {
            Notification::ResponseLine(s) => assert_eq!(s, "some response data"),
            other => panic!("Expected ResponseLine, got {:?}", other),
        }
    }
}
