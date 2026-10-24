# Mods — in-process JavaScript, TypeScript and JSX hooks

A **mod** is a `.js`, `.jsx`, `.ts` or `.tsx` file that turns CodeRex hook
events into decisions. It can observe an event, return `stop`, an updated
input, an updated tool output, or paths to watch, and it can call `next(e)` to
run the rest of the matching hooks and the downstream chain, and inspect or
rewrite their combined result. It cannot call tools, read files, open the
network, or reach the engine.

`.js` files are evaluated as they are. `.jsx`, `.ts` and `.tsx` files are
transformed first: TypeScript types are stripped and JSX is lowered to the
classic runtime, with `h` and `Fragment` mapped onto the same UI descriptors
as `$.ui`. See [TypeScript and JSX](#typescript-and-jsx).

The runtime is opt-in. Without the environment variable below, CodeRex does not
load mods at all and behaves exactly as before.

```
CODEREX_MODS=1                  # opt in
CODEREX_MODS_DIR=...            # optional directory override
CODEREX_MODS_TIMEOUT_LIMIT=3    # optional timeout-breaker threshold
CODEREX_MODS_MEMORY_LIMIT=64m   # optional JavaScript heap cap per runtime
```

`CODEREX_MODS` accepts `1`, `true`, `yes` or `on` (case-insensitive). Setting
`CODEREX_MODS_DIR` to a non-empty path also opts in. The default directory is
`config_home()/mods` — `~/.coderex/mods`, unless `CLAUDE_CONFIG_DIR` overrides
the config home.

## A mod file

Every `.js`, `.jsx`, `.ts` and `.tsx` file directly inside the mods directory
is loaded, sorted by file name. Extensions are matched case-insensitively; a
file with any other extension is ignored. A file must define a global
`register(on, options)` function:

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

  // next() runs the hooks registered after this one, then the downstream
  // chain (the settings hooks), and resolves with their combined result for
  // this hook to rewrite. It returns a promise, so await it.
  on('PostToolUse', 'Read', async function ($, e, next) {
    const downstream = await next();
    if (!downstream.failed) {
      return { updatedToolOutput: '[mod] ' + downstream.updatedToolOutput };
    }
    return undefined;
  });
}
```

`options` is a frozen object supplied by the host: currently `{ name: "<file name>" }`.

### `register(on, options)`

`register` runs synchronously, once when the runtime loads, in that mod's own
realm. It must call `on` to subscribe; a file with no `register` is skipped with
a load error. Keep it deterministic: it must register the same hooks every time.

Because the realm lives for the whole runtime, state that a mod declares at
top level persists across hook calls (for example a counter). Each mod gets its
own realm, so two mods cannot see each other's globals.

### `on(event, matcher, hook)`

- `event` — `"*"`, or a hook event name such as `"PreToolUse"`. Matching is
  case-insensitive.
- `matcher` — `"*"`, a tool name, or a `|`-separated list (`"Bash|Write"`).
  Each alternative is compared case-insensitively with the event input (the
  tool name for tool events). `on(event, hook)` is accepted as shorthand for
  `on(event, "*", hook)`.
- `hook` — a function `hook($, e, next)`. It may be `async`: the host awaits
  its return value and drives any promise it yields under the call's deadline.
  `next` is always supplied but may be ignored.

Matching hooks run in load order (file name, then registration order). A mod
may register at most 256 hooks.

### `e` — the event object

`e` is the parsed event input plus the context the host passed:

- If the input string is a JSON object, its fields are copied onto `e`.
- `e.input` is always the raw input string (for tool events, the tool name).
- `e.event` is the event name that fired.
- `e.surface` says where the person is seeing this session. CodeRex ships one
  surface — the terminal — so it is **always `"terminal"`** today. The field
  exists so a mod can branch on it without a later API change; the reference's
  other values (`desktop`, `vscode`, `mobile`) are not reachable because those
  products do not exist here, and the UI renderer is terminal-only.
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

## TypeScript and JSX

Every mod file is picked by extension:

| Extension | Parsed as | Transform before evaluation |
| --- | --- | --- |
| `.js` | JavaScript | none — the source is evaluated as it is |
| `.jsx` | JavaScript with JSX | JSX lowered to `h(...)` |
| `.ts` | TypeScript | types stripped |
| `.tsx` | TypeScript with JSX | types stripped, JSX lowered |

The transform is built in source; there is no `tsconfig.json` to supply. It
uses the classic JSX runtime with `pragma: "h"` and
`pragmaFrag: "Fragment"`, and TypeScript syntax is enabled. The lowered
JavaScript is then evaluated in the same per-mod realm as a `.js` mod, so
everything else in this document applies unchanged.

A transform or parse failure is reported **once** at load as
`<file>: line N, column M: message`, and that mod is skipped. The remaining
mods still load. `.ts` and `.tsx` are never silently treated as `.js`: they are
always parsed as TypeScript.

### The `h` and `Fragment` runtime

`h(type, props, ...children)` and `Fragment` are non-writable globals in each
mod's realm, created after `$.ui`. They return exactly the descriptors
`$.ui.text` / `$.ui.box` / `$.ui.button` already accept, so the host snapshot
and renderer are unchanged.

| JSX | Lowered call | Result |
| --- | --- | --- |
| `<text style="bold">hi</text>` | `h("text", { style: "bold" }, "hi")` | `$.ui.text("hi", "bold")` |
| `<box title="Report">…</box>` | `h("box", { title: "Report" }, …)` | `$.ui.box({ title: "Report" }, …)` |
| `<button id="retry">Retry</button>` | `h("button", { id: "retry" }, "Retry")` | `$.ui.button("Retry", "retry")` |
| `<Badge label="ok" />` | `h(Badge, { label: "ok" })` | the component's return value |
| `<><text>a</text><text>b</text></>` | `h(Fragment, null, …)` | an array of children |

- **Text children** are stringified and concatenated. Numbers are accepted;
  any other value throws.
- **Box children** are flattened one level (so a fragment or an array returned
  by a component can sit inside a box) and validated by `$.ui.box`.
- **Button label** is its child text; its `id` comes from the `id` prop.
- **Components** are called synchronously with a props object that also
  carries `children` (a single child, or an array when there are several). A
  component may return any element or a fragment array.
- `null`, `undefined`, `true` and `false` children are ignored, so
  `{condition && <text>shown</text>}` works.
- **Unknown element names** such as `<div />` throw
  `unknown element type 'div'`; that becomes a normal hook error and, on a
  gating event, a failure that stops.

```tsx
function Badge(props: { label: string }) {
  return <text style="bold">{props.label}</text>;
}

function register(on) {
  on('PostToolUse', 'Read', function ($) {
    $.ui.render(
      <box title="Read result">
        <Badge label="ok" />
        <button id="retry">Retry</button>
      </box>);
  });
}
```

### Dependency cost

The transform is implemented with oxc (`oxc_parser`, `oxc_semantic`,
`oxc_transformer`, `oxc_codegen`, `oxc_allocator`, `oxc_span`, plus
`oxc_diagnostics` for locations), pinned at `0.155`. It adds 47 package names
to `coderex-mods`' resolved dependency tree (311 with it, 264 without), and a
cold `cargo check -p coderex-mods` completed in about 26 seconds on the
development machine used for this change.

`swc_core` was the first choice but cannot resolve in this workspace at
version 82: `swc_common 26` requires `unicode-width ^0.2.1`, while the
workspace's `ratatui 0.29` pins `unicode-width = "=0.2.0"`. Moving to swc
would mean upgrading ratatui and the UI crate, which is outside the mod
runtime.

## `next(e)` — continuing the chain

Every hook receives `next` as its third argument:

```js
on('PostToolUse', 'Read', async function ($, e, next) {
  const downstream = await next();  // the hooks after this one, then the settings hooks
  if (downstream.stop) return { stop: true, updatedToolOutput: 'denied downstream' };
  return { updatedToolOutput: downstream.updatedToolOutput.toUpperCase() };
});
```

`next` returns a **promise**, and the hook must `await` it to read the result.
The downstream chain ends in the configured settings hooks, which are
asynchronous native work; the JavaScript thread is never blocked waiting for
them. The host drives the promise, runs the engine's job queue between
suspensions, and settles it when the downstream run answers or the budget
expires. At the deadline the promise is rejected with the wall-clock message,
so an `async` hook sees a normal exception at the `await`; catching it does not
turn the call into a success — the call is still reported as timed out.

A hook that calls `next()` without awaiting it still works: the level awaits
the pending chain after the hook returns before merging, so the downstream
chain and the inner executor run exactly once. The hook simply cannot use the
downstream result in that case.

This is a breaking change from the earlier synchronous `next`: a mod written
for the old API must make its hook `async` and `await next()`.

### What it runs

Matching JavaScript hooks run in registration order. A hook's `next(e)`
continues that order from just after the calling hook: the remaining matching
JavaScript hooks run, and then the request's **inner executor** — the
downstream hook chain, normally the hooks loaded from settings — runs. The
combined result of the remaining hooks and the inner executor is returned to
the caller.

With hooks A, B and C registered in that order:

- `A`'s `next()` runs B, then C, then the inner executor.
- `B`'s `next()` runs C, then the inner executor.
- `C`'s `next()` runs only the inner executor.

Because those hooks already ran inside `next`, they do **not** run again when
the calling hook returns. If no hook in a chain segment calls `next`, the inner
executor runs once at the end of that segment, after every hook in it, with the
input the hooks settled on.

A hook that returns `stop` before the inner executor has run prevents it from
running on that leg of the chain. `stop` is OR-ed across the chain, so a
downstream stop survives the caller's own return value.

What the remaining chain receives:

- `next()` — the calling hook's own input and context (the original event, or
  what an enclosing `next` passed down).
- `next(value)` where `value` is a string — that string becomes the input; the
  context is unchanged.
- `next(event)` where `value` is an object:
  - `value.input`, when it is a string, becomes the input;
  - `value.context`, when it is a plain object, replaces the context. Only
    string-valued entries are forwarded; other value types are ignored.

The input a hook passes to `next` applies to the hooks after it and to the
inner executor. It does not change the input of hooks that already ran. It is
also not the `updatedInput` returned by an earlier JavaScript hook.

With no inner executor installed (for example a `ModRuntime` used directly,
without a `JsHookExecutor` wrapper), the remaining hooks still run and `next`
returns their combined result with the empty downstream fields: `stop: false`,
`failed: false`, no updated values, no watch paths, no errors.

### The result object

The promise resolves with:

```js
{
  stop: false,                  // the chain asked to stop
  updatedInput: null,           // input replacement, or null
  updatedToolOutput: null,      // tool-output replacement, or null
  watchPaths: [],               // paths to watch
  errors: [],                   // failure messages
  failed: false                 // a hook or the inner executor failed
}
```

`updatedInput` and `updatedToolOutput` are `null` when no hook in the
downstream chain set them.

### Single use

`next` may be called **once per hook invocation**. A second call — even after
the first one returned — throws a `TypeError` with a message containing "may
only be called once", and does **not** run the downstream chain again. The
first result is unaffected. A hook may catch the error and carry on:

```js
on('PostToolUse', '*', async function ($, e, next) {
  const first = await next();
  try { next(); } catch (error) { $.log(String(error.message)); }
  return { updatedToolOutput: first.updatedToolOutput };
});
```

Each matching hook has its own `next`, positioned at that hook, so it
continues the chain from wherever the hook sits rather than from the start.

### Chain depth

Nested `next(e)` calls are bounded. One `fire` call may nest at most **64**
`next` levels. A `next` that would exceed the limit throws a `RangeError`; if
the hook does not catch it, that hook is marked failed and the error is
reported. The bound exists so a mod cannot make `next` recurse without limit.
See [Limits and isolation](#limits-and-isolation).

### How a hook's result composes with `next`

- **The hook called `next` and returned an object.** Its non-null fields
  (`updatedInput`, `updatedToolOutput`) win over the combined downstream
  result. `stop` is a disjunction: `true` from either side is `true` and cannot
  be cleared by returning `stop: false`. `watchPaths` from both sides are
  combined and deduplicated. Downstream `errors` and `failed` are carried
  forward.
- **The hook called `next` and returned `undefined` / `null`.** The downstream
  result passes through unchanged.
- **The hook did not call `next`.** It decides on its own, exactly as before.
  Its result is merged with the earlier hooks' results, and the inner executor
  at the end of the chain runs later and wins an updated-value conflict.
- **The hook threw after calling `next`.** The downstream run still counts,
  its result is preserved, and the hook is marked `failed` with the exception
  message. On a gating event that also sets `stop`.
- **Any hook called `next`.** The inner executor is not run a second time
  after the JavaScript hooks finish. If no hook called `next`, it runs once at
  the end with the possibly updated input, exactly as in phase 1.

The timeout budget covers the **whole** call: every hook, every nested `next`,
and the inner executor. See [Limits and isolation](#limits-and-isolation).

## The `$` host API

`$` is frozen and contains exactly four things:

| Member | Meaning |
| --- | --- |
| `$.session` | Frozen `{ cwd, model, permission_mode }`, captured when the runtime was constructed. Also carries `$.session.notify(text)` and `$.session.submit(text)`. See [Notices](#sessionnotifytext) and [Queued user messages](#sessionsubmittext). |
| `$.log(message)` | Writes a line at `info` under the `coderex_mods` tracing target. |
| `$.env(name)` | The value of an allowlisted environment variable, or `null`. |
| `$.ui` | Build and render a declarative UI tree, and register button handlers. See [UI elements](#ui-elements). |

`$.env` returns `null` for every name outside this exact, case-sensitive list:

```
LANG  LC_ALL  LC_COLLATE  LC_CTYPE  LC_MESSAGES  TZ
TERM  TERM_PROGRAM  TERM_PROGRAM_VERSION  OS  CI
```

Credentials, tokens, keys, passwords, `HOME`, `PATH`, user names, working
directories and every `CODEREX_*` variable are not readable. The session facts
a mod legitimately needs come from `$.session`.

### `$.session.notify(text)`

Stages a short, non-blocking notice for the person using the session. It is
**display-only**: there is no button, no interaction, no engine access, and it
cannot change the hook result.

```js
on('PostToolUse', 'Read', function ($, e) {
  $.session.notify('read ' + e.input);
  $.ui.render($.ui.text('details below', 'dim'));
});
```

- `text` must be a non-empty string. Anything else — including `''` — throws a
  `TypeError` at the call site, which becomes an ordinary hook error.
- The text is capped at **2048 characters**. A longer string is truncated and
  a `…` marker is appended, so the displayed value is at most 2049 characters.
- One hook call may emit at most **16 notices**. Further calls are dropped and
  counted; one `dim` line reports how many were dropped.
- Notices appear in emission order, **before** anything the same hook call
  stages with `$.ui.render`.
- A notice renders as `UiStyle::Notice` (cyan) in the transcript and in
  headless output. It travels through `HookFireResult.ui` and
  `EngineEvent::ModUi`, exactly like a rendered element: no new event is
  generated and the notice cannot re-enter the hook system.

### `$.session.submit(text)`

Queues a **user message** to be submitted **after the current turn finishes**.
It returns immediately and never calls the engine, so a hook cannot re-enter the
engine while it is running a turn.

```js
on('PostToolUse', 'Bash', function ($, e) {
  // After this turn, ask the model to look at what the command changed.
  $.session.submit('Review the files the last command touched.');
});
```

- `text` must be a non-empty string. Anything else — including `''` — throws a
  `TypeError` at the call site, which becomes an ordinary hook error.
- The text is capped at **8192 characters**. A longer string is truncated and a
  `…` marker is appended.
- One hook call may submit at most **2 messages**. Further calls are dropped and
  counted; one `dim` line in the hook result reports how many were dropped.
- One turn may leave at most **8 queued messages** in total across every hook
  call it fires. Further submissions are dropped, counted, and reported the same
  way.
- Messages merge **in call order**. Identical adjacent messages are folded, so
  two matching hooks that queue the same text submit it once.
- The queued text travels in
  `HookFireResult::pending_prompts`; the engine ignores that field. The CLI
  drains the runtime's queue when the turn is idle and submits each message as
  a normal user turn. It goes through the **same permission pipeline** as
  anything a person types: permission rules, `PreToolUse` / `UserPromptSubmit`
  hooks, and any confirmation dialogs all apply.
- Auto-submitted turns are marked in the transcript (`[mod] Auto-submitted turn
  N/3 …`) and the user message is prefixed with `[mod]`. Headless runs log the
  same information and append each auto turn's text to the run's output.
- **Loop guard:** at most **3 consecutive auto-submitted turns** per
  person-initiated turn. Once three have run, further queued messages are
  dropped and a system notice says so. The counter resets when the person
  submits a prompt. This prevents a mod from keeping the session in an endless
  turn loop.
- Submissions are never started while a turn is running, while an overlay or
  confirmation is active, or from inside a hook. In headless mode the queued
  prompts are drained after the prompt's turn, up to the same caps, and then the
  run exits normally.

There is no `$.tools`, `$.files`, `$.engine`, `$`-level filesystem, `require`,
`process`, `fetch`, `XMLHttpRequest`, timer or network access. `next` is the
only capability added on top of the phase-1 surface, and it only continues the
host's existing hook chain and downstream executor — it exposes no engine
object.

## Limits and isolation

A QuickJS runtime and its contexts are confined to one thread, and `next` must
resume in the same realm that called it. Each `ModRuntime` therefore owns:

- **one worker thread**, created when the runtime is constructed;
- **one QuickJS runtime** on that thread, with the heap, stack and interrupt
  limits installed;
- **one context (realm) per loaded mod**, created at load;
- **one small current-thread `tokio` runtime** on the worker, used only to
  `block_on` the downstream executor when the chain reaches it.

`ModRuntime` itself holds strings and configuration plus a channel, which is
what lets `JsHookExecutor` be shared as `Arc<dyn HookExecutor>`.

`fire` sends a request to the worker and waits for its reply. The worker runs
the matching JavaScript hooks and any nested `next` calls as one chain, then
(unless a hook called `next` or stopped the event) the downstream executor.

Three limits are enforced by the engine itself:

- **heap:** `JS_SetMemoryLimit` caps every allocation the runtime makes,
  including one large `ArrayBuffer` or `String.prototype.repeat`;
- **stack:** `JS_SetMaxStackSize` makes a runaway recursion throw a catchable
  `RangeError` instead of overflowing the native stack;
- **interrupt:** `JS_SetInterruptHandler` runs periodically while JavaScript
  executes. The handler compares the current time with the call's deadline and,
  once it passes, makes QuickJS raise an uncatchable exception and return
  control to the worker. The hook is reported as a timeout, the engine is
  discarded, and the next call rebuilds the runtime and reloads the mods, so a
  half-mutated realm cannot affect it.

The calling thread also keeps its own deadline, 100 ms later than the engine's.
It is only a backstop for work QuickJS cannot interrupt — a downstream executor
blocked in a native call, or one that never resolves. When that backstop fires
the worker is detached, as before. Requests are serialized, so a detached
runaway delays only its own runtime.

| Limit | Default | Behaviour |
| --- | --- | --- |
| Wall clock | 5 s per call | Covers every matching JavaScript hook, every nested `next` **and** the downstream run. The engine interrupts JavaScript at the deadline; the caller's backstop detaches the worker only if that cannot stop the work. |
| Heap | 64 MiB | `JS_SetMemoryLimit`. An over-limit allocation throws an out-of-memory error, reported as a hook error. `CODEREX_MODS_MEMORY_LIMIT` overrides it (bytes, or `k`/`m`/`g` with optional `i`/`b`); values below 4 MiB are clamped up. |
| Stack | 4 MiB | `JS_SetMaxStackSize`, clamped to 256 KiB..=8 MiB. A runaway recursion throws a `RangeError`. The worker thread's native stack is 16 MiB so QuickJS can raise that error before the process aborts. |
| GC threshold | 2 MiB | `JS_SetGCThreshold`, clamped below the heap cap. |
| Chain depth | 64 nested `next(e)` calls per call | The 65th `next` throws a `RangeError`; uncaught, it marks that hook failed. |
| Mod file size | 1 MiB | Larger files are skipped with a load error. |
| Hooks per mod | 256 | `on` throws when the cap is reached. |
| Activation handlers per mod | 256 | `$.ui.onActivate` throws when the cap is reached. |
| Submissions per hook call | 2 | Further `$.session.submit` calls are dropped and one `dim` notice reports the count. |
| Queued messages per turn | 8 | The runtime drops further submissions for that turn and reports the count. |
| Consecutive auto-turns | 3 per person-initiated turn | The CLI stops draining, drops the rest with a system notice, and resets the counter at the next person prompt. |
| Timeouts | 3 per session | The circuit breaker disables mods for the rest of the session. See below. |

A file that fails to read, transform, parse, evaluate or register is reported
**once** at load (`tracing::warn!`, plus `ModRuntime::load_errors`) and
skipped. The remaining mods still load. A TypeScript/JSX transform error
includes its line and column. Top-level code gets the same wall-clock budget as
a hook call, so a mod that loops forever while loading is skipped instead of
hanging the session.

The heap cap is not a full sandbox. It bounds what the script can allocate for
itself, but QuickJS still runs in the CodeRex process; do not load untrusted
mods. The directory is for code the person chose to install.

### Timeout circuit breaker

Every wall-clock timeout counts, whether the engine's interrupt handler stopped
the JavaScript or the caller's backstop had to detach the worker. CodeRex
counts them per runtime (one runtime per session) and stops feeding the problem
after a small threshold.

The downstream settings hooks reached through `next()` share that budget. That
hop is a native call outside QuickJS, so it runs on a separate runner thread
owned by the runtime and the worker waits for it with a bounded receive: a
downstream run that does not finish in time returns a failed result at the
deadline, the JavaScript resumes and finishes normally, and the worker — with
its realm and module state — stays alive. The overrun runner is not reused, so
a later call gets a fresh thread instead of queueing behind a blocked one. A
cooperative executor is dropped at the deadline; an executor that blocks its
thread inside a single poll cannot be stopped and finishes on its own thread
without holding up the session. When mods are disabled, the caller goes through
the same pool with a five-second budget.

- **What counts:** only a call that exceeds the wall-clock budget — a hook
  `fire`, a nested `next`, or a UI activation handler. Ordinary hook
  exceptions, heap and stack errors, and a worker that disconnects do **not**
  count.
- **Threshold:** `3` by default. `CODEREX_MODS_TIMEOUT_LIMIT` overrides it with
  a non-negative integer; `0` disables the breaker, and an invalid value falls
  back to `3`.
- **What happens at the threshold:** mods are disabled for the rest of the
  session. The state is sticky — re-enabling requires a new process/session.
  No new worker is started, so no further mod code runs.
- **What a call returns while disabled:** the result carries `failed: true` and
  an error explaining the shutdown. On a gating event (`PreToolUse`,
  `UserPromptSubmit`) it also sets `stop`, so a disabled runtime fails closed.
  On a non-gating event `stop` is unchanged (`false`), and the downstream
  settings hooks still run normally: disabling mods does not disable the
  configured hooks. `activate` returns the reason as an error instead.
- **Telling the person:** `ModRuntime::is_disabled()` and
  `disabled_reason()` expose the state; `JsHookExecutor` and `ModUiHandle`
  forward them. The interactive CLI shows the reason once, in the transcript.
  `timeout_abandonments()` reports the count so far.

```rust
// The threshold in force; 0 means "never disable".
runtime.timeout_breaker_limit();
// Once true, it stays true for this runtime.
runtime.is_disabled();
// Some("mods are disabled ...") after the breaker trips.
runtime.disabled_reason();
```

## Mod file hashes and the load report

`ModRuntime::load_report()` returns one `ModInfo` per `.js`, `.jsx`, `.ts` and
`.tsx` file found:

```rust
pub struct ModInfo {
    pub name: String,       // file name, also `options.name`
    pub path: PathBuf,      // full path read
    pub sha256: String,     // lowercase hex, original file bytes
    pub loaded: bool,       // evaluated, registered, no error
    pub errors: Vec<String>,// why a skipped file was skipped
}
```

**What is hashed.** The SHA-256 covers the **original bytes on disk**, before
TypeScript stripping or JSX lowering. That is the file the person installed,
not the runtime's transformed copy. The hash is empty when the file could not
be read (there are no bytes to hash); a file that read but failed to transform
or evaluate still has its hash.

**What the hash does not establish.** It is an integrity fingerprint, not a
trust decision. CodeRex has **no signature scheme and no trust store**, and
does not compare hashes against any allowlist. A hash proves only that two
byte sequences are the same; it says nothing about who wrote them or whether
they are safe. Mods are trusted because the person chose to put the files in
their own mods directory. A changed hash across runs means the file changed —
not that the change is malicious, and not that an unchanged hash means the
code is trustworthy. The runtime also has no way to know that the bytes it
hashed are the bytes that ran if the file is replaced mid-session: the report
describes the load, not a live guarantee.

**The startup line.** When mods are enabled, the CLI prints one line at
startup: how many loaded, from which directory, and that the hashes are
informational. With at most three loaded mods it also inlines a short
(12-character) hash per file; above that it reports only the count, so a large
directory does not turn startup into a wall of hashes. The full report is
always available programmatically.

```text
mods: 2 loaded from /home/you/.coderex/mods [guard.js=1f2e3d4c5b6a, fmt.js=9a8b7c6d5e4f]; sha256 is informational only (no signature or trust store); this code runs because you installed it
```

## Composing with settings hooks

`JsHookExecutor` implements `coderex_core::hook_executor::HookExecutor` and can
wrap the existing `HookRegistry`:

1. JavaScript hooks run in load order. Each one may call `next(e)` to run the
   hooks registered after it and then the settings hooks immediately, and to
   inspect or rewrite their combined result.
2. If a JavaScript hook returns `stop` and no hook called `next`, the inner
   executor is not called.
3. If no hook called `next`, the inner executor runs once with the possibly
   updated input. Results merge as before: `stop` and `failed` are OR-ed, the
   inner executor wins an `updatedInput` / `updatedToolOutput` conflict (it ran
   later), watch paths are combined and deduplicated, and errors are appended.
4. If a hook called `next`, that hook's own non-null fields win over the
   combined downstream result instead (see the composition rules above), and
   the inner executor is not run again at the end. The settings hooks still
   ran inside `next` unless an earlier hook stopped that leg of the chain.

`fire_hook` is still a blocking call from the caller's point of view and moves
its wait to `tokio::task::spawn_blocking`, so it does not block an async worker.
Inside the mod worker, `next()` is a promise: the thread that owns QuickJS
returns to the host between suspensions instead of waiting on the native
executor.

## UI elements

A hook may contribute UI to the terminal surface with `$.ui`. The host owns
the tree: it is a serializable **snapshot**, not a live JavaScript object, so
it can be rendered after the hook returns, times out, or its worker is
unloaded.

```js
on('PostToolUse', 'Read', function ($, e) {
  $.ui.render(
    $.ui.box({ title: 'Read result' },
      $.ui.text('checked ' + e.input, 'dim'),
      $.ui.button('Show details', 'read-details'))
  );
});
```

### `$.ui.text(text, style?)`

Returns a text element. `text` must be a string. `style` is optional and one
of `plain` (the default), `bold`, `dim`, `warning`, `error` or `notice`
(case-insensitive). An unknown style throws a `TypeError` at the call site.
`notice` is the style `$.session.notify` uses; a mod may also select it
explicitly.

### `$.ui.box(options?, children...)`

Returns a bordered container. The first argument, when present, is an options
object with an optional string `title`. Every following argument is a child
element; an array is flattened one level. Invalid options or children throw.

### `$.ui.button(label, id)`

Returns a labelled button. `label` and `id` must be non-empty strings. The id
is what a focused activation sends to the handler registered with
`$.ui.onActivate(id, handler)`.

### `$.ui.onActivate(id, handler)`

Registers the handler for a button id. `handler($, event)` runs on the mod's
worker, in the same realm, under the same wall-clock budget and engine limits
as a hook, and may be `async`. It may call `$.ui.render(...)` and/or return one
element or an array of elements; the host renders the staged elements followed
by the returned ones.

```js
function register(on) {
  on('PostToolUse', 'Read', function ($) {
    $.ui.render($.ui.button('Retry', 'retry'));
  });

  $.ui.onActivate('retry', function ($, event) {
    // event.id is "retry"; event.event is "ModUiActivate"; event.surface is
    // "terminal". Returning elements replaces the button block in the
    // transcript.
    return [$.ui.text('Retried ' + event.id)];
  });
}
```

- Registering the same id again **replaces** the earlier handler.
- Activating an id with no handler is ignored: it logs a warning and renders
  nothing. It is not an error.
- A thrown exception, an invalid returned tree or a timeout is reported as a
  system message in the terminal.
- A mod may register at most 256 activation handlers.
- An `async` handler is awaited like an `async` hook, under the same budget.

### `$.ui.render(element(s))`

Stages one element, an array of elements, or several arguments for the
current hook call. Elements render in call order. A hook may call `render`
more than once; every staged element is appended to that call's result.
Rendering is independent of the return value: a hook that returns `undefined`
still contributes its UI. The staging area is cleared before each hook call,
so a later call never replays what an earlier one produced.

`render` validates the whole tree before staging it. A value that is not a
`$.ui` element, an unknown type or style, or a malformed nested box throws at
the call site. A thrown error becomes a hook error like any other exception:
the call is marked `failed` and no partial tree is kept.

### Delivery and rendering

The elements travel in `HookFireResult.ui`, and the engine emits
`EngineEvent::ModUi(Vec<UiElement>)` for a non-empty tree (empty trees emit
nothing). The terminal REPL renders the block into the transcript as a system
message; a headless run appends it to the turn's output. Boxes render with
nested borders and titles, text uses the terminal style vocabulary, and
buttons render as `[label]`. A `$.session.notify` notice is an ordinary
`notice`-styled text element on this same path, so it needs no separate
rendering. A hook that only notifies still produces a block; an all-`notice`
block contains no buttons, so it is inert.

### Interaction

In the interactive terminal, the most recent rendered block is focusable when
it contains at least one button:

| Key | Action |
| --- | --- |
| `Tab` | Start focus on the first button, then cycle forward |
| `Shift+Tab` | Cycle backward while a button is focused |
| `Enter` | Activate the focused button |
| `Esc` | Clear focus |

While no button is focused, `Enter`, `Esc` and `Shift+Tab` keep their usual
meanings (submit, interrupt/clear, permission-mode cycle). Ordinary typing
always reaches the input line; focus never swallows a character. The focused
button renders as `> [label] <`.

A **left mouse click** on a button in the most recent block activates it too.
The transcript re-renders that block from its live tree at the message pane's
actual width, so the button's screen cells are known exactly even when the
block wraps or the transcript is scrolled. Clicks on borders, padding or text
are ignored, and so is a click while an overlay (command palette, dialog), a
tool-permission confirmation, or an MCP form is open. Wheel scrolling is
unaffected. In the compact transcript (a message pane shorter than ten rows) a
block is truncated to one line and is not clickable; keyboard activation still
works there.

Mouse reporting must be enabled and supported by the terminal. CodeRex enables
it when the TUI starts (`EnableMouseCapture`) and disables it on exit; iTerm2,
kitty, WezTerm, Ghostty, Windows Terminal and the VS Code/Cursor terminals
report mouse input. In a terminal that does not, or with mouse reporting
turned off, clicks do nothing — the keyboard bindings above remain the
fallback, and focus/activation behave identically either way.

Activation calls the handler on the mod worker. The elements it staged and
returned are appended to the transcript as a new mod UI block, rendered with
the same renderer. An error is shown as a system message. Keyboard and mouse
activation share this path.

**Headless runs do not interact.** `--print` / non-interactive output renders
the block and does not read keys or wait for input; a button is inert there.
The same applies when the REPL is not attached to a terminal.

### Limitations

- **Snapshot, not live.** Elements are plain data. There is no `mount`,
  `update` or `unmount`, no stable node identity, and no way to mutate an
  element after `render`. Render again from a later hook to show new content.
- **Terminal only.** There is no web or other surface. Only the most recent
  block with buttons is interactive; an older block's buttons are inert, and
  activation is still available through keyboard focus plus `Enter`.
- **One block per hook call.** The host renders a hook's tree on its own; it
  is not merged into the built-in panes.

## Not yet

- A promise a hook waits on that nothing in the runtime can settle (no
  `next()`, no engine work) is reported as a hook error rather than hanging
  until the deadline.
- There is no timer API (`setTimeout`), so an `async` hook can only await its
  own promises and `next()`.
