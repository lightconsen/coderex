//! The line-delimited wire protocol.
//!
//! Exactly one JSON object per line on an output stream (`stdout` for the CLI).
//! The producer is the agent loop; every consumer is **out of process** — an
//! SDK client reading the stream, a desktop/IDE host driving the CLI over the
//! bridge, or the transcript mirror. There is deliberately no in-process
//! registry here: one producer, one serialized output.
//!
//! The five message frame kinds mirror the reference's wire format:
//! [`Assistant`], [`User`], [`System`], [`Result`] and [`StreamEvent`].
//! `System` carries a `subtype` string, `result` carries a `subtype` too, and
//! [`Result`] is the **turn-complete signal** — exactly one per turn, including
//! when the turn failed.
//!
//! On top of those, [`ControlRequest`] and [`ControlResponse`] carry the
//! out-of-band approval exchange: the headless child asks a host whether a
//! tool may run, and the host answers on stdin. They are not turn frames and
//! do not participate in the one-result guard.
//!
//! [`Assistant`]: ProtocolMessage::Assistant
//! [`User`]: ProtocolMessage::User
//! [`System`]: ProtocolMessage::System
//! [`Result`]: ProtocolMessage::Result
//! [`StreamEvent`]: ProtocolMessage::StreamEvent
//! [`ControlRequest`]: ProtocolMessage::ControlRequest
//! [`ControlResponse`]: ProtocolMessage::ControlResponse

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::{self, Write};

/// `result` subtypes this crate names explicitly.
///
/// The reference has more; a caller with an exotic termination reason may pass
/// any string, because [`ResultFrame::subtype`] is a `String`.
pub mod result_subtype {
    /// The turn finished normally.
    pub const SUCCESS: &str = "success";
    /// The turn hit its turn limit.
    pub const ERROR_MAX_TURNS: &str = "error_max_turns";
    /// The turn failed while running.
    pub const ERROR_DURING_EXECUTION: &str = "error_during_execution";
}

/// Subtype of a [`ControlRequestFrame`] that asks whether a tool may run.
pub const CAN_USE_TOOL_SUBTYPE: &str = "can_use_tool";
/// Subtype of a successful [`ControlResponseFrame`].
pub const CONTROL_SUCCESS_SUBTYPE: &str = "success";
/// Subtype of a failed [`ControlResponseFrame`].
pub const CONTROL_ERROR_SUBTYPE: &str = "error";

/// Behavior value in a [`ControlResponseBody`] that approves the tool call.
pub const CONTROL_BEHAVIOR_ALLOW: &str = "allow";
/// Behavior value in a [`ControlResponseBody`] that approves the tool for the
/// rest of the session.
pub const CONTROL_BEHAVIOR_ALLOW_SESSION: &str = "allow_session";
/// Behavior value in a [`ControlResponseBody`] that refuses the tool call.
pub const CONTROL_BEHAVIOR_DENY: &str = "deny";

// ── Frames ──────────────────────────────────────────────────────────────

/// One frame of the line-delimited protocol.
///
/// Serialized with a `type` discriminant spelled the way the reference spells
/// it: `assistant`, `user`, `system`, `result`, `stream_event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProtocolMessage {
    /// A complete assistant message (one model step's worth of content).
    Assistant(AssistantFrame),
    /// A message injected on the user's behalf — queued user input, or the
    /// `tool_result` blocks the harness sends back to the model.
    User(UserFrame),
    /// A system notice. The `subtype` says which kind.
    System(SystemFrame),
    /// The turn-complete frame. Emitted exactly once per turn.
    Result(ResultFrame),
    /// A raw streaming event forwarded from the model API, unmodified.
    StreamEvent(StreamEventFrame),
    /// A tool-approval request from the agent to its host.
    ///
    /// This is the producer half of the out-of-band control channel: the
    /// agent stops waiting on the tool and asks the host, which answers with a
    /// [`ProtocolMessage::ControlResponse`] on stdin.
    ControlRequest(ControlRequestFrame),
    /// A host's answer to a [`ProtocolMessage::ControlRequest`].
    ///
    /// This frame travels in the opposite direction from every other frame in
    /// the enum: the host writes it to the agent's stdin.
    ControlResponse(ControlResponseFrame),
}

impl ProtocolMessage {
    /// Serialize this frame as **exactly one line**.
    ///
    /// The returned string contains no unescaped newline: a payload containing
    /// `\n` is JSON-escaped by the serializer, so the only newline present is
    /// the trailing record separator. Write this to the sink verbatim.
    ///
    /// # Panics
    ///
    /// Never in practice: every field of every frame is JSON-serializable, and
    /// `serde_json` cannot fail to serialize it. The panic is a programming
    /// guard, not a runtime path.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut line =
            serde_json::to_string(self).expect("protocol frames are always serializable");
        debug_assert!(
            !line.contains('\n') && !line.contains('\r'),
            "serializer emitted a raw newline; the protocol requires one line per frame"
        );
        line.push('\n');
        line
    }

    /// The frame's `type` discriminant (`"assistant"`, `"user"`, …).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Assistant(_) => "assistant",
            Self::User(_) => "user",
            Self::System(_) => "system",
            Self::Result(_) => "result",
            Self::StreamEvent(_) => "stream_event",
            Self::ControlRequest(_) => "control_request",
            Self::ControlResponse(_) => "control_response",
        }
    }

    /// The `subtype`, for the frames that carry one (`system`, `result`).
    // Not `const`: reading a `String` as `&str` is not const-callable.
    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn subtype(&self) -> Option<&str> {
        match self {
            Self::System(frame) => Some(&frame.subtype),
            Self::Result(frame) => Some(&frame.subtype),
            _ => None,
        }
    }

    /// Whether this is the turn-complete frame.
    #[must_use]
    pub const fn is_result(&self) -> bool {
        matches!(self, Self::Result(_))
    }
}

/// An `assistant` frame: the model's message plus the routing fields the host
/// needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantFrame {
    /// The model message.
    pub message: MessageBody,
    /// Set when this message came from a sub-agent's tool call, so the host
    /// can nest it under the right tool use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// The session this message belongs to.
    pub session_id: String,
}

/// A `user` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFrame {
    /// The user-side message (usually `tool_result` blocks).
    pub message: MessageBody,
    /// See [`AssistantFrame::parent_tool_use_id`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// The session this message belongs to.
    pub session_id: String,
}

/// The `message` object shared by assistant and user frames.
///
/// `content` is a list of content blocks. Block types are a large union in the
/// reference and are **not** modelled here: this crate carries blocks as raw
/// JSON and provides [`text_block`], [`tool_use_block`] and
/// [`tool_result_block`] constructors for the three it emits. Anything else
/// round-trips untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    /// `"assistant"` or `"user"`.
    pub role: String,
    /// Content blocks, passed through unmodelled.
    pub content: Vec<Value>,
    /// Message-level fields this crate does not model (`id`, `model`,
    /// `usage`, `stop_reason`, …), preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl MessageBody {
    /// Build an assistant message body.
    #[must_use]
    pub fn assistant(content: Vec<Value>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            extra: Map::new(),
        }
    }

    /// Build a user message body.
    #[must_use]
    pub fn user(content: Vec<Value>) -> Self {
        Self {
            role: "user".into(),
            content,
            extra: Map::new(),
        }
    }

    /// Attach an unmodelled message-level field.
    #[must_use]
    pub fn with_extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}

/// A `system` frame.
///
/// The reference defines 64 `system` subtypes and their payloads differ
/// entirely. This crate models the two fields every system frame shares — the
/// `subtype` discriminant and the session id — and passes the rest through
/// in [`SystemFrame::data`] rather than copying a list it cannot keep in sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemFrame {
    /// The subtype discriminant (`"init"`, `"compact_boundary"`, …).
    pub subtype: String,
    /// The session this notice belongs to.
    pub session_id: String,
    /// Subtype-specific fields, unmodelled and preserved verbatim.
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

/// The `result` frame: the turn-complete signal.
///
/// Emitted exactly once per turn, including when the turn failed. Treat
/// receiving it as the end of the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultFrame {
    /// The termination reason, e.g. [`result_subtype::SUCCESS`].
    pub subtype: String,
    /// Whether the turn is an error. `true` for every subtype except
    /// [`result_subtype::SUCCESS`].
    pub is_error: bool,
    /// Wall-clock duration of the turn, in milliseconds.
    pub duration_ms: u64,
    /// Number of model turns consumed.
    pub num_turns: u32,
    /// The session that just completed a turn.
    pub session_id: String,
    /// The final assistant text, when the turn produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Subtype-specific fields this crate does not model (`usage`,
    /// `total_cost_usd`, …), preserved verbatim.
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

impl ResultFrame {
    /// A successful turn.
    #[must_use]
    pub fn success(
        session_id: impl Into<String>,
        result: impl Into<String>,
        duration_ms: u64,
        num_turns: u32,
    ) -> Self {
        Self {
            subtype: result_subtype::SUCCESS.into(),
            is_error: false,
            duration_ms,
            num_turns,
            session_id: session_id.into(),
            result: Some(result.into()),
            data: Map::new(),
        }
    }

    /// A failed turn.
    #[must_use]
    pub fn error(
        session_id: impl Into<String>,
        subtype: impl Into<String>,
        duration_ms: u64,
        num_turns: u32,
    ) -> Self {
        Self {
            subtype: subtype.into(),
            is_error: true,
            duration_ms,
            num_turns,
            session_id: session_id.into(),
            result: None,
            data: Map::new(),
        }
    }
}

/// A `stream_event` frame: one raw model streaming event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEventFrame {
    /// The raw streaming event, unmodified.
    pub event: Value,
    /// See [`AssistantFrame::parent_tool_use_id`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// The session this event belongs to.
    pub session_id: String,
}

// ── Control frames ──────────────────────────────────────────────────────

/// A `control_request` frame: the agent asks its host to approve a tool call.
///
/// Unlike the other frames, this one is a question: the turn does not proceed
/// until the host writes a [`ControlResponseFrame`] back on stdin. A host that
/// never answers leaves the turn waiting — there is deliberately no timeout,
/// because a timeout would turn a slow person into a silent denial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequestFrame {
    /// Correlates the answer with this request. The agent generates it; the
    /// host must echo it on the response.
    pub request_id: String,
    /// The request body. Only `can_use_tool` is defined today.
    pub request: CanUseToolRequest,
}

/// The `can_use_tool` body of a [`ControlRequestFrame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanUseToolRequest {
    /// Always [`CAN_USE_TOOL_SUBTYPE`] for a tool approval.
    pub subtype: String,
    /// Name of the tool that wants to run.
    pub tool_name: String,
    /// The tool's input, exactly as the model produced it.
    pub input: Value,
    /// Why the tool needs approval.
    #[serde(default)]
    pub reason: String,
    /// Whether the host may answer "approve for the rest of the session".
    #[serde(default)]
    pub allow_session: bool,
}

impl ControlRequestFrame {
    /// Build a `can_use_tool` request.
    #[must_use]
    pub fn can_use_tool(
        request_id: impl Into<String>,
        tool_name: impl Into<String>,
        input: Value,
        reason: impl Into<String>,
        allow_session: bool,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            request: CanUseToolRequest {
                subtype: CAN_USE_TOOL_SUBTYPE.into(),
                tool_name: tool_name.into(),
                input,
                reason: reason.into(),
                allow_session,
            },
        }
    }

    /// Serialize as exactly one line, including the `type` discriminant.
    #[must_use]
    pub fn to_line(&self) -> String {
        ProtocolMessage::ControlRequest(self.clone()).to_line()
    }

    /// Parse one protocol line and require it to be a `can_use_tool` request.
    ///
    /// # Errors
    ///
    /// Returns [`ControlFrameError`] for malformed JSON, a different frame
    /// type, or a request subtype other than `can_use_tool`.
    pub fn parse_line(line: &str) -> Result<Self, ControlFrameError> {
        let message: ProtocolMessage = serde_json::from_str(line)?;
        match message {
            ProtocolMessage::ControlRequest(frame) => {
                if frame.request.subtype != CAN_USE_TOOL_SUBTYPE {
                    return Err(ControlFrameError::UnexpectedSubtype(frame.request.subtype));
                }
                Ok(frame)
            }
            other => Err(ControlFrameError::WrongType {
                expected: "control_request",
                actual: other.kind(),
            }),
        }
    }
}

/// A `control_response` frame: the host's answer to a
/// [`ControlRequestFrame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponseFrame {
    /// The answer envelope.
    pub response: ControlResponseEnvelope,
}

/// The `response` object of a `control_response`.
///
/// `subtype` is `"success"` when [`response`](Self::response) is present and
/// `"error"` when the host could not handle the request; the message then
/// lives in [`error`](Self::error).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponseEnvelope {
    /// [`CONTROL_SUCCESS_SUBTYPE`] or [`CONTROL_ERROR_SUBTYPE`].
    pub subtype: String,
    /// The request being answered.
    pub request_id: String,
    /// The decision, for a `success` answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ControlResponseBody>,
    /// The failure detail, for an `error` answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The decision object of a successful control response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponseBody {
    /// [`CONTROL_BEHAVIOR_ALLOW`], [`CONTROL_BEHAVIOR_ALLOW_SESSION`], or
    /// [`CONTROL_BEHAVIOR_DENY`].
    pub behavior: String,
    /// Human-readable detail; may be empty.
    #[serde(default)]
    pub message: String,
    /// When `true` and the original request permitted it, approve the tool for
    /// the rest of the session rather than just this call.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_session: bool,
}

/// The decision carried by a [`ControlResponseFrame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlDecision {
    /// Run this one call.
    Allow,
    /// Run this call and stop asking for the tool for the rest of the session.
    AllowSession,
    /// Do not run the call.
    Deny,
    /// The host could not answer; the caller decides how to fail closed.
    Error(String),
}

impl ControlResponseFrame {
    /// Build a successful answer.
    #[must_use]
    pub fn success(
        request_id: impl Into<String>,
        behavior: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            response: ControlResponseEnvelope {
                subtype: CONTROL_SUCCESS_SUBTYPE.into(),
                request_id: request_id.into(),
                response: Some(ControlResponseBody {
                    behavior: behavior.into(),
                    message: message.into(),
                    allow_session: false,
                }),
                error: None,
            },
        }
    }

    /// Approve one call.
    #[must_use]
    pub fn allow(request_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::success(request_id, CONTROL_BEHAVIOR_ALLOW, message)
    }

    /// Deny the call.
    #[must_use]
    pub fn deny(request_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::success(request_id, CONTROL_BEHAVIOR_DENY, message)
    }

    /// Report that the host could not answer.
    #[must_use]
    pub fn error(request_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            response: ControlResponseEnvelope {
                subtype: CONTROL_ERROR_SUBTYPE.into(),
                request_id: request_id.into(),
                response: None,
                error: Some(message.into()),
            },
        }
    }

    /// Mark a successful allow as session-wide.
    #[must_use]
    pub const fn with_allow_session(mut self, allow_session: bool) -> Self {
        if let Some(body) = self.response.response.as_mut() {
            body.allow_session = allow_session;
        }
        self
    }

    /// Serialize as exactly one line, including the `type` discriminant.
    #[must_use]
    pub fn to_line(&self) -> String {
        ProtocolMessage::ControlResponse(self.clone()).to_line()
    }

    /// Parse one protocol line and require it to be a `control_response`.
    ///
    /// A `success` answer must carry a `response` object whose behavior is
    /// `allow`, `allow_session`, or `deny`; an `error` answer must name the
    /// request it failed.
    ///
    /// # Errors
    ///
    /// Returns [`ControlFrameError`] for malformed JSON, a different frame
    /// type, an unknown envelope subtype, or a success answer with no usable
    /// decision.
    pub fn parse_line(line: &str) -> Result<Self, ControlFrameError> {
        let message: ProtocolMessage = serde_json::from_str(line)?;
        let frame = match message {
            ProtocolMessage::ControlResponse(frame) => frame,
            other => {
                return Err(ControlFrameError::WrongType {
                    expected: "control_response",
                    actual: other.kind(),
                });
            }
        };
        frame.validate()?;
        Ok(frame)
    }

    /// The decision this frame carries.
    ///
    /// # Errors
    ///
    /// Same validation as [`parse_line`](Self::parse_line).
    pub fn decision(&self) -> Result<ControlDecision, ControlFrameError> {
        self.validate()?;
        match self.response.subtype.as_str() {
            CONTROL_SUCCESS_SUBTYPE => {
                let body = self
                    .response
                    .response
                    .as_ref()
                    .ok_or(ControlFrameError::MissingResponse)?;
                Ok(match body.behavior.as_str() {
                    CONTROL_BEHAVIOR_ALLOW_SESSION => ControlDecision::AllowSession,
                    CONTROL_BEHAVIOR_ALLOW if body.allow_session => ControlDecision::AllowSession,
                    CONTROL_BEHAVIOR_ALLOW => ControlDecision::Allow,
                    CONTROL_BEHAVIOR_DENY => ControlDecision::Deny,
                    other => {
                        return Err(ControlFrameError::UnknownBehavior(other.to_string()));
                    }
                })
            }
            CONTROL_ERROR_SUBTYPE => Ok(ControlDecision::Error(
                self.response
                    .error
                    .clone()
                    .or_else(|| {
                        self.response
                            .response
                            .as_ref()
                            .map(|body| body.message.clone())
                    })
                    .unwrap_or_else(|| "the host could not answer the request".into()),
            )),
            _ => unreachable!("validate rejects unknown subtypes"),
        }
    }

    fn validate(&self) -> Result<(), ControlFrameError> {
        match self.response.subtype.as_str() {
            CONTROL_SUCCESS_SUBTYPE => match self.response.response.as_ref() {
                Some(body)
                    if body.behavior == CONTROL_BEHAVIOR_ALLOW
                        || body.behavior == CONTROL_BEHAVIOR_ALLOW_SESSION
                        || body.behavior == CONTROL_BEHAVIOR_DENY =>
                {
                    Ok(())
                }
                Some(body) => Err(ControlFrameError::UnknownBehavior(body.behavior.clone())),
                None => Err(ControlFrameError::MissingResponse),
            },
            CONTROL_ERROR_SUBTYPE => Ok(()),
            other => Err(ControlFrameError::UnexpectedSubtype(other.to_string())),
        }
    }
}

/// `serde` helper: omit a `false` boolean from the wire form.
// `skip_serializing_if` requires the predicate to take its field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Error parsing a control frame.
#[derive(Debug, thiserror::Error)]
pub enum ControlFrameError {
    /// The line was not valid JSON, or did not have the expected shape.
    #[error("malformed control frame: {0}")]
    Json(#[from] serde_json::Error),
    /// The line carried a different protocol frame type.
    #[error("expected a {expected} frame, got {actual}")]
    WrongType {
        /// The frame kind the caller required.
        expected: &'static str,
        /// The frame kind that was actually present.
        actual: &'static str,
    },
    /// The request/response subtype is not one this crate understands.
    #[error("unexpected control subtype: {0}")]
    UnexpectedSubtype(String),
    /// A successful response carried no decision object.
    #[error("control response is missing its response body")]
    MissingResponse,
    /// The decision object named a behavior other than allow or deny.
    #[error("unknown control response behavior: {0}")]
    UnknownBehavior(String),
}

// ── Content-block constructors ──────────────────────────────────────────

/// A `text` content block.
#[must_use]
pub fn text_block(text: impl Into<String>) -> Value {
    serde_json::json!({ "type": "text", "text": text.into() })
}

/// A `tool_use` content block.
#[must_use]
pub fn tool_use_block(
    id: impl Into<String>,
    name: impl Into<String>,
    input: impl Into<Value>,
) -> Value {
    serde_json::json!({
        "type": "tool_use",
        "id": id.into(),
        "name": name.into(),
        "input": input.into(),
    })
}

/// A `tool_result` content block.
#[must_use]
pub fn tool_result_block(tool_use_id: impl Into<String>, content: impl Into<String>) -> Value {
    serde_json::json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id.into(),
        "content": content.into(),
    })
}

/// A `tool_result` block that carries images beside its text.
///
/// `images` is `(media_type, base64 data)` per image. With no images this is
/// exactly [`tool_result_block`]; with images the `content` becomes the same
/// text-plus-image array the provider receives, so a stream-json consumer can
/// render what the model saw.
#[must_use]
pub fn tool_result_block_with_images(
    tool_use_id: impl Into<String>,
    content: impl Into<String>,
    images: &[(String, String)],
) -> Value {
    let tool_use_id = tool_use_id.into();
    let content = content.into();
    if images.is_empty() {
        return tool_result_block(tool_use_id, content);
    }
    let mut blocks = Vec::with_capacity(images.len() + 1);
    if !content.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": content }));
    }
    for (media_type, data) in images {
        blocks.push(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            },
        }));
    }
    serde_json::json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": blocks,
    })
}

// ── Sink ────────────────────────────────────────────────────────────────

/// Writes protocol frames to an output stream, one line per frame.
///
/// This is a sink, not a bus: it holds exactly one writer and no subscription
/// registry. The CLI hands it `stdout`; conceptually it is "one producer → one
/// serialized output".
#[derive(Debug)]
pub struct MessageSink<W: Write> {
    writer: W,
}

/// A [`MessageSink`] writing to standard output — the CLI's ordinary case.
pub type StdoutSink = MessageSink<io::Stdout>;

impl<W: Write> MessageSink<W> {
    /// Wrap a writer.
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Write one frame as one line and flush.
    ///
    /// Flushing per frame is deliberate: a host reading this stream must see a
    /// frame as soon as the producer emits it, not when an internal buffer
    /// fills.
    ///
    /// A `result` frame is refused here: it is the turn's one and only closing
    /// signal, and [`Turn::emit_turn_complete`] is the only path that writes
    /// it. Without the refusal a caller could put two results in one turn by
    /// bypassing the guard.
    pub fn emit(&mut self, message: &ProtocolMessage) -> io::Result<()> {
        if message.is_result() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "result frames must go through Turn::emit_turn_complete",
            ));
        }
        self.write(message)
    }

    /// Write a frame without the public guard. Only [`Turn`] calls this.
    fn write(&mut self, message: &ProtocolMessage) -> io::Result<()> {
        self.writer.write_all(message.to_line().as_bytes())?;
        self.writer.flush()
    }

    /// Begin a turn.
    ///
    /// The returned [`Turn`] is the only sanctioned way to emit the turn's
    /// `result`, and it consumes itself when doing so — so a turn cannot
    /// produce two. Dropping it without a result writes an error result rather
    /// than leaving the turn with none.
    pub fn begin_turn(&mut self, session_id: impl Into<String>) -> Turn<'_, W> {
        Turn {
            sink: self,
            session_id: session_id.into(),
            started: std::time::Instant::now(),
            completed: false,
        }
    }

    /// Borrow the underlying writer.
    pub const fn get_ref(&self) -> &W {
        &self.writer
    }

    /// Unwrap the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

/// A turn in progress.
///
/// Dropping a `Turn` without completing it writes an error `result` rather
/// than leaving the turn open: the reference always emits a `result`, even for
/// a failed turn.
#[derive(Debug)]
pub struct Turn<'a, W: Write> {
    sink: &'a mut MessageSink<W>,
    session_id: String,
    started: std::time::Instant,
    completed: bool,
}

impl<W: Write> Turn<'_, W> {
    /// Emit a non-`result` frame for this turn.
    ///
    /// A `result` frame is refused here — use
    /// [`emit_turn_complete`](Self::emit_turn_complete), which is what makes
    /// "exactly one result" hold.
    pub fn emit(&mut self, message: &ProtocolMessage) -> Result<(), TurnError> {
        if message.is_result() {
            return Err(TurnError::ResultMustUseEmitTurnComplete);
        }
        self.sink.emit(message).map_err(TurnError::Io)
    }

    /// Finish the turn, writing its single `result` frame.
    ///
    /// Consumes the turn, so it cannot be called twice.
    pub fn emit_turn_complete(mut self, result: ResultFrame) -> Result<(), TurnError> {
        self.sink
            .write(&ProtocolMessage::Result(result))
            .map_err(TurnError::Io)?;
        self.completed = true;
        Ok(())
    }

    /// Whether the turn's `result` has been written.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        self.completed
    }
}

impl<W: Write> Drop for Turn<'_, W> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        let frame = ResultFrame::error(
            self.session_id.clone(),
            result_subtype::ERROR_DURING_EXECUTION,
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            1,
        );
        if self.sink.write(&ProtocolMessage::Result(frame)).is_err() {
            tracing::warn!(
                "turn ended without a result and the fallback result could not be written"
            );
        }
    }
}

/// Error from emitting a turn.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// The output stream failed.
    #[error("protocol sink I/O error: {0}")]
    Io(#[from] io::Error),
    /// A `result` frame was passed to [`Turn::emit`].
    #[error("result frames must be emitted with Turn::emit_turn_complete")]
    ResultMustUseEmitTurnComplete,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::BufWriter;
    use std::rc::Rc;

    #[test]
    fn a_tool_result_carries_images_only_when_present() {
        let plain = tool_result_block_with_images("t1", "text", &[]);
        assert_eq!(plain["content"], "text");

        let with_image = tool_result_block_with_images(
            "t1",
            "text",
            &[("image/png".to_string(), "AAAA".to_string())],
        );
        let blocks = with_image["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AAAA");
    }

    fn assistant_line(session: &str, text: &str) -> ProtocolMessage {
        ProtocolMessage::Assistant(AssistantFrame {
            message: MessageBody::assistant(vec![text_block(text)]),
            parent_tool_use_id: None,
            session_id: session.into(),
        })
    }

    #[test]
    fn to_line_is_exactly_one_line_even_when_text_has_newlines() {
        let frame = assistant_line("s1", "first\nsecond\r\nthird");
        let line = frame.to_line();

        assert!(line.ends_with('\n'), "frame must be newline-terminated");
        assert_eq!(
            line.matches('\n').count(),
            1,
            "the only newline is the record separator: {line:?}"
        );
        assert!(!line.contains('\r'), "carriage returns must be escaped too");

        // The newline survives as data, escaped inside the JSON string.
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(
            parsed["message"]["content"][0]["text"],
            "first\nsecond\r\nthird"
        );
    }

    #[test]
    fn discriminants_are_spelled_as_the_reference_spells_them() {
        let frames = vec![
            (assistant_line("s1", "hi"), "assistant", None::<&str>),
            (
                ProtocolMessage::User(UserFrame {
                    message: MessageBody::user(vec![tool_result_block("t1", "ok")]),
                    parent_tool_use_id: None,
                    session_id: "s1".into(),
                }),
                "user",
                None,
            ),
            (
                ProtocolMessage::System(SystemFrame {
                    subtype: "init".into(),
                    session_id: "s1".into(),
                    data: Map::new(),
                }),
                "system",
                Some("init"),
            ),
            (
                ProtocolMessage::Result(ResultFrame::success("s1", "done", 12, 3)),
                "result",
                Some(result_subtype::SUCCESS),
            ),
            (
                ProtocolMessage::StreamEvent(StreamEventFrame {
                    event: serde_json::json!({"type": "content_block_delta"}),
                    parent_tool_use_id: None,
                    session_id: "s1".into(),
                }),
                "stream_event",
                None,
            ),
        ];

        for (frame, expected_type, expected_subtype) in frames {
            let value: Value = serde_json::from_str(frame.to_line().trim_end()).unwrap();
            assert_eq!(value["type"], expected_type, "frame: {frame:?}");
            assert_eq!(frame.kind(), expected_type);
            match expected_subtype {
                Some(sub) => assert_eq!(value["subtype"], sub, "frame: {frame:?}"),
                None => assert!(value.get("subtype").is_none(), "frame: {frame:?}"),
            }
            assert_eq!(frame.subtype(), expected_subtype);
        }
    }

    #[test]
    fn system_subtype_sits_beside_type_at_the_top_level() {
        let frame = ProtocolMessage::System(SystemFrame {
            subtype: "compact_boundary".into(),
            session_id: "s1".into(),
            data: Map::new(),
        });
        let value: Value = serde_json::from_str(frame.to_line().trim_end()).unwrap();
        assert_eq!(value["type"], "system");
        assert_eq!(value["subtype"], "compact_boundary");
    }

    #[test]
    fn frames_roundtrip_through_json() {
        let frames = vec![
            assistant_line("s1", "hello\nworld"),
            ProtocolMessage::User(UserFrame {
                message: MessageBody::user(vec![tool_result_block("t1", "ok")])
                    .with_extra("uuid", serde_json::json!("u-1")),
                parent_tool_use_id: Some("t0".into()),
                session_id: "s1".into(),
            }),
            ProtocolMessage::System(SystemFrame {
                subtype: "init".into(),
                session_id: "s1".into(),
                data: serde_json::json!({"cwd": "/tmp"})
                    .as_object()
                    .unwrap()
                    .clone(),
            }),
            ProtocolMessage::Result(ResultFrame::error(
                "s1",
                result_subtype::ERROR_DURING_EXECUTION,
                5,
                1,
            )),
            ProtocolMessage::StreamEvent(StreamEventFrame {
                event: serde_json::json!({"type": "message_start"}),
                parent_tool_use_id: None,
                session_id: "s1".into(),
            }),
        ];

        for frame in frames {
            let decoded: ProtocolMessage =
                serde_json::from_str(frame.to_line().trim_end()).unwrap();
            assert_eq!(decoded, frame, "round-trip must preserve the frame");
        }
    }

    #[test]
    fn unmodelled_system_fields_are_preserved() {
        let frame = ProtocolMessage::System(SystemFrame {
            subtype: "init".into(),
            session_id: "s1".into(),
            data: serde_json::json!({"cwd": "/tmp", "model": "m"})
                .as_object()
                .unwrap()
                .clone(),
        });
        let value: Value = serde_json::from_str(frame.to_line().trim_end()).unwrap();
        assert_eq!(value["cwd"], "/tmp");
        assert_eq!(value["model"], "m");
    }

    #[test]
    fn a_turn_emits_exactly_one_result() {
        let mut sink = MessageSink::new(Vec::new());
        {
            let mut turn = sink.begin_turn("s1");
            turn.emit(&assistant_line("s1", "hi")).unwrap();
            // A result may not sneak in through the ordinary emit path.
            let stray = ProtocolMessage::Result(ResultFrame::success("s1", "nope", 0, 1));
            assert!(matches!(
                turn.emit(&stray),
                Err(TurnError::ResultMustUseEmitTurnComplete)
            ));
            turn.emit_turn_complete(ResultFrame::success("s1", "done", 1, 1))
                .unwrap();
        }

        let out = String::from_utf8(sink.into_inner()).unwrap();
        let results = out
            .lines()
            .filter(|line| serde_json::from_str::<Value>(line).unwrap()["type"] == "result")
            .count();
        assert_eq!(results, 1, "a turn must end with exactly one result frame");
    }

    /// The sink itself refuses a result, so a caller cannot bypass the turn
    /// guard by writing a second one directly.
    #[test]
    fn the_sink_refuses_a_result_that_did_not_come_from_a_turn() {
        let mut sink = MessageSink::new(Vec::new());
        let stray = ProtocolMessage::Result(ResultFrame::success("s1", "nope", 0, 1));
        let error = sink.emit(&stray).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    /// A turn dropped without its result still closes the stream with an error
    /// result; zero results would leave a host waiting forever.
    #[test]
    fn dropping_a_turn_writes_an_error_result() {
        let mut sink = MessageSink::new(Vec::new());
        {
            let mut turn = sink.begin_turn("s1");
            turn.emit(&assistant_line("s1", "partial")).unwrap();
            // Dropped without emit_turn_complete.
        }

        let out = String::from_utf8(sink.into_inner()).unwrap();
        let results: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|value: &Value| value["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "a dropped turn must still close itself");
        assert_eq!(results[0]["subtype"], "error_during_execution");
        assert_eq!(results[0]["is_error"], true);
        assert_eq!(results[0]["session_id"], "s1");
    }

    #[test]
    fn sink_writes_in_order() {
        let mut sink = MessageSink::new(Vec::new());
        sink.emit(&assistant_line("s1", "one")).unwrap();
        sink.emit(&ProtocolMessage::System(SystemFrame {
            subtype: "status".into(),
            session_id: "s1".into(),
            data: Map::new(),
        }))
        .unwrap();

        let out = String::from_utf8(sink.into_inner()).unwrap();
        let kinds: Vec<String> = out
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds, vec!["assistant", "system"]);
    }

    /// A writer whose buffer a test can inspect, to observe when data arrives.
    struct SharedWriter(Rc<RefCell<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sink_flushes_each_frame() {
        // A BufWriter that is only flushed by the sink: if `emit` stopped
        // flushing, nothing would reach the shared buffer before the drop.
        let shared: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));

        let mut sink = MessageSink::new(BufWriter::new(SharedWriter(Rc::clone(&shared))));
        sink.emit(&assistant_line("s1", "hello")).unwrap();

        assert!(
            !shared.borrow().is_empty(),
            "emit must flush, or a slow reader never sees the frame"
        );
    }

    #[test]
    fn control_request_roundtrips_with_the_documented_shape() {
        let frame = ControlRequestFrame::can_use_tool(
            "req-1",
            "Bash",
            serde_json::json!({"command": "rm -rf /tmp/x"}),
            "Bash needs approval",
            true,
        );
        let line = frame.to_line();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "one frame, one line");

        let value: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["type"], "control_request");
        assert_eq!(value["request_id"], "req-1");
        assert_eq!(value["request"]["subtype"], "can_use_tool");
        assert_eq!(value["request"]["tool_name"], "Bash");
        assert_eq!(value["request"]["input"]["command"], "rm -rf /tmp/x");
        assert_eq!(value["request"]["reason"], "Bash needs approval");
        assert_eq!(value["request"]["allow_session"], true);

        assert_eq!(
            ControlRequestFrame::parse_line(line.trim_end()).unwrap(),
            frame
        );
    }

    #[test]
    fn control_response_roundtrips_for_success_and_error() {
        for frame in [
            ControlResponseFrame::allow("req-1", "ok"),
            ControlResponseFrame::deny("req-2", "no"),
            ControlResponseFrame::allow("req-3", "always").with_allow_session(true),
            ControlResponseFrame::error("req-4", "unsupported tool"),
        ] {
            let line = frame.to_line();
            assert!(line.ends_with('\n'));
            assert_eq!(line.matches('\n').count(), 1, "one frame, one line");
            let value: Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(value["type"], "control_response");
            assert_eq!(
                ControlResponseFrame::parse_line(line.trim_end()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn control_response_decisions_match_the_behavior() {
        assert_eq!(
            ControlResponseFrame::allow("r", "").decision().unwrap(),
            ControlDecision::Allow
        );
        assert_eq!(
            ControlResponseFrame::deny("r", "").decision().unwrap(),
            ControlDecision::Deny
        );
        assert_eq!(
            ControlResponseFrame::allow("r", "")
                .with_allow_session(true)
                .decision()
                .unwrap(),
            ControlDecision::AllowSession
        );
        // A host may also spell the session-wide answer as its own behavior.
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r","response":{"behavior":"allow_session"}}}"#;
        assert_eq!(
            ControlResponseFrame::parse_line(line)
                .unwrap()
                .decision()
                .unwrap(),
            ControlDecision::AllowSession
        );
        assert!(matches!(
            ControlResponseFrame::error("r", "boom").decision().unwrap(),
            ControlDecision::Error(message) if message == "boom"
        ));
    }

    #[test]
    fn malformed_control_frames_are_rejected() {
        // Not JSON at all.
        assert!(ControlRequestFrame::parse_line("not json").is_err());
        assert!(ControlResponseFrame::parse_line("not json").is_err());

        // A different frame kind.
        assert!(matches!(
            ControlRequestFrame::parse_line(&assistant_line("s1", "hi").to_line()),
            Err(ControlFrameError::WrongType {
                expected: "control_request",
                ..
            })
        ));

        // Missing required field.
        assert!(
            ControlRequestFrame::parse_line(r#"{"type":"control_request","request_id":"r"}"#)
                .is_err()
        );

        // Unknown request subtype.
        assert!(matches!(
            ControlRequestFrame::parse_line(
                r#"{"type":"control_request","request_id":"r","request":{"subtype":"other","tool_name":"Bash","input":{}}}"#
            ),
            Err(ControlFrameError::UnexpectedSubtype(_))
        ));

        // A success response with no decision.
        assert!(matches!(
            ControlResponseFrame::parse_line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"r"}}"#
            ),
            Err(ControlFrameError::MissingResponse)
        ));

        // A success response with an unknown behavior.
        assert!(matches!(
            ControlResponseFrame::parse_line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"r","response":{"behavior":"maybe","message":""}}}"#
            ),
            Err(ControlFrameError::UnknownBehavior(_))
        ));

        // An unknown envelope subtype.
        assert!(matches!(
            ControlResponseFrame::parse_line(
                r#"{"type":"control_response","response":{"subtype":"weird","request_id":"r"}}"#
            ),
            Err(ControlFrameError::UnexpectedSubtype(_))
        ));
    }

    /// The control frames are real protocol frames: the same one-line and
    /// discriminant guarantees apply to them.
    #[test]
    fn control_frames_report_their_kind_and_stay_on_one_line() {
        let request = ProtocolMessage::ControlRequest(ControlRequestFrame::can_use_tool(
            "r",
            "Bash",
            serde_json::json!({"command": "a\nb"}),
            "reason\nwith newline",
            false,
        ));
        let response = ProtocolMessage::ControlResponse(ControlResponseFrame::deny("r", "no"));

        assert_eq!(request.kind(), "control_request");
        assert_eq!(response.kind(), "control_response");
        for frame in [request, response] {
            assert_eq!(frame.to_line().matches('\n').count(), 1);
            let decoded: ProtocolMessage =
                serde_json::from_str(frame.to_line().trim_end()).unwrap();
            assert_eq!(decoded, frame);
        }
    }
}
