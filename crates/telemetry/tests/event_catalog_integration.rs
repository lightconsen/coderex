//! The telemetry event catalogue: the five event kinds a gateway may receive,
//! their wire names, and the round-trip through the store.

use coderex_telemetry::{TelemetryEvent, TelemetryHandle, TelemetryStore};

#[test]
fn every_event_kind_is_stored_under_its_documented_name() {
    let dir = tempfile::tempdir().expect("dir");
    let store = TelemetryStore::open(&dir.path().join("telemetry.db")).expect("store");
    let (handle, uploader) = TelemetryHandle::new(&store, None);
    assert!(uploader.is_none(), "no config means no uploader");

    handle.session_start("s1".into());
    handle.tool_used("Bash".into(), true, 12);
    handle.api_call("anthropic".into(), "claude-sonnet-4-6".into(), 10, 20);
    handle.error("transport".into(), "provider unavailable".into());
    handle.session_end("s1".into(), 3);
    handle.shutdown();

    let mut pending = store.pending_events(10).expect("pending events");
    pending.sort_by(|a, b| a.event_type.cmp(&b.event_type));
    let kinds: Vec<&str> = pending
        .iter()
        .map(|event| event.event_type.as_str())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "api_call",
            "error",
            "session_end",
            "session_start",
            "tool_used"
        ],
        "the catalogue gained or lost an event kind"
    );

    // Every payload is the event itself, so a consumer can decode it rather
    // than parse prose.
    for event in &pending {
        let decoded: TelemetryEvent =
            serde_json::from_str(&event.payload).expect("payload is a TelemetryEvent");
        let name = match decoded {
            TelemetryEvent::SessionStart { .. } => "session_start",
            TelemetryEvent::SessionEnd { .. } => "session_end",
            TelemetryEvent::ToolUsed { .. } => "tool_used",
            TelemetryEvent::ApiCall { .. } => "api_call",
            TelemetryEvent::Error { .. } => "error",
        };
        assert_eq!(event.event_type, name);
    }
}

/// A credential in an error message must not reach the payload.
#[test]
fn the_catalogue_scrubs_credentials_before_storage() {
    let dir = tempfile::tempdir().expect("dir");
    let store = TelemetryStore::open(&dir.path().join("telemetry.db")).expect("store");
    let (handle, _uploader) = TelemetryHandle::new(&store, None);

    handle.error(
        "transport".into(),
        "request failed with key sk-ant-api03-abcdefghijklmnopqrstuvwxyz".into(),
    );
    handle.shutdown();

    let pending = store.pending_events(10).expect("pending events");
    let error = pending
        .iter()
        .find(|event| event.event_type == "error")
        .expect("error event");
    assert!(
        !error
            .payload
            .contains("sk-ant-api03-abcdefghijklmnopqrstuvwxyz"),
        "the credential reached the stored payload: {}",
        error.payload
    );
}
