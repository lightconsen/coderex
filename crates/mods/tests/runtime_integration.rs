//! Integration tests for the in-process mod runtime.
//!
//! These load real files from disk through the public [`ModRuntime`] API (the
//! unit tests in `src/runtime.rs` build mods from inline strings), exercise
//! the worker boundary with a downstream executor, and render a mod's UI
//! tree through `coderex-ui`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use coderex_core::hook_executor::{HookExecutor, HookFireResult};
use coderex_core::surface::SurfaceDescriptor;
use coderex_core::ui_protocol::{UiElement, UiStyle};
use coderex_mods::{
    DEFAULT_MEMORY_LIMIT, DEFAULT_TIMEOUT_BREAKER_LIMIT, MODS_MEMORY_LIMIT_ENV, ModLimits,
    ModRuntime, ModSession,
};

fn session() -> ModSession {
    ModSession {
        cwd: "/work".to_string(),
        model: "test-model".to_string(),
        permission_mode: "default".to_string(),
        surface: SurfaceDescriptor::terminal(),
    }
}

fn load_dir(path: &Path) -> ModRuntime {
    ModRuntime::load_dir(path, session()).expect("load the mod directory")
}

fn load_dir_with(path: &Path, limits: ModLimits) -> ModRuntime {
    ModRuntime::load_dir_with_limits(path, session(), limits).expect("load the mod directory")
}

fn write_mod(dir: &Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write mod");
}

fn fire(runtime: &ModRuntime, event: &str, input: &str) -> HookFireResult {
    runtime.fire(event, input, &HashMap::new())
}

/// A downstream executor that answers once and records that it ran.
struct Inner {
    calls: std::sync::Mutex<Vec<String>>,
    answer: HookFireResult,
}

impl Inner {
    fn answering(updated_input: &str) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            answer: HookFireResult {
                updated_input: Some(updated_input.to_string()),
                ..HookFireResult::default()
            },
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl HookExecutor for Inner {
    async fn fire_hook(
        &self,
        event_name: &str,
        _input: &str,
        _context: &HashMap<String, String>,
    ) -> HookFireResult {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event_name.to_string());
        self.answer.clone()
    }
}

#[test]
fn js_ts_and_tsx_load_from_disk_in_name_order() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "a.js",
        "function register(on) { \
         on('SessionStart', '*', function () { return { updatedInput: 'from-js' }; }); }",
    );
    write_mod(
        dir.path(),
        "b.ts",
        "function register(on: any): void { \
         on('Stop', '*', function (): any { return { updatedInput: 'from-ts' }; }); }",
    );
    write_mod(
        dir.path(),
        "c.tsx",
        "function register(on: any) { \
         on('TurnEnd', '*', function ($: any) { \
           $.ui.render(<box title=\"tsx\"><text>from-tsx</text></box>); }); }",
    );

    let runtime = load_dir(dir.path());

    let report = runtime.load_report();
    let names: Vec<&str> = report.iter().map(|info| info.name.as_str()).collect();
    assert_eq!(names, ["a.js", "b.ts", "c.tsx"]);
    assert!(report.iter().all(|info| info.loaded), "{report:?}");
    assert_eq!(runtime.hook_count(), 3);

    assert_eq!(
        fire(&runtime, "SessionStart", "").updated_input.as_deref(),
        Some("from-js")
    );
    assert_eq!(
        fire(&runtime, "Stop", "").updated_input.as_deref(),
        Some("from-ts")
    );
    let ui = fire(&runtime, "TurnEnd", "").ui;
    assert_eq!(
        ui,
        vec![UiElement::Box {
            title: Some("tsx".to_string()),
            children: vec![UiElement::Text {
                text: "from-tsx".to_string(),
                style: UiStyle::Plain,
            }],
        }]
    );
}

#[test]
fn a_tsx_mod_ui_renders_through_the_ui_crate() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "panel.tsx",
        "function register(on: any) { \
         on('PostToolUse', '*', function ($: any) { \
           $.ui.render(<box title=\"Build\"><text>bundle ok</text>\
             <button id=\"retry\">Retry</button></box>); }); }",
    );

    let runtime = load_dir(dir.path());
    let result = fire(&runtime, "PostToolUse", "Bash");
    assert!(!result.failed, "{result:?}");

    let rendered = coderex_ui::mod_elements::render_mod_ui_text(&result.ui, 40);
    assert!(rendered.contains("Build"), "{rendered}");
    assert!(rendered.contains("bundle ok"), "{rendered}");
    assert!(rendered.contains("[Retry]"), "{rendered}");
}

#[test]
fn the_load_report_hashes_the_bytes_on_disk() {
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let source =
        "function register(on) { on('Stop', '*', function () { return { stop: false }; }); }";
    write_mod(dir.path(), "hash.js", source);

    let runtime = load_dir(dir.path());
    let info = &runtime.load_report()[0];
    assert_eq!(info.sha256, hex::encode(Sha256::digest(source.as_bytes())));
    assert!(info.errors.is_empty(), "{info:?}");
}

#[test]
fn default_limits_are_the_documented_ones() {
    let limits = ModLimits::default();
    if std::env::var(MODS_MEMORY_LIMIT_ENV).is_err() {
        assert_eq!(limits.memory_limit, DEFAULT_MEMORY_LIMIT);
    }
    assert!(
        (256 * 1024..=8 * 1024 * 1024).contains(&limits.max_stack_size),
        "stack limit {} is outside the documented clamp",
        limits.max_stack_size
    );
    assert!(limits.gc_threshold <= limits.memory_limit);
}

#[test]
fn runaway_recursion_is_a_hook_error_and_the_runtime_survives() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "boom.js",
        "function register(on) { \
         on('PostToolUse', '*', function () { function f() { return f() + 1; } f(); }); \
         on('Stop', '*', function () { return { updatedInput: 'alive' }; }); }",
    );
    let runtime = load_dir_with(
        dir.path(),
        ModLimits {
            max_stack_size: 256 * 1024,
            ..ModLimits::default()
        },
    );

    let result = fire(&runtime, "PostToolUse", "Bash");
    assert!(result.failed, "{result:?}");
    assert!(!result.errors.is_empty(), "{result:?}");
    assert!(!runtime.is_disabled(), "a stack error is not a timeout");

    assert_eq!(
        fire(&runtime, "Stop", "").updated_input.as_deref(),
        Some("alive"),
        "the worker still answers after a stack error"
    );
}

#[test]
fn an_endless_loop_is_stopped_at_the_wall_clock_budget() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "loop.js",
        "function register(on) { \
         on('PostToolUse', '*', function () { while (true) {} }); \
         on('Stop', '*', function () { return { updatedInput: 'alive' }; }); }",
    );
    let runtime = load_dir_with(
        dir.path(),
        ModLimits {
            hook_timeout: Duration::from_millis(200),
            ..ModLimits::default()
        },
    );

    let started = Instant::now();
    let result = fire(&runtime, "PostToolUse", "Bash");
    let elapsed = started.elapsed();

    assert!(result.failed, "{result:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the budget did not stop the loop: {elapsed:?}"
    );
    assert_eq!(
        fire(&runtime, "Stop", "").updated_input.as_deref(),
        Some("alive"),
        "the runtime reloads the mods after an interrupted worker"
    );
}

#[test]
fn repeated_timeouts_disable_mods_for_the_session() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "loop.js",
        "function register(on) { \
         on('PostToolUse', '*', function () { while (true) {} }); \
         on('PreToolUse', '*', function () { return { stop: false }; }); }",
    );
    let runtime = load_dir_with(
        dir.path(),
        ModLimits {
            hook_timeout: Duration::from_millis(100),
            ..ModLimits::default()
        },
    );

    for _ in 0..DEFAULT_TIMEOUT_BREAKER_LIMIT {
        assert!(fire(&runtime, "PostToolUse", "Bash").failed);
    }

    assert!(runtime.is_disabled(), "the breaker tripped");
    assert!(
        runtime.timeout_abandonments() >= DEFAULT_TIMEOUT_BREAKER_LIMIT,
        "{:?}",
        runtime.timeout_abandonments()
    );
    assert!(runtime.disabled_reason().is_some());

    let gated = fire(&runtime, "PreToolUse", "Bash");
    assert!(gated.stop, "a gating call fails closed once mods are off");
    assert!(gated.failed);
}

#[test]
fn the_host_api_and_its_nested_snapshot_are_frozen() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "freeze.js",
        "function register(on) { on('SessionStart', '*', function ($) { \
         try { $.session.cwd = '/changed'; } catch (_) {} \
         try { $.session.surfaceCapabilities.mouse = false; } catch (_) {} \
         try { $.ui = null; } catch (_) {} \
         try { globalThis.$ = null; } catch (_) {} \
         return { updatedInput: [ \
           Object.isFrozen($), \
           Object.isFrozen($.ui), \
           Object.isFrozen($.session), \
           Object.isFrozen($.session.surfaceCapabilities), \
           $.session.cwd, \
           $.session.surfaceCapabilities.mouse, \
           typeof globalThis.$, \
           typeof $, \
         ].join('|') }; }); }",
    );

    let runtime = load_dir(dir.path());
    let result = fire(&runtime, "SessionStart", "");

    assert!(!result.failed, "{result:?}");
    assert_eq!(
        result.updated_input.as_deref(),
        Some("true|true|true|true|/work|true|object|object"),
        "every host object is frozen, including the nested capabilities"
    );
}

#[test]
fn next_returns_a_promise_and_async_hooks_can_await_it() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "async.js",
        "function register(on) { \
         on('PostToolUse', '*', async function ($, e, next) { \
           const pending = next(); \
           const thenable = typeof pending.then === 'function'; \
           const downstream = await pending; \
           return { updatedInput: thenable + ':' + downstream.updatedInput }; }); }",
    );
    let runtime = load_dir(dir.path());
    let inner = Arc::new(Inner::answering("inner"));

    let result = runtime.fire_with_inner(
        "PostToolUse",
        "Bash",
        &HashMap::new(),
        Some(inner.clone() as Arc<dyn HookExecutor>),
    );

    assert!(!result.failed, "{result:?}");
    assert_eq!(result.updated_input.as_deref(), Some("true:inner"));
    assert_eq!(inner.calls(), ["PostToolUse"]);
}

#[test]
fn a_rejection_after_await_is_reported_as_a_hook_error() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "reject.js",
        "function register(on) { \
         on('PostToolUse', '*', async function () { \
           await Promise.resolve(); \
           throw new Error('after-await'); }); }",
    );
    let runtime = load_dir(dir.path());

    let result = fire(&runtime, "PostToolUse", "Bash");

    assert!(result.failed, "{result:?}");
    assert!(
        result
            .errors
            .iter()
            .any(|error| error.contains("after-await")),
        "{:?}",
        result.errors
    );
}

#[test]
fn notify_and_submit_cross_the_worker_boundary() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "host.js",
        "function register(on) { on('PostToolUse', '*', function ($) { \
         $.session.notify('heads up'); \
         $.session.submit('follow-up'); }); }",
    );
    let runtime = load_dir(dir.path());

    let result = fire(&runtime, "PostToolUse", "Read");

    assert_eq!(
        result.ui,
        vec![UiElement::Text {
            text: "heads up".to_string(),
            style: UiStyle::Notice,
        }]
    );
    assert_eq!(result.pending_prompts, ["follow-up"]);
    assert_eq!(runtime.submit_queue().len(), 1);
    assert_eq!(runtime.submit_queue().take(), ["follow-up"]);
    assert!(runtime.submit_queue().is_empty());
}

#[test]
fn a_host_surface_is_passed_through_to_hooks() {
    use coderex_core::surface::SurfaceCapabilities;

    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "surface.js",
        "function register(on) { on('SessionStart', '*', function ($, e) { \
         return { updatedInput: e.surface + '|' + e.surfaceCapabilities.interactive + '|' \
           + e.surfaceCapabilities.mouse + '|' + e.surfaceCapabilities.images }; }); }",
    );
    let mut session = session();
    session.surface = SurfaceDescriptor::new("web", SurfaceCapabilities::TEXT_ONLY).unwrap();
    let runtime = ModRuntime::load_dir(dir.path(), session).expect("load");

    assert_eq!(
        fire(&runtime, "SessionStart", "").updated_input.as_deref(),
        Some("web|false|false|false"),
        "the mod sees the host's surface, not a hard-coded terminal"
    );
}

#[test]
fn an_activation_handler_runs_through_the_public_handle() {
    let dir = tempfile::tempdir().unwrap();
    write_mod(
        dir.path(),
        "button.js",
        "function register(on) { \
         $.ui.onActivate('retry', function ($, event) { \
           $.ui.render($.ui.button('Again', 'again')); \
           return $.ui.text('ran ' + event.id); }); }",
    );
    let runtime = load_dir(dir.path());

    let elements = runtime.activate("retry").expect("the handler runs");

    assert_eq!(
        elements,
        vec![
            UiElement::Button {
                label: "Again".to_string(),
                id: "again".to_string(),
            },
            UiElement::Text {
                text: "ran retry".to_string(),
                style: UiStyle::Plain,
            },
        ]
    );
}
