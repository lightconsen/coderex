# mcp Module Inspection

## Goal

Model Context Protocol (MCP) support — enables CodeRex to act as an MCP client, discovering and using tools/resources from MCP servers, and handling MCP authentication flows.

## Design

### Key Types

- **`McpManager`** (`manager.rs`) — Central coordinator for servers, tools, resources, OAuth, elicitation, and health monitoring. Uses `Arc<DashMap>` for concurrent server access.
- **`McpServer`** (`server.rs`) — Server descriptor with full lifecycle: spawn, health check (immutable), reconnect (max 3), stop. Tracks 5 statuses: Stopped, Starting, Running, Unhealthy, Errored.
- **`JsonRpcChannel`** (`channel.rs`) — JSON-RPC 2.0 stdio transport: `initialize()` (protocol 2024-11-05 + `initialized` notification), `send_request()`, `send_notification()`, `read_response()`, `list_tools()`, `list_resources()`.
- **`McpToolProxy`** (`proxy.rs`) — Tool call forwarding: `call(args)` sends `tools/call` over shared channel.
- **`McpToolRegistry`** (`proxy.rs`) — HashMap-based tool registry with `McpToolInfo` (name, description, input_schema, server). Public names are `mcp__<server>__<tool>`, or the raw name when `CLAUDE_AGENT_SDK_MCP_NO_PREFIX` is set (see *Tool naming* below).
- **`McpToolNameConflict`** (`proxy.rs`) — a raw name two servers both expose while the prefix is off. The later registration is refused and recorded via `McpToolRegistry::conflicts()`, because the name can only address one of them.
- **`McpResource`** / **`McpResourceContent`** / **`McpResourceRegistry`** (`resources.rs`) — Resource listing and access with MIME-type filtering.
- **`McpConfig`** / **`McpServerConfig`** (`config.rs`) — Configuration loading from `.mcp.json` or settings.json, with file persistence.
- **`McpOAuthState`** / **`OAuthManager`** (`oauth.rs`) — token bookkeeping for MCP servers: access_token, refresh_token, expires_at, scopes, and the discovered `authorization_endpoint` / `token_endpoint`. Expiry checking via `chrono`, persistence to `~/.coderex/mcp_tokens.json`, RFC 8414 discovery (`OAuthManager::discover`, with OpenID Connect discovery as the fallback), and a real `grant_type=refresh_token` exchange (`OAuthManager::refresh`) against the discovered endpoint. Near-expiry detection (5 min buffer) drives `OAuthManager::refresh_if_needed`, which is called by each server's token provider before every HTTP request. This module cannot *obtain* a first token — only the `McpAuth` tool can; this keeps a session alive.
- **`OAuthTokenProvider`** (`oauth.rs`) — implements `TokenProvider` (`http.rs`) on top of the manager's shared `OAuthManager`. `HttpChannel` consults it before every POST (requests, notifications, and answers to server-initiated requests), so a token that expires mid-session is refreshed on the next request. A failed refresh yields no credential rather than a failed request; stdio channels ignore the provider.
- **`McpAuthTool`** (`crates/tools/src/builtins/mcp_auth_tool.rs`) — the interactive half: authorization-code flow with PKCE (S256), a callback listener on `localhost:8765`, and a headless completion path. When no browser can be opened it returns the authorize URL, verifier and state immediately and persists the pending flow; a later call finishes it with either the pasted full redirect URL (state checked against the pending flow) or `code` + `code_verifier`. Endpoints come from the same RFC 8414 / OpenID discovery, falling back to `/oauth/authorize` and `/oauth/token`.
- **`ElicitationRequest`** / **`ElicitationManager`** (`elicitation.rs`) — User prompt forwarding from MCP servers with pending/response lifecycle.
- **`McpResultAdapter`** (`result_adapter.rs`) — Converts MCP `tools/call` response JSON (text, image, resource content) into `ToolResult`.
- **`McpTool`** (`crates/mcp/src/tool.rs`) — a discovered MCP tool as an ordinary `Tool`, registered in the shared `ToolRegistry` under its public name, so it goes through the same dispatcher, timeout, output limit and concurrency rules as a built-in. The separate `McpToolExecutor`/`CompositeToolExecutor` were removed.

### Tool naming

Public names are `mcp__<server>__<tool>`, which is load-bearing: two servers may expose the same raw tool name, and the prefix is what keeps them apart. Setting `CLAUDE_AGENT_SDK_MCP_NO_PREFIX` (to anything but `0`/`false`) addresses tools by their raw server-side name instead — the raw name is what the server expects in `tools/call` either way.

Without the prefix a shared raw name cannot address both servers, so the second registration is **refused** rather than allowed to take the name over (which would route its calls to the other server). The refusal is reported by `McpToolRegistry::conflicts()` and logged; turn the prefix back on to reach that tool. `McpToolInfo::prefixed_name()` always gives the unambiguous spelling.

### Hooked Into CLI

- **`StartupOrchestrator.phase_init_mcp()`** (`crates/cli/src/orchestrator.rs:495`) — Loads `.mcp.json`, starts servers, discovers tools/resources, sets `mcp_ready`.
- **`/mcp` commands** — Server listing and status via config extension commands.

### Architecture Patterns

- **MCP client over stdio** — Subprocesses communicate via JSON-RPC 2.0 on stdin/stdout.
- **Fail-open initialization** — Server spawn + initialize errors are logged but don't block startup.
- **Reconnection** — Up to 3 reconnection attempts per server with fresh channel + initialize handshake.
- **Health monitoring** — Background `tokio::spawn` task checks all servers every 30s via `health_check()`.
- **Concurrent access** — `Arc<DashMap>` for servers, `Mutex` for tool/resource registries and cache.
- **OAuth2 token management** — Token storage with expiry tracking, near-expiry detection, file persistence, and refresh-token exchange. Every remote request resolves its own `Authorization: Bearer` credential through a token provider, so a token inside the 5-minute window is refreshed before that request. Discovery caches the advertised endpoints in the token store; when discovery fails, the conventional paths are used.
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
- Discovery: `OAuthManager::discover` fetches `{origin}/.well-known/oauth-authorization-server`, falling back to `{origin}/.well-known/openid-configuration`, and records `authorization_endpoint` / `token_endpoint` in the persisted state. `refresh` and `refresh_if_needed` use the discovered token endpoint; a server that publishes nothing keeps the conventional `{server_url}/oauth/token` path.
- Token refresh: `OAuthManager::refresh` posts `grant_type=refresh_token` to the (discovered or conventional) token endpoint and stores the result, including a rotated refresh token. The token provider installed on each remote channel calls `refresh_if_needed` before every request, so a long-lived session refreshes rather than sending a expired token.
- Interactive sign-in: `McpAuth` performs authorization-code + PKCE (S256), with a `localhost:8765` callback listener. When the browser cannot be opened it returns immediately with the authorize URL, verifier and state, and persists the pending flow. A later call completes it from a pasted redirect URL (parsing `code` and `state` and verifying the state) or from `code` + `code_verifier`; both exchange the code with PKCE.
- Elicitation: `ElicitationManager` for pending requests and responses.
- Server-initiated requests: messages from the server are told apart by shape (method+id is a request, method alone a notification, id alone a response). They are answered on both transports — the stdio read loop directly, and the HTTP/SSE response stream incrementally, with the answer sent as a separate POST so a server holding its response open cannot deadlock the client. With no handler the answer is a JSON-RPC `-32601` refusal rather than silence.
- Config loading: `.mcp.json` file parsing + settings.json integration, save/load roundtrip.
- Result adapter: `McpResultAdapter` converts MCP content (text, image, resource) to `ToolResult`.
- Tool executor: `McpToolExecutor` implements `ToolExecutor` trait; `CompositeToolExecutor` provides fallback dispatch.
- Health monitor: Background task checks all servers every 30s.
- Startup wiring: `StartupOrchestrator` phase loads config, starts servers, discovers tools/resources, sets `mcp_ready`.
- 60 tests across all 10 modules covering channel, server, proxy, config, OAuth, elicitation, manager, result adapter, and tool executor.

### What Is Stubbed/Partial

- **OAuth token refresh only covers servers that issue refresh tokens.** A server that hands back an access token with no refresh token cannot be kept alive by `OAuthManager`; the token expires and the user has to run `McpAuth` again. That is a property of the server, and the code reports it rather than retrying forever.
- **Discovery is origin-based.** The two well-known paths are composed from the server URL's origin, as the reference does; an issuer that lives under a path component (RFC 8414's path-insertion form) is not probed. A server in that position can still be used by setting `token_endpoint` explicitly, and an unreachable metadata document simply falls back to the conventional paths.

## Gaps for Production

- **MCP tool schema in prompts**: Discovered MCP tools are registered but not yet formatted into the system prompt sent to the LLM. The engine needs to serialize `McpToolInfo` into the tool definitions section of prompts.
- **Resource read via cache**: The cache infrastructure exists but `resources/read` is not yet called through it. A full `read_resource(uri)` method that checks cache, falls back to server, and stores result is needed for complete integration.

### IDE tool whitelist

The reference allows two IDE tools (`mcp__ide__executeCode`, `mcp__ide__getDiagnostics`) to be used without a prompt. CodeRex has no IDE product to allowlist for: the only `ide` surfaces are the `--ide` CLI flag, which is accepted and explicitly does nothing ("this build has no IDE integration"), and the `/ide` status command, which reports "Not connected" and defers to a REPL that never connects. There is no `ide` MCP server, so there is no tool to whitelist — adding one would be building a policy for a product that does not exist.

## Remaining Considerations

- **OAuth authorization flow**: the browser-based authorization-code flow exists in the `McpAuth` tool, with PKCE and a local callback server. A headless run completes through the persisted hand-off — paste the full redirect URL (state is verified) or supply `code` + `code_verifier` — rather than waiting out the callback timeout, which is only entered when a browser was actually opened.
