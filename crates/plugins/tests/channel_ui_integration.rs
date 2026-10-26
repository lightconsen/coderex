//! Integration tests for plugin channel fan-out and structured plugin panels.
//!
//! The channel router is transport-free by design: these exercise the
//! in-process delivery contract (multiple channels, multiple sinks, sequence
//! ordering and reentrant unsubscribe) rather than a socket.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use coderex_plugins::plugin_ui::render_plugin_response;
use coderex_plugins::plugins::{ChannelSink, PluginChannelEvent, PluginManager, PluginManifest};

#[derive(Debug, Default)]
struct RecordingSink {
    events: Mutex<Vec<PluginChannelEvent>>,
}

impl RecordingSink {
    fn events(&self) -> Vec<PluginChannelEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl ChannelSink for RecordingSink {
    fn deliver(&self, event: PluginChannelEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// A sink that unsubscribes its channel while it is being delivered to.
#[derive(Debug)]
struct UnsubscribingSink {
    manager: Arc<PluginManager>,
    events: Mutex<Vec<PluginChannelEvent>>,
}

impl ChannelSink for UnsubscribingSink {
    fn deliver(&self, event: PluginChannelEvent) {
        self.manager.unsubscribe_channel(&event.channel);
        self.events.lock().unwrap().push(event);
    }
}

fn manifest(json: serde_json::Value) -> PluginManifest {
    serde_json::from_value(json).expect("manifest")
}

fn manager_with_channels() -> Arc<PluginManager> {
    let manager = Arc::new(PluginManager::new());
    manager.load_plugin(
        manifest(serde_json::json!({
            "name": "chat-plugin",
            "version": "1.0.0",
            "mcpServers": {"slack": {"command": "slack-server"}},
            "channels": {
                "slack": {"server": "slack", "displayName": "Slack"},
                "alerts": {"server": "slack", "displayName": "Alerts"},
                "nowhere": {"server": "missing", "displayName": "Nowhere"},
            },
        })),
        PathBuf::from("/plugins/chat-plugin"),
    );
    manager
}

#[test]
fn every_channel_bound_to_a_server_reaches_every_sink() {
    let manager = manager_with_channels();
    let slack = Arc::new(RecordingSink::default());
    let alerts = Arc::new(RecordingSink::default());
    let also_slack = Arc::new(RecordingSink::default());

    manager.subscribe_channel("slack", slack.clone());
    manager.subscribe_channel("slack", also_slack.clone());
    manager.subscribe_channel("alerts", alerts.clone());

    let delivered = manager.route_channel_notification(
        "slack",
        "notifications/resources/updated",
        Some(&serde_json::json!({"uri": "file:///x"})),
    );

    assert_eq!(delivered, 3, "two channels, three subscriptions");
    assert_eq!(slack.events().len(), 1);
    assert_eq!(also_slack.events().len(), 1);
    assert_eq!(alerts.events().len(), 1);

    let event = &slack.events()[0];
    assert_eq!(event.plugin, "chat-plugin");
    assert_eq!(event.server, "slack");
    assert_eq!(event.method, "notifications/resources/updated");
    assert_eq!(event.params.as_ref().unwrap()["uri"], "file:///x");
    assert_eq!(alerts.events()[0].channel, "alerts");

    let report = manager.plugin_channel_report();
    assert_eq!(report.len(), 3);
    let slack_report = report
        .iter()
        .find(|entry| entry.channel == "slack")
        .unwrap();
    assert_eq!(slack_report.subscribers, 2);
    let missing = report
        .iter()
        .find(|entry| entry.channel == "nowhere")
        .unwrap();
    assert_eq!(
        missing.status,
        coderex_plugins::plugins::ChannelStatus::ServerNotConfigured
    );
}

#[test]
fn sequences_increase_in_delivery_order() {
    let manager = manager_with_channels();
    let sink = Arc::new(RecordingSink::default());
    manager.subscribe_channel("slack", sink.clone());

    for _ in 0..3 {
        manager.route_channel_notification("slack", "notifications/ping", None);
    }

    let sequences: Vec<u64> = sink.events().iter().map(|event| event.sequence).collect();
    assert_eq!(sequences.len(), 3);
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "{sequences:?}"
    );
}

#[test]
fn unsubscribing_from_inside_delivery_does_not_skip_the_remaining_sinks() {
    let manager = manager_with_channels();
    let first = Arc::new(RecordingSink::default());
    let second = Arc::new(RecordingSink::default());
    let unsubscribing = Arc::new(UnsubscribingSink {
        manager: Arc::clone(&manager),
        events: Mutex::new(Vec::new()),
    });

    manager.subscribe_channel("slack", first.clone());
    manager.subscribe_channel("slack", unsubscribing.clone());
    manager.subscribe_channel("slack", second.clone());

    let delivered = manager.route_channel_notification("slack", "notifications/x", None);

    assert_eq!(
        delivered, 3,
        "the sink list is cloned before delivery, so removing a sink mid-delivery is safe"
    );
    assert_eq!(first.events().len(), 1);
    assert_eq!(unsubscribing.events.lock().unwrap().len(), 1);
    assert_eq!(second.events().len(), 1, "the later sink still received");
    assert_eq!(manager.channel_subscribers("slack"), 0);
}

#[test]
fn a_notification_for_an_unbound_server_delivers_nothing() {
    let manager = manager_with_channels();
    let sink = Arc::new(RecordingSink::default());
    manager.subscribe_channel("slack", sink.clone());

    assert_eq!(
        manager.route_channel_notification("other", "notifications/x", None),
        0
    );
    assert!(sink.events().is_empty());
}

// ─── Structured plugin panels ──────────────────────────────────────────────

#[test]
fn a_ui_payload_becomes_a_titled_panel_with_its_summary() {
    let output = serde_json::json!({
        "text": "deploy finished",
        "ui": {
            "title": "Deploy status",
            "style": "warning",
            "lines": ["api: ok", "web: failed"],
        }
    })
    .to_string();

    let rendered = render_plugin_response(&output);

    assert!(rendered.starts_with("deploy finished\n"), "{rendered}");
    assert!(rendered.contains("╭─ ! Deploy status"), "{rendered}");
    assert!(rendered.contains("api: ok"), "{rendered}");
    assert!(rendered.contains("web: failed"), "{rendered}");
}

#[test]
fn a_ui_payload_without_a_title_or_lines_is_an_empty_box() {
    let rendered = render_plugin_response(r#"{"ui":{}}"#);

    assert_eq!(rendered, "╭──╮\n╰──╯");
}

#[test]
fn non_object_ui_values_and_missing_ui_are_passed_through() {
    for output in [
        r#"{"ui":"not an object"}"#,
        r#"{"ui":null}"#,
        r#"{"text":"no panel here"}"#,
        r"not json at all",
    ] {
        assert_eq!(render_plugin_response(output), output);
    }
}

#[test]
fn long_lines_are_clipped_to_the_panel_width() {
    let long = "x".repeat(120);
    let output = serde_json::json!({
        "ui": {"title": "Long", "lines": [long]}
    })
    .to_string();

    let rendered = render_plugin_response(&output);

    for line in rendered.lines() {
        assert!(
            line.chars().count() <= 82,
            "line exceeds the panel: {} chars: {line}",
            line.chars().count()
        );
    }
    assert!(rendered.contains('…'), "the clip is visible: {rendered}");
    assert!(!rendered.contains(&"x".repeat(100)), "{rendered}");
}

#[test]
fn non_string_lines_render_blank_rather_than_failing() {
    let output = serde_json::json!({
        "ui": {"title": "Mixed", "lines": ["ok", 42, null, {"a": 1}]}
    })
    .to_string();

    let rendered = render_plugin_response(&output);

    assert!(rendered.contains("ok"), "{rendered}");
    assert_eq!(rendered.lines().count(), 6, "border + 4 lines + border");
}
