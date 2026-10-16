//! Terminal identification and keyboard-protocol selection.
//!
//! Two things here are load-bearing, and they are not the same kind of thing:
//!
//! 1. **Identification** — which emulator we are attached to, read from the
//!    environment rather than guessed from a rendered pixel. This is what lets
//!    CodeRex behave differently on iTerm2, VS Code, Cursor, Windows Terminal
//!    and friends without a pile of heuristics at the call site.
//!
//! 2. **Keyboard protocol** — whether to ask the terminal to report keys
//!    unambiguously. This is a *correctness* decision, not a cosmetic one.
//!    Under the legacy encoding `Ctrl+I` and `Tab`, `Ctrl+M` and `Enter`, and
//!    `Alt+[` and `Esc` are literally the same bytes on the wire, so no amount
//!    of decoding can tell them apart. The kitty keyboard protocol's
//!    disambiguation (`CSI > flags u`) makes them distinct events.
//!
//! The environment is read through a caller-supplied lookup so every rule below
//! is testable without mutating the process environment.

use crossterm::event::KeyboardEnhancementFlags;

/// Environment variable that overrides the automatic keyboard-protocol choice.
///
/// Accepted values: `disambiguate` / `on` to force the kitty keyboard protocol,
/// `legacy` / `off` to force the terminal's default encoding. Anything else,
/// including unset, falls back to detection. This exists because the allowlist
/// below is deliberately conservative — see [`TerminalKind::supports_kitty_keyboard`].
pub const KEYBOARD_PROTOCOL_ENV: &str = "CODEREX_KEYBOARD_PROTOCOL";

/// The terminal emulator CodeRex believes it is running inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    /// iTerm2 (`TERM_PROGRAM=iTerm.app`).
    Iterm2,
    /// Visual Studio Code's integrated terminal (`TERM_PROGRAM=vscode`).
    VsCode,
    /// Cursor's integrated terminal (a VS Code fork; sets `CURSOR_TRACE_ID`).
    Cursor,
    /// Windows Terminal (`WT_SESSION`).
    WindowsTerminal,
    /// kitty (`KITTY_WINDOW_ID`).
    Kitty,
    /// WezTerm (`WEZTERM_PANE`).
    WezTerm,
    /// Ghostty (`TERM_PROGRAM=ghostty`).
    Ghostty,
    /// macOS Terminal.app (`TERM_PROGRAM=Apple_Terminal`).
    AppleTerminal,
    /// A VTE-based terminal — GNOME Terminal, Tilix, Xfce Terminal, ….
    Vte,
    /// Not identified from the environment.
    Unknown,
}

impl TerminalKind {
    /// A short human-readable name, for logs and diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Iterm2 => "iTerm2",
            Self::VsCode => "VS Code",
            Self::Cursor => "Cursor",
            Self::WindowsTerminal => "Windows Terminal",
            Self::Kitty => "kitty",
            Self::WezTerm => "WezTerm",
            Self::Ghostty => "Ghostty",
            Self::AppleTerminal => "Terminal.app",
            Self::Vte => "VTE",
            Self::Unknown => "unknown terminal",
        }
    }

    /// Whether this terminal implements the kitty keyboard protocol's
    /// progressive-enhancement push (`CSI > flags u`).
    ///
    /// Only terminals that document the protocol are listed. VS Code, Cursor,
    /// iTerm2 and Windows Terminal are identified but *not* listed: their
    /// support could not be verified here, and asking a terminal to disambiguate
    /// when it does not understand the request is at best a no-op. Users on
    /// those terminals can opt in with [`KEYBOARD_PROTOCOL_ENV`].
    #[must_use]
    pub const fn supports_kitty_keyboard(self) -> bool {
        matches!(self, Self::Kitty | Self::WezTerm | Self::Ghostty)
    }
}

/// What CodeRex asks the terminal to do about key encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocol {
    /// Push `DISAMBIGUATE_ESCAPE_CODES` on startup, pop it on exit.
    Disambiguate,
    /// Leave the terminal's default (legacy) encoding alone.
    Legacy,
}

impl KeyboardProtocol {
    /// The crossterm flags to push, or `None` for the legacy encoding.
    #[must_use]
    pub const fn flags(self) -> Option<KeyboardEnhancementFlags> {
        match self {
            Self::Disambiguate => Some(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            Self::Legacy => None,
        }
    }
}

/// What the environment says about the terminal we are attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalProfile {
    /// The identified terminal.
    pub kind: TerminalKind,
    /// Raw `TERM_PROGRAM`, when the terminal sets it (used for display).
    pub program: Option<String>,
    /// The key encoding CodeRex will ask for.
    pub keyboard_protocol: KeyboardProtocol,
}

impl TerminalProfile {
    /// Detect the terminal from the real process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::detect(&|name| std::env::var(name).ok())
    }

    /// Detect the terminal from a caller-supplied environment lookup.
    ///
    /// Pure: nothing outside the lookup is consulted, so tests can describe a
    /// terminal without touching the process environment.
    #[must_use]
    pub fn detect(get: &impl Fn(&str) -> Option<String>) -> Self {
        let program = non_empty(get("TERM_PROGRAM"));
        let kind = classify(get, program.as_deref());
        let keyboard_protocol =
            decide_keyboard_protocol(kind, get(KEYBOARD_PROTOCOL_ENV).as_deref());
        Self {
            kind,
            program,
            keyboard_protocol,
        }
    }

    /// The name to show a user: `TERM_PROGRAM` when set, else the kind label.
    #[must_use]
    pub fn name(&self) -> &str {
        self.program.as_deref().unwrap_or_else(|| self.kind.label())
    }

    /// Flags to push on startup, or `None` to leave the encoding alone.
    #[must_use]
    pub const fn keyboard_flags(&self) -> Option<KeyboardEnhancementFlags> {
        self.keyboard_protocol.flags()
    }
}

#[must_use]
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Identify the terminal.
///
/// Order matters: Cursor reports `TERM_PROGRAM=vscode`, and VS Code's integrated
/// terminal on Windows can carry `WT_SESSION` too, so the more specific markers
/// are tested first.
fn classify(get: &impl Fn(&str) -> Option<String>, program: Option<&str>) -> TerminalKind {
    let present = |name: &str| get(name).is_some_and(|value| !value.is_empty());

    if present("CURSOR_TRACE_ID") || program == Some("cursor") {
        return TerminalKind::Cursor;
    }
    if program == Some("vscode") {
        return TerminalKind::VsCode;
    }
    if present("WT_SESSION") {
        return TerminalKind::WindowsTerminal;
    }
    if present("KITTY_WINDOW_ID") {
        return TerminalKind::Kitty;
    }
    if present("WEZTERM_PANE") || program == Some("WezTerm") {
        return TerminalKind::WezTerm;
    }
    match program {
        Some("iTerm.app") => TerminalKind::Iterm2,
        Some("Apple_Terminal") => TerminalKind::AppleTerminal,
        Some("ghostty") => TerminalKind::Ghostty,
        _ => {
            let term = get("TERM").unwrap_or_default().to_ascii_lowercase();
            if term.contains("kitty") {
                TerminalKind::Kitty
            } else if term.contains("ghostty") {
                TerminalKind::Ghostty
            } else if present("VTE_VERSION") {
                TerminalKind::Vte
            } else {
                TerminalKind::Unknown
            }
        }
    }
}

/// Choose the key encoding: an explicit override wins, otherwise the terminal
/// has to be one we know implements the protocol.
fn decide_keyboard_protocol(kind: TerminalKind, override_value: Option<&str>) -> KeyboardProtocol {
    match override_value {
        Some("disambiguate" | "on") => KeyboardProtocol::Disambiguate,
        Some("legacy" | "off") => KeyboardProtocol::Legacy,
        _ if kind.supports_kitty_keyboard() => KeyboardProtocol::Disambiguate,
        _ => KeyboardProtocol::Legacy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an environment lookup from a list of key/value pairs.
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn profile(pairs: &[(&str, &str)]) -> TerminalProfile {
        TerminalProfile::detect(&env(pairs))
    }

    #[test]
    fn identifies_the_named_terminals() {
        assert_eq!(
            profile(&[("TERM_PROGRAM", "iTerm.app")]).kind,
            TerminalKind::Iterm2
        );
        assert_eq!(
            profile(&[("TERM_PROGRAM", "vscode")]).kind,
            TerminalKind::VsCode
        );
        assert_eq!(
            profile(&[("WT_SESSION", "0e1f2a")]).kind,
            TerminalKind::WindowsTerminal
        );
        assert_eq!(
            profile(&[("KITTY_WINDOW_ID", "1")]).kind,
            TerminalKind::Kitty
        );
        assert_eq!(
            profile(&[("WEZTERM_PANE", "0")]).kind,
            TerminalKind::WezTerm
        );
        assert_eq!(
            profile(&[("TERM_PROGRAM", "ghostty")]).kind,
            TerminalKind::Ghostty
        );
        assert_eq!(
            profile(&[("TERM_PROGRAM", "Apple_Terminal")]).kind,
            TerminalKind::AppleTerminal
        );
        assert_eq!(profile(&[("VTE_VERSION", "7400")]).kind, TerminalKind::Vte);
    }

    /// Cursor is a VS Code fork and reports `TERM_PROGRAM=vscode`; it must be
    /// recognised as Cursor, not as plain VS Code.
    #[test]
    fn cursor_is_not_mistaken_for_vs_code() {
        let cursor = profile(&[("TERM_PROGRAM", "vscode"), ("CURSOR_TRACE_ID", "abc")]);
        assert_eq!(cursor.kind, TerminalKind::Cursor);

        let plain = profile(&[("TERM_PROGRAM", "vscode")]);
        assert_eq!(plain.kind, TerminalKind::VsCode);
    }

    /// VS Code's integrated terminal on Windows can also set `WT_SESSION`; the
    /// IDE marker is the more specific one.
    #[test]
    fn vs_code_wins_over_windows_terminal_marker() {
        let p = profile(&[("TERM_PROGRAM", "vscode"), ("WT_SESSION", "0e1f2a")]);
        assert_eq!(p.kind, TerminalKind::VsCode);
    }

    #[test]
    fn unidentified_environment_is_unknown() {
        let p = profile(&[("TERM", "xterm-256color")]);
        assert_eq!(p.kind, TerminalKind::Unknown);
        assert_eq!(p.keyboard_protocol, KeyboardProtocol::Legacy);
    }

    #[test]
    fn term_kitty_is_recognised_without_the_window_id() {
        assert_eq!(
            profile(&[("TERM", "xterm-kitty")]).kind,
            TerminalKind::Kitty
        );
    }

    #[test]
    fn empty_markers_do_not_count_as_present() {
        assert_eq!(
            profile(&[("TERM_PROGRAM", ""), ("TERM", "xterm")]).kind,
            TerminalKind::Unknown
        );
        assert_eq!(profile(&[("WT_SESSION", "")]).kind, TerminalKind::Unknown);
    }

    #[test]
    fn name_prefers_term_program() {
        let p = profile(&[("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(p.name(), "iTerm.app");
        assert_eq!(profile(&[("TERM", "xterm")]).name(), "unknown terminal");
    }

    /// The whole point of the profile: disambiguate where it is known to work,
    /// stay on the legacy encoding everywhere else.
    #[test]
    fn keyboard_protocol_follows_the_terminal() {
        assert_eq!(
            profile(&[("KITTY_WINDOW_ID", "1")]).keyboard_protocol,
            KeyboardProtocol::Disambiguate
        );
        assert_eq!(
            profile(&[("WEZTERM_PANE", "0")]).keyboard_protocol,
            KeyboardProtocol::Disambiguate
        );
        assert_eq!(
            profile(&[("TERM_PROGRAM", "ghostty")]).keyboard_protocol,
            KeyboardProtocol::Disambiguate
        );
        // Identified, but not claimed to support the protocol.
        assert_eq!(
            profile(&[("TERM_PROGRAM", "vscode")]).keyboard_protocol,
            KeyboardProtocol::Legacy
        );
        assert_eq!(
            profile(&[("TERM_PROGRAM", "iTerm.app")]).keyboard_protocol,
            KeyboardProtocol::Legacy
        );
        assert_eq!(
            profile(&[("WT_SESSION", "0e1f2a")]).keyboard_protocol,
            KeyboardProtocol::Legacy
        );
    }

    #[test]
    fn environment_override_wins_in_both_directions() {
        let forced_on = profile(&[
            ("TERM_PROGRAM", "vscode"),
            (KEYBOARD_PROTOCOL_ENV, "disambiguate"),
        ]);
        assert_eq!(forced_on.keyboard_protocol, KeyboardProtocol::Disambiguate);

        let forced_off = profile(&[("KITTY_WINDOW_ID", "1"), (KEYBOARD_PROTOCOL_ENV, "legacy")]);
        assert_eq!(forced_off.keyboard_protocol, KeyboardProtocol::Legacy);

        // `on` / `off` are accepted aliases.
        assert_eq!(
            profile(&[(KEYBOARD_PROTOCOL_ENV, "on")]).keyboard_protocol,
            KeyboardProtocol::Disambiguate
        );
        assert_eq!(
            profile(&[(KEYBOARD_PROTOCOL_ENV, "off")]).keyboard_protocol,
            KeyboardProtocol::Legacy
        );
    }

    #[test]
    fn unrecognised_override_falls_back_to_detection() {
        let p = profile(&[("KITTY_WINDOW_ID", "1"), (KEYBOARD_PROTOCOL_ENV, "maybe")]);
        assert_eq!(p.keyboard_protocol, KeyboardProtocol::Disambiguate);
    }

    #[test]
    fn flags_map_to_the_disambiguate_bit() {
        assert_eq!(
            KeyboardProtocol::Disambiguate.flags(),
            Some(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        assert_eq!(KeyboardProtocol::Legacy.flags(), None);
    }

    #[test]
    fn every_kind_has_a_label() {
        for kind in [
            TerminalKind::Iterm2,
            TerminalKind::VsCode,
            TerminalKind::Cursor,
            TerminalKind::WindowsTerminal,
            TerminalKind::Kitty,
            TerminalKind::WezTerm,
            TerminalKind::Ghostty,
            TerminalKind::AppleTerminal,
            TerminalKind::Vte,
            TerminalKind::Unknown,
        ] {
            assert!(!kind.label().is_empty());
        }
    }
}
