# Sandbox — what is actually enforced

This document describes the guarantees the sandbox makes today, per platform, and
draws the line between what the kernel enforces and what is only an in-process
check. The short version: **macOS, Windows (AppContainer),
Linux-with-bubblewrap and Linux-with-Landlock confine the filesystem; Linux with
neither bubblewrap nor Landlock does not.** Where they do not, the sandbox is a
syscall allowlist, a resource budget, or nothing at all — and it says so at
runtime rather than letting the caller assume otherwise. On Windows, confinement
requires the AppContainer APIs; when they are missing, creating a confined
sandbox fails rather than silently degrading.

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
| MCP servers spawn outside it | `JsonRpcChannel::spawn`, `crates/mcp/src/channel.rs:347` |

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

**Memory is best-effort on macOS; the other rlimits are not applied.**
`SandboxHandle::prepare` installs `RLIMIT_DATA` in `pre_exec` when
`max_memory_bytes` is set, and the hook path does the same. It is attempted, not
guaranteed: macOS rejects `RLIMIT_AS`, does not enforce `RLIMIT_RSS`, and on
current Darwin (macOS 15 was tested) `setrlimit(RLIMIT_DATA, …)` also returns
`EINVAL` for any useful finite value. The rejection is ignored rather than
failing the spawn. Even where the kernel accepts it, `RLIMIT_DATA` bounds the
data segment (`brk`), **not** `mmap` allocations, so it is not an address-space
cap and an allocator that maps memory can exceed it. `max_open_files` and
`max_cpu_seconds` have no effect on a macOS child; only the command's own
timeout bounds its lifetime. The test
`sandbox::tests::darwin_memory_limit_is_best_effort` records whether the running
kernel accepts the limit, and the hooks test
`macos_memory_ceiling_constrains_a_memory_hogging_hook` skips enforcement with a
clear message when it does not.

### Linux — bubblewrap, Landlock, seccomp + rlimits

Which filesystem backend is in force depends on whether bubblewrap is installed;
resource limits and the syscall filter are layered on top.

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

**Without `bwrap` — Landlock first.** When bubblewrap is absent, or switched off
with `CODEREX_BWRAP=0`, `prepare()` uses the Landlock LSM as the filesystem
fallback. It is in force (`landlock_in_force()`) when the kernel reports ABI 1 or
newer and `CODEREX_LANDLOCK` is not `0`/`false`/`no`. The ABI is read with
`landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`; the ruleset
is built in the parent, its descriptor survives `fork`, and `pre_exec` calls
`landlock_restrict_self` after the rlimits and before `execve`.

The ruleset is path-beneath, not a mount namespace:

| Access | Paths |
|---|---|
| Read (`READ_FILE`, `READ_DIR`) | Fixed system roots (`/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/etc`, `/proc`, `/dev`, `/sys`, `/run`), configured read paths, configured write paths, additional working dirs, `/dev/null` |
| Write (`WRITE_FILE`, `MAKE_*`, `REMOVE_*`, `REFER`, `TRUNCATE`) | Configured write paths, additional working dirs, `/dev/null` |
| Execute (`EXECUTE`) | `/bin`, `/sbin`, `/usr/bin`, `/usr/sbin`, `/usr/local/bin`, the loader/library directories (`/lib`, `/lib64`, `/usr/lib`, `/usr/lib64`), configured exec paths |
| Network (`BIND_TCP`, `CONNECT_TCP`) | Handled and denied when `allow_network` is false **and the ABI is 4 or newer** |

The library directories are in the exec set because the kernel opens a dynamic
program's ELF interpreter (for example `/lib64/ld-linux-x86-64.so.2`) with
`MAY_EXEC`; without `EXECUTE` on the loader path every dynamically linked
command fails with `EACCES`. `REFER` is handled from ABI 2 and `TRUNCATE` from
ABI 3, so the access mask follows the kernel. Directory-only rights are never
requested on a regular file —
the kernel rejects that with `EINVAL`, and `/dev/null` is the case every shell
hits. Below ABI 4 the network is still denied by the seccomp table installed
alongside Landlock, not by Landlock itself. Landlock does not hide path
existence: a denied path can still be `stat`-ed; its contents and writes are
what the kernel refuses.

`prepare()` also installs a `pre_exec` closure that applies `RLIMIT_NOFILE`,
`RLIMIT_AS` and `RLIMIT_CPU`, then a seccomp-BPF filter with an explicit
allowlist of syscalls and a default action of **`EPERM`**. Unlike the `bwrap`
path, the syscall filter stays installed under Landlock — the process does not
need mount or namespace syscalls, so the list is usable. The default used to be
`KILL`, which turned every syscall nobody had listed into `SIGSYS`, and the
filter read `seccomp_data.arch` and `.nr` at each other's offsets, so it never
matched the architecture and killed every process at its first syscall. A
sandbox should refuse a call, not murder the process it was asked to confine;
network syscalls are absent from the allowlist when the network is denied, so
they fail with `EPERM` either way. The audit token and the syscall numbers come
from the current architecture (`seccomp_arch`), so an aarch64 host does not get
the x86_64 table. `allow_network` **is** honoured there: the network syscalls are
a separate table concatenated only when the flag is set, and a test asserts the
two tables do not overlap.

When both backends are unavailable — no `bwrap`, `CODEREX_LANDLOCK=0` set or a
kernel without Landlock — `filesystem_isolation()` reports `None` and a gating
hook is denied rather than run unconfined. Nothing silently claims otherwise.

The shell tools still run in that state, with seccomp and rlimits only, and log
a warning saying so. Set `CODEREX_REQUIRE_SANDBOX=1` to make that a refusal
instead: `SandboxHandle::create` then fails with `IsolationUnavailable` and the
command is not started (`crates/cli/tests/e2e_sandbox_binary.rs` proves both the
refusal and the unset control). The switch only changes the case where the
backend is missing — macOS always reports kernel isolation, and Windows already
fails closed when the AppContainer runtime is absent.

**cgroup v2 memory limits.** When `max_memory_bytes` is configured, Linux also
creates a `coderex-<pid>-<id>` cgroup under the process's own cgroup (or
`$CODEREX_CGROUP_ROOT`, or `/sys/fs/cgroup`), sets `memory.max`, `pids.max`
(default 256) and `memory.oom.group=1`, and attaches the child by writing its pid
to `cgroup.procs` immediately after spawn. This is in addition to `RLIMIT_AS`:
`RLIMIT_AS` caps one process's address space, while the cgroup caps the whole
process tree the child forks. Groups are removed on drop.

Creation is deliberately fallible. Most unprivileged sessions cannot create
groups in a systemd-managed tree, containers often mount `/sys/fs/cgroup`
read-only, and a parent that has not delegated `memory`/`pids` through
`cgroup.subtree_control` rejects the child's limits. In every one of those cases
the caller logs a warning naming the failed path and continues with `RLIMIT_AS`
only — the command is never counted as cgroup-limited when it was not.
`CODEREX_CGROUP_ROOT` selects a writable delegated tree for containers and
tests. The helper is reusable (`coderex_core::sandbox::CgroupMemoryLimit`):
create, spawn, `attach(pid)`, keep alive until exit.

**Verified on Linux, in containers.** Two runs, because the backends need
different environments.

- A Debian container with `bwrap` 0.12 exercises the wrapper path.
  `the_bubblewrap_arguments_confine_a_real_process` runs `bwrap` with the
  arguments this module produces and asserts the root is read-only, the
  configured path writable, the child starts where it was told to, and the
  network namespace has no interface but loopback;
  `sandboxed_echo_works_on_linux` drives the Bash tool's own spawn path.
- A `rust:1.94` container without `bwrap`, run with
  `--security-opt seccomp=unconfined`, exercises the Landlock fallback. The
  full `sandbox::` suite (37 tests) passes, including
  `landlock_fallback_confines_a_real_process` (a real `sh` reads and writes
  inside the policy and is denied outside it),
  `the_seccomp_allowlist_lets_a_real_process_run`, and the pure plan/ABI tests.
  The hooks sandbox suite (13 tests) and the shell-tool suite (3 tests) pass in
  the same container, including
  `linux_hook_landlock_denies_a_write_outside_the_allowed_paths`.
  `CODEREX_BWRAP=0` is not needed there because the image has no `bwrap`.

The first Landlock run failed at `execve` with `EACCES` for every dynamically
linked program: the kernel opens the ELF interpreter with `MAY_EXEC`, and the
exec allowlist named only `/bin` and `/usr/bin`. The loader directories are now
in the exec set. That defect was invisible in planning tests and only a real
process could show it.

The cgroup tests (`cgroup_memory_limit_attaches_a_real_child` and
`cgroup_memory_limit_stops_a_memory_hog`) **skipped** in the standard container
with an explicit message: Docker mounts `/sys/fs/cgroup` read-only, so there is
no writable delegated tree. They run where one exists — a host with delegation,
or a container started with a writable cgroup mount and
`CODEREX_CGROUP_ROOT` pointing at a delegated subtree. The creation/attach
failure path is what the standard container exercised, and it fell back to
`RLIMIT_AS` with a warning rather than reporting a cgroup limit.

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
   read/write on the configured write paths and the working directories. No
   capability is placed in the token unless
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
| A per-sandbox private temp directory | read + write |
| Ancestors of every granted path | traverse only, not inheritable |

The user's shared `%TEMP%` is deliberately **not** granted. Lowering its
integrity label (see below) to make it writable would let every Low-integrity
process on the machine write there. Each sandbox instead creates
`%LOCALAPPDATA%\coderex\sandbox-tmp\<pid>-<n>`, grants it like any other write
path, and exports it to the child as `TEMP`/`TMP`/`TMPDIR`.


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
for their own workspace; system roots are marked best-effort because Windows
system directories normally already grant `ALL APPLICATION PACKAGES`, and a
user cannot rewrite their ACLs.

A Modify ACE is not enough on its own. An AppContainer token runs at **Low
integrity**, and the default `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP` policy
refuses writes to anything labelled Medium or higher regardless of the DACL.
Every write path is therefore also given a Low mandatory label
(`SetFileSecurityW(..., LABEL_SECURITY_INFORMATION, ...)` with SDDL
`S:(ML;;NW;;;LW)`); directories mark the label inheritable and the recursive
walk applies it to files that already exist. A failed label on a policy write
path refuses the sandbox (`IsolationUnavailable`); ancestor and system grants
stay best-effort. This is the defect the Windows runtime job reported as
"the granted workspace was not writable".

Verbatim paths (`\\?\C:\...`, which `canonicalize` returns on Windows) are
folded back before granting, so one path does not get two ACEs, one of them on
a path Win32 does not treat as the same object.

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
- Command hooks **do** get AppContainer confinement: the hook engine calls
  `finish_windows_spawn` after its own spawn, which attaches the token,
  assigns the same Job Object and resumes the child. `max_file_size_mb` has no
  Windows equivalent and is warned about. See `docs/hooks.md`.
- Stdio MCP servers are confined by the same backend when `sandbox.enabled`
  (the default) is on; see `docs/mcp.md`.
- The WASM compiler helper runs under the same handle (AppContainer plus a
  kill-on-close Job Object) instead of a private Job Object.

The Job Object is **anonymous and kill-on-close** (`ProcessTree`). Dropping
the handle terminates every process still in the job, including descendants; a
`taskkill /T` subprocess is no longer used anywhere for tree cleanup. Callers
must keep the handle alive for the child's lifetime.

**Verification status.** The pure policy functions (ACL plan, capability set,
command-line quoting) are compiled and unit-tested on macOS. With mingw-w64
installed, the Windows-relevant crates type-check against a Windows ABI on this
host:

```text
cargo check --workspace --target x86_64-pc-windows-gnu                      # passes
cargo check --tests --target x86_64-pc-windows-gnu \
  -p coderex-core -p coderex-tools -p coderex-hooks -p coderex-config   # passes
cargo test --no-run --lib --target x86_64-pc-windows-gnu \
  -p coderex-core -p coderex-tools -p coderex-hooks -p coderex-config -p coderex-cli
                                                                        # links 5 test .exe
```

That compiles the `cfg(target_os = "windows")` code, the Windows-gated tests,
and links the five test executables with the mingw linker. It does **not** run
them: a macOS host cannot create an AppContainer, attach a container token, or
spawn a confined process, so those runtime paths are still unverified locally.
The `coderex-cli` lib-test target now compiles and links too: its test helpers
in `crates/cli/src/from_pr.rs` and `crates/cli/src/tmux.rs` use Unix-only
`PermissionsExt::set_mode` and `/bin/sh` scripts, so those test modules are
`cfg(unix)`-gated. `cargo test --no-run --lib --target x86_64-pc-windows-gnu`
links five test executables (core, tools, hooks, config, cli).

`cargo check --workspace --target x86_64-pc-windows-gnu` also passes (the
Unix-only `openssh` dependency in `coderex-bridge` is target-gated; the Windows
build refuses the SSH spawn mode instead of failing to compile). CI now runs
this cross-check and the test-binary link as a **required** `cross-check-windows`
job; actual AppContainer/HKLM execution is delegated to the advisory
`test-windows` job (see the end of this document).

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
`sandbox-exec`; on Linux with `bwrap` it is wrapped in bubblewrap; on Linux
without `bwrap` it is confined by Landlock when the kernel provides it. On
Windows the hook engine creates its own child and has no AppContainer
post-spawn attach step, so hooks are not confined there even though shell tools
are. Where there is no hook filesystem isolation at all — Windows, or Linux
with neither `bwrap` nor Landlock (or with `CODEREX_LANDLOCK=0`) — a gating hook
(`PreToolUse`, `UserPromptSubmit`) fails closed with
`HookError::SandboxUnavailable` rather than running unconfined, and a
non-gating hook logs a warning and runs with resource limits only. An explicit
opt-out is not a failure. On Linux a configured `max_memory_mb` additionally
creates a cgroup v2 group for the hook and attaches it after spawn; if the tree
is unwritable the hook logs a warning and keeps `RLIMIT_AS`.

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
environment always arrives (`crates/mcp/src/channel.rs:376-379`).

Implemented in `crates/core/src/sandbox.rs` (`is_credential_key`,
`credential_scrub_keys`, `credential_scrub_keys_from`) and applied at
`crates/tools/src/builtins/bash_tool.rs:279`,
`crates/tools/src/builtins/powershell_tool.rs:77` and
`crates/mcp/src/channel.rs:376`.

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
| `Bash` command allowlist and shell-metacharacter rejection | `BashTool::validate_command`, `crates/tools/src/builtins/bash_tool.rs:182` | in-process, only active when an allowlist is configured |
| `Bash` dangerous-command warning (`BashAst`) | `crates/tools/src/builtins/bash_tool.rs:268-314` | advisory — it prepends a warning to the output, it does not block |

## Not enforced, and not claimed

- Linux filesystem confinement when **neither** `bwrap` nor Landlock is
  available (an old kernel, Landlock compiled out, or `CODEREX_LANDLOCK=0`).
  `filesystem_isolation()` reports which backend, if any, is in force.
- Filesystem confinement on Windows when the AppContainer APIs are unavailable —
  creation fails and `filesystem_isolation()` reports `None`; it does not fall
  back to limits-only.
- Filesystem or network confinement on unsupported platforms.
- Full rlimits on macOS. Only `RLIMIT_DATA` is attempted, it is best-effort
  (current Darwin returns `EINVAL`), and it does not cap `mmap` allocations.
  `max_open_files` and `max_cpu_seconds` are Linux-only. Windows uses a Job
  Object for CPU/memory.
- A cgroup memory limit when the cgroup v2 tree is not writable or has not
  delegated `memory`/`pids`. Linux then falls back to `RLIMIT_AS` and logs a
  warning; `RLIMIT_AS` bounds one process, not the tree it forks.
- Landlock hiding path existence. It denies access rights, not `stat`; a path
  outside the ruleset can still be observed to exist.
- Path checks for processes that do not go through the tool layer.
- Filesystem or network confinement for a `command` hook on Windows, or on
  Linux with neither `bwrap` nor Landlock. On Windows the hook engine has no
  AppContainer post-spawn attach step; the hook still gets the Job Object's
  resource limits. There, a gating hook is denied rather than run unconfined; a
  non-gating one runs with a warning. `CODEREX_HOOK_SANDBOX=0` and the per-hook
  `sandbox.enabled: false` opt out deliberately.

## Verifying the macOS guarantees

The Seatbelt behaviour is covered by tests that actually spawn `sandbox-exec`:

- `sandbox::tests::macos_sandbox_denies_read_outside_allowed_path` — a file
  outside the configured read paths is not readable.
- `sandbox::tests::macos_sandbox_allows_executing_a_tool_from_an_allowed_path` —
  a real binary (`ls`), not a shell builtin, runs from an allowed exec path.
- `sandbox::tests::macos_prepare_with_a_rejected_memory_limit_still_spawns` —
  setting `max_memory_bytes` does not fail the spawn when Darwin rejects the
  limit.
- `crates/core/tests/sandbox_integration.rs` — writes inside/outside the allowed
  paths, and network denial.
- `crates/tools/src/builtins/bash_tool.rs` — the `Bash` tool end to end, including
  credential stripping and the safeSpawn refusal.
- `crates/hooks/src/sandbox.rs` — command hooks end to end: a normal hook runs
  and writes inside its workspace, a write under `$HOME` is refused, and
  `unavailable_sandbox_fails_closed_on_a_gating_event` pins the deny path.
  `macos_memory_ceiling_constrains_a_memory_hogging_hook` probes
  `RLIMIT_DATA`: on macOS 15 it prints that the kernel rejects the limit and
  skips enforcement instead of claiming a ceiling that is not there.

**Run on macOS (Darwin 24.6, macOS 15):** `cargo test -p coderex-core --lib
sandbox::` — 34 passed; `cargo test -p coderex-hooks --lib sandbox::` — 13
passed (including the skip above). The macOS tests skip themselves when
`/usr/bin/sandbox-exec` is absent.

Linux behaviour (seccomp allowlist, Landlock planning and confinement,
`allow_network`, rlimits, cgroup fallback) is covered by Linux-gated tests that
cannot run on macOS. Linux hooks (Landlock write denial, and fail-closed when
neither `bwrap` nor Landlock exists) are also covered by Linux-gated tests that
cannot run on macOS.

Windows behaviour is exercised automatically by the `test-windows` job in
`.github/workflows/ci.yml` (on `windows-latest` with the MSVC toolchain). It
runs `sandbox_windows::tests::spawn_in_container_confines_a_real_process` (the
documented `STARTUPINFOEX` spawn), `sandbox::tests::windows_sandbox_confines_a_suspended_child`
(the token-attach path the shell tools use), the `coderex-tools` and
`coderex-hooks` sandbox tests, the `coderex-config` tests (which compile the
Windows registry reader), and `cargo test -p coderex-cli --lib`, all with
`--no-fail-fast`. Both sandbox tests write inside a granted directory and assert
an ungranted sibling stays unwritable; the suspended-child test also asserts the
fail-closed path when the AppContainer APIs are missing. The job is **advisory**
(`continue-on-error`) until it has a stable passing history, and every step
after the first uses `if: always()` so one failing crate does not hide the
others. The CLI lib-test target's Unix-only test helpers are `cfg(unix)`-gated,
so its tests compile for Windows; the sandbox and registry steps are unaffected.
The job has not been run from this macOS host, and further gaps remain even in
CI: if
`CreateAppContainerToken` is absent on the runner, the `STARTUPINFOEX` test
prints a skip instead of proving confinement, and the HKLM reader is only
compiled — no test writes or reads a real registry key (its pure
value-conversion layer is what runs). The pure ACL/capability/quoting policy is
tested on macOS.
