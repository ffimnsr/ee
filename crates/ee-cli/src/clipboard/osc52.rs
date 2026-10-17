//! OSC 52 terminal clipboard writes.
//!
//! The escape sequence is `ESC ] 52 ; c ; <base64> BEL`. It sets the clipboard
//! of the terminal emulator, so it works over SSH and inside containers where
//! no display server is reachable. Terminals that do not implement OSC 52
//! ignore the sequence, which makes emitting it always safe.
//!
//! Inside tmux or screen the sequence is swallowed unless it is wrapped in a
//! device-control-string passthrough, so both wrappers are emitted when the
//! multiplexer environment variables are present.

#[cfg(not(test))]
use std::io::Write;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::policy::TerminalWrap;

/// Why an OSC 52 write did not reach the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Osc52Error {
    /// Payload exceeds the configured cap; nothing was written.
    TooLarge { len: usize, cap: usize },
    /// No controlling terminal to write to.
    NoTerminal,
    /// The terminal rejected the write.
    #[cfg_attr(test, allow(dead_code))]
    Write,
}

impl Osc52Error {
    /// Human-readable status message, shown once per failure streak.
    pub(crate) fn message(self) -> String {
        match self {
            Self::TooLarge { len, cap } => format!(
                "clipboard text too large for OSC 52 ({len} > {cap} bytes) — not copied to the terminal"
            ),
            Self::NoTerminal => String::from("no terminal available for OSC 52 clipboard copy"),
            Self::Write => String::from("failed to write the OSC 52 clipboard sequence"),
        }
    }
}

/// Build the OSC 52 sequence carrying `text`.
///
/// The `c` parameter selects the clipboard selection; most terminal emulators
/// map it to the system clipboard. `wrap` adds the tmux/screen passthrough
/// envelope. Both selections use `c`: terminals that only implement the system
/// clipboard ignore the primary-selection parameter, which is a documented
/// limitation of OSC 52 rather than a bug.
pub(super) fn sequence(text: &str, wrap: TerminalWrap) -> String {
    let body = format!("\x1b]52;c;{}\x07", STANDARD.encode(text));
    match wrap {
        TerminalWrap::None => body,
        // Every `ESC` inside the passthrough payload must be doubled.
        TerminalWrap::Tmux => format!("\x1bPtmux;{}\x1b\\", body.replace('\x1b', "\x1b\x1b")),
        TerminalWrap::Screen => format!("\x1bP{body}\x1b\\"),
    }
}

/// Validate a payload against the configured cap.
///
/// The cap is measured on the raw bytes before base64, matching the
/// `clipboard_osc52_max_bytes` option.
pub(super) fn check_cap(len: usize, cap: usize) -> Result<(), Osc52Error> {
    if len > cap { Err(Osc52Error::TooLarge { len, cap }) } else { Ok(()) }
}

/// Emit the OSC 52 sequence for `text`.
///
/// Returns `Ok(())` when the sequence was written; that does not prove the
/// terminal honoured it. Writes to `/dev/tty` so ratatui's alternate-screen
/// buffering on stdout cannot swallow the sequence. When no controlling
/// terminal is open the write is skipped — never redirected to stdout, which
/// would corrupt the frame ratatui is drawing.
#[cfg(not(test))]
pub(super) fn write(text: &str, max_bytes: usize, wrap: TerminalWrap) -> Result<(), Osc52Error> {
    check_cap(text.len(), max_bytes)?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|_| Osc52Error::NoTerminal)?;
    write!(out, "{}", sequence(text, wrap)).map_err(|_| Osc52Error::Write)?;
    out.flush().map_err(|_| Osc52Error::Write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_matches_osc52_wire_format() {
        assert_eq!(sequence("hi", TerminalWrap::None), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn sequence_encodes_multibyte_text() {
        assert_eq!(sequence("é", TerminalWrap::None), "\x1b]52;c;w6k=\x07");
    }

    #[test]
    fn sequence_encodes_empty_text() {
        assert_eq!(sequence("", TerminalWrap::None), "\x1b]52;c;\x07");
    }

    #[test]
    fn tmux_wrapper_doubles_the_inner_escape() {
        assert_eq!(sequence("hi", TerminalWrap::Tmux), "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
    }

    #[test]
    fn screen_wrapper_uses_a_bare_passthrough() {
        assert_eq!(sequence("hi", TerminalWrap::Screen), "\x1bP\x1b]52;c;aGk=\x07\x1b\\");
    }

    #[test]
    fn cap_boundary_is_inclusive() {
        assert_eq!(check_cap(16_384, 16_384), Ok(()));
        assert_eq!(check_cap(0, 16_384), Ok(()));
        assert_eq!(
            check_cap(16_385, 16_384),
            Err(Osc52Error::TooLarge { len: 16_385, cap: 16_384 })
        );
    }

    #[test]
    fn error_messages_name_the_failure() {
        let message = Osc52Error::TooLarge { len: 20_000, cap: 16_384 }.message();
        assert!(message.contains("20000"));
        assert!(message.contains("16384"));
        assert!(!Osc52Error::NoTerminal.message().is_empty());
    }
}
