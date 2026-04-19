# server Module Inspection

## Goal

Daemon server mode — provides WebSocket sessions and structured I/O for headless/SDK consumers. Enables CodeRex to run as a background service that external tools can connect to.

## Design

### Key Types

- **`DaemonServer`** (`session.rs`) — Main server accepting WebSocket connections.
- **`Session`** (`session.rs`) — Per-client session with message routing.
- **`ServerConfig`** (`session.rs`) — Host, port, auth, TLS settings.
- **`CommandFilter`** (`filter.rs`) — Filters and validates incoming commands by safety level.
- **`SafetyLevel`** (`filter.rs`) — Restricted, Normal, Permissive.

### Architecture Patterns

- **WebSocket per client** — Each connection gets a dedicated session.
- **Command filtering** — Safety levels restrict available commands/tools.
- **Structured I/O** — JSON protocol for external consumers.

## Current Status

### What Works

- Server and session types defined.
- Server configuration with host/port.
- Command filter with safety levels.
- Session lifecycle types.

### What Is Stubbed/Partial

- **WebSocket implementation** — Uses `tokio-tungstenite` but full protocol not complete.
- **Auth/TLS** — Config types exist but not enforced.
- **Structured I/O protocol** — No formal protocol specification.
- **Session multiplexing** — Single session per connection; no shared sessions.

## Gaps for Production

- **No authentication**: Anyone who can reach the port can connect.
  → **Suggestion**: Implement token-based authentication. On connection, require a `Bearer` token in the `Authorization` header. Validate against a token file or OS keychain entry. Reject connections with invalid/missing tokens.
- **No TLS by default**: Plaintext WebSocket.
  → **Suggestion**: Generate a self-signed certificate on first server startup (using `rcgen` crate) and store it in `~/.coderex/certs/`. Default to `wss://` and reject `ws://` unless `--insecure` is passed. Support user-provided certificates via config.
- **No rate limiting**: Clients can flood the server.
  → **Suggestion**: Add a token-bucket rate limiter per IP address and per session. Limit to 100 messages/minute per session and 1000 connections/minute per IP. Return `429 Too Many Requests` with a `Retry-After` header.
- **No session persistence**: Server restart loses all active sessions.
  → **Suggestion**: Serialize session state (messages, context) to SQLite on every turn end. On restart, reload active sessions from the database and re-establish WebSocket connections (clients will need to reconnect).
- **No health endpoint**: No HTTP health check for load balancers.
  → **Suggestion**: Add an HTTP `/health` endpoint (on a separate port or path) that returns `{"status": "ok", "version": "x.y.z", "sessions": N}`. Include readiness (can accept connections) and liveness (not deadlocked) checks.
- **Protocol not documented**: External consumers must reverse-engineer the JSON format.
  → **Suggestion**: Document the WebSocket protocol in `docs/server-protocol.md` with: message schema (request/response), event types, authentication flow, error codes, and a TypeScript type definition for SDK consumers. Provide example clients in Python and TypeScript.
