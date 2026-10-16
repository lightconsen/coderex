# server Module Inspection

## Goal

Daemon server mode — provides WebSocket sessions and structured I/O for headless/SDK consumers. Enables CodeRex to run as a background service that external tools can connect to.

This crate is the **external protocol**: out-of-process clients (IDEs, web/mobile, scripts) connect here. It is distinct from `coderex-events` (`docs/events.md`), which is the **line-delimited stream protocol** on stdout (`--output-format stream-json`) plus session bookkeeping — there is no in-process event bus in either crate.

## Design

### Key Types

- **`DaemonServer`** (`session.rs`) — Main server accepting WebSocket connections with rate limiting and session persistence.
- **`Session`** (`session.rs`) — Per-client session with authentication, activity tracking, and idle timeout.
- **`ServerConfig`** (`session.rs`) — Host, port, auth keys, rate limits, health port, persistence path.
- **`CommandFilter`** (`filter.rs`) — Filters and validates incoming commands by safety level.
- **`SafetyLevel`** (`filter.rs`) — Remote (read-only), Bridge (limited writes).
- **`WsRequest`/`WsResponse`/`WsEvent`** (`session.rs`) — Structured JSON protocol types.
- **`SessionStore`** (`persistence.rs`) — SQLite-backed session persistence.
- **`TokenBucket`** (`session.rs`) — Per-connection rate limiter.

### Architecture Patterns

- **WebSocket per client** — Each connection gets a dedicated session with auth, rate limiting, and message routing.
- **Structured JSON protocol** — Request/response with correlation IDs, typed events, and streaming support.
- **Session persistence** — SQLite storage with restore on server restart.
- **Command filtering** — Safety levels restrict available commands/tools.
- **Health endpoint** — Optional HTTP `/health` on a separate port.

## Current Status

The `coderex-server` crate is fully implemented with no stubs or gaps for production.
