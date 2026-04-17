//! Global keybinding system — configurable key-to-action mapping.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;

/// A keybinding trigger action.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyTrigger {
    /// The key code.
    pub code: KeyCode,
    /// Active modifiers.
    pub modifiers: KeyModifiers,
}

impl KeyTrigger {
    /// Create a new key trigger.
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// From a raw key event.
    #[must_use]
    pub const fn from_event(event: KeyEvent) -> Self {
        Self {
            code: event.code,
            modifiers: event.modifiers,
        }
    }
}

/// Keybinding manager — maps key triggers to named actions.
#[derive(Debug)]
pub struct Keymap {
    bindings: HashMap<KeyTrigger, String>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new()
    }
}

impl Keymap {
    /// Create a new keymap with default bindings.
    #[must_use]
    pub fn new() -> Self {
        let mut map = Self {
            bindings: HashMap::new(),
        };
        map.load_defaults();
        map
    }

    fn load_defaults(&mut self) {
        // Navigation
        self.bind(KeyCode::Up, KeyModifiers::NONE, "history_up");
        self.bind(KeyCode::Down, KeyModifiers::NONE, "history_down");
        self.bind(KeyCode::PageUp, KeyModifiers::NONE, "scroll_up");
        self.bind(KeyCode::PageDown, KeyModifiers::NONE, "scroll_down");
        self.bind(KeyCode::Home, KeyModifiers::NONE, "scroll_top");
        self.bind(KeyCode::End, KeyModifiers::NONE, "scroll_bottom");

        // Editing
        self.bind(KeyCode::Left, KeyModifiers::NONE, "cursor_left");
        self.bind(KeyCode::Right, KeyModifiers::NONE, "cursor_right");
        self.bind(KeyCode::Backspace, KeyModifiers::NONE, "backspace");
        self.bind(KeyCode::Delete, KeyModifiers::NONE, "delete");

        // Actions
        self.bind(KeyCode::Enter, KeyModifiers::NONE, "submit");
        self.bind(KeyCode::Esc, KeyModifiers::NONE, "interrupt");
        self.bind(KeyCode::Tab, KeyModifiers::NONE, "tab_complete");

        // Ctrl shortcuts
        self.bind(KeyCode::Char('c'), KeyModifiers::CONTROL, "interrupt");
        self.bind(KeyCode::Char('a'), KeyModifiers::CONTROL, "move_start");
        self.bind(KeyCode::Char('e'), KeyModifiers::CONTROL, "move_end");
        self.bind(KeyCode::Char('k'), KeyModifiers::CONTROL, "kill_end");
        self.bind(KeyCode::Char('u'), KeyModifiers::CONTROL, "kill_start");
        self.bind(KeyCode::Char('w'), KeyModifiers::CONTROL, "delete_word");
        self.bind(KeyCode::Char('f'), KeyModifiers::CONTROL, "search");
        self.bind(KeyCode::Char('p'), KeyModifiers::CONTROL, "command_palette");
    }

    /// Bind a key combination to a named action.
    pub fn bind(&mut self, code: KeyCode, modifiers: KeyModifiers, action: impl Into<String>) {
        self.bindings.insert(KeyTrigger::new(code, modifiers), action.into());
    }

    /// Look up the action for a key event.
    #[must_use]
    pub fn lookup(&self, event: KeyEvent) -> Option<&str> {
        let trigger = KeyTrigger::from_event(event);
        self.bindings.get(&trigger).map(String::as_str)
    }

    /// Remove a binding.
    pub fn unbind(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.bindings.remove(&KeyTrigger::new(code, modifiers));
    }

    /// List all bindings.
    #[must_use]
    pub fn list(&self) -> Vec<(&KeyTrigger, &String)> {
        self.bindings.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings_present() {
        let km = Keymap::new();
        assert_eq!(
            km.lookup(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some("submit")
        );
        assert_eq!(
            km.lookup(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some("history_up")
        );
        assert_eq!(
            km.lookup(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some("interrupt")
        );
    }

    #[test]
    fn custom_binding() {
        let mut km = Keymap::new();
        km.bind(KeyCode::Char('x'), KeyModifiers::NONE, "custom_action");
        assert_eq!(
            km.lookup(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some("custom_action")
        );
    }

    #[test]
    fn unbind_removes() {
        let mut km = Keymap::new();
        km.unbind(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            km.lookup(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
    }

    #[test]
    fn list_returns_all() {
        let km = Keymap::new();
        let bindings = km.list();
        assert!(!bindings.is_empty());
    }

    #[test]
    fn unknown_key_returns_none() {
        let km = Keymap::new();
        assert!(
            km.lookup(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE))
                .is_none()
        );
    }
}
