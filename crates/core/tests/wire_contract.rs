//! Wire-protocol transcript contract.
//!
//! Ported from qwen-code's `chat-transcript-contract`: the fixture freezes the
//! JSON shape an external host consumes. Each line must round-trip unchanged,
//! every event type must carry exactly its declared keys, and raw renderer
//! fields must not survive the projection.

use coderex_core::wire_protocol::WireEvent;
use std::collections::HashMap;

const FIXTURE: &str = include_str!("fixtures/wire_contract.jsonl");

fn expected_keys() -> HashMap<&'static str, &'static [&'static str]> {
    HashMap::from([
        ("text_delta", &["type", "delta"][..]),
        ("tool_call_start", &["type", "id", "name", "summary"][..]),
        ("tool_call_end", &["type", "id", "result"][..]),
        ("mod_ui", &["type", "elements"][..]),
        ("compact_boundary", &["type"][..]),
        ("permission_request", &["type", "id", "tool", "args"][..]),
        (
            "usage_summary",
            &[
                "type",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_creation_tokens",
            ][..],
        ),
        ("turn_start", &["type", "turn_number"][..]),
        ("turn_end", &["type", "turn_number", "stop_reason"][..]),
        ("session_end", &["type"][..]),
        ("error", &["type", "message"][..]),
        ("notification", &["type", "message"][..]),
        (
            "permission_explanation",
            &["type", "tool", "explanation"][..],
        ),
        (
            "stream_interrupted",
            &["type", "reason", "partial_len", "will_retry"][..],
        ),
    ])
}

#[test]
fn every_wire_event_round_trips_and_keeps_its_schema() {
    let expected = expected_keys();
    let mut seen = std::collections::HashSet::new();

    for line in FIXTURE.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}\n{line}"));
        let event: WireEvent =
            serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{error}\n{line}"));
        let encoded = serde_json::to_value(&event).expect("serialize");
        assert_eq!(encoded, value, "round trip changed {line}");

        let event_type = value["type"].as_str().expect("a type tag");
        let keys = expected
            .get(event_type)
            .unwrap_or_else(|| panic!("unfrozen event type {event_type}"));
        let mut actual: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        actual.sort_unstable();
        let mut wanted = keys.to_vec();
        wanted.sort_unstable();
        assert_eq!(actual, wanted, "schema drift for {event_type}");
        seen.insert(event_type.to_string());
    }

    assert_eq!(
        seen.len(),
        expected.len(),
        "the fixture must cover every frozen event type"
    );
}

#[test]
fn raw_renderer_fields_are_dropped_by_the_projection() {
    let line = r#"{"type":"tool_call_end","id":"tc_1","result":{"tool_call_id":"tc_1","rawOutput":{"x":1},"output":"ok","is_error":false,"eventId":7}}"#;
    let event: WireEvent = serde_json::from_str(line).unwrap();
    let encoded = serde_json::to_string(&event).unwrap();

    assert!(!encoded.contains("rawOutput"));
    assert!(!encoded.contains("eventId"));
    assert_eq!(
        encoded,
        r#"{"type":"tool_call_end","id":"tc_1","result":{"tool_call_id":"tc_1","output":"ok","is_error":false}}"#
    );
}

#[test]
fn an_unknown_event_type_is_rejected() {
    let result = serde_json::from_str::<WireEvent>(r#"{"type":"not_an_event"}"#);
    assert!(result.is_err());
}
