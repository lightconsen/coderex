//! Notification center — in-app notification queue with buddy/companion teasers.
//!
//! Notifications are displayed as overlays with auto-dismiss timers.
//! Different notification types trigger distinct UI behaviors.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Notification severity and display style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationKind {
    /// Informational message.
    Info,
    /// Warning about a potential issue.
    Warning,
    /// Error notification.
    Error,
    /// Success confirmation.
    Success,
    /// Buddy/companion teaser — invites the user to try a new feature.
    Teaser,
    /// Progress indicator for a long-running operation.
    Progress,
}

/// A notification to be displayed to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// Unique identifier for this notification.
    pub id: String,
    /// The notification message.
    pub message: String,
    /// Type of notification.
    pub kind: NotificationKind,
    /// When this notification was created.
    pub created_at: DateTime<Utc>,
    /// Auto-dismiss duration (None = persists until dismissed).
    pub auto_dismiss: Option<Duration>,
    /// Optional action label (e.g., "Try it").
    pub action_label: Option<String>,
    /// Whether this notification has been dismissed.
    pub dismissed: bool,
}

impl Notification {
    /// Create a new informational notification with a 5-second auto-dismiss.
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            id: Self::generate_id(),
            message: message.into(),
            kind: NotificationKind::Info,
            created_at: Utc::now(),
            auto_dismiss: Some(Duration::from_secs(5)),
            action_label: None,
            dismissed: false,
        }
    }

    /// Create a new warning notification (no auto-dismiss).
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            id: Self::generate_id(),
            message: message.into(),
            kind: NotificationKind::Warning,
            created_at: Utc::now(),
            auto_dismiss: None,
            action_label: None,
            dismissed: false,
        }
    }

    /// Create a new error notification (no auto-dismiss).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            id: Self::generate_id(),
            message: message.into(),
            kind: NotificationKind::Error,
            created_at: Utc::now(),
            auto_dismiss: None,
            action_label: None,
            dismissed: false,
        }
    }

    /// Create a new success notification with 3-second auto-dismiss.
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            id: Self::generate_id(),
            message: message.into(),
            kind: NotificationKind::Success,
            created_at: Utc::now(),
            auto_dismiss: Some(Duration::from_secs(3)),
            action_label: None,
            dismissed: false,
        }
    }

    /// Create a buddy/companion teaser notification.
    ///
    /// Teasers have a longer auto-dismiss (10s) and an action label.
    pub fn teaser(message: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            id: Self::generate_id(),
            message: message.into(),
            kind: NotificationKind::Teaser,
            created_at: Utc::now(),
            auto_dismiss: Some(Duration::from_secs(10)),
            action_label: Some(action.into()),
            dismissed: false,
        }
    }

    /// Create a progress notification (no auto-dismiss, manually cleared).
    pub fn progress(message: impl Into<String>) -> Self {
        Self {
            id: Self::generate_id(),
            message: message.into(),
            kind: NotificationKind::Progress,
            created_at: Utc::now(),
            auto_dismiss: None,
            action_label: None,
            dismissed: false,
        }
    }

    /// Check if this notification should auto-dismiss.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        if let Some(duration) = self.auto_dismiss {
            let elapsed = Utc::now().signed_duration_since(self.created_at);
            elapsed > chrono::TimeDelta::from_std(duration).unwrap_or(chrono::TimeDelta::zero())
        } else {
            false
        }
    }

    /// Dismiss this notification.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// Check if the notification is still active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.dismissed && !self.is_expired()
    }

    fn generate_id() -> String {
        let ts = Utc::now().timestamp_millis();
        format!("notif_{ts}")
    }
}

/// Notification center — manages the queue of active notifications.
#[derive(Debug)]
pub struct NotificationCenter {
    /// All notifications (including dismissed, for history).
    notifications: Vec<Notification>,
    /// Maximum number of notifications to retain in history.
    max_history: usize,
    /// Counter for unseen notifications.
    unseen_count: usize,
}

impl Default for NotificationCenter {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationCenter {
    /// Create a new notification center.
    #[must_use]
    pub fn new() -> Self {
        Self {
            notifications: Vec::new(),
            max_history: 50,
            unseen_count: 0,
        }
    }

    /// Add a notification to the center.
    pub fn notify(&mut self, notification: Notification) {
        self.unseen_count += 1;
        self.notifications.push(notification);
        // Trim old notifications
        if self.notifications.len() > self.max_history {
            let excess = self.notifications.len() - self.max_history;
            self.notifications.drain(..excess);
        }
    }

    /// Convenience: add an info notification.
    pub fn info(&mut self, message: impl Into<String>) {
        self.notify(Notification::info(message));
    }

    /// Convenience: add a warning notification.
    pub fn warn(&mut self, message: impl Into<String>) {
        self.notify(Notification::warning(message));
    }

    /// Convenience: add an error notification.
    pub fn err(&mut self, message: impl Into<String>) {
        self.notify(Notification::error(message));
    }

    /// Convenience: add a success notification.
    pub fn success(&mut self, message: impl Into<String>) {
        self.notify(Notification::success(message));
    }

    /// Convenience: add a teaser notification.
    pub fn teaser(&mut self, message: impl Into<String>, action: impl Into<String>) {
        self.notify(Notification::teaser(message, action));
    }

    /// Get all active (non-dismissed, non-expired) notifications.
    #[must_use]
    pub fn active(&self) -> Vec<&Notification> {
        self.notifications.iter().filter(|n| n.is_active()).collect()
    }

    /// Get the most recent active notification.
    #[must_use]
    pub fn latest(&self) -> Option<&Notification> {
        self.active().last().copied()
    }

    /// Dismiss a notification by ID.
    pub fn dismiss(&mut self, id: &str) -> bool {
        if let Some(n) = self.notifications.iter_mut().find(|n| n.id == id) {
            n.dismiss();
            true
        } else {
            false
        }
    }

    /// Dismiss all notifications.
    pub fn dismiss_all(&mut self) {
        for n in &mut self.notifications {
            n.dismiss();
        }
    }

    /// Mark all notifications as seen.
    pub fn mark_seen(&mut self) {
        self.unseen_count = 0;
    }

    /// Get the count of unseen notifications.
    #[must_use]
    pub fn unseen_count(&self) -> usize {
        self.unseen_count
    }

    /// Clean up expired and dismissed notifications.
    pub fn cleanup(&mut self) {
        self.notifications.retain(|n| n.is_active());
    }

    /// Get the number of active notifications.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active().len()
    }

    /// Get notification history (most recent first).
    #[must_use]
    pub fn history(&self) -> Vec<&Notification> {
        let mut hist: Vec<&Notification> = self.notifications.iter().collect();
        hist.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        hist
    }

    /// Check if there are any active teaser notifications.
    #[must_use]
    pub fn has_active_teasers(&self) -> bool {
        self.notifications
            .iter()
            .any(|n| n.is_active() && matches!(n.kind, NotificationKind::Teaser))
    }
}

/// Pre-built teaser messages for buddy/companion features.
pub mod teasers {
    use super::{Notification, NotificationCenter};

    /// Create a teaser for the buddy feature.
    #[must_use]
    pub fn buddy_introduction() -> Notification {
        Notification::teaser(
            "Meet Buddy — your AI companion that works alongside you. Want to try it?",
            "Enable Buddy",
        )
    }

    /// Create a teaser for the companion panel.
    #[must_use]
    pub fn companion_panel() -> Notification {
        Notification::teaser(
            "The companion panel shows real-time context from your workspace. Open it?",
            "Open Panel",
        )
    }

    /// Create a teaser for Vim mode.
    #[must_use]
    pub fn vim_mode_intro() -> Notification {
        Notification::teaser(
            "Vim mode is available! Press Esc to switch between normal and insert modes.",
            "Learn More",
        )
    }

    /// Create a teaser for team memory sync.
    #[must_use]
    pub fn team_memory_intro() -> Notification {
        Notification::teaser(
            "Team memory sync lets you share knowledge across your org. Set it up?",
            "Configure",
        )
    }

    /// Push a sequence of feature teasers to the notification center.
    pub fn push_feature_discovery(center: &mut NotificationCenter) {
        center.notify(buddy_introduction());
        center.notify(vim_mode_intro());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_notifications() {
        let n = Notification::info("hello");
        assert_eq!(n.kind, NotificationKind::Info);
        assert!(!n.dismissed);

        let t = Notification::teaser("Try this!", "Go");
        assert_eq!(t.kind, NotificationKind::Teaser);
        assert!(t.action_label.is_some());
    }

    #[test]
    fn notification_auto_expires() {
        let mut n = Notification::info("quick");
        assert!(n.is_active());
        // Manually expire
        n.created_at = Utc::now() - Duration::from_secs(10);
        assert!(n.is_expired());
        assert!(!n.is_active());
    }

    #[test]
    fn notification_no_expiry() {
        let n = Notification::warning("important");
        assert!(!n.is_expired());
        assert!(n.is_active());
    }

    #[test]
    fn dismiss_notification() {
        let mut n = Notification::info("dismiss me");
        n.dismiss();
        assert!(n.dismissed);
        assert!(!n.is_active());
    }

    #[test]
    fn notification_center_adds_and_lists() {
        let mut center = NotificationCenter::new();
        center.info("one");
        center.info("two");
        center.warn("three");

        let active = center.active();
        assert_eq!(active.len(), 3);
    }

    #[test]
    fn notification_center_dismiss_by_id() {
        let mut center = NotificationCenter::new();
        center.info("first");
        let id = center.active()[0].id.clone();
        assert!(center.dismiss(&id));
        assert_eq!(center.active_count(), 0);
    }

    #[test]
    fn notification_center_dismiss_all() {
        let mut center = NotificationCenter::new();
        center.info("a");
        center.info("b");
        center.info("c");
        center.dismiss_all();
        assert_eq!(center.active_count(), 0);
    }

    #[test]
    fn unseen_count_tracks_notifications() {
        let mut center = NotificationCenter::new();
        center.info("one");
        center.info("two");
        assert_eq!(center.unseen_count(), 2);
        center.mark_seen();
        assert_eq!(center.unseen_count(), 0);
    }

    #[test]
    fn cleanup_removes_expired() {
        let mut center = NotificationCenter::new();
        center.info("will expire");
        // Force expire
        if let Some(n) = center.notifications.last_mut() {
            n.created_at = Utc::now() - Duration::from_secs(10);
        }
        center.info("fresh");
        center.cleanup();
        assert_eq!(center.active_count(), 1);
    }

    #[test]
    fn history_sorted_most_recent_first() {
        let mut center = NotificationCenter::new();
        center.info("first");
        std::thread::sleep(Duration::from_millis(1));
        center.info("second");
        std::thread::sleep(Duration::from_millis(1));
        center.info("third");

        let hist = center.history();
        assert_eq!(hist[0].message, "third");
        assert_eq!(hist[1].message, "second");
        assert_eq!(hist[2].message, "first");
    }

    #[test]
    fn teaser_helpers() {
        let teaser = teasers::buddy_introduction();
        assert_eq!(teaser.kind, NotificationKind::Teaser);
        assert!(teaser.message.contains("Buddy"));

        let teaser = teasers::vim_mode_intro();
        assert!(teaser.message.contains("Vim"));
    }

    #[test]
    fn push_feature_discovery() {
        let mut center = NotificationCenter::new();
        teasers::push_feature_discovery(&mut center);
        assert_eq!(center.active_count(), 2);
        assert!(center.has_active_teasers());
    }

    #[test]
    fn has_active_teasers() {
        let mut center = NotificationCenter::new();
        assert!(!center.has_active_teasers());
        center.teaser("Try vim mode!", "Learn");
        assert!(center.has_active_teasers());
    }

    #[test]
    fn convenience_methods() {
        let mut center = NotificationCenter::new();
        center.info("info msg");
        center.warn("warn msg");
        center.err("error msg");
        center.success("success msg");
        center.teaser("teaser", "Action");

        assert_eq!(center.active_count(), 5);
        assert_eq!(center.unseen_count(), 5);
    }

    #[test]
    fn progress_notification() {
        let mut center = NotificationCenter::new();
        center.notify(Notification::progress("Processing..."));
        let active = center.active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].kind, NotificationKind::Progress);
        // Progress notifications don't auto-expire
        assert!(!active[0].is_expired());
    }
}
