//! Clipboard selection identity, routing policy, and transport selection.
//!
//! Register routing (`clipboard=unnamedplus`) is resolved here and consumed by
//! [`crate::registers`]. This module also owns the pure decisions the worker
//! applies: which transport to try first (`auto` prefers OSC 52 over SSH),
//! which multiplexer wrapper OSC 52 needs, and how an `arboard` error maps onto
//! retry/fallback/surface. Keeping the decisions here — rather than inside the
//! `arboard`-only worker — is what makes them unit-testable without a display
//! server.

/// Which OS clipboard a register targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ClipboardSelection {
    /// `+` — the system clipboard, used by explicit copy/paste actions.
    Clipboard,
    /// `*` — the primary selection on Linux (middle-click paste).  macOS and
    /// Windows have no primary selection, so `*` targets the system clipboard
    /// there.
    Primary,
}

/// A transport that can carry a clipboard operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    /// The platform clipboard through `arboard` (read and write).
    Arboard,
    /// A terminal OSC 52 escape sequence. Write-only: terminals do not answer
    /// OSC 52 reads.
    Osc52,
}

/// Ordered transports for one clipboard operation.
///
/// `primary` is tried first; `fallback` is tried only when `primary` cannot
/// serve the request (an unavailable display server, or a transport that
/// reports `ClipboardNotSupported`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransportPlan {
    pub(crate) primary: Transport,
    pub(crate) fallback: Option<Transport>,
}

/// Terminal multiplexer whose OSC 52 sequences need passthrough wrapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalWrap {
    /// Emit the bare OSC 52 sequence.
    #[default]
    None,
    /// `TMUX` is set: wrap in a DCS passthrough.
    Tmux,
    /// `STY` is set: wrap in a screen passthrough.
    Screen,
}

/// Default OSC 52 payload cap in raw bytes, measured before base64.
///
/// Terminals buffer escape sequences in memory; a multi-megabyte yank can wedge
/// them. 16 KiB covers every practical line/paragraph yank.
pub(crate) const DEFAULT_OSC52_MAX_BYTES: usize = 16_384;

/// Resolved clipboard settings owned by [`crate::clipboard::ClipboardHandle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipboardConfig {
    /// Transport preference.
    pub(crate) backend: ClipboardBackendKind,
    /// Max raw bytes mirrored through OSC 52, measured before base64.
    pub(crate) osc52_max_bytes: usize,
    /// Hold the selection briefly on exit so the last yank survives.
    pub(crate) wait_on_exit: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            backend: ClipboardBackendKind::default(),
            osc52_max_bytes: DEFAULT_OSC52_MAX_BYTES,
            wait_on_exit: true,
        }
    }
}

/// Environment facts that pick the transport, detected once per process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClipboardEnvironment {
    /// Running inside an SSH session. The remote X11/Wayland clipboard is not
    /// the user's local clipboard, so OSC 52 to the terminal is correct.
    pub(crate) remote: bool,
    /// Multiplexer that needs OSC 52 passthrough wrapping.
    pub(crate) wrap: TerminalWrap,
}

impl ClipboardEnvironment {
    /// Detect from the process environment.
    pub(crate) fn detect() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Detect from an injected lookup so tests never touch process env.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let set = |key: &str| lookup(key).is_some_and(|value| !value.is_empty());
        let remote = set("SSH_CONNECTION") || set("SSH_TTY") || set("SSH_CLIENT");
        let wrap = if set("TMUX") {
            TerminalWrap::Tmux
        } else if set("STY") {
            TerminalWrap::Screen
        } else {
            TerminalWrap::None
        };
        Self { remote, wrap }
    }
}

/// What the worker does after a transport reports an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureAction {
    /// Retry the same transport once after a short pause (`ClipboardOccupied`).
    Retry,
    /// Move on to the fallback transport.
    Fallback,
    /// Treat the result as authoritative: an empty read, or a write with
    /// nothing to report.
    Ignore,
    /// Report one status message for this failure streak.
    Surface,
}

/// Map an `arboard` write error onto the recovery action.
///
/// `ContentNotAvailable` on a write means there was nothing to place; it is not
/// worth a message. `ClipboardOccupied` is transient. Everything else either
/// falls back (when a fallback exists) or surfaces.
pub(crate) fn classify_write_error(error: &arboard::Error) -> FailureAction {
    match error {
        arboard::Error::ClipboardOccupied => FailureAction::Retry,
        arboard::Error::ClipboardNotSupported | arboard::Error::Unknown { .. } => {
            FailureAction::Fallback
        }
        arboard::Error::ContentNotAvailable => FailureAction::Ignore,
        arboard::Error::ConversionFailure => FailureAction::Surface,
        // `arboard::Error` is `#[non_exhaustive]`.
        _ => FailureAction::Surface,
    }
}

/// Map an `arboard` read error onto the recovery action.
///
/// `ContentNotAvailable` is an authoritative empty clipboard: the caller must
/// not substitute a cached value, and no message is warranted.
pub(crate) fn classify_read_error(error: &arboard::Error) -> FailureAction {
    match error {
        arboard::Error::ContentNotAvailable => FailureAction::Ignore,
        arboard::Error::ClipboardOccupied => FailureAction::Retry,
        arboard::Error::ClipboardNotSupported
        | arboard::Error::ConversionFailure
        | arboard::Error::Unknown { .. } => FailureAction::Surface,
        _ => FailureAction::Surface,
    }
}

/// How plain (prefix-less) yank/delete/paste interact with the OS clipboard.
///
/// Mirrors Vim's `clipboard` option: with `unnamedplus` the unnamed register is
/// aliased to `+`, with `unnamed` to `*`, and with `off` it stays internal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ClipboardRouting {
    /// `clipboard=unnamedplus` (default): plain operations mirror to `+`.
    #[default]
    UnnamedPlus,
    /// `clipboard=unnamed`: plain operations mirror to `*`.
    Unnamed,
    /// `clipboard=off` or an empty value: plain operations stay internal.
    Off,
}

impl ClipboardRouting {
    /// Parse a Vim-style `clipboard` value.
    ///
    /// The option is a comma-separated list; `unnamedplus` wins over `unnamed`
    /// regardless of order, and `off` disables routing. Unknown tokens are
    /// returned so the caller can warn.
    ///
    /// Returns `None` when the value is non-empty but names no known token, so a
    /// typo cannot silently downgrade routing to `off`; an empty value is a
    /// deliberate `Off`.
    pub(crate) fn parse(value: &str) -> (Option<Self>, Vec<String>) {
        let mut unnamedplus = false;
        let mut unnamed = false;
        let mut off = false;
        let mut unknown = Vec::new();
        for token in value.split(',').map(str::trim).filter(|token| !token.is_empty()) {
            match token {
                "unnamedplus" => unnamedplus = true,
                "unnamed" => unnamed = true,
                "off" => off = true,
                _ => unknown.push(token.to_owned()),
            }
        }
        let routing = if unnamedplus {
            Some(Self::UnnamedPlus)
        } else if unnamed {
            Some(Self::Unnamed)
        } else if off || value.trim().is_empty() {
            Some(Self::Off)
        } else {
            None
        };
        (routing, unknown)
    }

    /// Canonical option spelling, used by `:set clipboard?` and the summary.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::UnnamedPlus => "unnamedplus",
            Self::Unnamed => "unnamed",
            Self::Off => "off",
        }
    }

    /// The selection plain yank/delete mirrors to, or `None` when routing is off.
    pub(crate) fn mirror_selection(self) -> Option<ClipboardSelection> {
        match self {
            Self::UnnamedPlus => Some(ClipboardSelection::Clipboard),
            Self::Unnamed => Some(ClipboardSelection::Primary),
            Self::Off => None,
        }
    }
}

/// Which transport carries clipboard reads and writes.
///
/// `auto` picks between `arboard` and OSC 52 from the detected environment at
/// request time; `arboard` and `osc52` force one transport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ClipboardBackendKind {
    /// Prefer the platform clipboard, fall back to OSC 52 when unavailable.
    #[default]
    Auto,
    /// Force the `arboard` platform transport.
    Arboard,
    /// Force terminal OSC 52 sequences.
    Osc52,
}

impl ClipboardBackendKind {
    /// Parse a `clipboard_backend` value, or `None` when unrecognised.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "arboard" => Some(Self::Arboard),
            "osc52" => Some(Self::Osc52),
            _ => None,
        }
    }

    /// Canonical option spelling, used by `:set clipboard_backend?`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Arboard => "arboard",
            Self::Osc52 => "osc52",
        }
    }

    /// Ordered transports for a write in this environment.
    ///
    /// `auto` prefers OSC 52 over SSH — arboard would target the remote
    /// display, not the user's terminal — and prefers arboard locally with OSC
    /// 52 as the fallback.
    pub(crate) fn write_plan(self, remote: bool) -> TransportPlan {
        match self {
            Self::Osc52 => TransportPlan { primary: Transport::Osc52, fallback: None },
            Self::Arboard => TransportPlan { primary: Transport::Arboard, fallback: None },
            Self::Auto if remote => {
                TransportPlan { primary: Transport::Osc52, fallback: Some(Transport::Arboard) }
            }
            Self::Auto => {
                TransportPlan { primary: Transport::Arboard, fallback: Some(Transport::Osc52) }
            }
        }
    }

    /// The transport a read uses, or `None` when reads are unavailable.
    ///
    /// OSC 52 cannot read, so `osc52` has no read transport; reads then fall
    /// back to the last value this process wrote.
    pub(crate) fn read_transport(self) -> Option<Transport> {
        match self {
            Self::Osc52 => None,
            Self::Arboard | Self::Auto => Some(Transport::Arboard),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_routing_matrix() {
        assert_eq!(ClipboardRouting::parse("unnamedplus").0, Some(ClipboardRouting::UnnamedPlus));
        assert_eq!(ClipboardRouting::parse("unnamed").0, Some(ClipboardRouting::Unnamed));
        assert_eq!(ClipboardRouting::parse("off").0, Some(ClipboardRouting::Off));
        assert_eq!(ClipboardRouting::parse("").0, Some(ClipboardRouting::Off));
        assert_eq!(ClipboardRouting::parse("   ").0, Some(ClipboardRouting::Off));
    }

    #[test]
    fn parse_routing_rejects_a_value_with_no_known_token() {
        // A typo must not silently downgrade routing to `off`.
        assert_eq!(ClipboardRouting::parse("unnamedpluss").0, None);
        let (routing, unknown) = ClipboardRouting::parse("bogus");
        assert_eq!(routing, None);
        assert_eq!(unknown, vec![String::from("bogus")]);
    }

    #[test]
    fn parse_routing_list_prefers_unnamedplus() {
        for value in ["unnamed,unnamedplus", "unnamedplus,unnamed", " unnamed , unnamedplus "] {
            assert_eq!(ClipboardRouting::parse(value).0, Some(ClipboardRouting::UnnamedPlus));
        }
    }

    #[test]
    fn parse_routing_reports_unknown_tokens() {
        let (routing, unknown) = ClipboardRouting::parse("unnamedplus,bogus");
        assert_eq!(routing, Some(ClipboardRouting::UnnamedPlus));
        assert_eq!(unknown, vec![String::from("bogus")]);
    }

    #[test]
    fn mirror_selection_maps_each_routing() {
        assert_eq!(
            ClipboardRouting::UnnamedPlus.mirror_selection(),
            Some(ClipboardSelection::Clipboard)
        );
        assert_eq!(ClipboardRouting::Unnamed.mirror_selection(), Some(ClipboardSelection::Primary));
        assert_eq!(ClipboardRouting::Off.mirror_selection(), None);
    }

    #[test]
    fn routing_default_is_unnamedplus() {
        assert_eq!(ClipboardRouting::default(), ClipboardRouting::UnnamedPlus);
    }

    #[test]
    fn backend_kind_round_trips() {
        for kind in
            [ClipboardBackendKind::Auto, ClipboardBackendKind::Arboard, ClipboardBackendKind::Osc52]
        {
            assert_eq!(ClipboardBackendKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ClipboardBackendKind::parse("  OSC52 "), Some(ClipboardBackendKind::Osc52));
        assert_eq!(ClipboardBackendKind::parse("nope"), None);
    }

    #[test]
    fn auto_write_plan_prefers_osc52_only_when_remote() {
        assert_eq!(
            ClipboardBackendKind::Auto.write_plan(false),
            TransportPlan { primary: Transport::Arboard, fallback: Some(Transport::Osc52) }
        );
        assert_eq!(
            ClipboardBackendKind::Auto.write_plan(true),
            TransportPlan { primary: Transport::Osc52, fallback: Some(Transport::Arboard) }
        );
    }

    #[test]
    fn forced_backends_have_no_fallback() {
        for remote in [false, true] {
            assert_eq!(
                ClipboardBackendKind::Arboard.write_plan(remote),
                TransportPlan { primary: Transport::Arboard, fallback: None }
            );
            assert_eq!(
                ClipboardBackendKind::Osc52.write_plan(remote),
                TransportPlan { primary: Transport::Osc52, fallback: None }
            );
        }
    }

    #[test]
    fn read_transport_is_arboard_except_when_forced_to_osc52() {
        assert_eq!(ClipboardBackendKind::Auto.read_transport(), Some(Transport::Arboard));
        assert_eq!(ClipboardBackendKind::Arboard.read_transport(), Some(Transport::Arboard));
        assert_eq!(ClipboardBackendKind::Osc52.read_transport(), None);
    }

    #[test]
    fn environment_detects_ssh_and_multiplexers() {
        let detect = |values: &[(&str, &str)]| {
            ClipboardEnvironment::from_lookup(|key| {
                values.iter().find(|(k, _)| *k == key).map(|(_, v)| String::from(*v))
            })
        };

        assert_eq!(detect(&[]), ClipboardEnvironment::default());
        for key in ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"] {
            let env = detect(&[(key, "set")]);
            assert!(env.remote, "{key} must mark the session remote");
            assert_eq!(env.wrap, TerminalWrap::None);
        }
        // An empty variable is treated as unset.
        assert!(!detect(&[("SSH_TTY", "")]).remote);
        assert_eq!(detect(&[("TMUX", "/tmp/tmux-1000/default,1,0")]).wrap, TerminalWrap::Tmux);
        assert_eq!(detect(&[("STY", "1234.pts-0.host")]).wrap, TerminalWrap::Screen);
        // tmux wins when both are set (tmux nests inside screen).
        assert_eq!(detect(&[("STY", "1.x"), ("TMUX", "1")]).wrap, TerminalWrap::Tmux);
    }

    #[test]
    fn write_error_classification_matches_the_design_table() {
        use arboard::Error;
        assert_eq!(classify_write_error(&Error::ClipboardOccupied), FailureAction::Retry);
        assert_eq!(classify_write_error(&Error::ClipboardNotSupported), FailureAction::Fallback);
        assert_eq!(classify_write_error(&Error::ContentNotAvailable), FailureAction::Ignore);
        assert_eq!(classify_write_error(&Error::ConversionFailure), FailureAction::Surface);
        assert_eq!(
            classify_write_error(&Error::Unknown { description: String::from("x") }),
            FailureAction::Fallback
        );
    }

    #[test]
    fn read_error_classification_keeps_empty_clipboard_authoritative() {
        use arboard::Error;
        assert_eq!(classify_read_error(&Error::ContentNotAvailable), FailureAction::Ignore);
        assert_eq!(classify_read_error(&Error::ClipboardOccupied), FailureAction::Retry);
        assert_eq!(classify_read_error(&Error::ClipboardNotSupported), FailureAction::Surface);
    }

    #[test]
    fn config_default_matches_the_locked_decisions() {
        let config = ClipboardConfig::default();
        assert_eq!(config.backend, ClipboardBackendKind::Auto);
        assert_eq!(config.osc52_max_bytes, 16_384);
        assert!(config.wait_on_exit);
    }
}
