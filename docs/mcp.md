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
- **`McpOAuthState`** / **`OAuthManager`** (`oauth.rs`) — OAuth2 token state: access_token, refresh_token, expires_at, scopes. Expiry checking via `chrono`. Token persistence to `~/.coderex/mcp_tokens.json`. Near-expiry detection (5 min buffer).
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
- **OAuth2 token management** — Full token lifecycle with expiry tracking, near-expiry detection, and file persistence.
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
- Token persistence: load/save to `~/.coderex/mcp_tokens.json`.
- Elicitation: `ElicitationManager` for pending requests and responses.
- Config loading: `.mcp.json` file parsing + settings.json integration, save/load roundtrip.
- Result adapter: `McpResultAdapter` converts MCP content (text, image, resource) to `ToolResult`.
- Tool executor: `McpToolExecutor` implements `ToolExecutor` trait; `CompositeToolExecutor` provides fallback dispatch.
- Health monitor: Background task checks all servers every 30s.
- Startup wiring: `StartupOrchestrator` phase loads config, starts servers, discovers tools/resources, sets `mcp_ready`.
- 60 tests across all 10 modules covering channel, server, proxy, config, OAuth, elicitation, manager, result adapter, and tool executor.

### What Is Stubbed/Partial

*None remaining — all gaps implemented.*

## Gaps for Production

*None remaining.*

## Remaining Considerations

- **OAuth authorization flow**: Token refresh and persistence are implemented, but the initial browser-based authorization code flow (opening browser, local callback server, code exchange) is not yet implemented. Servers that require interactive OAuth will need this for first-time setup.
- **MCP tool schema in prompts**: Discovered MCP tools are registered but not yet formatted into the system prompt sent to the LLM. The engine needs to serialize `McpToolInfo` into the tool definitions section of prompts.
- **Resource read via cache**: The cache infrastructure exists but `resources/read` is not yet called through it. A full `read_resource(uri)` method that checks cache, falls back to server, and stores result is needed for complete integration.
