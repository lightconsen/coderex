//! Gateway-side relay for permission frames.
//!
//! The bridge emits `permission_request` frames and consumes
//! `permission_response` frames, but it does not sit directly in front of the
//! person answering them: a gateway does. [`PermissionFrameRelay`] is the
//! reusable half of that gateway. It keeps the two frame kinds moving between
//! the worker side (the bridge's transport) and the host side (the connection a
//! human client reads and writes), and it tracks which requests are still
//! outstanding so an answer is relayed exactly once.
//!
//! Frames the relay does not own are returned as `None`. Callers keep those
//! frames on their normal path; the relay only decides what happens to
//! permission traffic.
//!
//! The relay does not rewrite a frame. The correlation ID and the payload's
//! `request_id` are the ids the bridge and the child already agreed on, so the
//! message is forwarded verbatim and the bridge's id alias mapping keeps
//! working.

use std::collections::HashSet;

use tracing::warn;

use crate::orchestrator::{PERMISSION_REQUEST_TYPE, PERMISSION_RESPONSE_TYPE};
use crate::transport::TransportMessage;

/// The alternate spelling the bridge accepts for a permission answer.
///
/// [`crate::orchestrator::Bridge::handle_transport_message`] treats `approval`
/// as a synonym for `permission_response`, so a gateway that uses that spelling
/// is relayed the same way.
const PERMISSION_RESPONSE_ALIAS: &str = "approval";

/// Routes permission frames between a worker side and a host side.
///
/// One relay belongs to one worker connection: the pending set is the set of
/// questions that worker is still waiting on.
#[derive(Debug, Default)]
pub struct PermissionFrameRelay {
    /// Request ids the host has been asked about and has not answered.
    pending: HashSet<String>,
}

impl PermissionFrameRelay {
    /// Create a relay with no outstanding requests.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a frame from the worker side.
    ///
    /// Returns the frame to send on to the host when it is a
    /// `permission_request`, and `None` for anything else (the caller passes
    /// those through unchanged). A request is remembered until answered, so an
    /// answer for an id the relay never saw is refused rather than forwarded.
    ///
    /// A request without a usable id, or one repeating an id that is already
    /// outstanding, is also refused: forwarding either would leave the host
    /// with a question it cannot answer unambiguously.
    pub fn relay_worker_frame(&mut self, message: TransportMessage) -> Option<TransportMessage> {
        if message.r#type != PERMISSION_REQUEST_TYPE {
            return None;
        }

        let Some(request_id) = frame_request_id(&message) else {
            warn!(
                frame_type = %message.r#type,
                "permission_request has no request id; not relaying"
            );
            return None;
        };

        if !self.pending.insert(request_id.clone()) {
            warn!(
                request_id = %request_id,
                "permission_request repeats an outstanding id; not relaying again"
            );
            return None;
        }

        Some(message)
    }

    /// Take a frame from the host side.
    ///
    /// Returns the frame to send on to the worker when it answers an
    /// outstanding request, and `None` for anything else (the caller passes
    /// those through unchanged). A `permission_response` for an unknown or
    /// already-answered id is not forwarded; the pending entry is removed so a
    /// duplicate cannot answer the same request twice.
    pub fn relay_host_frame(&mut self, message: TransportMessage) -> Option<TransportMessage> {
        if message.r#type != PERMISSION_RESPONSE_TYPE && message.r#type != PERMISSION_RESPONSE_ALIAS
        {
            return None;
        }

        let Some(request_id) = frame_request_id(&message) else {
            warn!(
                frame_type = %message.r#type,
                "permission_response has no request id; not relaying"
            );
            return None;
        };

        if !self.pending.remove(&request_id) {
            warn!(
                request_id = %request_id,
                "permission_response has no pending request; not relaying"
            );
            return None;
        }

        // Forward the original frame: the correlation ID and payload ids are
        // the worker's own and must not be re-encoded.
        Some(message)
    }

    /// How many requests are waiting for an answer.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// The request id a permission frame names.
///
/// The correlation ID is the id the worker routes on. The payload's
/// `request_id` is what the bridge resolves against. Both are normally present
/// and equal; if they disagree, or neither is usable, there is no unambiguous
/// request to relay and the frame is refused.
fn frame_request_id(message: &TransportMessage) -> Option<String> {
    let correlation_id = message
        .correlation_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let payload_id = payload_request_id(&message.payload);

    match (correlation_id, payload_id) {
        (Some(correlation), Some(payload)) if correlation != payload => {
            warn!(
                correlation_id = %correlation,
                request_id = %payload,
                "permission frame names two different request ids; not relaying"
            );
            None
        }
        (Some(correlation), _) => Some(correlation.to_string()),
        (None, payload) => payload,
    }
}

/// Read `request_id` from a frame payload, if it is a JSON object with one.
fn payload_request_id(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    value
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn frame(frame_type: &str, correlation_id: Option<&str>, payload: &str) -> TransportMessage {
        TransportMessage {
            r#type: frame_type.to_string(),
            correlation_id: correlation_id.map(str::to_string),
            payload: payload.to_string(),
            metadata: HashMap::new(),
        }
    }

    fn request_frame(request_id: &str) -> TransportMessage {
        frame(
            PERMISSION_REQUEST_TYPE,
            Some(request_id),
            &format!(
                r#"{{ "request_id": "{request_id}", "session_id": "sess_1", "tool": "Bash", "args": "ls" }}"#
            ),
        )
    }

    fn response_frame(request_id: &str) -> TransportMessage {
        frame(
            PERMISSION_RESPONSE_TYPE,
            Some(request_id),
            &format!(r#"{{ "request_id": "{request_id}", "allowed": true, "always": false }}"#),
        )
    }

    #[test]
    fn a_request_flows_worker_to_host_unchanged() {
        let mut relay = PermissionFrameRelay::new();
        let request = request_frame("child-req-7");
        let expected_payload = request.payload.clone();

        let relayed = relay.relay_worker_frame(request).expect("request relayed");

        assert_eq!(relayed.r#type, PERMISSION_REQUEST_TYPE);
        assert_eq!(relayed.correlation_id.as_deref(), Some("child-req-7"));
        assert_eq!(relayed.payload, expected_payload);
        assert_eq!(relay.pending(), 1);
    }

    #[test]
    fn a_response_flows_host_to_worker_unchanged() {
        let mut relay = PermissionFrameRelay::new();
        relay
            .relay_worker_frame(request_frame("child-req-7"))
            .unwrap();

        let response = response_frame("child-req-7");
        let expected_payload = response.payload.clone();
        let relayed = relay.relay_host_frame(response).expect("response relayed");

        assert_eq!(relayed.r#type, PERMISSION_RESPONSE_TYPE);
        assert_eq!(relayed.correlation_id.as_deref(), Some("child-req-7"));
        assert_eq!(relayed.payload, expected_payload);
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn the_alias_spelling_of_a_response_is_relayed() {
        let mut relay = PermissionFrameRelay::new();
        relay.relay_worker_frame(request_frame("perm_1")).unwrap();

        let relayed = relay
            .relay_host_frame(frame(
                PERMISSION_RESPONSE_ALIAS,
                Some("perm_1"),
                r#"{"request_id":"perm_1","allowed":false}"#,
            ))
            .expect("alias relayed");

        assert_eq!(relayed.r#type, PERMISSION_RESPONSE_ALIAS);
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn a_response_with_no_pending_request_is_not_forwarded() {
        let mut relay = PermissionFrameRelay::new();

        assert!(
            relay
                .relay_host_frame(response_frame("perm_missing"))
                .is_none()
        );
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn a_duplicate_response_is_not_forwarded_twice() {
        let mut relay = PermissionFrameRelay::new();
        relay.relay_worker_frame(request_frame("perm_1")).unwrap();

        assert!(relay.relay_host_frame(response_frame("perm_1")).is_some());
        // The request was answered, so the replay has nothing left to answer.
        assert!(relay.relay_host_frame(response_frame("perm_1")).is_none());
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn a_duplicate_request_is_not_forwarded_twice() {
        let mut relay = PermissionFrameRelay::new();

        assert!(relay.relay_worker_frame(request_frame("perm_1")).is_some());
        assert!(relay.relay_worker_frame(request_frame("perm_1")).is_none());
        assert_eq!(relay.pending(), 1);
    }

    #[test]
    fn pending_goes_up_and_down_per_request() {
        let mut relay = PermissionFrameRelay::new();

        relay.relay_worker_frame(request_frame("perm_1")).unwrap();
        relay.relay_worker_frame(request_frame("perm_2")).unwrap();
        assert_eq!(relay.pending(), 2);

        relay.relay_host_frame(response_frame("perm_1")).unwrap();
        assert_eq!(relay.pending(), 1);

        relay.relay_host_frame(response_frame("perm_2")).unwrap();
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn unrelated_frames_return_none_in_both_directions() {
        let mut relay = PermissionFrameRelay::new();

        for frame_type in [
            "session_output",
            "heartbeat",
            "relay_call",
            "permission_result",
        ] {
            let worker_frame = frame(frame_type, None, "{}");
            assert!(
                relay.relay_worker_frame(worker_frame).is_none(),
                "worker frame {frame_type}"
            );

            let host_frame = frame(frame_type, None, "{}");
            assert!(
                relay.relay_host_frame(host_frame).is_none(),
                "host frame {frame_type}"
            );
        }
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn the_payload_id_is_used_when_the_correlation_id_is_absent() {
        let mut relay = PermissionFrameRelay::new();
        let request = frame(
            PERMISSION_REQUEST_TYPE,
            None,
            r#"{"request_id":"prop-req-3","tool":"Read"}"#,
        );

        assert!(relay.relay_worker_frame(request).is_some());
        assert_eq!(relay.pending(), 1);

        let response = frame(
            PERMISSION_RESPONSE_TYPE,
            None,
            r#"{"request_id":"prop-req-3","allowed":true}"#,
        );
        assert!(relay.relay_host_frame(response).is_some());
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn a_frame_without_an_id_is_not_relayed() {
        let mut relay = PermissionFrameRelay::new();

        assert!(
            relay
                .relay_worker_frame(frame(PERMISSION_REQUEST_TYPE, None, "{}"))
                .is_none()
        );
        assert!(
            relay
                .relay_host_frame(frame(PERMISSION_RESPONSE_TYPE, None, "not json"))
                .is_none()
        );
        assert_eq!(relay.pending(), 0);
    }

    #[test]
    fn a_frame_naming_two_different_ids_is_not_relayed() {
        let mut relay = PermissionFrameRelay::new();
        let mixed = frame(
            PERMISSION_REQUEST_TYPE,
            Some("perm_a"),
            r#"{"request_id":"perm_b"}"#,
        );

        assert!(relay.relay_worker_frame(mixed).is_none());
        assert_eq!(relay.pending(), 0);
    }
    #[tokio::test]
    async fn relayed_frames_travel_over_the_in_process_transport_pair() {
        use crate::transport::{InProcessTransport, Transport};

        let (mut bridge_side, mut host_side) =
            InProcessTransport::create_pair("worker".into(), "host".into());
        bridge_side.connect().await.unwrap();
        host_side.connect().await.unwrap();
        let mut relay = PermissionFrameRelay::new();

        // Worker -> relay -> host.
        bridge_side.send(request_frame("perm_1")).await.unwrap();
        let inbound = host_side.receive().await.unwrap();
        let to_host = relay.relay_worker_frame(inbound).expect("request relayed");
        assert_eq!(to_host.correlation_id.as_deref(), Some("perm_1"));

        // Host -> relay -> worker.
        host_side.send(response_frame("perm_1")).await.unwrap();
        let answer = bridge_side.receive().await.unwrap();
        assert!(relay.relay_host_frame(answer).is_some());
        assert_eq!(relay.pending(), 0);
    }
}
