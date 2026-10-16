# mcp Module Inspection

## Goal

Model Context Protocol (MCP) support — enables CodeRex to act as an MCP client, discovering and using tools/resources from MCP servers, and handling MCP authentication flows.

## Design

### Key Types

- **`McpManager`** (`manager.rs`) — Central coordinator for servers, tools, resources, OAuth, elicitation, and health monitoring. Uses `Arc<DashMap>` for concurrent server access.
- **`McpServer`** (`server.rs`) — Server descriptor with full lifecycle: spawn, health check (immutable), reconnect (max 3), stop. Tracks 5 statuses: Stopped, Starting, Running, Unhealthy, Errored.
- **`JsonRpcChannel`** (`channel.rs`) — JSON-RPC 2.0 stdio transport: `initialize()` (protocol 2024-11-05 + `initialized` notification), `send_request()`, `send_notification()`, `read_response()`, `list_tools()`, `list_resources()`.
- **`McpToolProxy`** (`proxy.rs`) — Tool call forwarding: `call(args)` sends `tools/call` over shared channel.
- **`McpToolRegistry`** (`proxy.rs`) — HashMap-based tool registry with `McpToolInfo` (name, description, input_schema, server).
- **`McpResource`** / **`McpResourceContent`** / **`McpResourceRegistry`** (`resources.rs`) — Resource listing and access with MIME-type filtering.
- **`McpConfig`** / **`McpServerConfig`** (`config.rs`) — Configuration loading from `.mcp.json` or settings.json, with file persistence.
- **`McpOAuthState`** / **`OAuthManager`** (`oauth.rs`) — token bookkeeping for MCP servers: access_token, refresh_token, expires_at, scopes. Expiry checking via `chrono`, persistence to `~/.coderex/mcp_tokens.json`, and a real `grant_type=refresh_token` exchange (`OAuthManager::refresh`) against the server's token endpoint. Near-expiry detection (5 min buffer) drives `OAuthManager::refresh_if_needed`, which `McpManager::start_all` calls before connecting to a remote server. This module cannot *obtain* a token — it can only keep one alive.
- **`McpAuthTool`** (`crates/tools/src/builtins/mcp_auth_tool.rs`) — the interactive half: authorization-code flow with PKCE (S256), a callback listener on `localhost:8765`, and a manual completion path (`code` + `code_verifier`) for when no browser can be opened. It is the only thing that can sign in, and it needs a browser and a user to approve.
- **`ElicitationRequest`** / **`ElicitationManager`** (`elicitation.rs`) — User prompt forwarding from MCP servers with pending/response lifecycle.
- **`McpResultAdapter`** (`result_adapter.rs`) — Converts MCP `tools/call` response JSON (text, image, resource content) into `ToolResult`.
- **`McpToolExecutor`** / **`CompositeToolExecutor`** (`tool_executor.rs`) — Implements `ToolExecutor` trait for MCP tool dispatch. Composite tries primary executor first, falls back to MCP.

### Hooked Into CLI

- **`StartupOrchestrator.phase_init_mcp()`** (`crates/cli/src/orchestrator.rs:495`) — Loads `.mcp.json`, starts servers, discovers tools/resources, sets `mcp_ready`.
- **`/mcp` commands** — Server listing and status via config extension commands.

### Architecture Patterns

- **MCP client over stdio** — Subprocesses communicate via JSON-RPC 2.0 on stdin/stdout.
- **Fail-open initialization** — Server spawn + initialize errors are logged but don't block startup.
- **Reconnection** — Up to 3 reconnection attempts per server with fresh channel + initialize handshake.
- **Health monitoring** — Background `tokio::spawn` task checks all servers every 30s via `health_check()`.
- **Concurrent access** — `Arc<DashMap>` for servers, `Mutex` for tool/resource registries and cache.
- **OAuth2 token management** — Token storage with expiry tracking, near-expiry detection, file persistence, and refresh-token exchange. Tokens are resolved when a remote server is started and sent as `Authorization: Bearer`; a token inside the 5-minute window is refreshed first.
- **Result adaptation** — MCP content arrays (text/image/resource) normalized to `ToolResult`.
- **Resource caching** — In-memory TTL cache (default 60s) keyed by `(server_id, resource_uri)`.
- **Composite dispatch** — Primary tool executor + MCP fallback in a single `ToolExecutor`.

## Current Status

### What Works

- Full JSON-RPC 2.0 stdio channel with `initialize()` handshake (protocol version 2024-11-05).
- Server lifecycle: spawn, health check (immutable), reconnect (max 3), stop with status tracking.
- Auto-discovery: `tools/list` and `resources/list` called after server spawn, results parsed and registered.
- Tool proxy: `McpToolProxy.call(args)` sends `tools/call` over shared channel.
- Tool registry: `McpToolRegistry` with register, get, unregister, list.
- Resource types: `McpResource`, `McpResourceContent`, registry with MIME filtering.
- Resource caching: TTL-based in-memory cache with get, store, invalidate, clear operations.
- OAuth2 types: `McpOAuthState` with token storage, expiry checking, near-expiry detection (5 min buffer), refresh_token support.
- Token persistence: load/save to `~/.coderex/mcp_tokens.json`, in the one shape both the `McpAuth` tool and `OAuthManager` read (`{"states": [...]}`).
- Token refresh: `OAuthManager::refresh` posts `grant_type=refresh_token` to the server's token endpoint and stores the result, including a rotated refresh token. `OAuthManager::refresh_if_needed` is called by `McpManager::start_all` for every remote server, and the resulting token is sent as `Authorization: Bearer`.
- Interactive sign-in: `McpAuth` performs authorization-code + PKCE (S256), with a `localhost:8765` callback listener and a manual `code` + `code_verifier` path when the browser cannot be opened.
- Elicitation: `ElicitationManager` for pending requests and responses.
- Config loading: `.mcp.json` file parsing + settings.json integration, save/load roundtrip.
- Result adapter: `McpResultAdapter` converts MCP content (text, image, resource) to `ToolResult`.
- Tool executor: `McpToolExecutor` implements `ToolExecutor` trait; `CompositeToolExecutor` provides fallback dispatch.
- Health monitor: Background task checks all servers every 30s.
- Startup wiring: `StartupOrchestrator` phase loads config, starts servers, discovers tools/resources, sets `mcp_ready`.
- 60 tests across all 10 modules covering channel, server, proxy, config, OAuth, elicitation, manager, result adapter, and tool executor.

### What Is Stubbed/Partial

- **OAuth token refresh only covers servers that issue refresh tokens.** A server that hands back an access token with no refresh token cannot be kept alive by `OAuthManager`; the token expires and the user has to run `McpAuth` again. That is a property of the server, and the code reports it rather than retrying forever.
- **OAuth discovery is not implemented.** The token endpoint is either taken from the stored state or derived as `{server_url}/oauth/token`; the `/.well-known/oauth-authorization-server` document is not fetched. A server that advertises its endpoints elsewhere needs `token_endpoint` set explicitly.
- **Refreshed credentials are applied when a server is started**, not per request. A session that outlives its token's full lifetime — longer than the 5-minute window can cover — will keep using the token it started with until the server is restarted or reconnects.

## Gaps for Production

- **MCP tool schema in prompts**: Discovered MCP tools are registered but not yet formatted into the system prompt sent to the LLM. The engine needs to serialize `McpToolInfo` into the tool definitions section of prompts.
- **Resource read via cache**: The cache infrastructure exists but `resources/read` is not yet called through it. A full `read_resource(uri)` method that checks cache, falls back to server, and stores result is needed for complete integration.

## Remaining Considerations

- **OAuth authorization flow**: the browser-based authorization-code flow exists in the `McpAuth` tool, with PKCE, a local callback server, and a manual fallback. It needs a browser and a user to approve; a headless run has no way to complete it, and the tool says so instead of waiting out its timeout.
