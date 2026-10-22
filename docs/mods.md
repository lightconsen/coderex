# Mods — in-process JavaScript hooks

A **mod** is a JavaScript file that turns CodeRex hook events into decisions.
Phase 1 is a small, deliberately limited runtime: a mod can observe an event
and return `stop`, an updated input, an updated tool output, or paths to watch.
It cannot call tools, read files, open the network, or reach the engine.

The runtime is opt-in. Without the environment variable below, CodeRex does not
load mods at all and behaves exactly as before.

```
CODEREX_MODS=1          # opt in
CODEREX_MODS_DIR=...    # optional directory override
```

`CODEREX_MODS` accepts `1`, `true`, `yes` or `on` (case-insensitive). Setting
`CODEREX_MODS_DIR` to a non-empty path also opts in. The default directory is
`config_home()/mods` — `~/.coderex/mods`, unless `CLAUDE_CONFIG_DIR` overrides
the config home.

## A mod file

Every `*.js` file directly inside the mods directory is loaded, sorted by file
name. A file must define a global `register(on, options)` function:

```js
// ~/.coderex/mods/guard.js
function register(on, options) {
  on('PreToolUse', 'Bash|Write', function ($, e) {
    if (e.tool_input && String(e.tool_input.command || '').includes('rm -rf /')) {
      $.log('refusing a destructive command');
      return { stop: true };
    }
    return undefined;
  });

  on('PostToolUse', '*', function ($, e) {
    return { updatedToolOutput: e.input + ' finished' };
  });
}
```

`options` is a frozen object supplied by the host: currently `{ name: "<file name>" }`.

### `register(on, options)`

`register` runs synchronously, once at load (to validate the file and record
its hooks) and again in each hook call's fresh realm. It must call `on` to
subscribe; a file with no `register` is skipped with a load error. Keep it
deterministic: it must register the same hooks each time, so it should not
depend on the clock or on randomness.

### `on(event, matcher, hook)`

- `event` — `"*"`, or a hook event name such as `"PreToolUse"`. Matching is
  case-insensitive.
- `matcher` — `"*"`, a tool name, or a `|`-separated list (`"Bash|Write"`).
  Each alternative is compared case-insensitively with the event input (the
  tool name for tool events). `on(event, hook)` is accepted as shorthand for
  `on(event, "*", hook)`.
- `hook` — a **synchronous** function `hook($, e)`. An `async` function is
  rejected at call time with an error, because phase 1 has no continuation to
  await.

Matching hooks run in load order (file name, then registration order). A mod
may register at most 256 hooks.

### `e` — the event object

`e` is the parsed event input plus the context the host passed:

- If the input string is a JSON object, its fields are copied onto `e`.
- `e.input` is always the raw input string (for tool events, the tool name).
- `e.event` is the event name that fired.
- Every context entry is copied onto `e` as a string. `tool_input` is the
  exception: when it parses as JSON (the usual case for tool arguments), it is
  exposed as an object.

The host owns `e`; mutate it freely inside the call, but only the return value
is read.

### The return value

A hook returns an object (or `undefined` / `null` for no decision):

```js
return {
  stop: true,                       // deny / stop
  updatedInput: '...',              // replacement for the input
  updatedToolOutput: '...',         // replacement for a tool result
  watchPaths: ['/absolute/path']    // ask the host to watch files
};
```

The snake_case spellings `updated_input`, `updated_tool_output` and
`watch_paths` are accepted too. A non-object return value is ignored. A thrown
exception, an engine limit or a timeout becomes a hook error: other hooks still
run, and the merged result is marked `failed`. On the gating events
(`PreToolUse`, `UserPromptSubmit`) a failure also sets `stop`, so a broken hook
can never read as an allow.

## The `$` host API

`$` is frozen and contains exactly three things:

| Member | Meaning |
| --- | --- |
| `$.session` | Frozen `{ cwd, model, permission_mode }`, captured when the runtime was constructed. |
| `$.log(message)` | Writes a line at `info` under the `coderex_mods` tracing target. |
| `$.env(name)` | The value of an allowlisted environment variable, or `null`. |

`$.env` returns `null` for every name outside this exact, case-sensitive list:

```
LANG  LC_ALL  LC_COLLATE  LC_CTYPE  LC_MESSAGES  TZ
TERM  TERM_PROGRAM  TERM_PROGRAM_VERSION  OS  CI
```

Credentials, tokens, keys, passwords, `HOME`, `PATH`, user names, working
directories and every `CODEREX_*` variable are not readable. The session facts
a mod legitimately needs come from `$.session`.

There is no `$.tools`, `$.files`, `$.engine`, `$`-level filesystem, `require`,
`process`, `fetch`, `XMLHttpRequest`, timer or network access. The realm is a
default `boa_engine` `Context` with only the above installed.

## Limits and isolation

A `boa_engine::Context` is neither `Send` nor interruptible from another
thread. Each hook call therefore gets **its own OS thread and its own realm**.
The runtime itself holds only strings and configuration, which is what lets the
executor be shared as `Arc<dyn HookExecutor>`.

| Limit | Default | Behaviour |
| --- | --- | --- |
| Wall clock | 5 s per hook | The calling thread stops waiting and reports a hook error. |
| Loop iterations | 10,000,000 per frame | `while (true) {}` throws an engine runtime-limit error. |
| Recursion | 256 nested calls | A runaway recursive hook throws instead of overflowing the stack. |
| Stack values | 10,240 | The engine's stack ceiling. |
| Mod file size | 1 MiB | Larger files are skipped with a load error. |
| Hooks per mod | 256 | `on` throws when the cap is reached. |

A file that fails to parse, evaluate or register is reported **once** at load
(`tracing::warn!`, plus `ModRuntime::load_errors`) and skipped. The remaining
mods still load.

**Memory caveat.** `boa_engine` 0.20 exposes no heap or allocation limit. The
wall clock and loop ceiling bound ordinary runaway code, and recursion/stack
limits bound call depth, but a single native allocation (for example a very
large `String.prototype.repeat`) can still allocate before either limit fires.
Do not load untrusted mods; the directory is for code the person chose to
install. A hard memory cap needs either a future boa API or a container/process
boundary.

The wall-clock budget is enforced by the caller, not by the engine: boa 0.20
has no interrupt hook. When the budget expires the call is reported and the
worker is detached; the engine limits make it terminate shortly afterwards. A
hook that blocked inside a native call would outlive the timeout until that
call returned.

## Composing with settings hooks

`JsHookExecutor` implements `coderex_core::hook_executor::HookExecutor` and can
wrap the existing `HookRegistry`:

1. JavaScript hooks run first.
2. If any JavaScript hook returned `stop`, the inner executor is not called.
3. Otherwise the inner executor runs with the possibly updated input.
4. Results merge: `stop` and `failed` are OR-ed, the inner executor wins an
   `updatedInput` / `updatedToolOutput` conflict (it ran later), watch paths
   are combined and deduplicated, and errors are appended.

The runtime is synchronous; `fire_hook` moves the blocking JavaScript work to
`tokio::task::spawn_blocking` and awaits it, so it does not block an async
worker.

## Not yet

Phase 1 has no `next(e)`, no UI elements and no async hooks. These are the
designs, not shipped behaviour.

### `next(e)` — chaining a hook with the rest of the chain

The reference runtime passes a `next(e)` function to each hook; calling it
continues the chain, and the hook can inspect or replace what comes back.
Implementing that needs **engine continuation**, which this runtime does not
have:

- A hook is currently a single synchronous call whose value is read when it
  returns. `next(e)` must suspend the hook in the middle of its JavaScript
  frame, let the host run the remaining hooks (including the inner executor),
  then resume it with the downstream result.
- The realm is `!Send`, so the suspended frame must stay on one owner thread.
  The viable shape is a persistent mod worker thread that owns a `Context`,
  with `fire` requests and `next` round-trips over channels. `next` would
  block that worker until the host answers; the agent's async side drives the
  chain, so the agent never blocks.
- On the JavaScript side the cleanest representation is a generator or an
  explicitly driven state machine: compile/run the hook as a generator, yield
  a `NextRequest(e)` when `next` is called, and resume with the downstream
  `HookFireResult`. Boa 0.20 can run a generator, but the host must drive
  `run_jobs` in rounds (the `crates/workflow` runtime already has this loop
  shape).
- Semantics to pin down: whether `next` composes results forward (this hook
  then inner hooks then downstream mods) or backward; how a downstream
  `stop` is surfaced; whether the hook may call `next` more than once; and
  how a timeout applies to the whole suspended chain rather than one call.
- The current per-call thread model is incompatible with continuation, so
  enabling `next` requires moving to a long-lived worker per runtime while
  keeping the same limits (the worker can still be abandoned on timeout, but
  now the whole chain's budget must be tracked).

### UI elements

The reference lets a mod contribute panes, status lines, toasts and prompts.
That needs two pieces this phase does not have:

1. **An element tree owned by the host.** A mod would describe elements
   declaratively (for example `{ type: 'status', text }`,
   `{ type: 'pane', id, lines }`, `{ type: 'toast', message }`), with a stable
   id and a lifecycle (`mount`, `update`, `unmount`). The tree must be a
   serializable value crossing the worker boundary, not JavaScript objects.
2. **A renderer.** `coderex-ui` would need a render pass that merges mod
   elements with the built-in TUI layout, respects the terminal profile and
   accessibility settings, and routes input (slash commands, key bindings)
   back to the owning mod. Rendering must remain possible when a mod is
   timing out or has been unloaded, so the tree is a snapshot rather than a
   live engine reference.

Both features would extend `register`/`on` rather than change the read-only
`$` surface: a mod still gets no tools or files, only the ability to hand the
host a decision or a description of pixels.
