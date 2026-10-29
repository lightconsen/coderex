# Teleport: moving a session between machines

`/teleport` moves a conversation. It has two halves:

- **Directory switching** — `/teleport <directory>` saves the session into
  `<directory>/.coderex/teleport_state.json`, changes the working directory, and
  loads whatever session that directory holds. This works offline, with no
  service involved.
- **Sharing** — `/teleport --share` produces a token, and `/teleport --join
  <token>` restores the session behind it. On its own the token refers to a file
  under `~/.coderex/teleport/shared`, which only works on the same machine.
  Point both machines at a rendezvous service and the token works across them.

## The rendezvous service

```bash
# On a host both machines can reach:
coderex teleport-service --port 8787            # keeps shares for a week
coderex teleport-service --port 8787 --ttl-secs 3600

# On each machine:
export CODEREX_TELEPORT_URL=http://teleport.internal:8787
```

Then in one session `/teleport --share` prints a token, and on the other
machine `/teleport --join <token>` restores the conversation (messages, usage,
cost and the original working directory) and switches to it.

`/teleport --list` and `/teleport --revoke <token>` act on the service too. The
service stores the snapshot under an unguessable token until it is revoked or
its time to live runs out (one week by default), and keeps no history.

### HTTP surface

| Request | Effect |
|---|---|
| `POST /teleport/shares` with `{"snapshot": {...}}` | `201 {"token": "..."}` |
| `GET /teleport/shares` | `{"shares": [{"token","session_id","created_at"}]}` |
| `GET /teleport/shares/<token>` | the snapshot, or `404` when unknown or expired |
| `DELETE /teleport/shares/<token>` | `204`, or `404` when it is already gone |

### Security

The token is a capability: anyone who has it can read the session, which
contains the conversation. The service speaks plain HTTP and has no accounts of
its own — run it behind the deployment's TLS and authentication (a reverse
proxy on a private network), and revoke a share when the other side has joined.
`/teleport` never sends the provider credential; the snapshot holds messages,
usage, cost, the title, the working directory and a revision hash.

## What a join restores

The snapshot is the portable part of a session: messages, token usage, cost,
title, working directory and the revision hash the delta sync uses. It is not
the whole session state — pending permissions, background tasks and the
provider client stay with the process that owns them, which is why a joined
session starts a fresh turn rather than resuming an in-flight one.

## Tests

- `crates/server/src/teleport.rs` — the store: share/join/list/revoke, time to
  live, a snapshot without a session id is refused, path parsing.
- `crates/bridge/tests/teleport_rendezvous.rs` — the client against the real
  service on a loopback port: share on one manager, join on a second one that
  shares nothing but the URL; list with its session id; revoke ends the share;
  an unreachable service is a service error, not a panic.
