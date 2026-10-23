//! Render a mod's declarative UI tree into terminal text.
//!
//! The renderer is a pure transformation: it takes the host-owned
//! [`UiElement`] snapshot plus a width and returns styled [`Line`]s. It keeps
//! no TUI state, so the same tree can be rendered into the interactive
//! transcript or, through [`render_mod_ui_text`], into a headless run's output.
//!
//! Buttons render as `[label]`. The caller may pass the id of the focused
//! button; that button renders as `> [label] <` with reversed emphasis, while
//! every other button keeps the plain form. The renderer still keeps no state:
//! focus is an argument, not something cached here.

use coderex_core::ui_protocol::{UiElement, UiStyle};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Width used when a caller has no terminal measurement to offer.
pub const MOD_UI_DEFAULT_WIDTH: usize = 80;

/// The position of a button within a rendered mod UI block.
///
/// Rows and columns are zero-based and relative to the block's first rendered
/// line and column. `col_end` is exclusive, so a click lands on the button when
/// `col_start <= column < col_end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonHit {
    /// The button's host-owned id: the value passed to the mod's activation
    /// handler.
    pub id: String,
    /// Zero-based row within the rendered block.
    pub row: u16,
    /// First column of the button, inclusive.
    pub col_start: u16,
    /// One past the button's last column.
    pub col_end: u16,
}

/// Map a mod's [`UiStyle`] onto the terminal UI's style vocabulary.
///
/// `Warning` and `Error` use the same colors as the notification center, and
/// `Bold`/`Dim` the same modifiers as the rest of the UI.
#[must_use]
pub const fn ui_style(style: UiStyle) -> Style {
    match style {
        UiStyle::Plain => Style::new(),
        UiStyle::Bold => Style::new().add_modifier(Modifier::BOLD),
        UiStyle::Dim => Style::new().add_modifier(Modifier::DIM),
        UiStyle::Warning => Style::new().fg(Color::Yellow),
        UiStyle::Error => Style::new().fg(Color::Red),
    }
}

/// Render a mod UI tree into styled terminal lines at most `width` columns wide.
///
/// Boxes nest with box-drawing borders and an optional title. Text wraps at
/// `width`; buttons render as `[label]`. The result is a snapshot: nothing
/// here refers back to the mod or the engine.
#[must_use]
pub fn render_mod_ui(elements: &[UiElement], width: usize) -> Vec<Line<'static>> {
    render_mod_ui_with_focus(elements, width, None)
}

/// Render a mod UI tree, giving one button the focused treatment.
///
/// The button whose id equals `focused` renders as `> [label] <` with reversed
/// emphasis; every other button renders as `[label]`. `None` is exactly
/// [`render_mod_ui`].
#[must_use]
pub fn render_mod_ui_with_focus(
    elements: &[UiElement],
    width: usize,
    focused: Option<&str>,
) -> Vec<Line<'static>> {
    render_mod_ui_with_focus_and_hits(elements, width, focused).0
}

/// Render a mod UI tree together with the position of every button.
///
/// The returned lines are exactly [`render_mod_ui`]'s output. Each
/// [`ButtonHit`] is relative to those lines, so a caller that prefixes or
/// scrolls the block only needs to translate rows and columns.
#[must_use]
pub fn render_mod_ui_with_hits(
    elements: &[UiElement],
    width: usize,
) -> (Vec<Line<'static>>, Vec<ButtonHit>) {
    render_mod_ui_with_focus_and_hits(elements, width, None)
}

/// The focus-aware form of [`render_mod_ui_with_hits`].
///
/// The focus marker changes the button's rendered width, so hits are computed
/// from the same text the caller will see rather than from the label alone.
#[must_use]
pub fn render_mod_ui_with_focus_and_hits(
    elements: &[UiElement],
    width: usize,
    focused: Option<&str>,
) -> (Vec<Line<'static>>, Vec<ButtonHit>) {
    let mut hits = Vec::new();
    let lines = render_children_at(elements, width.max(1), focused, 0, 0, &mut hits);
    (lines, hits)
}

/// Find the button under a click.
///
/// `rect` is the block's absolute screen rect: its `x` is the column of the
/// block's first rendered column and its `y` is the screen row of the first
/// rendered row that is visible. `scroll_offset` is the number of block rows
/// clipped above `rect.y`; the mapping is `block_row = scroll_offset +
/// (row - rect.y)`. Clicks outside `rect`, or on padding and borders, return
/// `None`.
#[must_use]
pub fn button_at(
    rect: Rect,
    scroll_offset: usize,
    column: u16,
    row: u16,
    hits: &[ButtonHit],
) -> Option<&str> {
    if rect.width == 0 || rect.height == 0 || column < rect.x || row < rect.y {
        return None;
    }
    let offset_x = column - rect.x;
    let offset_y = row - rect.y;
    if offset_x >= rect.width || offset_y >= rect.height {
        return None;
    }
    let block_row = u16::try_from(scroll_offset)
        .unwrap_or(u16::MAX)
        .saturating_add(offset_y);
    hits.iter()
        .find(|hit| hit.row == block_row && hit.col_start <= offset_x && offset_x < hit.col_end)
        .map(|hit| hit.id.as_str())
}

/// Render a mod UI tree as a plain text block, one line per row.
///
/// This is the headless/transcript form of [`render_mod_ui`]; styling is
/// intentionally dropped because both consumers store plain text.
#[must_use]
pub fn render_mod_ui_text(elements: &[UiElement], width: usize) -> String {
    render_mod_ui_text_with_focus(elements, width, None)
}

/// The plain-text form of [`render_mod_ui_with_focus`].
#[must_use]
pub fn render_mod_ui_text_with_focus(
    elements: &[UiElement],
    width: usize,
    focused: Option<&str>,
) -> String {
    render_mod_ui_with_focus(elements, width, focused)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_text(line: &Line<'static>) -> String {
    let mut text = String::new();
    for span in &line.spans {
        text.push_str(span.content.as_ref());
    }
    text
}

fn render_children_at(
    elements: &[UiElement],
    width: usize,
    focused: Option<&str>,
    row: u16,
    col: u16,
    hits: &mut Vec<ButtonHit>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for element in elements {
        let child_row = row.saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        lines.extend(render_element_at(
            element, width, focused, child_row, col, hits,
        ));
    }
    lines
}

fn render_element_at(
    element: &UiElement,
    width: usize,
    focused: Option<&str>,
    row: u16,
    col: u16,
    hits: &mut Vec<ButtonHit>,
) -> Vec<Line<'static>> {
    match element {
        UiElement::Text { text, style } => {
            let style = ui_style(*style);
            wrap_text(text, width)
                .into_iter()
                .map(|text| Line::from(Span::styled(text, style)))
                .collect()
        }
        UiElement::Button { label, id } => {
            let is_focused = focused == Some(id.as_str());
            let text = if is_focused {
                format!("> [{label}] <")
            } else {
                format!("[{label}]")
            };
            let text = truncate_to_width(&text, width);
            let text_width = display_width(&text);
            if text_width > 0 {
                hits.push(ButtonHit {
                    id: id.clone(),
                    row,
                    col_start: col,
                    col_end: col.saturating_add(u16::try_from(text_width).unwrap_or(u16::MAX)),
                });
            }
            let style = if is_focused {
                ui_style(UiStyle::Bold).add_modifier(Modifier::REVERSED)
            } else {
                ui_style(UiStyle::Bold)
            };
            vec![Line::from(Span::styled(text, style))]
        }
        UiElement::Box { title, children } => {
            render_box_at(title.as_deref(), children, width, focused, row, col, hits)
        }
    }
}

/// A box needs two border columns, two padding columns and at least one
/// content column. Narrower than that, the borders would be noise, so the
/// children render on their own.
const MIN_BOX_WIDTH: usize = 5;

fn render_box_at(
    title: Option<&str>,
    children: &[UiElement],
    width: usize,
    focused: Option<&str>,
    row: u16,
    col: u16,
    hits: &mut Vec<ButtonHit>,
) -> Vec<Line<'static>> {
    if width < MIN_BOX_WIDTH {
        return render_children_at(children, width, focused, row, col, hits);
    }

    let border = Style::new().fg(Color::DarkGray);
    let inner_width = width - 4;
    let mut lines = vec![top_border(title, width, border)];
    let child_lines = render_children_at(
        children,
        inner_width,
        focused,
        row.saturating_add(1),
        col.saturating_add(2),
        hits,
    );
    if child_lines.is_empty() {
        lines.push(content_line(Line::default(), inner_width, border));
    } else {
        lines.extend(
            child_lines
                .into_iter()
                .map(|line| content_line(line, inner_width, border)),
        );
    }
    lines.push(simple_border('└', '┘', width, border));
    lines
}

fn top_border(title: Option<&str>, width: usize, style: Style) -> Line<'static> {
    let Some(title) = title.filter(|title| !title.is_empty()) else {
        return simple_border('┌', '┐', width, style);
    };

    // `┌─ ` + title + ` ` + fill + `┐`
    let available = width.saturating_sub(5);
    if available == 0 {
        return simple_border('┌', '┐', width, style);
    }
    let title = truncate_to_width(title, available);
    let fill = available.saturating_sub(display_width(&title));

    let mut text = String::from("┌─ ");
    text.push_str(&title);
    text.push(' ');
    text.extend(std::iter::repeat_n('─', fill));
    text.push('┐');
    Line::from(Span::styled(text, style))
}

fn simple_border(left: char, right: char, width: usize, style: Style) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    text.extend(std::iter::repeat_n('─', width.saturating_sub(2)));
    text.push(right);
    Line::from(Span::styled(text, style))
}

fn content_line(content: Line<'static>, inner_width: usize, border: Style) -> Line<'static> {
    let content_width = content
        .spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum::<usize>();
    let padding = inner_width.saturating_sub(content_width);

    let mut spans = Vec::with_capacity(content.spans.len() + 3);
    spans.push(Span::styled("│ ", border));
    spans.extend(content.spans);
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(" │", border));
    Line::from(spans)
}

/// Wrap `text` at `width` columns, preserving explicit newlines.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0;
        for word in paragraph.split_whitespace() {
            let word_width = display_width(word);
            if word_width > width {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                let mut remaining = word.to_string();
                while display_width(&remaining) > width {
                    let (head, tail) = split_at_width(&remaining, width);
                    lines.push(head);
                    remaining = tail;
                }
                current_width = display_width(&remaining);
                current = remaining;
            } else if current.is_empty() {
                current.push_str(word);
                current_width = word_width;
            } else if current_width + 1 + word_width <= width {
                current.push(' ');
                current.push_str(word);
                current_width += 1 + word_width;
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
                current_width = word_width;
            }
        }
        lines.push(current);
    }

    lines
}

/// Split `text` after the last character that fits in `width` columns.
///
/// At least one character is always taken, so a glyph wider than `width`
/// cannot make the caller loop forever.
fn split_at_width(text: &str, width: usize) -> (String, String) {
    let mut used = 0;
    let mut boundary = text.len();
    for (index, character) in text.char_indices() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width && used > 0 {
            boundary = index;
            break;
        }
        used += character_width;
    }
    let boundary = boundary.max(text.chars().next().map_or(0, char::len_utf8));
    let (head, tail) = text.split_at(boundary);
    (head.to_string(), tail.to_string())
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut truncated = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width {
            break;
        }
        truncated.push(character);
        used += character_width;
    }
    truncated
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> UiElement {
        UiElement::Text {
            text: text.to_string(),
            style: UiStyle::Plain,
        }
    }

    fn button(label: &str) -> UiElement {
        UiElement::Button {
            label: label.to_string(),
            id: label.to_ascii_lowercase(),
        }
    }

    #[test]
    fn text_and_buttons_render_in_order() {
        let elements = vec![text("hello"), button("Retry")];
        assert_eq!(render_mod_ui_text(&elements, 20), "hello\n[Retry]");
    }

    #[test]
    fn buttons_are_labelled_affordances_not_interactive_widgets() {
        let lines = render_mod_ui(&[button("Run")], 20);
        let rendered = line_text(&lines[0]);
        assert_eq!(rendered, "[Run]");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "a button should read as an affordance"
        );
    }

    #[test]
    fn a_focused_button_is_marked_and_reversed() {
        let lines = render_mod_ui_with_focus(&[button("Run")], 20, Some("run"));
        assert_eq!(line_text(&lines[0]), "> [Run] <");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED),
            "the focused button should be visually distinct"
        );
    }

    #[test]
    fn only_the_focused_button_changes_its_text() {
        let elements = [button("Run"), button("Stop")];
        let rendered = render_mod_ui_text_with_focus(&elements, 20, Some("stop"));
        assert_eq!(rendered, "[Run]\n> [Stop] <");
        assert_eq!(
            render_mod_ui_text(&elements, 20),
            "[Run]\n[Stop]",
            "no focus keeps the existing output"
        );
    }

    #[test]
    fn focus_reaches_buttons_nested_in_boxes() {
        let tree = vec![UiElement::Box {
            title: Some("Panel".to_string()),
            children: vec![button("Go")],
        }];
        let rendered = render_mod_ui_text_with_focus(&tree, 24, Some("go"));
        assert!(rendered.contains("> [Go] <"), "{rendered}");
    }

    #[test]
    fn nested_boxes_snapshot() {
        let tree = vec![UiElement::Box {
            title: Some("Outer".to_string()),
            children: vec![
                text("hi"),
                UiElement::Box {
                    title: Some("Inner".to_string()),
                    children: vec![button("Go")],
                },
            ],
        }];

        let rendered = render_mod_ui_text(&tree, 24);
        let expected = "\
┌─ Outer ──────────────┐
│ hi                   │
│ ┌─ Inner ──────────┐ │
│ │ [Go]             │ │
│ └──────────────────┘ │
└──────────────────────┘";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn a_box_without_a_title_or_children_still_has_a_frame() {
        let tree = vec![UiElement::Box {
            title: None,
            children: Vec::new(),
        }];
        assert_eq!(
            render_mod_ui_text(&tree, 12),
            "┌──────────┐\n│          │\n└──────────┘"
        );
    }

    #[test]
    fn styles_map_to_the_terminal_vocabulary() {
        assert_eq!(ui_style(UiStyle::Plain), Style::new());
        assert!(
            ui_style(UiStyle::Bold)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(ui_style(UiStyle::Dim).add_modifier.contains(Modifier::DIM));
        assert_eq!(ui_style(UiStyle::Warning).fg, Some(Color::Yellow));
        assert_eq!(ui_style(UiStyle::Error).fg, Some(Color::Red));
    }

    #[test]
    fn a_text_style_reaches_the_rendered_spans() {
        let lines = render_mod_ui(
            &[UiElement::Text {
                text: "careful".to_string(),
                style: UiStyle::Warning,
            }],
            20,
        );
        assert_eq!(lines[0].spans[0].content.as_ref(), "careful");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn text_wraps_to_the_given_width() {
        let tree = vec![text("one two three four")];
        assert_eq!(render_mod_ui_text(&tree, 9), "one two\nthree\nfour");
    }

    #[test]
    fn long_words_and_narrow_buttons_are_clipped_to_the_width() {
        let tree = vec![text("abcdefghij"), button("Very long label")];
        let rendered = render_mod_ui_text(&tree, 5);
        for line in rendered.lines() {
            assert!(display_width(line) <= 5, "{line:?} is wider than 5 columns");
        }
    }

    #[test]
    fn box_lines_share_one_width() {
        let tree = vec![UiElement::Box {
            title: Some("T".to_string()),
            children: vec![text("a longer line of text"), button("Ok")],
        }];
        let rendered = render_mod_ui_text(&tree, 16);
        for line in rendered.lines() {
            assert_eq!(display_width(line), 16, "{line:?}");
        }
    }

    #[test]
    fn hits_match_the_rendered_lines_for_nested_boxes() {
        let tree = vec![UiElement::Box {
            title: Some("Outer".to_string()),
            children: vec![
                text("hi"),
                UiElement::Box {
                    title: Some("Inner".to_string()),
                    children: vec![button("Go")],
                },
            ],
        }];
        let (lines, hits) = render_mod_ui_with_hits(&tree, 24);
        assert_eq!(lines, render_mod_ui(&tree, 24), "rendering must not change");
        assert_eq!(
            hits,
            vec![ButtonHit {
                id: "go".to_string(),
                row: 3,
                col_start: 4,
                col_end: 8,
            }]
        );
    }

    #[test]
    fn wrapped_text_moves_a_button_down() {
        let tree = vec![text("one two three four"), button("Ok")];
        let (lines, hits) = render_mod_ui_with_hits(&tree, 9);
        assert_eq!(lines.len(), 4, "three text rows and one button row");
        assert_eq!(hits[0].row, 3);
        assert_eq!(hits[0].col_start, 0);
        assert_eq!(hits[0].col_end, 4);
    }

    #[test]
    fn the_focus_marker_changes_the_hit_width_but_not_its_origin() {
        let plain = render_mod_ui_with_hits(&[button("Run")], 20).1;
        assert_eq!(plain[0].col_start, 0);
        assert_eq!(plain[0].col_end, 5, "[Run]");

        let focused = render_mod_ui_with_focus_and_hits(&[button("Run")], 20, Some("run")).1;
        assert_eq!(focused[0].col_start, 0);
        assert_eq!(focused[0].col_end, 9, "> [Run] <");
    }

    #[test]
    fn clicks_on_borders_padding_or_outside_the_block_miss() {
        let tree = vec![UiElement::Box {
            title: None,
            children: vec![button("Go")],
        }];
        let (_, hits) = render_mod_ui_with_hits(&tree, 20);
        // The box renders three rows; the button is on row 1 at columns 2..6.
        let rect = Rect::new(10, 5, 20, 3);
        assert_eq!(button_at(rect, 0, 12, 6, &hits), Some("go"));
        // Left border (block column 0) and the padding after it.
        assert_eq!(button_at(rect, 0, 10, 6, &hits), None);
        assert_eq!(button_at(rect, 0, 11, 6, &hits), None);
        // Right of the label.
        assert_eq!(button_at(rect, 0, 17, 6, &hits), None);
        // Left of and above the rect.
        assert_eq!(button_at(rect, 0, 9, 6, &hits), None);
        assert_eq!(button_at(rect, 0, 12, 4, &hits), None);
        // Below the visible block.
        assert_eq!(button_at(rect, 0, 12, 8, &hits), None);
    }

    #[test]
    fn scroll_offset_maps_the_visible_top_row_back_to_the_block() {
        let tree = vec![text("a"), text("b"), button("Go")];
        let (_, hits) = render_mod_ui_with_hits(&tree, 10);
        assert_eq!(hits[0].row, 2);

        let rect = Rect::new(0, 10, 10, 2);
        assert_eq!(button_at(rect, 2, 1, 10, &hits), Some("go"));
        // The next visible screen row is block row 3, not a button.
        assert_eq!(button_at(rect, 2, 1, 11, &hits), None);
        // Without the scroll offset the same click lands on text row 1.
        assert_eq!(button_at(rect, 0, 1, 10, &hits), None);
    }

    #[test]
    fn a_click_between_buttons_misses_both() {
        let tree = vec![button("A"), button("B")];
        let (_, hits) = render_mod_ui_with_hits(&tree, 10);
        let rect = Rect::new(0, 0, 10, 2);
        assert_eq!(button_at(rect, 0, 0, 0, &hits), Some("a"));
        assert_eq!(button_at(rect, 0, 0, 1, &hits), Some("b"));
        assert_eq!(button_at(rect, 0, 3, 0, &hits), None);
    }

    #[test]
    fn a_zero_sized_rect_never_matches() {
        let (_, hits) = render_mod_ui_with_hits(&[button("Go")], 10);
        assert_eq!(button_at(Rect::new(0, 0, 0, 1), 0, 0, 0, &hits), None);
        assert_eq!(button_at(Rect::new(0, 0, 1, 0), 0, 0, 0, &hits), None);
    }
}
