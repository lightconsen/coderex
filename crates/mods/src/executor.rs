//! The `HookExecutor` adapter: run JavaScript hooks, then the configured hooks.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use coderex_core::hook_executor::{HookExecutor, HookFireResult};

use crate::runtime::{ModRuntime, ModSession, dedupe_paths, is_gating_event};

/// A [`HookExecutor`] that runs in-process JavaScript hooks first and then an
/// optional inner executor.
///
/// The inner executor is normally a `coderex_hooks::HookRegistry`. When the
/// JavaScript hooks do not stop the event, the inner executor receives the
/// possibly updated input and its result is merged: either side can stop, the
/// inner side wins a field conflict (it ran later), watch paths are combined,
/// and errors are appended.
#[derive(Clone)]
pub struct JsHookExecutor {
    runtime: Arc<ModRuntime>,
    inner: Option<Arc<dyn HookExecutor>>,
}

impl fmt::Debug for JsHookExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JsHookExecutor")
            .field("runtime", &self.runtime)
            .field("has_inner", &self.inner.is_some())
            .finish()
    }
}

impl JsHookExecutor {
    /// Run `runtime`'s hooks with no inner executor.
    #[must_use]
    pub fn new(runtime: ModRuntime) -> Self {
        Self {
            runtime: Arc::new(runtime),
            inner: None,
        }
    }

    /// Run `runtime`'s hooks, then delegate to `inner` when they do not stop.
    #[must_use]
    pub fn with_inner(runtime: ModRuntime, inner: Arc<dyn HookExecutor>) -> Self {
        Self {
            runtime: Arc::new(runtime),
            inner: Some(inner),
        }
    }

    /// Attach or replace the inner executor.
    #[must_use]
    pub fn inner(mut self, inner: Arc<dyn HookExecutor>) -> Self {
        self.inner = Some(inner);
        self
    }

    /// Build the opt-in executor.
    ///
    /// Returns `None` when [`is_enabled`](crate::is_enabled) is false. When it
    /// is true, a load failure is returned to the caller rather than silently
    /// disabling an explicitly requested feature.
    pub fn load_from_env(
        session: ModSession,
        inner: Option<Arc<dyn HookExecutor>>,
    ) -> Option<Result<Self, String>> {
        ModRuntime::load_from_env(session).map(|loaded| {
            loaded.map(|runtime| match inner {
                Some(inner) => Self::with_inner(runtime, inner),
                None => Self::new(runtime),
            })
        })
    }

    /// The wrapped runtime.
    #[must_use]
    pub fn runtime(&self) -> &ModRuntime {
        &self.runtime
    }
}

#[async_trait::async_trait]
impl HookExecutor for JsHookExecutor {
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult {
        let runtime = Arc::clone(&self.runtime);
        let event = event_name.to_string();
        let js_input = input.to_string();
        let js_context = context.clone();
        let js_result =
            match tokio::task::spawn_blocking(move || runtime.fire(&event, &js_input, &js_context))
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    let mut failed = HookFireResult {
                        failed: true,
                        errors: vec![format!("the mod hook task failed: {error}")],
                        ..HookFireResult::default()
                    };
                    if is_gating_event(event_name) {
                        failed.stop = true;
                    }
                    failed
                }
            };

        if js_result.stop {
            return js_result;
        }

        let Some(inner) = &self.inner else {
            return js_result;
        };

        // A JavaScript hook that rewrote the input is rewriting what the rest
        // of the chain sees, so the inner executor runs on the new value.
        let inner_input = js_result
            .updated_input
            .clone()
            .unwrap_or_else(|| input.to_string());
        let inner_result = inner.fire_hook(event_name, &inner_input, context).await;
        merge_results(js_result, inner_result)
    }

    fn watch_paths(&self) -> Vec<String> {
        self.inner
            .as_ref()
            .map_or_else(Vec::new, |inner| inner.watch_paths())
    }
}

/// Merge the JavaScript result with the inner executor's result.
///
/// Both sides ran, so `stop` and `failed` are disjunctions. The inner executor
/// ran second and wins an `updated_*` conflict; a side that left the field
/// unset does not erase what the other set. Watch paths are combined and
/// deduplicated in first-seen order.
fn merge_results(js: HookFireResult, inner: HookFireResult) -> HookFireResult {
    let mut watch_paths = js.watch_paths;
    watch_paths.extend(inner.watch_paths);
    let mut errors = js.errors;
    errors.extend(inner.errors);

    HookFireResult {
        stop: js.stop || inner.stop,
        updated_input: inner.updated_input.or(js.updated_input),
        updated_tool_output: inner.updated_tool_output.or(js.updated_tool_output),
        errors,
        failed: js.failed || inner.failed,
        watch_paths: dedupe_paths(watch_paths),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::runtime::ModLimits;

    /// Records what the inner executor was asked, and answers with a scripted
    /// result.
    #[derive(Debug, Default)]
    struct Inner {
        calls: Mutex<Vec<(String, String)>>,
        result: Mutex<HookFireResult>,
    }

    impl Inner {
        fn answering(result: HookFireResult) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(result),
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HookExecutor for Inner {
        async fn fire_hook(
            &self,
            event_name: &str,
            input: &str,
            _context: &HashMap<String, String>,
        ) -> HookFireResult {
            self.calls
                .lock()
                .unwrap()
                .push((event_name.to_string(), input.to_string()));
            self.result.lock().unwrap().clone()
        }
    }

    fn session() -> ModSession {
        ModSession::default()
    }

    fn runtime_with(source: &str) -> ModRuntime {
        runtime_with_limits(source, ModLimits::default())
    }

    fn runtime_with_limits(source: &str, limits: ModLimits) -> ModRuntime {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("test.js"), source).unwrap();
        ModRuntime::load_dir_with_limits(directory.path(), session(), limits).unwrap()
    }

    fn directory_with(source: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("test.js"), source).unwrap();
        directory
    }

    #[tokio::test]
    async fn js_hooks_run_first_and_the_inner_receives_the_updated_input() {
        let inner = Arc::new(Inner::answering(HookFireResult {
            updated_tool_output: Some("inner-output".to_string()),
            watch_paths: vec!["/tmp/inner".to_string()],
            ..HookFireResult::default()
        }));
        let executor = JsHookExecutor::with_inner(
            runtime_with(
                "function register(on) { on('PostToolUse', '*', function () { \
                 return { updatedInput: 'js-input', updatedToolOutput: 'js-output', \
                 watchPaths: ['/tmp/js', '/tmp/inner'] }; }); }",
            ),
            Arc::clone(&inner) as Arc<dyn HookExecutor>,
        );

        let result = executor
            .fire_hook("PostToolUse", "Bash", &HashMap::new())
            .await;

        assert_eq!(
            inner.calls(),
            vec![("PostToolUse".to_string(), "js-input".to_string())]
        );
        assert_eq!(result.updated_input.as_deref(), Some("js-input"));
        assert_eq!(
            result.updated_tool_output.as_deref(),
            Some("inner-output"),
            "the inner executor ran later and should win the conflict"
        );
        assert_eq!(result.watch_paths, vec!["/tmp/js", "/tmp/inner"]);
        assert!(!result.failed);
    }

    #[tokio::test]
    async fn a_js_stop_does_not_reach_the_inner_executor() {
        let inner = Arc::new(Inner::answering(HookFireResult::default()));
        let executor = JsHookExecutor::with_inner(
            runtime_with(
                "function register(on) { on('PreToolUse', 'Bash', function () { return { stop: true }; }); }",
            ),
            Arc::clone(&inner) as Arc<dyn HookExecutor>,
        );

        let result = executor
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;

        assert!(result.stop);
        assert_eq!(inner.calls().len(), 0);
    }

    #[tokio::test]
    async fn an_inner_stop_is_merged() {
        let inner = Arc::new(Inner::answering(HookFireResult {
            stop: true,
            ..HookFireResult::default()
        }));
        let executor = JsHookExecutor::with_inner(
            runtime_with(
                "function register(on) { on('PreToolUse', 'Bash', function () { return {}; }); }",
            ),
            Arc::clone(&inner) as Arc<dyn HookExecutor>,
        );

        let result = executor
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;

        assert!(result.stop);
        assert_eq!(inner.calls().len(), 1);
    }

    #[tokio::test]
    async fn no_matching_js_hook_still_delegates() {
        let inner = Arc::new(Inner::answering(HookFireResult {
            updated_input: Some("inner-input".to_string()),
            ..HookFireResult::default()
        }));
        let executor = JsHookExecutor::with_inner(
            runtime_with(
                "function register(on) { on('PreToolUse', 'Bash', function () { return { stop: true }; }); }",
            ),
            Arc::clone(&inner) as Arc<dyn HookExecutor>,
        );

        let result = executor
            .fire_hook("PreToolUse", "Write", &HashMap::new())
            .await;

        assert_eq!(
            inner.calls(),
            vec![("PreToolUse".to_string(), "Write".to_string())]
        );
        assert_eq!(result.updated_input.as_deref(), Some("inner-input"));
    }

    #[tokio::test]
    async fn a_js_failure_is_fail_closed_on_a_gating_event_and_still_reaches_the_inner() {
        // A broken hook on a non-gating event is recorded, and the rest of the
        // chain still runs.
        let inner = Arc::new(Inner::answering(HookFireResult::default()));
        let executor = JsHookExecutor::with_inner(
            runtime_with_limits(
                "function register(on) { on('PostToolUse', '*', function () { while (true) {} }); }",
                ModLimits {
                    max_loop_iterations: 10_000,
                    hook_timeout: std::time::Duration::from_secs(5),
                    ..ModLimits::default()
                },
            ),
            Arc::clone(&inner) as Arc<dyn HookExecutor>,
        );

        let result = executor
            .fire_hook("PostToolUse", "Bash", &HashMap::new())
            .await;

        assert!(result.failed);
        assert!(!result.stop, "PostToolUse is not a gating event");
        assert_eq!(inner.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_js_failure_stops_a_gating_event() {
        let executor = JsHookExecutor::new(runtime_with_limits(
            "function register(on) { on('PreToolUse', 'Bash', function () { throw new Error('broken'); }); }",
            ModLimits::default(),
        ));

        let result = executor
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;

        assert!(result.failed);
        assert!(result.stop);
    }

    #[tokio::test]
    async fn the_inner_executors_watch_paths_pass_through() {
        struct Watching;

        #[async_trait::async_trait]
        impl HookExecutor for Watching {
            async fn fire_hook(
                &self,
                _event_name: &str,
                _input: &str,
                _context: &HashMap<String, String>,
            ) -> HookFireResult {
                HookFireResult::default()
            }

            fn watch_paths(&self) -> Vec<String> {
                vec!["/tmp/watched".to_string()]
            }
        }

        let directory = directory_with("function register(on) {}");
        let runtime = ModRuntime::load_dir(directory.path(), session()).unwrap();
        let executor = JsHookExecutor::with_inner(runtime, Arc::new(Watching));

        assert_eq!(executor.watch_paths(), vec!["/tmp/watched"]);
    }
}
