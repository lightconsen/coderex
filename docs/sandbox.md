# Sandbox — what is actually enforced

This document describes the guarantees the sandbox makes today, per platform, and
draws the line between what the kernel enforces and what is only an in-process
check. The short version: **only macOS confines the filesystem.** Everywhere else
the sandbox is a syscall allowlist, a resource budget, or nothing at all — and it
says so at runtime rather than letting the caller assume otherwise.

## Where it is wired

| Piece | Location |
|---|---|
| Platform backends (Seatbelt / seccomp / Job Objects) | `crates/core/src/sandbox.rs` |
| Per-tool default policy (which paths, which limits) | `crates/tools/src/sandbox_policy.rs` |
| Policy injected into `ToolContext.extra["sandbox_config"]` | `RegistryToolDispatcher` in `crates/tools/src/dispatcher.rs` |
| `Bash` applies the sandbox | `build_sandboxed_command`, `crates/tools/src/builtins/bash_tool.rs:43` |
| `PowerShell` applies the sandbox | `crates/tools/src/builtins/powershell_tool.rs` |
| MCP servers spawn outside it | `JsonRpcChannel::spawn`, `crates/mcp/src/channel.rs:176` |

Sandboxing is on by default (`ExecutionConfig::sandbox_enabled = true`, the
`Default` impl in `crates/tools/src/executor.rs`). The default policy for `Bash`
allows reads and writes under the working directory, executing `/bin`, `/usr/bin`
and `/usr/local/bin`, and no network.

## What the kernel enforces

### macOS — Seatbelt (`sandbox-exec`)

`SandboxHandle::create` writes a profile and the command runs through
`/usr/bin/sandbox-exec -f <profile>`. The profile starts from `(deny default)`, so
everything not named is refused.

| Operation | Rule | Enforced by |
|---|---|---|
| Read file contents | Configured read paths, plus fixed system roots (`/usr`, `/bin`, `/sbin`, `/System`, `/Library`, `/private/etc`, `/private/var/db`, `/private/var/select`, `/dev`) | kernel |
| Read metadata (`stat`) | Anywhere | kernel — deliberate: answers "does this exist" without disclosing contents |
| Read the root directory itself | `/` | kernel — required, the loader reads it before anything else |
| Write | Configured write paths only | kernel |
| Execute | The system shells, plus each configured exec path and its subtree | kernel |
| Network | `(allow network*)` when `allow_network`, `(deny network*)` otherwise | kernel |
| Process fork / interpreters | Allowed | — |
| `mach-lookup`, `ipc-posix-shm`, `sysctl-read` | Allowed unconditionally | — |

Two allowances are wider than the configured paths and are called out so they are
not mistaken for confinement: metadata reads are global, and the root directory
listing is readable. Neither discloses file contents.

**rlimits are not applied on macOS.** `SandboxHandle::prepare` is a no-op there
(`sandbox.rs`), so `max_open_files`, `max_memory_bytes` and `max_cpu_seconds` in a
`SandboxConfig` have no effect on a macOS child. Only the command's own timeout
bounds it.

### Linux — bubblewrap, seccomp + rlimits

Two mechanisms, and which one is in force depends on whether bubblewrap is
installed.

**With `bwrap` on `PATH`** (and not switched off — see below), the command is
wrapped in a mount namespace: the root is bound read-only, `/proc`, `/dev` and
`/tmp` are fresh mounts, only the configured paths are re-bound writable, and the
child starts in the working directory the policy names. `--unshare-net` is added
when `allow_network` is false, which confines the network more strongly than a
syscall filter can. In this mode the seccomp allowlist is **not** installed: the
wrapper would inherit it, and mounting, unsharing and pivoting a root are exactly
the syscalls an allowlist for a shell refuses. `filesystem_isolation()` reports
`Kernel`, and the arguments are built by `bubblewrap_args` — a pure function, so
the policy they encode is compiled and tested on every platform, not only on the
one that runs it.

`CODEREX_BWRAP=0` switches the wrapper off. Anything but an explicit negative
(`0`, `false`, `no`) leaves it on, because a sandbox that disables itself over an
unexpected value is worse than one that says what it wants.

**Without `bwrap`**, `prepare()` installs a `pre_exec` closure that applies
`RLIMIT_NOFILE`, `RLIMIT_AS` and `RLIMIT_CPU`, then installs a seccomp-BPF filter
with a default action of kill and an explicit allowlist of syscalls. The audit
token and the syscall numbers come from the current architecture
(`seccomp_arch`), so an aarch64 host does not get the x86_64 table.
`allow_network` **is** honoured there: the network syscalls are a separate table
concatenated only when the flag is set, and a test asserts the two tables do not
overlap. In this mode `filesystem_isolation()` reports `None` — seccomp filters
syscalls, not paths, so a wrapped process can still read and write anything its
user could — and the sandbox logs a warning saying so. Nothing silently claims
otherwise.

**Unverified at runtime:** the wrapper's flags were written on macOS, where
`bwrap` does not exist. The argument construction is unit-tested here and the
Linux-gated code is reviewed but **cannot be compiled or run on this machine**
(`ring`'s build script needs a cross C compiler, and no Linux toolchain is
installed). If a flag is wrong on a real Linux host, every sandboxed command
fails loudly rather than silently running unconfined — set `CODEREX_BWRAP=0` to
fall back to the syscall filter.

### Windows — Job Objects

Creates a Job Object with per-job user time and memory limits and assigns the
child to it, plus UI restrictions (no desktop, no clipboard, no global atoms).
This limits resources and UI reach; it does not restrict file paths or network.

### Other platforms

`NoOpSandbox`: logs and does not restrict. `filesystem_isolation()` reports
`None`.

## Child-process hardening (all platforms)

Two protections are applied by the spawning code itself, independently of any
platform sandbox.

### Credential stripping

A `Bash` shell, a `PowerShell` shell and an MCP server are all spawned **without
this session's provider credentials**. The variables removed are:

- exact names: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_OAUTH_TOKEN`
- prefixes: `CLAUDE_*` (covers `CLAUDE_CODE_OAUTH_TOKEN` and the
  `CLAUDE_CODE_*` switches), `OTEL_*`

The rule is *strip when the child does not need it*: a tool or MCP server is not
itself a provider client, so it is given the variables it names and nothing else.
An MCP server that genuinely needs one gets it back by naming it in its own
`env` block — an explicit environment entry beats a removal, so the configured
environment always arrives (`crates/mcp/src/channel.rs:196-204`).

Implemented in `crates/core/src/sandbox.rs` (`is_credential_key`,
`credential_scrub_keys`, `credential_scrub_keys_from`) and applied at
`crates/tools/src/builtins/bash_tool.rs:223`,
`crates/tools/src/builtins/powershell_tool.rs:81` and
`crates/mcp/src/channel.rs:199`.

Not scrubbed: anything the child's own configuration names, and any credential
that is not in the lists above (for example `AWS_SECRET_ACCESS_KEY`, which a
shell may legitimately need for `aws`).

The `SandboxConfig` itself needs no scrubbing: it carries paths, rlimits and the
network flag, and no credential of any kind. It is serializable and travels
between processes (for example to sub-agents), so the fields it does carry are
worth keeping that way — adding an environment map to it would make it a
credential carrier.

### `allow_network`, per platform

Honoured on macOS (`(allow network*)` vs `(deny network*)`) and on Linux (the
network syscalls are a separate seccomp table, concatenated only when the flag is
set). Not honoured on Windows: a Job Object does not filter network access, and
nothing else does there either. `NoOpSandbox` honours nothing.

### Refusing to execute out of the working directory (`safeSpawn`)

A checkout can ship a binary named after a system tool (`./sh`) or a `PATH` entry
pointing back at the workspace; either one turns "run the shell" into "run
whatever the repository shipped". Before spawning, the program name is resolved
the way the loader would — `PATH` order, relative entries resolved against the
working directory — and a program whose first match lands inside the working
directory is refused with a `PermissionDenied` error.

Checked: the shell (`sh`), the macOS wrapper (`sandbox-exec`) and the MCP server
command. Implemented in `crates/core/src/sandbox.rs` (`resolve_program`,
`resolves_inside_cwd`).

Limits of this check, stated plainly:

- It covers the program that is *spawned*, not every command the shell goes on to
  run from the command string.
- With no `PATH` set, or when nothing matches, it refuses nothing and lets the
  spawn report its own failure.
- When the working directory is the filesystem root the check is skipped, since
  nothing can be "outside" it.

## In-process checks — not kernel-enforced

These refuse an operation inside the CodeRex process. They are policy, not
confinement: a subprocess spawned by `Bash` is not bound by them, and code that
does not go through the tool layer is not bound by them either.

| Check | Location | Boundary |
|---|---|---|
| `Edit` refuses a path outside the configured read/write paths | `crates/tools/src/builtins/edit_tool.rs:59-76` | in-process |
| `Write` refuses a path outside the configured write paths | `crates/tools/src/builtins/write_tool.rs:51-68` | in-process |
| `Bash` command allowlist and shell-metacharacter rejection | `BashTool::validate_command`, `crates/tools/src/builtins/bash_tool.rs:145` | in-process, only active when an allowlist is configured |
| `Bash` dangerous-command warning (`BashAst`) | `crates/tools/src/builtins/bash_tool.rs:211-216` | advisory — it prepends a warning to the output, it does not block |

## Not enforced, and not claimed

- Linux filesystem confinement **when `bwrap` is absent or switched off** — it is
  enforced when bubblewrap is installed, and `filesystem_isolation()` reports
  which of the two is the case.
- Any filesystem confinement on Windows or unsupported platforms.
- rlimits on macOS.
- Network confinement on Windows.
- cgroup memory control. The reference uses a cgroup v1 memory controller for
  tool and MCP children; this is Linux-only and is **not implemented** here. It
  is deferred rather than written blind: there is no way to exercise it from
  macOS, and a memory limit that is never observed to fire is a claim rather
  than a control.
- Path checks for processes that do not go through the tool layer.

## Verifying the macOS guarantees

The Seatbelt behaviour is covered by tests that actually spawn `sandbox-exec`:

- `sandbox::tests::macos_sandbox_denies_read_outside_allowed_path` — a file
  outside the configured read paths is not readable.
- `sandbox::tests::macos_sandbox_allows_executing_a_tool_from_an_allowed_path` —
  a real binary (`ls`), not a shell builtin, runs from an allowed exec path.
- `crates/core/tests/sandbox_integration.rs` — writes inside/outside the allowed
  paths, and network denial.
- `crates/tools/src/builtins/bash_tool.rs` — the `Bash` tool end to end, including
  credential stripping and the safeSpawn refusal.

Linux behaviour (seccomp allowlist, `allow_network`, rlimits) is covered by
Linux-gated tests that cannot run on macOS.
