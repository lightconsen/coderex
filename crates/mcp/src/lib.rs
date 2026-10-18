//! CodeRex MCP (Model Context Protocol) system.

#![warn(missing_docs)]

pub mod channel;
pub mod config;
pub mod elicitation;
mod http;
/// How hard that stream retries before it gives up.
pub use http::EventStreamRetryPolicy;
/// The state of a remote server's standalone server-to-client stream.
pub use http::EventStreamState;
/// The credential resolver a remote channel consults before each request.
pub use http::TokenProvider;
pub mod manager;
pub mod oauth;
pub mod proxy;
pub mod resources;
pub mod result_adapter;
pub mod server;
pub mod tool;
