//! TodoStore — in-memory task list storage shared via `ToolContext.extra`.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// A single todo item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    /// Short subject/title of the todo.
    pub subject: String,
    /// Longer description (optional).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Current status.
    pub status: TodoStatus,
    /// Whether this todo has been explicitly marked active.
    #[serde(default)]
    pub active: bool,
}

/// Status of a todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not yet started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Done.
    Completed,
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoStatus::Pending => write!(f, "pending"),
            TodoStatus::InProgress => write!(f, "in_progress"),
            TodoStatus::Completed => write!(f, "completed"),
        }
    }
}

impl Default for TodoStatus {
    fn default() -> Self {
        Self::Pending
    }
}

impl TodoItem {
    /// Create a new todo item with the given subject.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            description: String::new(),
            status: TodoStatus::Pending,
            active: false,
        }
    }
}

/// Shared storage for the todo list.
#[derive(Debug, Default)]
pub struct TodoStore {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoStore {
    /// Create an empty todo store.
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }

    /// Replace the entire todo list.
    pub fn set_all(&self, items: Vec<TodoItem>) {
        *self.items.lock() = items;
    }

    /// Get a clone of all todos.
    pub fn get_all(&self) -> Vec<TodoItem> {
        self.items.lock().clone()
    }

    /// Clear all todos.
    pub fn clear(&self) {
        self.items.lock().clear();
    }
}

/// Key used in `ToolContext.extra` to access the TodoStore.
pub const TODO_STORE_KEY: &str = "todo_store";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_store_new_is_empty() {
        let store = TodoStore::new();
        assert!(store.get_all().is_empty());
    }

    #[test]
    fn todo_store_set_and_get() {
        let store = TodoStore::new();
        let items = vec![TodoItem::new("task1"), TodoItem::new("task2")];
        store.set_all(items);
        let got = store.get_all();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].subject, "task1");
    }

    #[test]
    fn todo_store_clear() {
        let store = TodoStore::new();
        store.set_all(vec![TodoItem::new("x")]);
        store.clear();
        assert!(store.get_all().is_empty());
    }

    #[test]
    fn todo_status_display() {
        assert_eq!(TodoStatus::Pending.to_string(), "pending");
        assert_eq!(TodoStatus::InProgress.to_string(), "in_progress");
        assert_eq!(TodoStatus::Completed.to_string(), "completed");
    }
}
