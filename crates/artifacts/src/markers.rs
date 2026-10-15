//! Transcript markers for artifacts a session has touched.
//!
//! A published page is not part of the conversation, but the conversation has
//! to know about it: which page was just published, which one was deleted. A
//! marker line records that in the transcript, in a form a later turn can read
//! back — and `own` distinguishes a page this session published from one it
//! merely read.

/// Something an artifact marker says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// A page was published or read.
    Attached {
        /// The page's URL.
        url: String,
        /// Its title, when the marker carries one.
        title: Option<String>,
        /// Whether this session published it.
        own: bool,
    },
    /// A page was deleted.
    Deleted {
        /// The page's URL.
        url: String,
    },
}

/// Render the marker for a page this session touched.
#[must_use]
pub fn attached_marker(url: &str, title: Option<&str>, own: bool) -> String {
    use std::fmt::Write as _;

    let mut marker = format!("<artifact-attached url=\"{}\"", escape(url));
    if let Some(title) = title {
        let _ = write!(marker, " title=\"{}\"", escape(title));
    }
    if own {
        marker.push_str(" own");
    }
    marker.push_str("/>");
    marker
}

/// Render the marker for a deleted page.
#[must_use]
pub fn deleted_marker(url: &str) -> String {
    format!("<artifact-deleted url=\"{}\"/>", escape(url))
}

/// Read every marker out of a block of text.
#[must_use]
pub fn parse_markers(text: &str) -> Vec<Marker> {
    let mut markers = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(marker) = parse_marker(line) {
            markers.push(marker);
        }
    }
    markers
}

/// Read one marker, if the line is one.
#[must_use]
pub fn parse_marker(line: &str) -> Option<Marker> {
    if let Some(rest) = line.strip_prefix("<artifact-attached ") {
        let body = rest.strip_suffix("/>")?;
        let (url, remainder) = attribute(body, "url")?;
        let (title, remainder) = match attribute(&remainder, "title") {
            Some((title, remainder)) => (Some(title), remainder),
            None => (None, remainder),
        };
        let own = remainder.trim() == "own";
        return Some(Marker::Attached { url, title, own });
    }

    if let Some(rest) = line.strip_prefix("<artifact-deleted ") {
        let body = rest.strip_suffix("/>")?;
        let (url, _) = attribute(body, "url")?;
        return Some(Marker::Deleted { url });
    }

    None
}

/// Pull one `name="value"` attribute out, returning it and what is left.
fn attribute(input: &str, name: &str) -> Option<(String, String)> {
    let head = format!("{name}=\"");
    let start = input.find(&head)? + head.len();
    let value = &input[start..];

    // The value ends at the closing quote, but an escaped quote inside it does
    // not end it — that is why this walks instead of splitting.
    let mut end = None;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => {
                end = Some(index);
                break;
            }
            _ => {}
        }
    }

    let end = end?;
    let value = unescape(&value[..end]);
    let remainder = input[start + end + 1..].to_string();
    Some((value, remainder))
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attached_marker_round_trips() {
        let marker = attached_marker("http://127.0.0.1:8722/artifact/abc", Some("Report"), true);
        assert_eq!(
            marker,
            "<artifact-attached url=\"http://127.0.0.1:8722/artifact/abc\" title=\"Report\" own/>"
        );
        assert_eq!(
            parse_markers(&marker),
            vec![Marker::Attached {
                url: "http://127.0.0.1:8722/artifact/abc".into(),
                title: Some("Report".into()),
                own: true,
            }]
        );
    }

    #[test]
    fn a_marker_without_a_title_or_ownership_parses() {
        let marker = attached_marker("https://claude.ai/artifact/abc", None, false);
        assert_eq!(
            marker,
            "<artifact-attached url=\"https://claude.ai/artifact/abc\"/>"
        );
        assert_eq!(
            parse_markers(&marker),
            vec![Marker::Attached {
                url: "https://claude.ai/artifact/abc".into(),
                title: None,
                own: false,
            }]
        );
    }

    #[test]
    fn a_deleted_marker_round_trips() {
        let marker = deleted_marker("http://127.0.0.1:8722/artifact/abc");
        assert_eq!(
            marker,
            "<artifact-deleted url=\"http://127.0.0.1:8722/artifact/abc\"/>"
        );
        assert_eq!(
            parse_markers(&marker),
            vec![Marker::Deleted {
                url: "http://127.0.0.1:8722/artifact/abc".into(),
            }]
        );
    }

    #[test]
    fn quotes_inside_a_title_survive() {
        let marker = attached_marker("http://x/artifact/y", Some("A \"quoted\" title"), false);
        let parsed = parse_markers(&marker);
        assert_eq!(
            parsed,
            vec![Marker::Attached {
                url: "http://x/artifact/y".into(),
                title: Some("A \"quoted\" title".into()),
                own: false,
            }]
        );
    }

    #[test]
    fn markers_are_found_among_other_text() {
        // Markers are written one per line, and only a line that is a marker
        // (rather than one that merely mentions one) is read back.
        let text = format!(
            "Here is the page.\n{}\nand it replaced the older one:\n{}\n",
            attached_marker("http://x/artifact/a", Some("A"), true),
            deleted_marker("http://x/artifact/b")
        );
        let markers = parse_markers(&text);
        assert_eq!(markers.len(), 2);
        assert!(matches!(markers[0], Marker::Attached { .. }));
        assert!(matches!(markers[1], Marker::Deleted { .. }));
    }

    #[test]
    fn ordinary_text_is_not_a_marker() {
        assert!(parse_markers("nothing to see").is_empty());
        assert!(parse_markers("<artifact-attached/>").is_empty());
        assert!(parse_markers("<artifact-deleted url=>").is_empty());
    }
}
