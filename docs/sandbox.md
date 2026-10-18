# Sandbox — what is actually enforced

This document describes the guarantees the sandbox makes today, per platform, and
draws the line between what the kernel enforces and what is only an in-process
check. The short version: **macOS, Windows (AppContainer), and
Linux-with-bubblewrap confine the filesystem; Linux without bubblewrap does
not.** Where they do not, the sandbox is a syscall allowlist, a resource
budget, or nothing at all — and it says so at runtime rather than letting the
caller assume otherwise. On Windows, confinement requires the AppContainer
APIs; when they are missing, creating a confined sandbox fails rather than
silently degrading.

## Where it is wired

| Piece | Location |
|---|---|
| Platform backends (Seatbelt / seccomp / Job Objects) | `crates/core/src/sandbox.rs` |
| Windows AppContainer, ACL policy, and raw `STARTUPINFOEX` spawn | `crates/core/src/sandbox_windows.rs` |
| Per-tool default policy (which paths, which limits) | `crates/tools/src/sandbox_policy.rs` |
| Policy injected into `ToolContext.extra["sandbox_config"]` | `RegistryToolDispatcher` in `crates/tools/src/dispatcher.rs` |
| **Shell wrapping lives in one place** — both shell tools use it | `sandboxed_shell` / `finish_spawn`, `crates/tools/src/sandbox_shell.rs` |
| `Bash` applies the sandbox | `crates/tools/src/builtins/bash_tool.rs` |
| `PowerShell` applies the sandbox too | `crates/tools/src/builtins/powershell_tool.rs` |
| **Command hooks apply it** (policy, wrapper, opt-outs) | `crates/hooks/src/sandbox.rs`, called from `execute_hook` in `crates/hooks/src/engine.rs` |
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
with an explicit allowlist of syscalls and a default action of **`EPERM`**. The
default used to be `KILL`, which turned every syscall nobody had listed into
`SIGSYS` — and, because the filter read `seccomp_data.arch` and `.nr` at each
other's offsets, it never matched the architecture and killed every process at
its first syscall. A sandbox should refuse a call, not murder the process it was
asked to confine; network syscalls are still absent from the allowlist when the
network is denied, so they fail with `EPERM` either way. The audit
token and the syscall numbers come from the current architecture
(`seccomp_arch`), so an aarch64 host does not get the x86_64 table.
`allow_network` **is** honoured there: the network syscalls are a separate table
concatenated only when the flag is set, and a test asserts the two tables do not
overlap. In this mode `filesystem_isolation()` reports `None` — seccomp filters
syscalls, not paths, so a wrapped process can still read and write anything its
user could — and the sandbox logs a warning saying so. Nothing silently claims
otherwise.

**Verified on Linux, in a container.** The whole workspace is compiled and its
core and tools suites run on a Linux kernel (Debian, `bwrap` 0.12) in both modes
— wrapper on, and `CODEREX_BWRAP=0` for the syscall path. Three tests exist so it
stays that way: `the_bubblewrap_arguments_confine_a_real_process` runs `bwrap`
with the arguments this module produces and asserts the root is read-only, the
configured path writable, the child starts where it was told to, and the network
namespace has no interface but loopback; `sandboxed_echo_works_on_linux` and
`sandboxed_echo_works_without_bubblewrap` drive the Bash tool's own spawn path
through each mode.

That verification is what found the defects this file used to describe as
"reviewed only": `coderex-core` did not compile on Linux at all (`libc` was
declared in the workspace and never listed in the crate), `sandboxed_program`
was gated to macOS so the Linux caller could not see it, the seccomp filter read
the architecture and syscall numbers from each other's offsets, and the shell
fallback ran `sh sh -c …`. None of that was visible from macOS, which is the
point: a platform `cfg` that is never compiled on its platform is a comment, not
code.

### Windows — AppContainer + Job Objects

Two layers, both applied to a shell child:

1. **AppContainer** — the real security boundary. A profile with a stable name
   (`CodeRex.Sandbox`, or `CodeRex.Sandbox.Network` when the policy allows the
   network) gives the child a container SID instead of the user's SID. The SID
   is granted read/execute on the configured read and execute paths and
   read/write on the configured write paths, the working directories and the
   temp directory. No capability is placed in the token unless
   `allow_network` is set, so without the `internetClient` capability the
   container has **no network access**.
2. **Job Object** — CPU time and memory limits, plus UI restrictions (no
   desktop, no clipboard, no global atoms).

#### Granting paths

ACLs are merged into each path's existing DACL with `SetEntriesInAclW` and
written with `SetNamedSecurityInfoW`; the existing ACEs are left in place. The
container SID gets:

| Path | Access |
|---|---|
| Configured read paths, execute paths, and every `PATH` directory | read + execute |
| System roots (`%SystemRoot%`, `System32`, `SysWOW64`, `Program Files*`, WindowsApps) | read + execute, best-effort |
| Configured write paths, additional working dirs | read + write |
| `%TEMP%` | read + write, not recursive |
| Ancestors of every granted path | traverse only, not inheritable |

Ancestor traverse is required because an AppContainer token has no
`SeChangeNotifyPrivilege` and therefore cannot bypass traverse checking.
Traverse is not read or list access. It is deliberately not inheritable, so
granting it on `C:\Users\me` does not widen access to everything below.

Directories get inheritable ACEs for new files. Pre-existing files do not
inherit retroactively, so each recursively granted root is walked once per
process to apply the ACE to the tree already there (symlinks are skipped,
following a link could grant a path the policy never named; the walk stops
after 200,000 entries and logs that descendants now rely on inheritance). The
walk and each profile SID are stable across commands, and grants are cached per
profile, so the cost is paid once per profile per process, not on every shell
invocation.

Changing a DACL requires `WRITE_DAC` on the object. The user normally has it
for their own workspace and `%TEMP%`; system roots are marked best-effort
because Windows system directories normally already grant
`ALL APPLICATION PACKAGES`, and a user cannot rewrite their ACLs. A failure is
logged and the path simply stays unreachable to the container (fail closed).

#### Creating the process

The documented mechanism is `CreateProcessW` with `STARTUPINFOEX` carrying
`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`; that primitive lives in
`crates/core/src/sandbox_windows.rs`. Stable Rust cannot pass a process
attribute list through `std::process::Command` (`spawn_with_attributes` is
unstable, and tokio's equivalent needs `tokio_unstable`), so the shell tools use
an equivalent post-spawn path:

1. `prepare` sets `CREATE_SUSPENDED`.
2. `post_spawn_raw` creates an AppContainer token with the
   `CreateAppContainerToken` export from `kernelbase.dll`, replaces the
   suspended child's primary token through
   `NtSetInformationProcess(ProcessAccessToken)`, assigns the Job Object, and
   resumes with `NtResumeProcess`.
3. Any failure terminates the suspended child and returns an error. A command
   is never reported as sandboxed after running outside the container.

`CreateAppContainerToken` is not in the public SDK. It has been stable since
Windows 8 and is the call Chromium used for the same purpose, but it is
undocumented, so availability is checked at runtime and the sandbox refuses to
start if it is missing.

#### Windows limitations, stated plainly

- Confinement requires AppContainer support. When the profile or token APIs are
  absent, `SandboxHandle::create` returns `IsolationUnavailable` and shell
  tools fail rather than run unconfined. `filesystem_isolation()` reports
  `None` in that case.
- The ACL grant only helps where the process can change the DACL. A workspace
  owned by another user may be unreadable to the container even though it is in
  the policy.
- Command hooks do **not** get AppContainer confinement: the hook engine owns
  its spawn and has no post-spawn attach step, so a handle given to it uses
  `create_limits_only` (Job Object only). Gating hooks fail closed instead of
  claiming confinement. See `docs/hooks.md`.
- The MCP server and the WASM compiler helper still spawn through their own
  paths; only the Job Object applies to them.

**Verification status.** The pure policy functions (ACL plan, capability set,
command-line quoting) are compiled and unit-tested on macOS. The Win32 code and
its integration in `sandbox.rs` were type-checked for
`x86_64-pc-windows-gnu` with `cargo check` in an isolated scratch crate — the
workspace's own cross-check stops in `ring`'s build script because no mingw C
compiler is installed. The code has **not been linked or run on Windows**. Two
Windows-gated tests are ready for a Windows CI host: one exercises the
documented `STARTUPINFOEX` spawn, the other the suspended-token attach used by
the shell tools; both check that a granted directory is writable and an
ungranted sibling is not. Until they run, the Win32 path is reviewed and
type-checked, not verified.

### Other platforms

`NoOpSandbox`: logs and does not restrict. `filesystem_isolation()` reports
`None`.

## Command hooks

`command` hooks run through the same `SandboxHandle` as the shell tools;
`crates/hooks/src/sandbox.rs` builds the policy and wraps the spawn. The
policy is applied **in addition to** the hook's `RLIMIT_*`/Job Object limits,
which remain installed before exec.

Default policy for a hook:

- **Read:** the workspace (the hook's `cwd`, else the session's working
  directory), temp, the user's home and `XDG_CONFIG_HOME`, system roots, and
  absolute `PATH` directories (plus their prefix for `bin`/`sbin` entries, so a
  conda/Homebrew interpreter can read its standard library).
- **Write:** the workspace and temp only.
- **Execute:** system shell/tool directories, Homebrew, and the `PATH`
  directories above.
- **Network:** denied.

A hook may extend these sets, permit the network, or opt out with
`sandbox: false` / `sandbox.enabled: false`; `CODEREX_HOOK_SANDBOX=0` opts out
process-wide. Additional paths are absolute or the load is refused; unknown
sandbox keys are load errors. See `docs/hooks.md` for the settings shape.

Enforcement matches the platform table above. On macOS the hook is wrapped in
`sandbox-exec`; on Linux with `bwrap` it is wrapped in bubblewrap. On Windows
the hook engine creates its own child and has no AppContainer post-spawn attach
step, so hooks are not confined there even though shell tools are. Where there
is no hook filesystem isolation — Windows, or Linux without `bwrap` — a gating
hook (`PreToolUse`, `UserPromptSubmit`) fails closed with
`HookError::SandboxUnavailable` rather than running unconfined, and a
non-gating hook logs a warning and runs with resource limits only. An explicit
opt-out is not a failure.

The wrapper is transparent to the hook protocols: stdin/stdout/stderr are
inherited, so both the one-way contract and `stream: true` line protocol work
unchanged.

## Child-process hardening (all platforms)

Two protections are applied by the spawning code itself, independently of any
platform sandbox.

### Git configuration

A repository can carry configuration that makes `git` **execute programs**:
`core.fsmonitor` fires a hook on `git status`, `core.attributesFile` can name
filters that run on diff, `init.templateDir` can land hooks in a clone,
`core.alternateRefsCommand` and the `gpg.*.program` keys are commands by
definition, and `protocol.*.allow` can re-enable transports that execute. None of
it appears in a command line.

Every `git` process CodeRex spawns on its own behalf therefore gets a fixed
config table injected through `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` /
`GIT_CONFIG_VALUE_<n>` — the reference's mechanism, key for key
(`crates/core/src/git_hardening.rs`). Injecting through the environment rather
than rewriting argv is what makes it apply to anything the child spawns as well.
`protocol.file.allow=user` is kept as the one deliberate exception: local paths
are needed for submodules.

A test builds a repository whose `core.fsmonitor` writes a marker and runs
`git status` twice: unhardened, the hook runs (so the hazard is real); through
`git_command()`, it does not. That control is what makes the test mean something.

**Not covered:** the user's own `git` inside the Bash tool is governed by the
sandbox, not by this table. The reference injects it into the shell it runs; this
port applies it to CodeRex's internal invocations.

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

Honoured on macOS (`(allow network*)` vs `(deny network*)`), on Linux (the
network syscalls are a separate seccomp table, concatenated only when the flag is
set), and on Windows (the AppContainer token gets the `internetClient`
capability only when the flag is set; a container without it cannot open
outbound connections). `NoOpSandbox` honours nothing.

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
- Filesystem confinement on Windows when the AppContainer APIs are unavailable —
  creation fails and `filesystem_isolation()` reports `None`; it does not fall
  back to limits-only.
- Filesystem or network confinement on unsupported platforms.
- rlimits on macOS.
- cgroup memory control. The reference uses a cgroup v1 memory controller for
  tool and MCP children; this is Linux-only and is **not implemented** here. It
  is deferred rather than written blind: there is no way to exercise it from
  macOS, and a memory limit that is never observed to fire is a claim rather
  than a control.
- Path checks for processes that do not go through the tool layer.
- Filesystem or network confinement for a `command` hook on Windows, or on
  Linux without `bwrap`. On Windows the hook engine has no AppContainer
  post-spawn attach step; the hook still gets the Job Object's resource limits.
  There, a gating hook is denied rather than run unconfined; a non-gating one
  runs with a warning. `CODEREX_HOOK_SANDBOX=0` and the per-hook
  `sandbox.enabled: false` opt out deliberately.

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
- `crates/hooks/src/sandbox.rs` — command hooks end to end: a normal hook runs
  and writes inside its workspace, a write under `$HOME` is refused, and
  `unavailable_sandbox_fails_closed_on_a_gating_event` pins the deny path. The
  macOS tests skip themselves when `/usr/bin/sandbox-exec` is absent; the
  Linux write test skips unless `bwrap` is in force.

Linux hooks (mount-namespace write denial, and fail-closed without `bwrap`) are
covered by Linux-gated tests that cannot run on macOS.

Linux behaviour (seccomp allowlist, `allow_network`, rlimits) is covered by
Linux-gated tests that cannot run on macOS.

Windows behaviour is covered by Windows-gated tests, none of which has run yet:
`sandbox_windows::tests::spawn_in_container_confines_a_real_process` (the
documented `STARTUPINFOEX` spawn) and
`sandbox::tests::windows_sandbox_confines_a_suspended_child` (the token-attach
path the shell tools use). Both write inside a granted directory and assert an
ungranted sibling stays unwritable, and both assert the fail-closed path when
the AppContainer APIs are missing. The pure ACL/capability/quoting policy is
tested on macOS.
