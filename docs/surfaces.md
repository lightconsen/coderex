# Surfaces

A **surface** is the product a session is presented in: the terminal, an IDE
panel, a desktop window, a web page, a chat product. It is not a widget toolkit
and not a rendering API. The abstraction in `coderex-core::surface` answers one
question — *which front end is showing this session, and what can it present?* —
and gives the engine, the mod runtime and every host the same answer.

CodeRex ships **one** surface implementation, the terminal. The abstraction
exists so that adding a second one is a host-side project, not a fork of the
engine or the mod runtime.

## The types

| Type | Meaning |
| --- | --- |
| `SurfaceId` | Validated stable name (`[a-z0-9_-]`, at most 32 characters). `SurfaceId::TERMINAL` is `"terminal"`. This is what a mod sees as `e.surface`. |
| `SurfaceCapabilities` | `{ interactive, mouse, images }`. The terminal is `{ true, true, false }`; `TEXT_ONLY` is `{ false, false, false }`. Mods see it as `e.surfaceCapabilities`. |
| `SurfaceDescriptor` | `SurfaceId` + `SurfaceCapabilities`. The host passes it to the runtime; `SurfaceDescriptor::terminal()` is the default. |
| `UiSurface` | The trait a host implements: `descriptor()` and `render_text(elements, width)`. |
| `SurfaceRegistry` | The surfaces a host can present, plus the active one. Registering does not change the active surface; `set_active` does. The first registration becomes active. |

`render_text` is the lowest-common-denominator presentation used by headless
transcripts, wire clients and tests. A rich surface keeps it as a fallback and
renders `UiElement`s natively for its own medium.

## How a mod contribution reaches a surface

```
mod hook returns $.ui.render(...)
        │
        ▼
HookFireResult.ui ──► EngineEvent::ModUi(Vec<UiElement>)
        │
        ├─ in-process host: the active surface renders the tree
        │     terminal: ratatui block + hit regions (TUI), or render_text (headless)
        │
        └─ out-of-process host:
              core::wire_protocol  → WireEvent::ModUi { elements }
              stream-json          → {"type":"system","subtype":"mod_ui","elements":[...]}
```

Activation is surface-agnostic: a surface delivers the button `id` to
`ModUiHandle::activate(id)`, which runs the mod's `$.ui.onActivate` handler and
returns the elements to render in its place. The terminal routes both `Enter`
and mouse clicks through it.

The descriptor also travels to mods: `$.session.surface` is the active id and
`$.session.surfaceCapabilities` is the capability object. A mod that renders a
button can check `interactive` first, and a mod that changes behaviour per
surface branches on the id — with no assumption that the surface is the
terminal.

## What is implemented

- `coderex-core::surface` — the ids, capabilities, descriptor, trait and
  registry, with tests for validation, duplicate registration and selection.
- `coderex-ui::surface::TerminalSurface` — the terminal implementation. It
  reports `interactive: true, mouse: true, images: false` and renders through
  the same text renderer the headless transcript uses.
- The CLI registers exactly that surface when it builds the mod session, and
  passes `active_descriptor()` into `ModSession`.
- `core::wire_protocol` and the stream-json protocol both carry mod UI, so a
  surface outside this process receives the same tree.

Only the terminal is registered. There is no IDE, desktop, web or chat host in
this repository; that is a product gap, not an abstraction gap.

## Adding a surface

1. Implement `UiSurface` for the host's type — `descriptor()` and
   `render_text()`; render `UiElement`s with the host's own widgets.
2. Register it in a `SurfaceRegistry` and `set_active` when that host is the
   one presenting the session.
3. Pass `active_descriptor()` as `ModSession.surface`.
4. Route button activation to `ModUiHandle::activate`.
5. For an out-of-process host, consume `WireEvent::ModUi` (or the stream-json
   `system`/`mod_ui` frame) and send activation back through the same protocol
   the host already uses for control requests.

No change is needed in the engine, `crates/mods`, or the element schema. The
element tree is the shared contract; the surface decides how it looks.

## Boundaries

- The terminal does not render images (`images: false`); the element tree has no
  image element.
- A surface that reports `interactive: false` still receives hooks and UI; it is
  the host's job to hide affordances it cannot honour.
- The registry is in-memory and per session. Persisting a preference for a
  surface is a host concern, not part of this contract.
