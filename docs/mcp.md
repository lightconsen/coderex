# mcp Module Inspection

## Goal

Model Context Protocol (MCP) support — enables CodeRex to act as an MCP client, discovering and using tools/resources from MCP servers, and handling MCP authentication flows.

## Design

### Key Types

- **`McpManager`** (`manager.rs`) — Central coordinator for servers, tools, resources, OAuth, elicitation, and health monitoring. Uses `Arc<DashMap>` for concurrent server access.
- **`McpServer`** (`server.rs`) — Server descriptor with full lifecycle: spawn, health check (immutable), reconnect (max 3), stop. Tracks 5 statuses: Stopped, Starting, Running, Unhealthy, Errored.
- **`JsonRpcChannel`** (`channel.rs`) — JSON-RPC 2.0 stdio transport: `initialize()` (protocol 2024-11-05 + `initialized` notification), `send_request()`, `send_notification()`, `read_response()`, `list_tools()`, `list_resources()`, `list_prompts()`, `get_prompt()`.
- **`McpToolProxy`** (`proxy.rs`) — Tool call forwarding: `call(args)` sends `tools/call` over shared channel.
- **`McpToolRegistry`** (`proxy.rs`) — HashMap-based tool registry with `McpToolInfo` (name, description, input_schema, server). Public names are `mcp__<server>__<tool>`, or the raw name when `CLAUDE_AGENT_SDK_MCP_NO_PREFIX` is set (see *Tool naming* below).
- **`McpToolNameConflict`** (`proxy.rs`) — a raw name two servers both expose while the prefix is off. The later registration is refused and recorded via `McpToolRegistry::conflicts()`, because the name can only address one of them.
- **`McpResource`** / **`McpResourceContent`** / **`McpResourceRegistry`** (`resources.rs`) — Resource listing and access with MIME-type filtering.
- **`McpPromptInfo`** / **`McpPromptResult`** / **`ServerPrompt`** / **`McpPromptRegistry`** (`prompts.rs`) — Typed `prompts/list` and `prompts/get` results. `ServerPrompt` tags a prompt with the server that offered it; the registry can replace one server's prompts after a re-discovery without touching the others.
- **`McpConfig`** / **`McpServerConfig`** (`config.rs`) — Configuration loading from `.mcp.json` or settings.json, with file persistence.
- **`McpOAuthState`** / **`OAuthManager`** (`oauth.rs`) — token bookkeeping for MCP servers: access_token, refresh_token, expires_at, scopes, and the discovered `authorization_endpoint` / `token_endpoint` / `scopes_supported`. Expiry checking via `chrono`, persistence to `~/.coderex/mcp_tokens.json`, RFC 9728 Protected Resource Metadata (`OAuthManager::discover`), RFC 8414 and OpenID Connect authorization-server discovery, RFC 7591 dynamic client registration, and a real `grant_type=refresh_token` exchange (`OAuthManager::refresh`) against the discovered endpoint. Discovery first looks for the resource's protected-resource document (the `resource_metadata` URL from a `WWW-Authenticate` challenge when one is supplied, otherwise `{origin}/.well-known/oauth-protected-resource{path}`); the first `authorization_servers` entry becomes the issuer, so the MCP endpoint and its authorization server may live on different origins. It normalizes that issuer (query/fragment/trailing slash removed, path kept), tries the RFC 8414 path-insertion document first (`{origin}/.well-known/oauth-authorization-server{path}`), then the origin form, then the two OpenID Connect forms; it rejects a document whose `issuer` does not match and, when a document omits `issuer`, binds it to the configured issuer only when it was served from one of that issuer's own well-known URLs (an origin-level document with no `issuer` is rejected with a clear reason and the next candidate is tried); it caches the endpoints/scopes in an `issuer_metadata` map keyed by the normalized issuer, with an expiry from `Cache-Control: max-age`/`Expires` (one hour by default) and an `ETag` revalidated through `If-None-Match` (a `304` reuses the cached document). An expired document is re-fetched before a refresh. When no client id is configured and the server publishes `registration_endpoint`, a client is registered with the redirect URI, both grants, `response_types: ["code"]`, `code_challenge_method: S256`, and the strongest advertised `token_endpoint_auth_method`; the issued `client_id`/`client_secret` are stored and reused. The chosen method (`client_secret_basic`, `client_secret_post`, or `none` for a public PKCE client) is used for both the code exchange and refresh, and every authorize/token/refresh request carries the RFC 8707 `resource` indicator. Discovery refuses a server that advertises PKCE methods but not `S256`, and refuses a refresh when `grant_types_supported` excludes `refresh_token`. Near-expiry detection (5 min buffer) drives `OAuthManager::refresh_if_needed`, which is called by each server's token provider before every HTTP request; `OAuthManager::handle_unauthorized` implements the `401 invalid_token` policy (clear the token, re-fetch metadata if expired, then refresh once or delegate to the installed `ReauthHandler`). This module cannot *obtain* a first token itself — only the `McpAuth` tool can — but it can drive that tool through the handler; otherwise it keeps an existing session alive.
- **`OAuthTokenProvider`** (`oauth.rs`) — implements `TokenProvider` (`http.rs`) on top of the manager's shared `OAuthManager`. `HttpChannel` consults it before every POST (requests, notifications, and answers to server-initiated requests), so a token that expires mid-session is refreshed on the next request. It also handles a `401` whose `WWW-Authenticate: Bearer` challenge names `invalid_token`: the transport asks the provider for a replacement once, retries the request exactly once with that token, and treats a second `401` as an error. When there is no refresh token, the manager invokes the installed `ReauthHandler` once instead. A failed refresh hands back no credential rather than failing the request; stdio channels ignore the provider.
- **`ReauthHandler`** (`oauth.rs`) — the sign-in seam for servers that issue no refresh token. `OAuthManager` consults it for a `401 invalid_token` that no refresh token can recover, and proactively when `refresh_if_needed` finds the stored credential already expired with nothing to refresh it with (`refresh_if_needed` runs the handler once and returns the token it stored; with no handler installed the request goes out unauthenticated and the server's `401` is surfaced, as before). The CLI installs `CliReauthHandler` (`crates/cli/src/mcp_reauth.rs`) before MCP servers start: an interactive session runs the existing `McpAuth` authorization-code/PKCE flow (browser + localhost callback), while a headless session requests the URL without opening a browser, prints it with instructions, and exits the recovery path with `ReauthRequired`. The handler must persist the credential in the shared token store; on success the manager reloads that store, returns the new token, and the transport retries the request once. No handler installed (or a handler that fails) produces `ReauthRequired`, whose message tells the user to run `McpAuth`.
- **`McpAuthTool`** (`crates/tools/src/builtins/mcp_auth_tool.rs`) — the interactive half: authorization-code flow with PKCE (S256), a callback listener on `localhost:8765`, and a headless completion path. When no browser can be opened it returns the authorize URL, verifier and state immediately and persists the pending flow; a later call finishes it with either the pasted full redirect URL (state checked against the pending flow) or `code` + `code_verifier`. Endpoints come from the same RFC 9728 protected-resource lookup, issuer-normalized RFC 8414 / OpenID discovery, issuer check, `issuer_metadata` cache and `ETag` revalidation as `OAuthManager`; when nothing is published it falls back to `/oauth/authorize` and `/oauth/token`. It mirrors the same dynamic client registration (skipped when a `client_id` is supplied or already stored), client-authentication selection (`client_secret_basic`, `client_secret_post`, or public `none` + PKCE), S256 refusal, and RFC 8707 `resource` indicator in the authorize URL and token request. The two crates cannot share these types (`tools <- mcp`), so both read and write the same fields through the shared `coderex-config` secret store: the OS keychain when available, otherwise `~/.coderex/mcp_tokens.json` with owner-only permissions.
- **`InboundRequestHandler`** / **`SharedInboundHandler`** (`channel.rs`) — the async interface for requests the *server* makes of the client. `handle` can await the user, because the server blocks until it is answered. The shared slot lets a session install or replace its handler after servers are already running (and reconnecting servers keep it).
- **`McpElicitationHandler`** / **`FormSpec`** / **`FormField`** / **`ElicitationParams`** / **`McpElicitationResult`** (`elicitation.rs`) — translates `elicitation/create` (`message`, `requestedSchema`) into a typed form and back into `{"action": "accept"|"decline"|"cancel", "content": {k1: v1, ...}}`. `FormSpec::parse` reads `properties` in declaration order into `string`, `integer`, `number`, and `boolean` fields, with `required`, `default`, `title`, `description`, `enum`, `minLength`/`maxLength`, `pattern`, and `minimum`/`maximum`. Every answer is validated before an `accept` is sent: a wrong type, missing required field, out-of-range number, length violation, or pattern mismatch becomes a `decline`. Nested objects/arrays, `oneOf`, `$ref`, and other constructs outside MCP's flat elicitation vocabulary are declined with a logged reason rather than asked as text. A `FormElicitationHandler` can present the whole form at once (the TUI does); a prompt that only implements `ElicitationHandler` is still supported and asked one field at a time. `ElicitationRequest` / `ElicitationManager` remain for the pending-request bookkeeping API.
- **`McpResultAdapter`** (`result_adapter.rs`) — Converts MCP `tools/call` response JSON (text, image, resource content) into `ToolResult`.
- **`McpTool`** (`crates/mcp/src/tool.rs`) — a discovered MCP tool as an ordinary `Tool`, registered in the shared `ToolRegistry` under its public name, so it goes through the same dispatcher, timeout, output limit and concurrency rules as a built-in. The separate `McpToolExecutor`/`CompositeToolExecutor` were removed.

### Tool naming

Public names are `<prefix><server>__<tool>`, where the prefix comes from `CODEREX_MCP_PREFIX`:

| `CODEREX_MCP_PREFIX` | Public name for `filesystem`'s `read_file` |
|---|---|
| unset | `mcp__filesystem__read_file` (the default) |
| `coderex_` | `coderex_filesystem__read_file` |
| empty string | `read_file` |

The prefix may only contain ASCII letters, digits and `_`; anything else is ignored with a warning and the default is used. A value that is set always wins over the reference SDK's `CLAUDE_AGENT_SDK_MCP_NO_PREFIX` (anything but `0`/`false` still means "no prefix" when `CODEREX_MCP_PREFIX` is unset). The same rule names tools, permission rules, hook `mcp_tool` actions, sub-agent `allowed_tools`, `/mcp` output and the `<prefix><server>__<prompt>` slash commands; the raw name is what the server expects in `tools/call` either way.

The server part is load-bearing: two servers may expose the same raw tool name, and the prefix is what keeps them apart.

Without the prefix a shared raw name cannot address both servers, so the second registration is **refused** rather than allowed to take the name over (which would route its calls to the other server). The refusal is logged and recorded by `McpToolRegistry::conflicts()` and surfaced by `/mcp` alongside the server list; turn the prefix back on to reach that tool. `McpToolInfo::prefixed_name()` always gives the unambiguous spelling.

### Hooked Into CLI

- **`StartupOrchestrator.phase_init_mcp()`** (`crates/cli/src/orchestrator.rs:1353`) — Loads `.mcp.json`, starts servers, discovers tools/resources, sets `mcp_ready`.
- **`/mcp` commands** — Server listing and status via config extension commands, with discovered prompts appended under each server (see *Prompts*).
- **`/mcp__<server>__<prompt>`** — one slash command per discovered prompt; arguments are positional or `key=value`, and the resolved messages are printed as text (see *Prompts*).

### Architecture Patterns

- **MCP client over stdio** — Subprocesses communicate via JSON-RPC 2.0 on stdin/stdout.
- **Fail-open initialization** — Server spawn + initialize errors are logged but don't block startup.
- **Reconnection** — Up to 3 reconnection attempts per server with fresh channel + initialize handshake.
- **Health monitoring** — Background `tokio::spawn` task checks all servers every 30s via `health_check()`.
- **Concurrent access** — `Arc<DashMap>` for servers, `Mutex` for tool/resource registries and cache.
- **OAuth2 token management** — Token storage with expiry tracking, near-expiry detection, file persistence, and refresh-token exchange. Every remote request resolves its own `Authorization: Bearer` credential through a token provider, so a token inside the 5-minute window is refreshed before that request. Discovery follows Protected Resource Metadata to the resource's authorization server, then caches that server's endpoints, scopes, grants, PKCE methods and client-auth methods in an issuer-keyed map with an `ETag` and a `Cache-Control`/`Expires`-derived expiry; a fresh cache is reused without a request, a stale one is revalidated, and when discovery fails the conventional paths are used.
- **Result adaptation** — MCP content arrays (text/image/resource) normalized to `ToolResult`.
- **Resource caching** — In-memory TTL cache (default 60s) keyed by `(server_id, resource_uri)`.

## Prompts

MCP prompts are reusable, parameterised message templates a server offers for
a person to invoke. They are separate from tools: the client does not choose
one, the person does.

### Protocol

- **`prompts/list`** (no parameters) returns `{"prompts": [...]}`. Each entry
  has `name`, optional `description`, and an optional `arguments` array whose
  items carry `name`, optional `description`, and `required`. The client parses
  it into `McpPromptInfo` / `McpPromptArgument`.
- **`prompts/get`** takes `{"name": "...", "arguments": {...}}`; `arguments` is
  always a JSON object (empty when there are none) and every value is a string.
  The response is `{"description": "...", "messages": [{"role": "...",
  "content": {...}}]}` and is parsed into `McpPromptResult` /
  `McpPromptMessage`. Text is rendered as-is; image and audio blocks become
  `[image: <mime>]` / `[audio: <mime>]`, embedded resources show their URI and
  text, and an unknown block type renders as `[unsupported content]` rather
  than failing the response.

`McpManager::discover_all` calls `prompts/list` after tools and resources, but
only for a server that advertised the `prompts` capability in its `initialize`
result: a server is not required to answer a method it never declared, and one
that silently ignores an unknown method would otherwise leave the request
waiting. A server without the capability is treated as offering no prompts.
`notifications/prompts/list_changed` triggers a fresh list for that server.
Prompts are aggregated in `McpPromptRegistry` and tagged with their server;
`prompts_for_server` and `prompt_info` expose the cached list, and
`McpManager::get_prompt(server, name, arguments)` performs a live
`prompts/get`.

### Slash commands

Every discovered prompt becomes a slash command:

```
/mcp__<server>__<prompt> [positional ... | key=value ...]
```

Characters in `<server>` and `<prompt>` that are not ASCII letters, digits, or
`_` are replaced with `_` in the command name. The raw names are kept for the
request, so a prompt named `review/file` is invoked as
`/mcp__my_server__review_file` but asked for as `review/file`.

Arguments can be passed positionally or as `key=value`:

- Positional values fill the prompt's declared arguments in declaration order.
- `key=value` names its argument explicitly; a positional value then skips any
  argument already supplied by name.
- Values are always strings (MCP's schema requires strings). Tokens are split
  on whitespace by the REPL, so a value containing spaces needs to be supplied
  by a server that takes it another way.
- A missing required argument is refused locally with the argument names; extra
  positional arguments and an empty `key=` name are also refused.

`/mcp` lists the prompts under each server, with their command spelling,
description, and declared arguments. Prompt commands are registered when the
manager is attached and refreshed lazily before an `/mcp` or `mcp__…` command
runs, which is how prompts discovered after connection (or announced by
`prompts/list_changed`) become visible. The command registry is append-only, so
a prompt a server stops offering keeps its command; invoking it reports that
server's error.

### Per-server errors

Discovery and invocation are per server. If one server fails `prompts/list`,
its previous prompts are left untouched (or it contributes none on first
discovery), the failure is logged under that server's name, and the other
servers' prompts are still discovered. A failing `prompts/get` is reported as
`failed to get prompt '<name>' from server '<server>': …`; it is not turned
into a global "MCP is broken" error. Naming an unattached server reports
`MCP server '<name>' is not attached`.

The same data is available to the model through the `ListMcpPrompts` and
`GetMcpPrompt` baseline tools, which the CLI replaces with implementations
backed by the live manager.

## Current Status

### What Works

- Full JSON-RPC 2.0 stdio channel with `initialize()` handshake (protocol version 2024-11-05).
- Server lifecycle: spawn, health check (immutable), reconnect (max 3), stop with status tracking.
- Auto-discovery: `tools/list`, `resources/list`, and `prompts/list` called after server spawn, results parsed and registered.
- Tool proxy: `McpToolProxy.call(args)` sends `tools/call` over shared channel.
- Tool registry: `McpToolRegistry` with register, get, unregister, list.
- Resource types: `McpResource`, `McpResourceContent`, registry with MIME filtering.
- Resource caching: TTL-based in-memory cache with get, store, invalidate, clear operations.
- OAuth2 types: `McpOAuthState` with token storage, expiry checking, near-expiry detection (5 min buffer), refresh_token support.
- Token persistence: load/save through `coderex-config`'s `TokenPersistence`, in the one shape both the `McpAuth` tool and `OAuthManager` read: `{"states": [...], "issuer_metadata": {...}}`. The per-server states hold the token and a snapshot of what discovery learned; `issuer_metadata` is the authorization-server cache keyed by normalized issuer. The OS keychain is preferred (service `coderex`, one account per store path); if it is unavailable or a keychain operation fails, the store falls back to `~/.coderex/mcp_tokens.json` with a one-time warning. The fallback file is created atomically from a sibling temp file that is created with mode `0600` and renamed over the target on Unix, so it is never world-readable, including on overwrite. On Windows the temporary file gets a native, protected DACL before the rename: the process token's user SID is granted full access and inheritance is severed, so profile-inherited entries cannot widen the file. The DACL is read back and verified to contain exactly that one allow ACE; if it cannot be applied and verified the save fails closed — the temp file is discarded and no token file is written — instead of renaming a credential into place under an inherited ACL. That Windows path uses `windows-sys` and is compiled but has not been exercised on a Windows host in this tree (the Unix path is the tested one). Setting `CODEREX_DISABLE_KEYCHAIN` forces the private-file path.
- Discovery: `OAuthManager::discover` first fetches RFC 9728 Protected Resource Metadata — from the `resource_metadata` parameter of a `WWW-Authenticate: Bearer` challenge when the caller has one, otherwise from `{origin}/.well-known/oauth-protected-resource{path}` — and uses its first `authorization_servers` entry as the issuer, defaulting to the normalized MCP URL when none is published. This works across origins: the MCP endpoint and the authorization server may be different hosts. It then tries, in order, `{origin}/.well-known/oauth-authorization-server{path}` (RFC 8414 path insertion, the Keycloak/Auth0 shape), `{origin}/.well-known/oauth-authorization-server`, `{path}/.well-known/openid-configuration` and `{origin}/.well-known/openid-configuration`. A document whose `issuer` does not match is rejected and the next candidate is tried; a document that omits `issuer` is bound to the configured issuer only when it came from that issuer's own well-known URLs and is otherwise rejected with a clear reason. HTTPS is required except on loopback. `resource`, `authorization_endpoint`, `token_endpoint`, `scopes_supported`, `registration_endpoint`, `token_endpoint_auth_methods_supported`, `grant_types_supported` and `code_challenge_methods_supported` are recorded in the persisted state and in the issuer cache, with an expiry from `Cache-Control: max-age`/`Expires` (one hour by default) and an `ETag`; the cache is not re-fetched before it expires, and revalidation sends `If-None-Match` and accepts a `304`. `refresh`, `refresh_if_needed` and `McpAuth` use the discovered endpoints; a server that publishes nothing keeps the conventional `{server_url}/oauth/token` path.
- Client registration and authentication: when no client id is configured or stored and the authorization server exposes a `registration_endpoint`, discovery registers this client (RFC 7591) with `client_name`, `redirect_uris: ["http://localhost:8765/callback"]`, `grant_types: ["authorization_code", "refresh_token"]`, `response_types: ["code"]`, `code_challenge_method: "S256"` and a `token_endpoint_auth_method` chosen from what the server advertises; the returned `client_id`/`client_secret` are persisted and later calls reuse them instead of registering again. The code exchange and refresh use `client_secret_basic`, `client_secret_post`, or public-client `none` (PKCE) accordingly.
- Policy validation and resource indicator: discovery refuses to start a flow when `code_challenge_methods_supported` is present but excludes `S256`, and refuses a refresh when `grant_types_supported` is present but excludes `refresh_token`. Every authorize, token and refresh request includes `resource=<MCP server URL>` (RFC 8707), using the `resource` value from Protected Resource Metadata when it advertises one; servers that ignore the parameter are unaffected.
- Token refresh: `OAuthManager::refresh` re-discovers first when the cached metadata is missing or expired, posts `grant_type=refresh_token` (with the `resource` indicator and the selected client authentication) to the discovered token endpoint, and stores the result, including a rotated refresh token. The token provider installed on each remote channel calls `refresh_if_needed` before every request, so a long-lived session refreshes rather than sending an expired token. `OAuthManager::handle_unauthorized` handles a `401 invalid_token` by clearing the access token, re-fetching expired metadata, and then either refreshing exactly once or, when no refresh token exists, invoking the installed `ReauthHandler` exactly once; the HTTP transport calls it from every request path, including the standalone `GET` stream, and retries that request once with the new token. A retry that is rejected again is an error, not another refresh or another sign-in attempt. Another bearer error (for example `insufficient_scope`) does not spend the refresh token and is surfaced as an ordinary error. Concurrent requests that race on the same stale token share the first refresh: once the stored token differs from the one a request carried, the manager returns the current token instead of refreshing again.
- Interactive sign-in: `McpAuth` performs authorization-code + PKCE (S256), with a `localhost:8765` callback listener. When the browser cannot be opened it returns immediately with the authorize URL, verifier and state, and persists the pending flow. A later call completes it from a pasted redirect URL (parsing `code` and `state` and verifying the state) or from `code` + `code_verifier`; both exchange the code with PKCE, the chosen client authentication method and the `resource` indicator.
- Elicitation: `elicitation/create` is implemented end to end as a typed form. `FormSpec::parse` reads every `properties` entry (in declaration order) into `string` (with `enum`, `minLength`, `maxLength`, `pattern`), `integer`, `number` (with `minimum`/`maximum`), and `boolean` fields, honouring `required`, `default`, `title`, and `description`. The TUI presents one form with field navigation (`Tab`/`Shift+Tab` or the arrow keys), `*` on required fields, prefilled defaults, inline validation errors, and submission on `Enter`; it only sends `{"action": "accept", "content": {...}}` once every value passes the declared type and constraints. A wrong type, missing required field, out-of-range number, length violation, or pattern mismatch leaves the form open with an error. The headless/interactive-stdin path accepts a single JSON object for the whole schema and runs the same coercion and validation. A cancelled answer becomes `cancel`, a timed-out one becomes `decline`. A non-interactive (`--print`, piped stdin, no terminal) session answers according to `CODEREX_HEADLESS_ELICITATION` instead of leaving the server waiting: `deny` (the default, and what an unrecognized value falls back to) logs the request and answers `cancel`; `allow` accepts a form when the schema supplies a default for every required field; `defaults` accepts only when every declared field has a default. An accepted answer is built from the declared defaults and validated against the schema first, so a default that breaks its own constraint still cancels. A free-text question (no schema) has no defaults to take; it is answered with `CODEREX_HEADLESS_ELICITATION_ANSWER` only when that variable is set, non-empty, and the policy is `allow`. `deny` and `defaults` still cancel a free-text question even when the variable is set, and `allow` without it (or with an empty value) cancels rather than inventing text. Each decision is logged with the policy. A handler that does not recognise the method returns `-32601`.
- Server-initiated requests: messages from the server are told apart by shape (method+id is a request, method alone a notification, id alone a response). They are answered on every transport — the stdio read loop directly, the HTTP/SSE response stream incrementally, and the standalone `GET` stream — with HTTP answers sent as a separate POST so a server holding a response open cannot deadlock the client. The HTTP transport deduplicates every server message by its shape — `(session_id, request_id)` for a request, `(method, params)` for a notification — and, when the carrying SSE event has an id, by that id as well: a message that arrives again on the other stream (or after a reconnect/`Last-Event-ID` replay) is not acted on twice, and a short recent-answer cache lets a server retry and still receive the same answer. With no handler installed at all the answer remains a JSON-RPC `-32601` refusal rather than silence.
- Legacy HTTP+SSE sessions (`"type": "sse"`): the client opens a long-lived `GET` with `Accept: text/event-stream`, waits for the server's `endpoint` event, resolves the announced POST URL relative to the configured URL, and sends every JSON-RPC message there. Responses and server-initiated requests arrive on the event stream and are correlated by id; auth headers, `Mcp-Session-Id`, `MCP-Protocol-Version`, 401 refresh, and graceful shutdown apply to both directions. A server that does not provide an event stream is reported as `Errored` instead of appearing to be running. `http`/`streamable-http` and `sse` are distinct transports selected by `type`, not aliases.
- Streamable HTTP sessions: the negotiated protocol version is captured from the `initialize` result and sent as `MCP-Protocol-Version` on every later request; the `Mcp-Session-Id` header is remembered and sent on every later POST (requests, notifications, and answers). Responses may be plain JSON or an SSE stream; both are routed through the same incremental parser. After initialize each remote server gets one background `GET` stream (`Accept: text/event-stream`, session id, protocol version, current bearer token) which handles server requests and notifications from the same deduplicated pipeline, keeps `Last-Event-ID` across reconnects — advanced to the id of the last event actually processed, so a reconnect resumes past it rather than past the last one merely read — and reconnects with bounded exponential backoff plus jitter. If the failure or duration cap is reached the stream enters `Degraded` (observable via `McpServer::event_stream_state`) instead of retrying forever. A `404` means the session expired: the client clears it, re-runs `initialize` + `initialized` once, and resumes. A `405` means the server offers no standalone stream: the task stops permanently and quietly, and the server stays usable over POST. Shutdown cancels the stream task, drops the response body to close the connection, and awaits the task; inbound events flow through a bounded queue whose overflow also ends in `Degraded` rather than unbounded memory growth.
- Server notifications: `notifications/tools/list_changed` triggers a fresh `tools/list` and re-registers that server's tools; `notifications/resources/updated` invalidates the affected resource in the cache (or all of that server's cached resources when no `uri` is given); `notifications/prompts/list_changed` re-runs `prompts/list` and replaces that server's prompts. Unknown notifications are still logged.
- Config loading: `.mcp.json` file parsing + settings.json integration, save/load roundtrip.
- Result adapter: `McpResultAdapter` converts MCP content (text, image, resource) to `ToolResult`.
- Tool registration: discovered tools are `McpTool` entries in the shared `ToolRegistry` (`register_tools_into`); there is no separate MCP executor.
- Health monitor: Background task checks all servers every 30s.
- Startup wiring: `StartupOrchestrator` phase loads config, starts servers, discovers tools/resources, sets `mcp_ready`.
- 253 tests: 181 unit tests plus 27 socket-level HTTP/SSE tests and 45 integration tests covering channel, server, proxy, config, OAuth, re-authorization, typed elicitation, prompts, notification handling, manager, result adapter, and tool executor.

### What Is Stubbed/Partial

- **Servers without refresh tokens cannot be kept alive silently.** A server that hands back an access token with no refresh token cannot be refreshed by `OAuthManager`. An expired credential with no refresh token is handed to the installed `ReauthHandler` once, before the request goes out; the same handler is invoked for a `401 invalid_token` no refresh token can recover: an interactive CLI runs the authorization-code/PKCE flow again, while a headless run can only print the authorization URL and instructions and then surface `ReauthRequired`. With no handler installed the request goes out unauthenticated and the server's `401` is surfaced unchanged. Nothing retries forever, and there is no background sign-in.
- **The Windows DACL path is unverified in this tree.** Keychain storage is preferred on every platform; when it is unavailable the fallback token file is created with mode `0600` on Unix. On Windows the temp file's DACL is replaced with a protected, user-only DACL through `SetNamedSecurityInfoW` and then read back with `GetNamedSecurityInfoW` before the rename; if that cannot be applied and verified the save fails closed and no token file is written. The code is compile-gated with `cfg(windows)` and its size arithmetic is unit-tested on every platform, but this tree has no Windows host, so the Win32 calls themselves have not been executed. Protected Resource Metadata fetching during discovery remains best-effort: an unreachable or rejected document falls back to the normalized MCP URL and the conventional paths.
- **Nested/array/`oneOf` elicitation schemas are declined, not rendered.** They are outside MCP's restricted elicitation schema, so the client logs why and returns `decline` instead of asking for them as text. `$ref` and unknown field types are treated the same way. Property order relies on the JSON map preserving declaration order (`serde_json`'s `preserve_order`), which the client enables.
- **A degraded standalone GET stream probes instead of dying.** After the retry cap the stream is `Degraded` (observable via `McpServer::event_stream_state`); a probe re-opens it after `CODEREX_MCP_STREAM_PROBE_SECS` (default 60, `0` disables). POST traffic continues to work while degraded. `Last-Event-ID` now resumes from the id of the last event actually processed, and the dedup cache absorbs a redelivery of either a request or a notification — but a server that ignores the resume point and re-sends a notification with the same method and params within the 5-minute dedup window will have that second copy suppressed.
- **Resource notifications invalidate; they do not eagerly re-read.** `resources/updated` drops the cached entry so the next access fetches fresh content, but no background `resources/read` is issued.

## Gaps for Production

- **MCP tool schemas**: Discovered tools are registered into the same `ToolRegistry` as
  built-ins (`register_tools_into`), so the engine's tool listing carries their descriptors
  and input schemas. They are not copied into a separate system-prompt table — the registry
  listing is the single source.
- **Resource reads**: implemented. `McpManager::read_resource` /
  `read_resource_any` issue `resources/read`, parse `contents[]` (text or base64 blob),
  cache the result, and the CLI's `ReadMcpResource` tool calls them. `resources/updated`
  still invalidates rather than eagerly re-reading.
- **Servers from settings**: `mcp_servers` (also accepted as `mcpServers`) is parsed into
  the same `McpConfig` shape as `.mcp.json`; settings are loaded first and the project
  file can override a same-named server. Only explicit `.mcp.json`/`--mcp-config` was
  loaded before.
- **`coderex mcp add` supports stdio and remote servers**: a positional command
  registers a stdio server, `--url` registers an HTTP (default) or `--transport sse`
  server, and repeated `--header KEY=VALUE` entries are preserved. `mcp list` prints
  the command/args for stdio servers and the transport/URL for remote ones.
- **MCP image content is forwarded.** `McpResultAdapter` fills
  `ToolResult.images`, `McpTool` carries them through `ToolOutput`, and the
  engine serializes them as `image` content blocks beside the text result
  (stream-json frames carry them too). Base64 is normalized and malformed data
  is omitted with a note; images over 5 MiB decoded are skipped. A text
  placeholder (`[image: image/png, N bytes]`) is kept in `output` for text-only
  consumers.

### Stdio server sandbox

A stdio server is a program named by repository-writable configuration, so it
runs under the platform sandbox: the AppContainer token on Windows, a Seatbelt
profile (`sandbox-exec`) on macOS, bubblewrap or Landlock on Linux. Confinement
is applied on every platform, not only Windows — on a platform where the
command would otherwise be a wrapper (`sandbox-exec`, `bwrap`), the wrapper
becomes the program and the server command its argument, so a command that does
not exist is resolved and refused *before* the wrapper is built (otherwise it
would spawn the wrapper fine and only fail inside it, which reads as a protocol
error rather than a start failure).

Configuration, per server:

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "cwd": ".",
      "sandbox": {
        "enabled": true,
        "allow_network": true,
        "allowed_read_paths": [],
        "allowed_write_paths": [],
        "allowed_exec_paths": []
      }
    }
  }
}
```

`enabled` and `allow_network` both default to `true`: MCP servers commonly
exist to call a remote API, and turning the network off by default would break
them. Path access is bounded to the working directory (configured `cwd`, else
the session's), the server's own installation, and the package caches a
launcher reads. On Windows the user's whole profile is not granted (read
grants are recursive, and walking it once per server start is too expensive);
a server that needs another profile location names it in
`allowed_read_paths`. Directories the platform plan already grants
best-effort (system roots and `PATH`) are not repeated as required grants. Extra paths must be absolute and are
validated at load time; a relative path is skipped with a warning rather than
resolved against whatever directory the client happens to be in.

`CODEREX_MCP_SANDBOX=0` (or `false`/`no`) disables sandboxing for every server,
and a single server opts out with `"sandbox": {"enabled": false}`.

The policy is **fail-closed on every platform**, deliberately. An MCP server is
arbitrary code named by repository-writable configuration whose tools the agent
then calls, so it is held to the *gating* branch of the hook engine's
degradation policy: when a policy is in force and the platform cannot provide
confinement — no AppContainer runtime on Windows, no `sandbox-exec` on macOS,
neither bubblewrap nor Landlock on Linux — the server is **refused** rather than
started unconfined. The error names both escape hatches. This is a deliberate
choice, not an oversight: it means MCP is unavailable by default on a Linux host
with neither bubblewrap nor a Landlock-capable kernel, and the fix there is to
install bubblewrap or set `CODEREX_MCP_SANDBOX=0` knowingly.

The channel keeps the sandbox handle for the process lifetime, and `terminate`
kills the whole tree before the direct child.

`CODEREX_HEADLESS_ELICITATION` selects how a session with no terminal answers
`elicitation/create`: `deny` (the default, and the fallback for an unrecognized
value) cancels, `allow` accepts a form whose required fields all have schema
defaults, and `defaults` accepts only when every declared field has a default.
`CODEREX_HEADLESS_ELICITATION_ANSWER` supplies the answer to a free-text
question (one with no schema); it is read only under `allow`, and an unset or
empty value leaves the question cancelled. See the elicitation note above for
the validation rules.

### Sampling

MCP `sampling/createMessage` lets a server ask the *client's* model to
generate text in the middle of a request. It is the server's conversation and
the server's choice of prompt, but the completion is billed to the person
running CodeRex. Sampling is therefore **denied by default**: without an
explicit opt-in, the request is answered with a JSON-RPC `-32601` error whose
message names the switch, rather than being silently ignored or forwarded.

Set `CODEREX_MCP_SAMPLING=allow` to enable it. The value is matched
case-insensitively; any other value (including `deny`) leaves sampling
disabled and logs a warning. When enabled, CodeRex forwards the request to the
session's own API client and model:

- `maxTokens` is honoured only up to a ceiling: **4096** output tokens by
  default. Raise or lower it with `CODEREX_MCP_SAMPLING_MAX_TOKENS` (a positive
  integer; an invalid or zero value falls back to 4096).
- A model named in `modelPreferences.hints` is used only when CodeRex knows it
  (`opus`/`claude-opus-4-6`, `sonnet`/`claude-sonnet-4-6`,
  `haiku`/`claude-haiku-4-5-20251001`). Any other hint is ignored and the
  session's resolved model answers instead.
- Only `text` content blocks are forwarded. A request whose messages are
  empty, whose `maxTokens` is zero, or whose params do not parse is rejected
  with `-32602` (invalid params); an unsupported content type or a provider
  failure is returned as a JSON-RPC error, never a panic.

The handler is installed by the REPL and one-shot runners after the session's
API client exists. A headless run (`--print`, a piped prompt, or no terminal)
keeps the deny default: with the opt-in absent, nothing is installed and
sampling stays refused. With `CODEREX_MCP_SAMPLING=allow` a headless run uses
the same clamped handler, because the environment variable is the explicit
consent. Served sessions (`coderex serve`) install no sampling handler and
therefore keep refusing.

### IDE tools

CodeRex has no IDE integration and no bundled IDE. An IDE's own MCP server can still be configured like any other server (`mcp_servers` / `.mcp.json`); its tools are named `mcp__<server>__<tool>` and follow the ordinary permission rules. There is no fixed `ide` server, no `--ide` flag and no product-specific allowlist: a user who wants `mcp__ide__executeCode` prompt-free adds an explicit `mcp__ide__*` rule like any other.

## Remaining Considerations

- **OAuth authorization flow**: the browser-based authorization-code flow exists in the `McpAuth` tool, with PKCE and a local callback server. A headless run completes through the persisted hand-off — paste the full redirect URL (state is verified) or supply `code` + `code_verifier` — rather than waiting out the callback timeout, which is only entered when a browser was actually opened.
