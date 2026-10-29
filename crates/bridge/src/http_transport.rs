//! An HTTP-polling transport for the bridge gateway.
//!
//! The WebSocket transport needs the gateway to accept a socket. When the
//! worker can only make outbound HTTP requests — behind a restrictive proxy, or
//! because the gateway is a plain HTTP listener — the same frames can travel
//! over the gateway's per-environment mailbox:
//!
//! - `send` posts the frame addressed to the *other* role (a worker's output
//!   and permission requests go to `host`; an operator's answers go to
//!   `worker`).
//! - `receive` long-polls with an `after` cursor for frames addressed to this
//!   role, so a worker never reads back what it just sent.
//!
//! Reading does not consume a frame: the gateway keeps them until its limit,
//! and the cursor is the client's. A receive that is cancelled or times out
//! therefore cannot lose a frame — it is returned again next time — and only
//! the frames actually parsed advance the cursor.

use std::collections::VecDeque;
use std::time::Duration;

use serde::Deserialize;

use crate::transport::{Transport, TransportError, TransportMessage};

/// How long one long-poll holds the connection open.
const POLL_WAIT_SECS: u64 = 15;

/// Which side of the gateway a transport is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A bridge worker: sends output and permission requests, reads answers.
    Worker,
    /// An operator client: reads requests, sends answers.
    Host,
}

impl Role {
    /// The frame address this role posts to.
    const fn peer(self) -> &'static str {
        match self {
            Self::Worker => "host",
            Self::Host => "worker",
        }
    }

    /// The frame address this role reads.
    const fn this(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Host => "host",
        }
    }
}

/// A transport that moves frames over the gateway's HTTP mailbox.
#[derive(Debug)]
pub struct HttpPollTransport {
    base_url: String,
    env_id: String,
    client: reqwest::Client,
    connected: bool,
    /// The sequence number of the last frame delivered.
    cursor: u64,
    /// Frames received but not yet handed to the caller.
    pending: VecDeque<TransportMessage>,
    connection_id: String,
    role: Role,
}

impl HttpPollTransport {
    /// A worker's transport for one environment.
    #[must_use]
    pub fn new(base_url: impl Into<String>, env_id: impl Into<String>) -> Self {
        Self::with_role(base_url, env_id, Role::Worker)
    }

    /// A transport for one environment and one side of the gateway.
    #[must_use]
    pub fn with_role(base_url: impl Into<String>, env_id: impl Into<String>, role: Role) -> Self {
        let env_id = env_id.into();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            connection_id: format!("http-poll://{env_id}"),
            env_id,
            client: reqwest::Client::new(),
            connected: false,
            cursor: 0,
            pending: VecDeque::new(),
            role,
        }
    }

    /// The sequence number of the last delivered frame (0 before any).
    #[must_use]
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    fn frames_url(&self) -> String {
        format!("{}/api/environments/{}/frames", self.base_url, self.env_id)
    }

    /// Fetch the frames after the cursor. Empty when the wait expires.
    async fn poll(&mut self) -> Result<(), TransportError> {
        #[derive(Deserialize)]
        struct Entry {
            seq: u64,
            frame: serde_json::Value,
        }
        #[derive(Deserialize)]
        struct Listing {
            #[serde(default)]
            frames: Vec<Entry>,
        }

        let response = self
            .client
            .get(self.frames_url())
            .query(&[
                ("for", self.role.this()),
                ("after", self.cursor.to_string().as_str()),
                ("wait", POLL_WAIT_SECS.to_string().as_str()),
            ])
            .timeout(Duration::from_secs(POLL_WAIT_SECS + 10))
            .send()
            .await
            .map_err(|error| TransportError::ReceiveFailed(error.to_string()))?;
        if !response.status().is_success() {
            return Err(TransportError::ReceiveFailed(format!(
                "the gateway answered {}",
                response.status()
            )));
        }
        let listing: Listing = response
            .json()
            .await
            .map_err(|error| TransportError::DeserializationError(error.to_string()))?;
        for Entry { seq, frame } in listing.frames {
            let message: TransportMessage = serde_json::from_value(frame)
                .map_err(|error| TransportError::DeserializationError(error.to_string()))?;
            self.pending.push_back(message);
            self.cursor = self.cursor.max(seq);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Transport for HttpPollTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.connected = true;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn send(&mut self, message: TransportMessage) -> Result<(), TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        let response = self
            .client
            .post(self.frames_url())
            .query(&[("to", self.role.peer())])
            // The gateway stores the posted document verbatim as the frame.
            .json(&message)
            .send()
            .await
            .map_err(|error| TransportError::SendFailed(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(TransportError::SendFailed(format!(
                "the gateway answered {status}"
            )));
        }
        Ok(())
    }

    async fn receive(&mut self) -> Result<TransportMessage, TransportError> {
        loop {
            if let Some(message) = self.pending.pop_front() {
                return Ok(message);
            }
            if !self.connected {
                return Err(TransportError::NotConnected);
            }
            self.poll().await?;
        }
    }

    fn connection_id(&self) -> &str {
        &self.connection_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connection_id_names_the_environment() {
        let transport = HttpPollTransport::new("http://127.0.0.1:9/", "env-a");
        assert_eq!(transport.connection_id(), "http-poll://env-a");
        assert_eq!(transport.cursor(), 0);
        assert!(!transport.is_connected());
    }

    #[test]
    fn the_frames_url_does_not_double_the_slash() {
        let transport = HttpPollTransport::new("http://127.0.0.1:9/", "env-a");
        assert_eq!(
            transport.frames_url(),
            "http://127.0.0.1:9/api/environments/env-a/frames"
        );
    }

    #[tokio::test]
    async fn sending_before_connecting_is_refused() {
        let mut transport = HttpPollTransport::new("http://127.0.0.1:9", "env-a");
        let message = TransportMessage {
            r#type: "session_output".into(),
            correlation_id: None,
            payload: "{}".into(),
            metadata: std::collections::HashMap::new(),
        };
        assert!(matches!(
            transport.send(message).await,
            Err(TransportError::NotConnected)
        ));
    }
}
