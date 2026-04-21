# Server Protocol — WebSocket JSON Protocol

CodeRex daemon server communicates over WebSocket using a structured JSON protocol.

## Connection

1. Connect to `ws://host:port` (or `wss://` when TLS is enabled).
2. First message must be authentication: `{"token": "your-api-key"}`.
3. Server responds with `{"session_id": "sess_N"}` or `{"error": "..."}`.
4. After authentication, server sends an `event: "session_ready"` message.

## Client → Server Requests

Every request has an `id` (string) for correlation and a `type` field:

### Query
```json
{"id": "req_1", "type": "query", "content": "Hello, how are you?"}
```

### Interrupt
```json
{"id": "req_2", "type": "interrupt"}
```

### Ping
```json
{"id": "req_3", "type": "ping"}
```

## Server → Client Responses

Every response echoes the request `id` and includes a `type` field:

### Query OK (full response)
```json
{"id": "req_1", "type": "query_ok", "content": "I'm doing well, thanks!"}
```

### Query Done (after interrupt)
```json
{"id": "req_2", "type": "query_done"}
```

### Pong
```json
{"id": "req_3", "type": "pong"}
```

### Error
```json
{"id": "req_X", "type": "error", "code": "parse_error", "message": "invalid JSON request: ..."}
```

### Rate Limited (event, no id)
```json
{"event": "rate_limited"}
```

## Server → Client Events

Events are unsolicited messages (not correlated to a request):

### Session Ready
```json
{"event": "session_ready", "session_id": "sess_1"}
```

### Session Closed
```json
{"event": "session_closed", "reason": "idle timeout"}
```

### Idle Warning
```json
{"event": "idle_warning", "message": "Session will expire in 60s"}
```

## Error Codes

| Code | Description |
|------|-------------|
| `parse_error` | Invalid JSON or missing required fields |
| `rate_limited` | Too many messages per minute |
| `auth_required` | First message was not authentication |
| `auth_timeout` | Authentication message timed out |
| `invalid_token` | API key not recognized |
| `max_sessions` | Server at capacity |

## Health Endpoint

If `health_port` is configured, an HTTP server runs on that port:

```
GET /health
```

Response:
```json
{"status": "ok", "version": "0.1.0", "sessions": 3}
```

## TypeScript Types

```typescript
interface WsRequest {
  id: string;
  type: "query" | "interrupt" | "ping";
  content?: string;
}

interface WsResponse {
  id: string;
  type: "query_ok" | "query_done" | "pong" | "error" | "stream_chunk";
  content?: string;
  chunk?: string;
  code?: string;
  message?: string;
}

interface WsEvent {
  event: "session_ready" | "session_closed" | "idle_warning" | "rate_limited";
  session_id?: string;
  reason?: string;
  message?: string;
}
```

## Example Client (Python)

```python
import asyncio
import json
import websockets

async def main():
    async with websockets.connect("ws://localhost:8080") as ws:
        # Authenticate
        await ws.send(json.dumps({"token": "my-api-key"}))
        resp = json.loads(await ws.recv())
        print(f"Session: {resp}")

        # Wait for session_ready
        ready = json.loads(await ws.recv())
        print(f"Ready: {ready}")

        # Send a query
        await ws.send(json.dumps({
            "id": "q1",
            "type": "query",
            "content": "Hello!"
        }))
        resp = json.loads(await ws.recv())
        print(f"Response: {resp}")

asyncio.run(main())
```
