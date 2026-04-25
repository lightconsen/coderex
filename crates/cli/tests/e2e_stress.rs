//! Performance and stress tests — large message lists, many commands, etc.

mod e2e_harness;

use coderex_commands::registry::{CommandContext, CommandRegistry};
use coderex_core::types::ModelId;

// Stress: Large message list rendering (simulates 1000+ messages)
#[test]
fn stress_large_message_list() {
    use coderex_ui::message_list::{Message, MessageRole, MessageView};
    use compact_str::CompactString;

    let mut view = MessageView::new(100);
    view.set_visible_count(100);
    // Add 5000 messages
    for i in 0..5000 {
        let role = if i % 2 == 0 {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        view.push(Message {
            role,
            content: CompactString::new(format!("Message {i}")),
            tool_name: None,
            streaming: false,
            folded: false,
        });
    }

    // Should be able to get visible messages without panic
    let visible = view.visible();
    assert!(!visible.is_empty());
}

// Stress: Message virtualization with scrolling
#[test]
fn stress_virtualization_scroll() {
    use coderex_ui::message_list::{Message, MessageRole, MessageView};
    use compact_str::CompactString;

    let mut view = MessageView::new(100);
    view.set_visible_count(100);
    for i in 0..10000 {
        let role = if i % 2 == 0 {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        view.push(Message {
            role,
            content: CompactString::new(format!(
                "A very long message number {i} with extra content to make it more realistic"
            )),
            tool_name: None,
            streaming: false,
            folded: false,
        });
    }

    // Scroll through the entire list
    for _ in 0..100 {
        view.scroll_down();
    }
    let visible = view.visible();
    assert!(!visible.is_empty());

    // Scroll back to top
    for _ in 0..200 {
        view.scroll_up(1);
    }
    let visible = view.visible();
    assert!(!visible.is_empty());
}

// Stress: Command registry with many lookups
#[test]
fn stress_command_registry_many_lookups() {
    let mut reg = CommandRegistry::new();
    coderex_commands::registry::register_all_commands(&mut reg);

    let ctx = CommandContext::new(
        "/src".into(),
        "stress".into(),
        "Stress".into(),
        ModelId::sonnet(),
    );

    // Execute 1000 commands
    for _ in 0..1000 {
        let result = reg.execute("status", &ctx, &[]);
        assert!(!result.output.is_empty());
    }
}

// Stress: Notification center with many notifications
#[test]
fn stress_notification_center_many() {
    use coderex_ui::notification_center::NotificationCenter;

    let mut center = NotificationCenter::new();

    // Add 1000 notifications
    for i in 0..1000 {
        center.info(&format!("Notification {i}"));
    }

    // Should not exceed max history
    assert!(center.active_count() <= 50 || center.history().len() <= 50);

    // Cleanup should handle many efficiently
    center.cleanup();
}

// Stress: Theme manager with many themes
#[test]
fn stress_theme_manager_many_themes() {
    use coderex_ui::themes::ThemeManager;
    use ratatui::style::Color;

    let mut mgr = ThemeManager::new();

    // Register 100 custom themes
    for i in 0..100 {
        let theme = coderex_ui::themes::Theme {
            name: format!("theme-{i}"),
            background: Color::Reset,
            foreground: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Blue,
            accent_secondary: Color::Cyan,
            statusbar_bg: Color::DarkGray,
            statusbar_fg: Color::White,
            error: Color::Red,
            warning: Color::Yellow,
            success: Color::Green,
            border: Color::DarkGray,
            selection_bg: Color::DarkGray,
            selection_fg: Color::White,
            input_bg: Color::Reset,
            input_fg: Color::Reset,
            overlay_bg: Color::Black,
            overlay_border: Color::Gray,
            vim_normal: Color::Cyan,
            vim_insert: Color::Green,
            notif_info: Color::Blue,
            notif_warning: Color::Yellow,
            notif_error: Color::Red,
        };
        mgr.register(theme);
    }

    assert!(mgr.list_themes().len() >= 100);

    // Switch between themes rapidly
    for i in 0..200 {
        let name = format!("theme-{i}");
        if i < 100 {
            assert!(mgr.switch(&name));
        }
    }
}

// Stress: Input buffer with many operations
#[test]
fn stress_input_buffer_many_operations() {
    use coderex_ui::input::{InputAction, InputBuffer};

    let mut buf = InputBuffer::new();

    // Insert 10000 characters
    for c in 'a'..='z' {
        for _ in 0..385 {
            buf.apply(&InputAction::InsertChar(c));
        }
    }
    assert!(buf.text.len() > 5000);

    // Navigate through the buffer
    for _ in 0..100 {
        buf.apply(&InputAction::CursorLeft);
    }
    for _ in 0..50 {
        buf.apply(&InputAction::CursorRight);
    }

    // Kill and restore
    buf.apply(&InputAction::KillToEnd);
    assert!(buf.text.len() < 10000);
}

// Stress: Vim state with many operations
#[test]
fn stress_vim_many_operations() {
    use coderex_ui::vim::VimState;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut state = VimState::new();
    let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

    // Execute 1000 vim commands
    for _ in 0..250 {
        state.handle_key(key('j')); // down
        state.handle_key(key('k')); // up
        state.handle_key(key('h')); // left
        state.handle_key(key('l')); // right
    }

    // Should still be in normal mode
    assert!(state.is_normal());

    // Many dd operations
    for _ in 0..100 {
        state.handle_key(key('d'));
        state.handle_key(key('d'));
    }

    // Many yank operations
    for _ in 0..100 {
        state.handle_key(key('y'));
        state.handle_key(key('y'));
    }
}

// Stress: Message selector with many messages
#[test]
fn stress_message_selector_many() {
    use coderex_ui::selector::MessageSelector;

    let mut selector = MessageSelector::new(1000);

    // Select all 1000 messages
    selector.select_all();
    assert_eq!(selector.count(), 1000);

    // Clear and verify
    selector.clear();
    assert_eq!(selector.count(), 0);

    // Re-select all
    selector.select_all();
    assert_eq!(selector.count(), 1000);
}

// Stress: Search state with many matches
#[test]
fn stress_search_many_matches() {
    use coderex_ui::search::SearchState;

    let mut state = SearchState::new();
    state.set_query("test");

    // Search in 10000 lines with 1000 matches
    let lines: Vec<&str> = (0..10000)
        .map(|i| {
            if i % 10 == 0 {
                "this is a test line"
            } else {
                "regular line"
            }
        })
        .collect();

    let result = state.search_in(&lines);
    assert!(!result.is_empty());
}

// Stress: Undo stack with many entries
#[test]
fn stress_undo_stack_many_entries() {
    use coderex_ui::vim::VimState;

    let mut state = VimState::new();

    // Push 200 undo entries
    for i in 0..200 {
        state.push_undo(&format!("state {i}"));
    }

    // Stack should be capped
    assert!(state.undo_stack.len() <= 100);

    // Pop all remaining
    let mut count = 0;
    while state.pop_undo().is_some() {
        count += 1;
    }
    assert!(count <= 100);
}

// Stress: Overlay stack with many pushes
#[test]
fn stress_overlay_stack_many() {
    use coderex_ui::overlay::{Overlay, OverlayStack};

    let mut stack = OverlayStack::new();

    // Push 100 overlays
    for i in 0..100 {
        stack.push(Overlay::PermissionDialog {
            tool: format!("tool-{i}"),
            args: format!("args-{i}"),
            always_option: false,
        });
    }

    // Should handle many overlays
    assert!(stack.is_active());

    // Pop all
    for _ in 0..100 {
        stack.pop();
    }
    assert!(!stack.is_active());
}

// Stress: Export many messages
#[test]
fn stress_export_many_messages() {
    use coderex_core::types::Message;
    use coderex_ui::export::{ExportFormat, export_messages};
    use coderex_ui::selector::MessageSelector;

    let messages: Vec<Message> = (0..1000)
        .map(|i| {
            if i % 2 == 0 {
                Message::user(&format!("User message {i}"))
            } else {
                Message::assistant(&format!("Assistant message {i}"))
            }
        })
        .collect();

    let selector = MessageSelector::new(messages.len());

    // Export as plain text
    let output = export_messages(&messages, &selector, ExportFormat::PlainText);
    assert!(output.contains("User"));

    // Export as JSON
    let output = export_messages(&messages, &selector, ExportFormat::Json);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed.len(), 1000);
}
