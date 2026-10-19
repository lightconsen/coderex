//! CodeRex MCP (Model Context Protocol) system.

#![warn(missing_docs)]

pub mod channel;
pub mod config;
pub mod elicitation;
mod http;
/// The probe interval used when `CODEREX_MCP_STREAM_PROBE_SECS` is unset.
pub use http::DEFAULT_STREAM_PROBE_INTERVAL;
/// How hard that stream retries before it gives up.
pub use http::EventStreamRetryPolicy;
/// The state of a remote server's standalone server-to-client stream.
pub use http::EventStreamState;
/// Environment variable naming how often a degraded GET stream probes for
/// recovery, in seconds; `0` disables the probe.
pub use http::MCP_STREAM_PROBE_ENV;
/// The credential resolver a remote channel consults before each request.
pub use http::TokenProvider;
pub mod manager;
pub mod oauth;
pub mod proxy;
pub mod resources;
pub mod result_adapter;
pub mod sandbox;
pub mod server;
pub mod tool;
