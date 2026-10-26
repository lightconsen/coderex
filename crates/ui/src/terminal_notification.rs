//! Terminal-level desktop notifications and progress reporting.
//!
//! A TUI that owns the screen cannot print a notification line, so terminals
//! that support it accept an OSC 9 escape sequence instead. The rules here are
//! ported from kimi-code's `terminal-notification` module: sanitize the text,
//! truncate it, wrap it for tmux, and only use sequences the terminal actually
//! supports.
//!
//! Capability detection is deliberately conservative. In particular OSC 9 and
//! OSC 9;4 are treated separately: iTerm2 turns any OSC 9 payload into a
//! desktop notification, so sending it `OSC 9;4` progress would pop a
//! notification for every update.

use std::io::Write as _;

/// Longest notification message, in characters.
pub const MAX_MESSAGE_CHARS: usize = 240;

/// Build the notification text: `title: body` with control characters
/// replaced by spaces, whitespace collapsed, and the result truncated.
#[must_use]
pub fn format_message(title: &str, body: &str) -> String {
    fn clean(text: &str) -> String {
        text.chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    let title = clean(title);
    let body = clean(body);
    let message = match (title.is_empty(), body.is_empty()) {
        (false, false) => format!("{title}: {body}"),
        (false, true) => title,
        _ => body,
    };
    message.chars().take(MAX_MESSAGE_CHARS).collect()
}

/// Whether the terminal turns OSC 9 into a desktop notification.
#[must_use]
pub fn supports_osc9_notification(term_program: Option<&str>, term: Option<&str>) -> bool {
    matches!(
        term_program,
        Some("iTerm.app" | "WezTerm" | "ghostty" | "WarpTerminal")
    ) || matches!(term, Some("xterm-kitty" | "xterm-ghostty"))
}

/// Whether OSC 9;4 progress reporting is safe for this terminal.
///
/// The allow-list is disjoint from [`supports_osc9_notification`]: terminals
/// that treat any OSC 9 as a notification (iTerm2, Warp, kitty) are excluded.
#[must_use]
pub fn supports_terminal_progress(
    term_program: Option<&str>,
    term: Option<&str>,
    wt_session: Option<&str>,
    conemu_ansi: Option<&str>,
) -> bool {
    wt_session.is_some_and(|value| !value.is_empty())
        || conemu_ansi == Some("ON")
        || matches!(term_program, Some("ghostty" | "WezTerm"))
        || term == Some("xterm-ghostty")
}

/// Whether the process runs inside tmux, which needs the DCS wrapper.
#[must_use]
pub fn is_inside_tmux(tmux: Option<&str>) -> bool {
    tmux.is_some_and(|value| !value.is_empty())
}

/// The sequences to write for one notification.
///
/// An empty message writes nothing. Without OSC 9 support the BEL byte is the
/// only portable "attention" signal. Inside tmux the OSC payload is wrapped in
/// a DCS passthrough, with every ESC doubled.
#[must_use]
pub fn notification_sequences(
    message: &str,
    supports_osc9: bool,
    inside_tmux: bool,
) -> Vec<String> {
    if message.is_empty() {
        return Vec::new();
    }
    if !supports_osc9 {
        return vec!["\u{7}".to_string()];
    }
    let payload = format!("\u{1b}]9;{message}\u{7}");
    if inside_tmux {
        vec![tmux_wrap(&payload)]
    } else {
        vec![payload]
    }
}

/// An OSC 9;4 progress sequence.
///
/// `None` reports indeterminate progress; `Some(percent)` reports a value.
/// Call [`clear_progress_sequence`] when the turn ends.
#[must_use]
pub fn progress_sequence(percent: Option<u8>, inside_tmux: bool) -> String {
    let payload = match percent {
        Some(percent) => format!("\u{1b}]9;4;1;{}\u{7}", percent.min(100)),
        None => "\u{1b}]9;4;3;0\u{7}".to_string(),
    };
    if inside_tmux {
        tmux_wrap(&payload)
    } else {
        payload
    }
}

/// Clear the progress indicator (OSC 9;4 state 0).
#[must_use]
pub fn clear_progress_sequence(inside_tmux: bool) -> String {
    let payload = "\u{1b}]9;4;0;0\u{7}".to_string();
    if inside_tmux {
        tmux_wrap(&payload)
    } else {
        payload
    }
}

fn tmux_wrap(payload: &str) -> String {
    let escaped = payload.replace('\u{1b}', "\u{1b}\u{1b}");
    format!("\u{1b}Ptmux;{escaped}\u{1b}\\")
}

/// Write arbitrary terminal sequences to stdout and flush them.
///
/// OSC and DCS sequences do not move the cursor, so writing them between
/// frames is safe.
///
/// # Errors
///
/// Returns the writer's error; a failed notification must never fail a turn.
pub fn write_sequences(sequences: &[String]) -> std::io::Result<()> {
    if sequences.is_empty() {
        return Ok(());
    }
    let mut stdout = std::io::stdout().lock();
    for sequence in sequences {
        stdout.write_all(sequence.as_bytes())?;
    }
    stdout.flush()
}

/// Capability detection from the process environment.
#[must_use]
pub fn env_supports_osc9_notification() -> bool {
    supports_osc9_notification(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    )
}

/// Progress support from the process environment.
#[must_use]
pub fn env_supports_terminal_progress() -> bool {
    supports_terminal_progress(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var("WT_SESSION").ok().as_deref(),
        std::env::var("ConEmuANSI").ok().as_deref(),
    )
}

/// Whether this process is inside tmux.
#[must_use]
pub fn env_is_inside_tmux() -> bool {
    is_inside_tmux(std::env::var("TMUX").ok().as_deref())
}

/// Format, encode and write a notification, ignoring failures.
///
/// Unlike [`notification_sequences`], this does nothing when OSC 9 is not
/// supported: the BEL fallback would beep on every turn completion, which is
/// worse than a missing desktop notification.
pub fn emit(title: &str, body: &str, supports_osc9: bool, inside_tmux: bool) {
    if !supports_osc9 {
        return;
    }
    let message = format_message(title, body);
    let _ = write_sequences(&notification_sequences(&message, true, inside_tmux));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_is_sanitized_and_truncated() {
        assert_eq!(
            format_message("CodeRex", "Approval\nrequired"),
            "CodeRex: Approval required"
        );
        assert_eq!(format_message("", "body"), "body");
        assert_eq!(format_message("title", ""), "title");
        assert_eq!(format_message("", ""), "");
        // Control characters become spaces, then runs collapse.
        assert_eq!(format_message("a\u{1b}[31m", "b\tc"), "a [31m: b c");
        let long = "x".repeat(MAX_MESSAGE_CHARS + 50);
        assert_eq!(format_message("", &long).chars().count(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn notification_sequences_follow_the_terminal_capabilities() {
        assert!(notification_sequences("", true, false).is_empty());
        assert_eq!(notification_sequences("hi", false, false), vec!["\u{7}"]);
        assert_eq!(notification_sequences("hi", false, true), vec!["\u{7}"]);
        assert_eq!(
            notification_sequences("hi", true, false),
            vec!["\u{1b}]9;hi\u{7}"]
        );
        assert_eq!(
            notification_sequences("hi", true, true),
            vec!["\u{1b}Ptmux;\u{1b}\u{1b}]9;hi\u{7}\u{1b}\\"]
        );
    }

    #[test]
    fn progress_sequences_follow_the_same_tmux_wrapper() {
        assert_eq!(progress_sequence(None, false), "\u{1b}]9;4;3;0\u{7}");
        assert_eq!(progress_sequence(Some(42), false), "\u{1b}]9;4;1;42\u{7}");
        assert_eq!(progress_sequence(Some(150), false), "\u{1b}]9;4;1;100\u{7}");
        assert_eq!(clear_progress_sequence(false), "\u{1b}]9;4;0;0\u{7}");
        assert_eq!(
            clear_progress_sequence(true),
            "\u{1b}Ptmux;\u{1b}\u{1b}]9;4;0;0\u{7}\u{1b}\\"
        );
    }

    #[test]
    fn osc9_detection_is_an_allow_list() {
        assert!(supports_osc9_notification(Some("iTerm.app"), None));
        assert!(supports_osc9_notification(Some("WezTerm"), None));
        assert!(supports_osc9_notification(None, Some("xterm-kitty")));
        assert!(!supports_osc9_notification(Some("Apple_Terminal"), None));
        assert!(!supports_osc9_notification(Some("vscode"), None));
        assert!(!supports_osc9_notification(None, Some("xterm-256color")));
        assert!(!supports_osc9_notification(None, None));
    }

    #[test]
    fn progress_detection_excludes_notification_only_terminals() {
        assert!(supports_terminal_progress(None, None, Some("wt"), None));
        assert!(supports_terminal_progress(None, None, None, Some("ON")));
        assert!(supports_terminal_progress(
            Some("ghostty"),
            None,
            None,
            None
        ));
        assert!(supports_terminal_progress(
            None,
            Some("xterm-ghostty"),
            None,
            None
        ));
        assert!(!supports_terminal_progress(
            Some("iTerm.app"),
            None,
            None,
            None
        ));
        assert!(!supports_terminal_progress(
            Some("WarpTerminal"),
            None,
            None,
            None
        ));
        assert!(!supports_terminal_progress(
            None,
            Some("xterm-kitty"),
            None,
            None
        ));
        assert!(!supports_terminal_progress(
            None,
            None,
            Some(""),
            Some("OFF")
        ));
    }

    #[test]
    fn tmux_detection_requires_a_non_empty_session() {
        assert!(is_inside_tmux(Some("session")));
        assert!(!is_inside_tmux(Some("")));
        assert!(!is_inside_tmux(None));
    }
}
