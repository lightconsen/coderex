//! Relay approval binding — proves a relayed call is the one the person approved.
//!
//! A cloud session does not reach this machine directly. Its outbound requests
//! arrive through a gateway, which means the request that eventually lands here
//! has travelled through something that is not this process. Holding a
//! permission grant is not enough at that boundary: the gateway has to know the
//! call is *the one the person just confirmed*, not a call injected into a
//! session that already holds a grant. Two paired headers carry that:
//!
//! - `X-Ccr-Relay-Tool-Use-Id` names the call awaiting the decision.
//! - `X-Ccr-Relay-Approved-Intent` carries a snapshot of what was approved, as
//!   base64url JSON.
//!
//! The snapshot is what the person confirmed, recorded locally at the moment
//! they confirmed it ({@link RelayApprovalGate::confirm}). A call is refused
//! unless both headers are present and the snapshot matches a confirmation that
//! has not already been spent — a confirmation authorises one call, so a replayed
//! header pair is refused like any other.
//!
//! Refusals carry the gateway's own shape: `403` with error code
//! `write_relay_unapproved`, and a message telling the caller to retry so the
//! confirmation card is shown again.
//!
//! # What is CodeRex's own here
//!
//! The reference binds an artifact *share*: its snapshot holds `mode`,
//! `access`, and `user_uuids`. CodeRex has no artifact sharing, so the snapshot
//! here is the confirmed tool call — session, tool, and a digest of the
//! arguments. The binding rules, the wire format, and the refusal are the same;
//! the payload is what this codebase actually approves.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Header naming the call awaiting a decision.
pub const TOOL_USE_ID_HEADER: &str = "x-ccr-relay-tool-use-id";

/// Header carrying the approved intent snapshot (base64url JSON).
pub const APPROVED_INTENT_HEADER: &str = "x-ccr-relay-approved-intent";

/// Error code a refusal carries.
pub const UNAPPROVED_ERROR_CODE: &str = "write_relay_unapproved";

/// What the caller is told when a relayed call is refused.
pub const UNAPPROVED_MESSAGE: &str = "The cloud session's gateway did not accept this call as \
     approved by the person, so nothing was changed. Retry once so the confirmation card is shown \
     again.";

/// A snapshot of the call the person approved.
///
/// The digest stands in for the arguments: the header travels through a gateway
/// and lands in logs, and the snapshot only has to be *comparable*, not readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedIntent {
    /// Session the confirmation came from.
    pub session_id: String,
    /// Tool the approved call runs.
    pub tool: String,
    /// Hex SHA-256 of the approved arguments.
    pub args_digest: String,
}

impl ApprovedIntent {
    /// Snapshot the call identified by `session_id`, `tool`, and `args`.
    #[must_use]
    pub fn for_call(session_id: &str, tool: &str, args: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            args_digest: hex(&Sha256::digest(args.as_bytes())),
        }
    }

    /// Encode as base64url JSON, the form the header carries.
    ///
    /// # Panics
    ///
    /// Panics if the snapshot cannot be serialised. Every field is a `String`,
    /// so this cannot fail in practice.
    #[must_use]
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("approved intent is plain strings");
        URL_SAFE_NO_PAD.encode(json)
    }

    /// Decode a snapshot from its base64url JSON form.
    pub fn decode(encoded: &str) -> Result<Self, RelayDenial> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded.trim())
            .map_err(|e| RelayDenial::InvalidIntent {
                reason: format!("not base64url: {e}"),
            })?;
        serde_json::from_slice(&bytes).map_err(|e| RelayDenial::InvalidIntent {
            reason: format!("not an approved intent: {e}"),
        })
    }
}

/// Why a relayed call was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayDenial {
    /// The call did not carry both binding headers.
    #[error("relay call is missing the {missing} header")]
    MissingBinding {
        /// The header that was absent.
        missing: &'static str,
    },
    /// The snapshot could not be read.
    #[error("relay approval snapshot is unusable: {reason}")]
    InvalidIntent {
        /// What was wrong with it.
        reason: String,
    },
    /// The call does not match any confirmation the person has given.
    #[error("relay call '{tool_use_id}' was not approved by the person")]
    Unapproved {
        /// The call that was refused.
        tool_use_id: String,
    },
}

impl RelayDenial {
    /// HTTP status the refusal is reported with.
    #[must_use]
    pub const fn status(&self) -> u16 {
        403
    }

    /// Error code the refusal is reported with.
    #[must_use]
    pub const fn error_code(&self) -> &'static str {
        UNAPPROVED_ERROR_CODE
    }

    /// Message the caller is shown.
    ///
    /// The gateway reports one message for every refusal: from the caller's
    /// side they are the same situation, and the variants are for the log.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        UNAPPROVED_MESSAGE
    }
}

/// Records the confirmations the person has given, and checks relayed calls
/// against them.
#[derive(Debug, Default)]
pub struct RelayApprovalGate {
    confirmed: HashMap<String, ApprovedIntent>,
}

impl RelayApprovalGate {
    /// Create an empty gate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the confirmation the person just gave for `tool_use_id`.
    pub fn confirm(&mut self, tool_use_id: impl Into<String>, intent: ApprovedIntent) {
        self.confirmed.insert(tool_use_id.into(), intent);
    }

    /// Drop a confirmation whose request was withdrawn before it was used.
    ///
    /// Returns whether anything was dropped: the answer that arrived afterwards
    /// applies to nothing.
    pub fn withdraw(&mut self, tool_use_id: &str) -> bool {
        self.confirmed.remove(tool_use_id).is_some()
    }

    /// How many confirmations are waiting to be spent.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.confirmed.len()
    }

    /// Check a relayed call, spending the confirmation it matches.
    ///
    /// A confirmation authorises the one call it was given for, so it is
    /// consumed on success and a replay of the same pair is refused.
    pub fn authorize(
        &mut self,
        tool_use_id: &str,
        intent: &ApprovedIntent,
    ) -> Result<(), RelayDenial> {
        match self.confirmed.get(tool_use_id) {
            Some(confirmed) if confirmed == intent => {
                self.confirmed.remove(tool_use_id);
                Ok(())
            }
            // A mismatch leaves the confirmation in place: the person did
            // approve something, and a forged attempt should not spend it.
            _ => Err(RelayDenial::Unapproved {
                tool_use_id: tool_use_id.to_string(),
            }),
        }
    }

    /// Check a relayed call from its headers.
    ///
    /// `header` is a lookup by lowercase name, so any header map can be used.
    pub fn authorize_headers(
        &mut self,
        header: impl Fn(&str) -> Option<String>,
    ) -> Result<(), RelayDenial> {
        let tool_use_id = header(TOOL_USE_ID_HEADER).ok_or(RelayDenial::MissingBinding {
            missing: TOOL_USE_ID_HEADER,
        })?;
        let encoded = header(APPROVED_INTENT_HEADER).ok_or(RelayDenial::MissingBinding {
            missing: APPROVED_INTENT_HEADER,
        })?;
        let intent = ApprovedIntent::decode(&encoded)?;
        self.authorize(tool_use_id.trim(), &intent)
    }
}

/// Whether a relay may forward this header.
///
/// A relayed request arrives from a cloud session, so a header it names is not
/// a reason to trust it. Only the families the relay itself defines, plus
/// `accept` and `if-match`, cross the boundary; everything else is dropped
/// before the request is handled.
#[must_use]
pub fn is_forwardable_header(name: &str) -> bool {
    const FAMILIES: [&str; 2] = ["x-frame-", "x-ccr-relay-"];

    let name = name.trim();
    if name.eq_ignore_ascii_case("accept") || name.eq_ignore_ascii_case("if-match") {
        return true;
    }
    FAMILIES.iter().any(|prefix| {
        let Some(head) = name.get(..prefix.len()) else {
            return false;
        };
        let Some(rest) = name.get(prefix.len()..) else {
            return false;
        };
        head.eq_ignore_ascii_case(prefix)
            && !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> ApprovedIntent {
        ApprovedIntent::for_call("sess_1", "Bash", r#"{"command":"ls"}"#)
    }

    fn headers(tool_use_id: &str, intent: &ApprovedIntent) -> impl Fn(&str) -> Option<String> {
        let tool_use_id = tool_use_id.to_string();
        let encoded = intent.encode();
        move |name: &str| match name {
            TOOL_USE_ID_HEADER => Some(tool_use_id.clone()),
            APPROVED_INTENT_HEADER => Some(encoded.clone()),
            _ => None,
        }
    }

    #[test]
    fn intent_round_trips_through_base64url_json() {
        let decoded = ApprovedIntent::decode(&intent().encode()).unwrap();
        assert_eq!(decoded, intent());
    }

    #[test]
    fn intent_is_base64url_json_on_the_wire() {
        let encoded = intent().encode();
        assert!(!encoded.contains('='), "padding should be stripped");
        assert!(!encoded.contains('+') && !encoded.contains('/'), "{encoded}");

        let json: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&encoded).unwrap()).unwrap();
        assert_eq!(json["session_id"], "sess_1");
        assert_eq!(json["tool"], "Bash");
        assert_ne!(json["args_digest"], r#"{"command":"ls"}"#);
    }

    #[test]
    fn the_confirmed_pair_authorises_the_call() {
        let mut gate = RelayApprovalGate::new();
        gate.confirm("perm_0", intent());

        assert_eq!(gate.outstanding(), 1);
        assert!(gate.authorize_headers(headers("perm_0", &intent())).is_ok());
    }

    #[test]
    fn a_confirmation_authorises_one_call() {
        let mut gate = RelayApprovalGate::new();
        gate.confirm("perm_0", intent());
        assert!(gate.authorize_headers(headers("perm_0", &intent())).is_ok());

        let replay = gate.authorize_headers(headers("perm_0", &intent()));
        assert!(
            matches!(replay, Err(RelayDenial::Unapproved { .. })),
            "{replay:?}"
        );
        assert_eq!(gate.outstanding(), 0);
    }

    #[test]
    fn a_snapshot_that_differs_from_the_confirmation_is_refused() {
        let mut gate = RelayApprovalGate::new();
        gate.confirm("perm_0", intent());

        let forged = ApprovedIntent::for_call("sess_1", "Bash", r#"{"command":"rm -rf /"}"#);
        let denied = gate.authorize_headers(headers("perm_0", &forged));
        assert!(
            matches!(denied, Err(RelayDenial::Unapproved { .. })),
            "{denied:?}"
        );
        // The forged attempt did not spend the real confirmation.
        assert_eq!(gate.outstanding(), 1);
        assert!(gate.authorize_headers(headers("perm_0", &intent())).is_ok());
    }

    #[test]
    fn an_unconfirmed_call_is_refused() {
        let mut gate = RelayApprovalGate::new();
        let denied = gate.authorize_headers(headers("perm_9", &intent()));
        assert!(
            matches!(denied, Err(RelayDenial::Unapproved { .. })),
            "{denied:?}"
        );
    }

    #[test]
    fn one_header_without_the_other_is_refused() {
        let mut gate = RelayApprovalGate::new();
        gate.confirm("perm_0", intent());

        let encoded = intent().encode();
        let without_id = gate.authorize_headers(|name: &str| {
            (name == APPROVED_INTENT_HEADER).then(|| encoded.clone())
        });
        assert!(
            matches!(
                without_id,
                Err(RelayDenial::MissingBinding {
                    missing: TOOL_USE_ID_HEADER
                })
            ),
            "{without_id:?}"
        );

        let without_intent = gate
            .authorize_headers(|name: &str| (name == TOOL_USE_ID_HEADER).then(|| "perm_0".into()));
        assert!(
            matches!(
                without_intent,
                Err(RelayDenial::MissingBinding {
                    missing: APPROVED_INTENT_HEADER
                })
            ),
            "{without_intent:?}"
        );
        assert_eq!(gate.outstanding(), 1);
    }

    #[test]
    fn an_unreadable_snapshot_is_refused() {
        let mut gate = RelayApprovalGate::new();
        gate.confirm("perm_0", intent());

        for bad in ["not base64!!", &URL_SAFE_NO_PAD.encode(b"{}"), ""] {
            let denied = gate.authorize_headers(|name: &str| match name {
                TOOL_USE_ID_HEADER => Some("perm_0".into()),
                APPROVED_INTENT_HEADER => Some(bad.to_string()),
                _ => None,
            });
            assert!(
                matches!(
                    denied,
                    Err(RelayDenial::InvalidIntent { .. } | RelayDenial::Unapproved { .. })
                ),
                "{bad:?} gave {denied:?}"
            );
        }
        assert_eq!(gate.outstanding(), 1);
    }

    #[test]
    fn a_withdrawn_confirmation_applies_to_nothing() {
        let mut gate = RelayApprovalGate::new();
        gate.confirm("perm_0", intent());

        assert!(gate.withdraw("perm_0"));
        assert!(!gate.withdraw("perm_0"));
        assert!(matches!(
            gate.authorize_headers(headers("perm_0", &intent())),
            Err(RelayDenial::Unapproved { .. })
        ));
    }

    #[test]
    fn refusals_are_403s_with_the_gateway_error_code() {
        let denied = RelayDenial::Unapproved {
            tool_use_id: "perm_0".into(),
        };
        assert_eq!(denied.status(), 403);
        assert_eq!(denied.error_code(), "write_relay_unapproved");
        assert!(denied.message().contains("confirmation card"));
    }

    #[test]
    fn only_the_relay_header_families_are_forwarded() {
        for allowed in [
            "accept",
            "If-Match",
            "x-frame-ancestors",
            "X-Frame-Options",
            "x-ccr-relay-tool-use-id",
            "x-ccr-relay-approved-intent",
        ] {
            assert!(is_forwardable_header(allowed), "{allowed}");
        }

        for refused in [
            "authorization",
            "cookie",
            "x-api-key",
            "x-ccr-relay-",
            "x-frame-",
            "x-ccr-relay-tool use id",
            "x-ccr-relay-tool_use_id",
            "xff-ccr-relay-x",
            "x-ccr-relay-ünïcode",
        ] {
            assert!(!is_forwardable_header(refused), "{refused}");
        }
    }
}
