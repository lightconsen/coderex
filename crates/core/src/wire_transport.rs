//! Wire transport abstraction — pluggable backends for the wire protocol.
//!
//! Defines the [`WireTransport`] trait and concrete implementations for
//! in-memory testing and stdio-based subprocess communication.

use crate::wire_protocol::{WireEvent, WireRequest, decode_request, encode_event};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

/// Abstract transport for the wire protocol.
///
/// Implementations handle the framing and I/O of [`WireEvent`] (outgoing)
/// and [`WireRequest`] (incoming) over a specific medium.
#[async_trait]
pub trait WireTransport: Send + Sync {
    /// Send a single event to the transport.
    ///
    /// # Errors
    /// Returns an error if the underlying I/O fails.
    async fn send_event(&mut self, event: WireEvent) -> std::io::Result<()>;

    /// Receive the next request from the transport.
    ///
    /// Returns `None` when the transport is closed.
    ///
    /// # Errors
    /// Returns an error if deserialization fails.
    async fn recv_request(&mut self) -> std::io::Result<Option<WireRequest>>;

    /// Close the transport gracefully.
    async fn close(&mut self) -> std::io::Result<()>;
}

// ─── In-Memory Transport ────────────────────────────────────────────────────

/// A bidirectional in-memory transport for testing.
///
/// Uses unbounded channels to buffer events and requests. Clone the
/// `client_end` to get a second handle that sees events from the
/// server end and can send requests back.
#[derive(Debug, Clone)]
pub struct InMemoryTransport {
    event_rx: Arc<Mutex<mpsc::UnboundedReceiver<WireEvent>>>,
    request_tx: mpsc::UnboundedSender<WireRequest>,
}

/// The "server" side of an in-memory transport pair.
///
/// Created by [`InMemoryTransport::new_pair`].
#[derive(Debug)]
pub struct InMemoryTransportServer {
    event_tx: mpsc::UnboundedSender<WireEvent>,
    request_rx: Arc<Mutex<mpsc::UnboundedReceiver<WireRequest>>>,
}

#[async_trait]
impl WireTransport for InMemoryTransport {
    async fn send_event(&mut self, event: WireEvent) -> std::io::Result<()> {
        // InMemoryTransport is the client side — it receives events
        // from the server. Sending events from client to server is done
        // via requests, so this is a no-op.
        let _ = event;
        Ok(())
    }

    async fn recv_request(&mut self) -> std::io::Result<Option<WireRequest>> {
        let mut rx = self.event_rx.lock().await;
        Ok(rx.recv().await.map(WireRequest::from_event))
    }

    async fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl InMemoryTransport {
    /// Create a connected pair of in-memory transports.
    ///
    /// Events sent via the server end are received by the client end.
    /// Requests sent via the client end are received by the server end.
    #[must_use]
    pub fn new_pair() -> (InMemoryTransportServer, Self) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (request_tx, request_rx) = mpsc::unbounded_channel();

        let server = InMemoryTransportServer {
            event_tx,
            request_rx: Arc::new(Mutex::new(request_rx)),
        };
        let client = Self {
            event_rx: Arc::new(Mutex::new(event_rx)),
            request_tx,
        };
        (server, client)
    }

    /// Send a request to the server side.
    pub fn send_request(&self, req: WireRequest) -> std::io::Result<()> {
        self.request_tx
            .send(req)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "server closed"))
    }
}

#[async_trait]
impl WireTransport for InMemoryTransportServer {
    async fn send_event(&mut self, event: WireEvent) -> std::io::Result<()> {
        self.event_tx
            .send(event)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client closed"))
    }

    async fn recv_request(&mut self) -> std::io::Result<Option<WireRequest>> {
        let mut rx = self.request_rx.lock().await;
        Ok(rx.recv().await)
    }

    async fn close(&mut self) -> std::io::Result<()> {
        // Sender is dropped naturally when the struct is dropped
        Ok(())
    }
}

impl WireRequest {
    fn from_event(event: WireEvent) -> Self {
        // This is a test-only conversion for the in-memory transport
        // where the client "receives" events as if they were requests
        match event {
            WireEvent::TextDelta { delta } => Self::Submit {
                prompt: delta,
                system_prompt: None,
                max_turns: None,
            },
            _ => Self::Interrupt,
        }
    }
}

// ─── Stdio Transport ────────────────────────────────────────────────────────

/// NDJSON transport over stdin/stdout.
///
/// Reads requests from stdin and writes events to stdout.
/// Suitable for subprocess-based E2E tests and IDE integrations.
#[allow(missing_debug_implementations)]
pub struct StdioTransport {
    reader: BufReader<tokio::io::Stdin>,
    stdout: tokio::io::Stdout,
    buffer: String,
}

impl StdioTransport {
    /// Create a new stdio transport.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reader: BufReader::new(tokio::io::stdin()),
            stdout: tokio::io::stdout(),
            buffer: String::with_capacity(4096),
        }
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WireTransport for StdioTransport {
    async fn send_event(&mut self, event: WireEvent) -> std::io::Result<()> {
        let line = encode_event(&event);
        self.stdout.write_all(line.as_bytes()).await?;
        self.stdout.flush().await
    }

    async fn recv_request(&mut self) -> std::io::Result<Option<WireRequest>> {
        self.buffer.clear();
        let bytes_read = self.reader.read_line(&mut self.buffer).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        match decode_request(&self.buffer) {
            None => Ok(None),
            Some(Ok(req)) => Ok(Some(req)),
            Some(Err(e)) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid request JSON: {e}"),
            )),
        }
    }

    async fn close(&mut self) -> std::io::Result<()> {
        self.stdout.shutdown().await
    }
}

// ─── Sink Transport (for bridging mpsc channels) ────────────────────────────

/// A transport that forwards events to an async callback.
///
/// This is useful for bridging the wire protocol into existing
/// event loop architectures without changing their channel types.
#[allow(missing_debug_implementations)]
pub struct SinkTransport<F> {
    sink: F,
}

impl<F> SinkTransport<F> {
    /// Create a new sink transport with the given callback.
    #[must_use]
    pub const fn new(sink: F) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl<F> WireTransport for SinkTransport<F>
where
    F: FnMut(WireEvent) -> std::io::Result<()> + Send + Sync,
{
    async fn send_event(&mut self, event: WireEvent) -> std::io::Result<()> {
        (self.sink)(event)
    }

    async fn recv_request(&mut self) -> std::io::Result<Option<WireRequest>> {
        // Sink transport is output-only
        Ok(None)
    }

    async fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_roundtrip() {
        let (mut server, client) = InMemoryTransport::new_pair();

        server
            .send_event(WireEvent::TextDelta {
                delta: "hello".into(),
            })
            .await
            .unwrap();

        let received = {
            let mut rx = client.event_rx.lock().await;
            rx.recv().await
        };

        assert_eq!(
            received,
            Some(WireEvent::TextDelta {
                delta: "hello".into()
            })
        );
    }

    #[tokio::test]
    async fn in_memory_request_roundtrip() {
        let (mut server, client) = InMemoryTransport::new_pair();

        client
            .send_request(WireRequest::Submit {
                prompt: "hi".into(),
                system_prompt: None,
                max_turns: None,
            })
            .unwrap();

        let received = server.recv_request().await.unwrap();
        assert!(matches!(received, Some(WireRequest::Submit { prompt, .. }) if prompt == "hi"));
    }
}
