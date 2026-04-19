# mcp Module Inspection

## Goal

Model Context Protocol (MCP) support — enables CodeRex to act as an MCP client, discovering and using tools/resources from MCP servers, and handling MCP authentication flows.

## Design

### Key Types

- **`McpManager`** (`manager.rs`) — Discovers and manages MCP server connections.
- **`McpServer`** (`server.rs`) — MCP server descriptor and capabilities.
- **`McpChannel`** (`channel.rs`) — Transport channel for MCP protocol messages.
- **`McpProxy`** (`proxy.rs`) — Proxies tool calls between CodeRex and MCP servers.
- **`McpResources`** (`resources.rs`) — Resource listing and access.
- **`McpConfig`** (`config.rs`) — MCP server configuration.
- **`McpElicitation`** (`elicitation.rs`) — User prompts for MCP auth/confirmation.
- **`McpOAuth`** (`oauth.rs`) — OAuth2 flow for MCP server authentication.

### Architecture Patterns

- **MCP client model** — CodeRex connects to external MCP servers as a client.
- **OAuth2 auth flow** — Standard OAuth2 for server authentication.
- **Tool proxying** — MCP tools appear as native CodeRex tools.

## Current Status

### What Works

- MCP manager and server types.
- Configuration loading for MCP servers.
- OAuth2 flow types.
- Resource access abstractions.
- Channel and proxy types.

### What Is Stubbed/Partial

- **Actual MCP protocol implementation** — Types defined but wire protocol not complete.
- **OAuth2 flow** — Types exist but no actual token exchange.
- **Tool proxying** — Proxy structure defined but not wired to tool registry.
- **Server discovery** — No automatic MCP server discovery.

## Gaps for Production

- **MCP protocol incomplete**: Cannot actually communicate with MCP servers.
  → **Suggestion**: Implement the full MCP JSON-RPC protocol: `initialize`, `tools/list`, `tools/call`, `resources/list`, `resources/read`. Use `jsonrpc-core` or implement a lightweight client. Follow the official MCP specification closely.
- **No OAuth2 token refresh**: Tokens expire without renewal.
  → **Suggestion**: Store `expires_at` alongside each access token. Before making a request, check if the token is within 5 minutes of expiry. If so, refresh using the refresh token. Persist updated tokens to `~/.coderex/mcp_tokens.json`.
- **No MCP server health checks**: Dead servers hang tool calls.
  → **Suggestion**: Implement a periodic health check (every 30s) that sends a `ping` or lightweight `tools/list` request. Mark unhealthy servers and skip them in tool dispatch. Surface server status in the UI.
- **No tool result translation**: MCP tool results may not map cleanly to CodeRex types.
  → **Suggestion**: Define a `McpResultAdapter` trait that converts MCP result structures (text, images, errors) into `ToolResult` variants. Register adapters per server type. Default to string serialization for unknown result types.
- **No caching of MCP resources**: Repeated resource reads hit the server every time.
  → **Suggestion**: Add an LRU cache for MCP resources with a TTL (default 60s). Cache key: `(server_id, resource_uri)`. Invalidate on explicit user request (`/mcp refresh`).
- **Security review needed**: MCP servers can execute arbitrary code; sandboxing is essential.
  → **Suggestion**: Run MCP server processes in OS-level sandboxes (seccomp-bpf on Linux, app sandbox on macOS). Restrict filesystem access to a whitelist. Require user confirmation before connecting to a new MCP server for the first time.
