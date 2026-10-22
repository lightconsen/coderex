# Mods — in-process JavaScript, TypeScript and JSX hooks

A **mod** is a `.js`, `.jsx`, `.ts` or `.tsx` file that turns CodeRex hook
events into decisions. It can observe an event, return `stop`, an updated
input, an updated tool output, or paths to watch, and it can call `next(e)` to
run the downstream chain and inspect or rewrite its result. It cannot call
tools, read files, open the network, or reach the engine.

`.js` files are evaluated as they are. `.jsx`, `.ts` and `.tsx` files are
transformed first: TypeScript types are stripped and JSX is lowered to the
classic runtime, with `h` and `Fragment` mapped onto the same UI descriptors
as `$.ui`. See [TypeScript and JSX](#typescript-and-jsx).

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

  // next() runs the downstream chain (the settings hooks) and returns its
  // result to this hook, which may rewrite it.
  on('PostToolUse', 'Read', function ($, e, next) {
    const downstream = next();
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
- `hook` — a **synchronous** function `hook($, e, next)`. `next` is always
  supplied but may be ignored. An `async` function is rejected at call time
  with an error, because there is no promise to await.

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
JavaScript is then evaluated by the same `boa_engine` realm as a `.js` mod, so
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

## `next(e)` — running the downstream chain

Every hook receives `next` as its third argument:

```js
on('PostToolUse', 'Read', function ($, e, next) {
  const downstream = next();          // run the settings hooks
  if (downstream.stop) return { stop: true, updatedToolOutput: 'denied by a downstream hook' };
  return { updatedToolOutput: downstream.updatedToolOutput.toUpperCase() };
});
```

`next` is **synchronous**: it blocks the mod worker while the downstream
executor runs and returns that executor's result to the JavaScript hook. The
agent never blocks; it waits on the worker from its own thread.

### What it runs

`next` invokes the request's **inner executor** — the downstream hook chain,
normally the hooks loaded from settings — with the same event name. It does not
re-enter the other JavaScript hooks; those are this runtime's own list, and
they continue to run in order after the current hook returns.

What the downstream executor receives:

- `next()` — the event's original input and context.
- `next(value)` where `value` is a string — that string becomes the input; the
  context is unchanged.
- `next(event)` where `value` is an object:
  - `value.input`, when it is a string, becomes the input;
  - `value.context`, when it is a plain object, replaces the context. Only
    string-valued entries are forwarded; other value types are ignored.

The input a hook passes to `next` is the original event input (or what that
hook chose), not an `updatedInput` returned by an earlier JavaScript hook.

With no inner executor installed (for example a `ModRuntime` used directly,
without a `JsHookExecutor` wrapper), `next` returns an empty result:
`stop: false`, `failed: false`, no updated values, no watch paths, no errors.

### The result object

`next` returns:

```js
{
  stop: false,                  // downstream asked to stop
  updatedInput: null,           // downstream input replacement, or null
  updatedToolOutput: null,      // downstream tool-output replacement, or null
  watchPaths: [],               // downstream paths to watch
  errors: [],                   // downstream failure messages
  failed: false                 // downstream reported a failure
}
```

`updatedInput` and `updatedToolOutput` are `null` when the downstream executor
did not set them.

### Single use

`next` may be called **once per hook invocation**. A second call — even after
the first one returned — throws a `TypeError` with a message containing "may
only be called once", and does **not** run the downstream chain again. The
first result is unaffected. A hook may catch the error and carry on:

```js
on('PostToolUse', '*', function ($, e, next) {
  const first = next();
  try { next(); } catch (error) { $.log(String(error.message)); }
  return { updatedToolOutput: first.updatedToolOutput };
});
```

Each matching hook has its own `next`, so two different hooks on the same
event each get one downstream run if both call it.

### How a hook's result composes with `next`

- **The hook called `next` and returned an object.** Its non-null fields
  (`updatedInput`, `updatedToolOutput`) win over the downstream result.
  `stop` is a disjunction: `true` from either side is `true` and cannot be
  cleared by returning `stop: false`. `watchPaths` from both sides are
  combined and deduplicated. Downstream `errors` and `failed` are carried
  forward.
- **The hook called `next` and returned `undefined` / `null`.** The downstream
  result passes through unchanged.
- **The hook did not call `next`.** It decides on its own, exactly as before.
- **The hook threw after calling `next`.** The downstream run still counts,
  its result is preserved, and the hook is marked `failed` with the exception
  message. On a gating event that also sets `stop`.
- **Any hook called `next`.** The inner executor is not run a second time
  after the JavaScript hooks finish. If no hook called `next`, it runs once at
  the end with the possibly updated input, exactly as in phase 1.

The timeout budget covers the **whole** call, including the downstream run.
See [Limits and isolation](#limits-and-isolation).

## The `$` host API

`$` is frozen and contains exactly four things:

| Member | Meaning |
| --- | --- |
| `$.session` | Frozen `{ cwd, model, permission_mode }`, captured when the runtime was constructed. |
| `$.log(message)` | Writes a line at `info` under the `coderex_mods` tracing target. |
| `$.env(name)` | The value of an allowlisted environment variable, or `null`. |
| `$.ui` | Build and render a declarative UI tree. See [UI elements](#ui-elements). |

`$.env` returns `null` for every name outside this exact, case-sensitive list:

```
LANG  LC_ALL  LC_COLLATE  LC_CTYPE  LC_MESSAGES  TZ
TERM  TERM_PROGRAM  TERM_PROGRAM_VERSION  OS  CI
```

Credentials, tokens, keys, passwords, `HOME`, `PATH`, user names, working
directories and every `CODEREX_*` variable are not readable. The session facts
a mod legitimately needs come from `$.session`.

There is no `$.tools`, `$.files`, `$.engine`, `$`-level filesystem, `require`,
`process`, `fetch`, `XMLHttpRequest`, timer or network access. `next` is the
only capability added on top of the phase-1 surface, and it only invokes the
host's existing downstream hook executor — it exposes no engine object.

## Limits and isolation

A `boa_engine::Context` is neither `Send` nor interruptible from another
thread, and `next` must resume in the same context that called it. Each
`ModRuntime` therefore owns:

- **one worker thread**, created when the runtime is constructed;
- **one `Context`** on that thread;
- **one realm per loaded mod**, created at load;
- **one small current-thread `tokio` runtime** on the worker, used only to
  `block_on` the downstream executor when a hook calls `next`.

`ModRuntime` itself holds strings and configuration plus a channel, which is
what lets `JsHookExecutor` be shared as `Arc<dyn HookExecutor>`.

`fire` sends a request to the worker and waits for its reply. The worker runs
the matching JavaScript hooks in order, then (unless a hook called `next` or
stopped the event) the downstream executor. A timeout does not stop the
engine; the caller detaches the worker and reports a hook error. The next
`fire` starts a fresh worker, which reloads the mods.

| Limit | Default | Behaviour |
| --- | --- | --- |
| Wall clock | 5 s per call | Covers every matching JavaScript hook **and** the downstream run. The calling thread stops waiting and reports a hook error. |
| Loop iterations | 10,000,000 per frame | `while (true) {}` throws an engine runtime-limit error. |
| Recursion | 256 nested calls | A runaway recursive hook throws instead of overflowing the stack. |
| Stack values | 10,240 | The engine's stack ceiling. |
| Mod file size | 1 MiB | Larger files are skipped with a load error. |
| Hooks per mod | 256 | `on` throws when the cap is reached. |

A file that fails to read, transform, parse, evaluate or register is reported
**once** at load (`tracing::warn!`, plus `ModRuntime::load_errors`) and
skipped. The remaining mods still load. A TypeScript/JSX transform error
includes its line and column.

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
hook that blocked inside a native call, or a downstream executor that never
resolves, would outlive the timeout until that work returned. Requests are
serialized by the worker, so a detached runaway delays only its own runtime.

## Composing with settings hooks

`JsHookExecutor` implements `coderex_core::hook_executor::HookExecutor` and can
wrap the existing `HookRegistry`:

1. JavaScript hooks run in load order; each one may call `next(e)` to run the
   settings hooks immediately and inspect or rewrite the result.
2. If a JavaScript hook returns `stop` and no hook called `next`, the inner
   executor is not called.
3. If no hook called `next`, the inner executor runs with the possibly updated
   input. Results merge as before: `stop` and `failed` are OR-ed, the inner
   executor wins an `updatedInput` / `updatedToolOutput` conflict (it ran
   later), watch paths are combined and deduplicated, and errors are appended.
4. If a hook called `next`, that hook's own non-null fields win over the
   downstream result instead (see the composition rules above), and the inner
   executor is not run again at the end.

The runtime is synchronous; `fire_hook` moves the blocking wait to
`tokio::task::spawn_blocking`, so it does not block an async worker.

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
of `plain` (the default), `bold`, `dim`, `warning` or `error`
(case-insensitive). An unknown style throws a `TypeError` at the call site.

### `$.ui.box(options?, children...)`

Returns a bordered container. The first argument, when present, is an options
object with an optional string `title`. Every following argument is a child
element; an array is flattened one level. Invalid options or children throw.

### `$.ui.button(label, id)`

Returns a labelled button affordance. `label` and `id` must be non-empty
strings. The id is reserved for a future input-routing pass; nothing reads it
today.

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
buttons render as `[label]`.

### Limitations

- **Snapshot, not live.** Elements are plain data. There is no `mount`,
  `update` or `unmount`, no stable node identity, and no way to mutate an
  element after `render`. Render again from a later hook to show new content.
- **Terminal only.** There is no web or other surface.
- **Buttons do nothing.** They render as `[label]`. Input routing — clicks,
  focus, key bindings, and the callback that would receive a button's `id` —
  is not implemented. A button is a labelled affordance, not a control.
- **One block per hook call.** The host renders a hook's tree on its own; it
  is not merged into the built-in panes.

## Not yet

There are no async hooks. An `async` hook is rejected at call time.
