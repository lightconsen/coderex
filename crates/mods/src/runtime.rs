//! Loading mods and firing their hooks on a long-lived worker.
//!
//! A `boa_engine::Context` is neither `Send` nor safe to share, and `next(e)`
//! needs the same context to resume in after the downstream chain has run.
//! Each [`ModRuntime`] therefore owns one worker thread, one `Context`, and one
//! realm per loaded mod for as long as it lives. [`ModRuntime::fire`] sends a
//! request over a channel, waits for the reply under the wall-clock budget, and
//! never touches the engine directly.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use boa_engine::{
    Context, Finalize, JsObject, JsResult, JsString, JsValue, NativeFunction, Source, Trace,
    js_string,
    object::ObjectInitializer,
    object::builtins::JsArray,
    property::{Attribute, PropertyKey},
};
use coderex_core::hook_executor::{HookExecutor, HookFireResult};

/// Environment variable that opts the in-process runtime in.
pub const MODS_ENV: &str = "CODEREX_MODS";

/// Environment variable that overrides the mods directory.
pub const MODS_DIR_ENV: &str = "CODEREX_MODS_DIR";

/// Environment variables a mod may read through `$.env(name)`.
///
/// The list is deliberately small and contains only locale, terminal and
/// platform hints. It excludes credentials, machine identity, home and working
/// directories, and everything under `CODEREX_*`. Names not in this list
/// return `null`, whatever their value.
pub const ALLOWED_ENV_VARS: &[&str] = &[
    "LANG",
    "LC_ALL",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "OS",
    "CI",
];

/// Most bytes a single mod file may have.
const MAX_MOD_BYTES: u64 = 1_048_576;

/// Most hooks a single mod may register.
const MAX_HOOKS_PER_MOD: usize = 256;

/// The session facts a mod may read through `$.session`.
///
/// These are captured when the runtime is constructed so a mod cannot observe
/// mutable engine state, and so a hook running on another thread reads a
/// consistent snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModSession {
    /// The session's working directory.
    pub cwd: String,
    /// The model the session is using.
    pub model: String,
    /// The session's permission mode.
    pub permission_mode: String,
}

/// Limits every hook call is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModLimits {
    /// Wall-clock budget for one [`ModRuntime::fire`] call.
    ///
    /// The budget covers the whole chain: every matching JavaScript hook and,
    /// when a hook calls `next`, the downstream (inner) executor as well.
    /// `boa_engine` 0.20 has no interrupt API, so this is enforced by the
    /// calling thread: when the budget expires the call is reported as an
    /// error and the worker is detached. The engine limits below are what
    /// guarantee that it eventually terminates.
    pub hook_timeout: Duration,
    /// Most loop iterations one JavaScript frame may perform.
    ///
    /// This is the engine's instruction-shaped ceiling and the reason an
    /// infinite loop terminates even when the wall-clock budget has not fired.
    pub max_loop_iterations: u64,
    /// Most nested JavaScript calls.
    pub recursion_limit: usize,
    /// Most values one JavaScript stack may hold.
    pub stack_size_limit: usize,
}

impl Default for ModLimits {
    fn default() -> Self {
        Self {
            hook_timeout: Duration::from_secs(5),
            max_loop_iterations: 10_000_000,
            recursion_limit: 256,
            stack_size_limit: 10 * 1024,
        }
    }
}

/// Whether the in-process runtime should be constructed.
///
/// It is off by default. `CODEREX_MODS` accepts `1`, `true`, `yes` or `on`
/// (case-insensitive); setting `CODEREX_MODS_DIR` to a non-empty path also
/// opts in.
#[must_use]
pub fn is_enabled() -> bool {
    env_flag(MODS_ENV) || std::env::var_os(MODS_DIR_ENV).is_some_and(|value| !value.is_empty())
}

/// The directory mods are loaded from.
///
/// `CODEREX_MODS_DIR` wins when set and non-empty; otherwise this is
/// `config_home()/mods` (`~/.coderex/mods` unless `CLAUDE_CONFIG_DIR` says
/// otherwise).
#[must_use]
pub fn mods_dir() -> PathBuf {
    std::env::var_os(MODS_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(|| coderex_config::config_home().join("mods"), PathBuf::from)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// One mod file to load into the worker.
#[derive(Debug, Clone)]
struct ModSource {
    name: String,
    path: PathBuf,
    source: String,
}

/// One registration, pointing at a hook function in the worker's registry.
#[derive(Debug, Clone)]
struct WorkerHook {
    /// Index of the mod's registration array in the main realm's registry.
    mod_index: usize,
    /// Index of the hook within that array.
    hook_index: usize,
    event: String,
    matcher: String,
}

/// What a single hook asked the host to do.
#[derive(Debug, Clone, Default)]
struct HookEffect {
    stop: bool,
    updated_input: Option<String>,
    updated_tool_output: Option<String>,
    watch_paths: Vec<String>,
}

/// Everything the worker needs to (re)load the mods.
#[derive(Debug)]
struct WorkerConfig {
    mods: Vec<ModSource>,
    session: ModSession,
    limits: ModLimits,
}

/// The metadata the worker reports once it has loaded the mods.
#[derive(Debug)]
struct InitOutcome {
    hooks: Vec<WorkerHook>,
    load_errors: Vec<String>,
}

/// A request from the caller to the worker.
struct Request {
    event: String,
    input: String,
    context: HashMap<String, String>,
    inner: Option<Arc<dyn HookExecutor>>,
    reply: mpsc::SyncSender<HookFireResult>,
}

/// The caller's handle to the worker thread.
#[derive(Debug)]
struct WorkerHandle {
    sender: mpsc::Sender<Request>,
}

/// Loads mods and fires their hooks.
///
/// The JavaScript engine lives on a dedicated worker thread that owns one
/// `Context` and one realm per mod for the lifetime of the runtime. What
/// remains here is strings, configuration and a channel, which is what lets
/// [`JsHookExecutor`](crate::JsHookExecutor) be `Send + Sync`.
#[derive(Debug)]
pub struct ModRuntime {
    dir: PathBuf,
    session: ModSession,
    limits: ModLimits,
    hook_count: usize,
    load_errors: Vec<String>,
    config: Arc<WorkerConfig>,
    worker: Mutex<Option<WorkerHandle>>,
}

impl ModRuntime {
    /// Load every `*.js` file in `path`.
    ///
    /// A missing directory is an empty runtime: opting in before creating the
    /// directory is not an error. A file that cannot be read, is too large, or
    /// fails to evaluate or register is reported once through `tracing::warn`
    /// and in [`load_errors`](Self::load_errors), and is skipped; the rest
    /// still load.
    ///
    /// # Errors
    ///
    /// Fails when the directory exists but cannot be listed, or when the
    /// worker thread cannot be started.
    pub fn load_dir(path: &Path, session: ModSession) -> Result<Self, String> {
        Self::load_dir_with_limits(path, session, ModLimits::default())
    }

    /// Load with explicit limits. See [`load_dir`](Self::load_dir).
    ///
    /// # Errors
    ///
    /// Fails when the directory exists but cannot be listed, or when the
    /// worker thread cannot be started.
    pub fn load_dir_with_limits(
        path: &Path,
        session: ModSession,
        limits: ModLimits,
    ) -> Result<Self, String> {
        let files = collect_mod_files(path)?;

        let mut load_errors = Vec::new();
        let mut mods = Vec::new();
        for (name, file_path) in files {
            match read_source(&file_path) {
                Ok(source) => mods.push(ModSource {
                    name,
                    path: file_path,
                    source,
                }),
                Err(error) => {
                    let message = format!("{}: {error}", file_path.display());
                    tracing::warn!("{message}");
                    load_errors.push(message);
                }
            }
        }

        let config = Arc::new(WorkerConfig {
            mods,
            session: session.clone(),
            limits,
        });
        let (sender, outcome) = spawn_worker(Arc::clone(&config))?;
        for error in &outcome.load_errors {
            tracing::warn!("{error}");
        }
        load_errors.extend(outcome.load_errors);

        Ok(Self {
            dir: path.to_path_buf(),
            session,
            limits,
            hook_count: outcome.hooks.len(),
            load_errors,
            config,
            worker: Mutex::new(Some(WorkerHandle { sender })),
        })
    }

    /// Load from the opt-in environment.
    ///
    /// Returns `None` when [`is_enabled`] is false, so the default is "no
    /// runtime at all".
    pub fn load_from_env(session: ModSession) -> Option<Result<Self, String>> {
        if !is_enabled() {
            return None;
        }
        Some(Self::load_dir(&mods_dir(), session))
    }

    /// The directory this runtime was loaded from.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The session snapshot mods see as `$.session`.
    #[must_use]
    pub const fn session(&self) -> &ModSession {
        &self.session
    }

    /// The limits in force.
    #[must_use]
    pub const fn limits(&self) -> ModLimits {
        self.limits
    }

    /// How many hooks loaded across every mod.
    #[must_use]
    pub const fn hook_count(&self) -> usize {
        self.hook_count
    }

    /// Files that were skipped at load, with the reason. Each appears once.
    #[must_use]
    pub fn load_errors(&self) -> &[String] {
        &self.load_errors
    }

    /// Fire `event` against every matching hook and merge what they return,
    /// with no downstream executor installed.
    ///
    /// With no inner executor, `next(e)` still works but reports an empty
    /// result (`stop: false`, no updated values, no watch paths, no errors).
    /// Use [`fire_with_inner`](Self::fire_with_inner) to run a downstream
    /// chain.
    #[must_use]
    pub fn fire(
        &self,
        event: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult {
        self.fire_with_inner(event, input, context, None)
    }

    /// Fire `event` against every matching hook, with `inner` available as the
    /// downstream chain behind `next(e)`.
    ///
    /// Hooks run in load order (files sorted by name, then registration
    /// order). A hook that does not call `next` behaves exactly as before: its
    /// return value is merged into the result. A hook that calls `next`
    /// receives the downstream result and its own non-empty fields win over
    /// it; `stop` is OR-ed and watch paths are deduplicated. When no hook
    /// called `next`, `inner` runs after the JavaScript hooks with the
    /// possibly updated input, exactly like a plain
    /// [`JsHookExecutor`](crate::JsHookExecutor) delegation.
    ///
    /// An exception, timeout or engine limit becomes an entry in
    /// [`HookFireResult::errors`] and sets [`failed`](HookFireResult::failed);
    /// other hooks still run. On a gating event (`PreToolUse`,
    /// `UserPromptSubmit`) a failure also sets `stop`, so a broken hook cannot
    /// read as an allow. The wall-clock budget covers the whole call,
    /// including any downstream run.
    #[must_use]
    pub fn fire_with_inner(
        &self,
        event: &str,
        input: &str,
        context: &HashMap<String, String>,
        inner: Option<Arc<dyn HookExecutor>>,
    ) -> HookFireResult {
        let (reply, reply_receiver) = mpsc::sync_channel(1);
        let request = Request {
            event: event.to_string(),
            input: input.to_string(),
            context: context.clone(),
            inner,
            reply,
        };

        let result = self.dispatch(request, &reply_receiver);
        match result {
            Ok(result) => result,
            Err(error) => {
                let mut failed = HookFireResult {
                    failed: true,
                    errors: vec![error],
                    ..HookFireResult::default()
                };
                if is_gating_event(event) {
                    failed.stop = true;
                }
                failed
            }
        }
    }

    fn dispatch(
        &self,
        request: Request,
        reply_receiver: &mpsc::Receiver<HookFireResult>,
    ) -> Result<HookFireResult, String> {
        let sender = self.worker_sender()?;
        sender
            .send(request)
            .map_err(|_| "the mod worker stopped before the hook could run".to_string())?;

        match reply_receiver.recv_timeout(self.limits.hook_timeout) {
            Ok(result) => Ok(result),
            Err(RecvTimeoutError::Timeout) => {
                self.abandon_worker();
                Err(format!(
                    "the hook exceeded its {} ms wall-clock budget",
                    self.limits.hook_timeout.as_millis()
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.abandon_worker();
                Err("the mod worker stopped without reporting a result".to_string())
            }
        }
    }

    /// The live worker's sender, starting a fresh worker after a timeout.
    ///
    /// The lock deliberately covers the check-and-start sequence: releasing it
    /// between the check and the spawn would let two callers start two
    /// workers.
    #[allow(clippy::significant_drop_tightening)]
    fn worker_sender(&self) -> Result<mpsc::Sender<Request>, String> {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = worker.as_ref() {
            return Ok(handle.sender.clone());
        }

        // The previous worker was abandoned (it timed out) or stopped. Start
        // a fresh realm; the load outcome is already recorded on this runtime.
        let (sender, _outcome) = spawn_worker(Arc::clone(&self.config))?;
        *worker = Some(WorkerHandle {
            sender: sender.clone(),
        });
        Ok(sender)
    }

    /// Drop the worker handle. The detached worker exits when its request
    /// channel closes, which happens as soon as the engine returns.
    fn abandon_worker(&self) {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *worker = None;
    }
}

/// Builds a runtime with an explicit session and limits.
#[derive(Debug, Clone)]
pub struct ModRuntimeBuilder {
    session: ModSession,
    limits: ModLimits,
}

impl ModRuntimeBuilder {
    /// Start with the given session snapshot.
    #[must_use]
    pub const fn new(session: ModSession) -> Self {
        Self {
            session,
            limits: ModLimits {
                hook_timeout: Duration::from_secs(5),
                max_loop_iterations: 10_000_000,
                recursion_limit: 256,
                stack_size_limit: 10 * 1024,
            },
        }
    }

    /// Replace the execution limits.
    #[must_use]
    pub const fn limits(mut self, limits: ModLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Load `path` with the configured session and limits.
    ///
    /// # Errors
    ///
    /// Fails when the directory exists but cannot be listed, or when the
    /// worker thread cannot be started.
    pub fn load_dir(self, path: &Path) -> Result<ModRuntime, String> {
        ModRuntime::load_dir_with_limits(path, self.session, self.limits)
    }
}

/// List the `*.js` files directly in `path`, sorted by file name.
fn collect_mod_files(path: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut files = Vec::new();
    match std::fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|error| {
                    format!("could not read an entry in {}: {error}", path.display())
                })?;
                let file_path = entry.path();
                if !file_path.is_file() {
                    continue;
                }
                let is_javascript = file_path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("js"));
                if !is_javascript {
                    continue;
                }
                let name = file_path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("<unnamed>")
                    .to_string();
                files.push((name, file_path));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(dir = %path.display(), "mods directory does not exist");
        }
        Err(error) => {
            return Err(format!(
                "could not read the mods directory {}: {error}",
                path.display()
            ));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn read_source(path: &Path) -> Result<String, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("could not inspect the file: {error}"))?;
    if metadata.len() > MAX_MOD_BYTES {
        return Err(format!(
            "the file is larger than the {MAX_MOD_BYTES}-byte limit"
        ));
    }
    std::fs::read_to_string(path).map_err(|error| format!("could not read the file: {error}"))
}

/// Start a worker thread and wait until it has loaded the mods.
fn spawn_worker(config: Arc<WorkerConfig>) -> Result<(mpsc::Sender<Request>, InitOutcome), String> {
    let (sender, receiver) = mpsc::channel();
    let (init_sender, init_receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("coderex-mods".to_string())
        .spawn(move || worker_main(&config, &receiver, &init_sender))
        .map_err(|error| format!("could not start the mod worker: {error}"))?;

    match init_receiver.recv() {
        Ok(Ok(outcome)) => Ok((sender, outcome)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("the mod worker stopped during loading".to_string()),
    }
}

fn worker_main(
    config: &WorkerConfig,
    receiver: &mpsc::Receiver<Request>,
    init: &mpsc::SyncSender<Result<InitOutcome, String>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            let _ = init.send(Err(format!(
                "could not start the mod worker runtime: {error}"
            )));
            return;
        }
    };

    let mut state = match WorkerState::load(config, runtime) {
        Ok(state) => state,
        Err(error) => {
            let _ = init.send(Err(error));
            return;
        }
    };

    let outcome = InitOutcome {
        hooks: state.hooks.clone(),
        load_errors: state.load_errors.clone(),
    };
    if init.send(Ok(outcome)).is_err() {
        return;
    }

    while let Ok(request) = receiver.recv() {
        let result = state.fire(&request);
        let _ = request.reply.send(result);
    }
}

/// The worker thread's engine state.
struct WorkerState {
    context: Context,
    runtime: Arc<tokio::runtime::Runtime>,
    hooks: Vec<WorkerHook>,
    load_errors: Vec<String>,
    mod_names: Vec<String>,
}

impl WorkerState {
    /// Build one context, one realm per mod, and the flattened hook list.
    fn load(config: &WorkerConfig, runtime: Arc<tokio::runtime::Runtime>) -> Result<Self, String> {
        let mut context = Context::default();
        apply_limits(&mut context, &config.limits);
        context
            .eval(Source::from_bytes(MAIN_HARNESS.as_bytes()))
            .map_err(|error| format!("could not install the runtime harness: {error}"))?;

        let registry = context
            .global_object()
            .get(js_string!("__coderExMods"), &mut context)
            .map_err(|error| format!("could not read the runtime harness: {error}"))?;
        let registry = registry
            .as_object()
            .cloned()
            .and_then(|object| JsArray::from_object(object).ok())
            .ok_or_else(|| "the runtime harness registry is missing".to_string())?;

        let mut hooks = Vec::new();
        let mut load_errors = Vec::new();
        let mut mod_names = Vec::new();

        for source in &config.mods {
            match load_mod(&mut context, source, &config.session) {
                Ok(registrations) => {
                    registry
                        .push(registrations.clone(), &mut context)
                        .map_err(|error| {
                            format!("could not register {}: {error}", source.path.display())
                        })?;
                    let array = registrations
                        .as_object()
                        .cloned()
                        .and_then(|object| JsArray::from_object(object).ok())
                        .ok_or_else(|| {
                            format!(
                                "{}: register() left an unreadable registration list",
                                source.path.display()
                            )
                        })?;
                    let length = array.length(&mut context).map_err(|error| {
                        format!("could not read {}: {error}", source.path.display())
                    })?;

                    let mod_index = mod_names.len();
                    for hook_index in 0..length {
                        let Ok(hook_index) = i64::try_from(hook_index) else {
                            continue;
                        };
                        let entry = array.at(hook_index, &mut context).map_err(|error| {
                            format!("could not read {}: {error}", source.path.display())
                        })?;
                        let (Some(event), Some(matcher)) = (
                            js_string_field(&entry, "event", &mut context),
                            js_string_field(&entry, "matcher", &mut context),
                        ) else {
                            continue;
                        };
                        hooks.push(WorkerHook {
                            mod_index,
                            hook_index: usize::try_from(hook_index).unwrap_or_default(),
                            event,
                            matcher,
                        });
                    }
                    mod_names.push(source.name.clone());
                }
                Err(error) => {
                    load_errors.push(format!("{}: {error}", source.path.display()));
                }
            }
        }

        Ok(Self {
            context,
            runtime,
            hooks,
            load_errors,
            mod_names,
        })
    }

    /// Run every matching JavaScript hook for one request.
    fn fire(&mut self, request: &Request) -> HookFireResult {
        let context_json = serde_json::to_string(&request.context).unwrap_or_else(|_| "{}".into());
        let matching: Vec<WorkerHook> = self
            .hooks
            .iter()
            .filter(|hook| {
                event_matches(&hook.event, &request.event)
                    && matcher_matches(&hook.matcher, &request.input)
            })
            .cloned()
            .collect();

        let mut combined = HookFireResult::default();
        let mut next_called = false;

        for hook in &matching {
            let next = Arc::new(NextCall::new(
                request.inner.clone(),
                Arc::clone(&self.runtime),
                request.event.clone(),
                request.input.clone(),
                request.context.clone(),
            ));
            let outcome =
                self.invoke_hook(hook, &request.event, &request.input, &context_json, &next);
            next.close();
            match outcome {
                Ok(effect) => {
                    if let Some(downstream) = next.take_downstream() {
                        next_called = true;
                        apply_result(
                            &mut combined,
                            merge_effect_with_downstream(effect, downstream),
                        );
                    } else {
                        apply_effect(&mut combined, effect);
                    }
                }
                Err(error) => {
                    if let Some(downstream) = next.take_downstream() {
                        next_called = true;
                        apply_result(&mut combined, downstream);
                    }
                    combined.failed = true;
                    combined.errors.push(format!(
                        "mod `{}`: {error}",
                        self.mod_names
                            .get(hook.mod_index)
                            .map_or("<unknown>", String::as_str)
                    ));
                }
            }
        }

        if combined.failed && is_gating_event(&request.event) {
            combined.stop = true;
        }

        if !next_called && !combined.stop {
            if let Some(inner) = &request.inner {
                let inner_input = combined
                    .updated_input
                    .clone()
                    .unwrap_or_else(|| request.input.clone());
                let inner_result = self.runtime.block_on(inner.fire_hook(
                    &request.event,
                    &inner_input,
                    &request.context,
                ));
                combined = merge_with_inner(combined, inner_result);
            }
        }

        combined.watch_paths = dedupe_paths(combined.watch_paths);
        combined
    }

    /// Invoke one hook in its realm through the main-realm harness.
    fn invoke_hook(
        &mut self,
        hook: &WorkerHook,
        event: &str,
        input: &str,
        context_json: &str,
        next: &Arc<NextCall>,
    ) -> Result<HookEffect, String> {
        let next_function = NativeFunction::from_copy_closure_with_captures(
            next_entry,
            NextCapture {
                call: Arc::clone(next),
            },
        );
        let realm = self.context.realm().clone();
        let next_function = JsObject::from(next_function.to_js_function(&realm));

        let helper = self
            .context
            .global_object()
            .get(js_string!("__coderExCall"), &mut self.context)
            .map_err(|error| format!("could not read the runtime harness: {error}"))?;
        let callable = helper
            .as_callable()
            .cloned()
            .ok_or_else(|| "the runtime harness is missing".to_string())?;

        let args = [
            JsValue::from(hook.mod_index),
            JsValue::from(hook.hook_index),
            JsString::from(event).into(),
            JsString::from(input).into(),
            JsString::from(context_json).into(),
            JsValue::from(next_function),
        ];
        let value = callable
            .call(&JsValue::undefined(), &args, &mut self.context)
            .map_err(|error| format!("the hook could not be called: {error}"))?;
        let text = value
            .to_string(&mut self.context)
            .map_err(|error| format!("could not read the hook result: {error}"))?
            .to_std_string_escaped();
        let reply: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("the hook returned unreadable output: {error}"))?;
        parse_reply(&reply)
    }
}

/// Evaluate one mod in its own realm and return its registration array.
fn load_mod(
    context: &mut Context,
    source: &ModSource,
    session: &ModSession,
) -> Result<JsValue, String> {
    let realm = context
        .create_realm()
        .map_err(|error| format!("could not create a realm: {error}"))?;
    let previous = context.enter_realm(realm);
    let result = load_mod_in_realm(context, source, session);
    context.enter_realm(previous);
    result
}

fn load_mod_in_realm(
    context: &mut Context,
    source: &ModSource,
    session: &ModSession,
) -> Result<JsValue, String> {
    install_host_api(context, session, &source.name)
        .map_err(|error| format!("could not install the host API: {error}"))?;

    context
        .eval(Source::from_bytes(source.source.as_bytes()))
        .map_err(|error| format!("could not evaluate the mod: {error}"))?;

    install_mod_harness(context)
        .map_err(|error| format!("could not install the runtime harness: {error}"))?;

    let has_register = context
        .eval(Source::from_bytes("typeof register === 'function'"))
        .map_err(|error| format!("could not inspect the mod: {error}"))?
        .to_boolean();
    if !has_register {
        return Err("the mod does not define a global register(on, options) function".to_string());
    }

    let options = js_string_literal(&source.name);
    context
        .eval(Source::from_bytes(
            format!("register(on, Object.freeze({{ name: {options} }}))").as_bytes(),
        ))
        .map_err(|error| format!("register() failed: {error}"))?;

    context
        .eval(Source::from_bytes("__coderExRegs"))
        .map_err(|error| format!("register() did not leave readable hook registrations: {error}"))
}

/// A `next(e)` invocation, shared between the native function and the worker.
struct NextCall {
    used: AtomicBool,
    inner: Option<Arc<dyn HookExecutor>>,
    runtime: Arc<tokio::runtime::Runtime>,
    event: String,
    input: String,
    context: HashMap<String, String>,
    downstream: Mutex<Option<HookFireResult>>,
}

impl NextCall {
    fn new(
        inner: Option<Arc<dyn HookExecutor>>,
        runtime: Arc<tokio::runtime::Runtime>,
        event: String,
        input: String,
        context: HashMap<String, String>,
    ) -> Self {
        Self {
            used: AtomicBool::new(false),
            inner,
            runtime,
            event,
            input,
            context,
            downstream: Mutex::new(None),
        }
    }

    /// The downstream result, if this hook successfully called `next`.
    fn take_downstream(&self) -> Option<HookFireResult> {
        self.downstream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Expire the handle after its hook has returned.
    ///
    /// A hook that stashed `next` somewhere and calls it during a later event
    /// gets the single-use error instead of running the stale downstream
    /// chain.
    fn close(&self) {
        self.used.store(true, Ordering::SeqCst);
    }

    /// The body of `next(e)`: run the inner executor synchronously and return
    /// its result as a JavaScript object.
    fn invoke(&self, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        if self.used.swap(true, Ordering::SeqCst) {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("next(e) may only be called once per hook invocation")
                .into());
        }

        let mut input = self.input.clone();
        let mut inner_context = self.context.clone();
        if let Some(argument) = args.first() {
            if let Some(text) = argument.as_string() {
                input = text.to_std_string_escaped();
            } else if let Some(object) = argument.as_object() {
                if let Ok(value) = object.get(js_string!("input"), context) {
                    if let Some(text) = value.as_string() {
                        input = text.to_std_string_escaped();
                    }
                }
                if let Ok(value) = object.get(js_string!("context"), context) {
                    if let Some(modified) = context_from_value(&value, context) {
                        inner_context = modified;
                    }
                }
            }
        }

        let result = self
            .inner
            .as_ref()
            .map_or_else(HookFireResult::default, |inner| {
                self.runtime
                    .block_on(inner.fire_hook(&self.event, &input, &inner_context))
            });
        *self
            .downstream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result.clone());

        Ok(result_to_js(&result, context))
    }
}

/// The capture stored in the native `next` function.
///
/// It holds only Rust-owned state (an `Arc`), never a garbage-collected handle,
/// so it is safe to leave untraced.
#[derive(Finalize, Trace)]
struct NextCapture {
    #[unsafe_ignore_trace]
    call: Arc<NextCall>,
}

fn next_entry(
    _this: &JsValue,
    args: &[JsValue],
    capture: &NextCapture,
    context: &mut Context,
) -> JsResult<JsValue> {
    capture.call.invoke(args, context)
}

/// Read a plain object of string context entries.
fn context_from_value(value: &JsValue, context: &mut Context) -> Option<HashMap<String, String>> {
    let object = value.as_object()?;
    let keys = object.own_property_keys(context).ok()?;
    let mut entries = HashMap::new();
    for key in keys {
        let name = match &key {
            PropertyKey::String(name) => name.to_std_string_escaped(),
            PropertyKey::Index(index) => index.get().to_string(),
            PropertyKey::Symbol(_) => continue,
        };
        if let Ok(value) = object.get(key.clone(), context) {
            if let Some(text) = value.as_string() {
                entries.insert(name, text.to_std_string_escaped());
            }
        }
    }
    Some(entries)
}

/// Convert a downstream result into the object `next(e)` returns to JavaScript.
fn result_to_js(result: &HookFireResult, context: &mut Context) -> JsValue {
    let watch_paths = JsArray::from_iter(
        result
            .watch_paths
            .iter()
            .map(|path| JsString::from(path.clone()).into()),
        context,
    );
    let errors = JsArray::from_iter(
        result
            .errors
            .iter()
            .map(|error| JsString::from(error.clone()).into()),
        context,
    );

    let object = ObjectInitializer::new(context)
        .property(js_string!("stop"), result.stop, Attribute::all())
        .property(
            js_string!("updatedInput"),
            optional_string(result.updated_input.as_ref()),
            Attribute::all(),
        )
        .property(
            js_string!("updatedToolOutput"),
            optional_string(result.updated_tool_output.as_ref()),
            Attribute::all(),
        )
        .property(
            js_string!("watchPaths"),
            JsObject::from(watch_paths),
            Attribute::all(),
        )
        .property(
            js_string!("errors"),
            JsObject::from(errors),
            Attribute::all(),
        )
        .property(js_string!("failed"), result.failed, Attribute::all())
        .build();
    object.into()
}

fn optional_string(value: Option<&String>) -> JsValue {
    value.map_or_else(JsValue::null, |text| JsString::from(text.clone()).into())
}

/// Merge a hook's own return value over the downstream result it saw.
///
/// The hook's non-null fields win, `stop` is a disjunction, and watch paths
/// are combined and deduplicated. Downstream errors and failure state are
/// carried forward.
fn merge_effect_with_downstream(effect: HookEffect, downstream: HookFireResult) -> HookFireResult {
    let mut watch_paths = effect.watch_paths;
    watch_paths.extend(downstream.watch_paths);
    HookFireResult {
        stop: effect.stop || downstream.stop,
        updated_input: effect.updated_input.or(downstream.updated_input),
        updated_tool_output: effect
            .updated_tool_output
            .or(downstream.updated_tool_output),
        errors: downstream.errors,
        failed: downstream.failed,
        watch_paths: dedupe_paths(watch_paths),
    }
}

/// Merge the JavaScript result with the inner executor's result.
///
/// Both sides ran, so `stop` and `failed` are disjunctions. The inner executor
/// ran second and wins an `updated_*` conflict; a side that left the field
/// unset does not erase what the other set. Watch paths are combined and
/// deduplicated in first-seen order.
fn merge_with_inner(js: HookFireResult, inner: HookFireResult) -> HookFireResult {
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

fn apply_effect(combined: &mut HookFireResult, effect: HookEffect) {
    combined.stop |= effect.stop;
    if effect.updated_input.is_some() {
        combined.updated_input = effect.updated_input;
    }
    if effect.updated_tool_output.is_some() {
        combined.updated_tool_output = effect.updated_tool_output;
    }
    combined.watch_paths.extend(effect.watch_paths);
}

fn apply_result(combined: &mut HookFireResult, result: HookFireResult) {
    combined.stop |= result.stop;
    if result.updated_input.is_some() {
        combined.updated_input = result.updated_input;
    }
    if result.updated_tool_output.is_some() {
        combined.updated_tool_output = result.updated_tool_output;
    }
    combined.watch_paths.extend(result.watch_paths);
    combined.failed |= result.failed;
    combined.errors.extend(result.errors);
}

fn parse_reply(reply: &serde_json::Value) -> Result<HookEffect, String> {
    match reply.get("ok").and_then(serde_json::Value::as_bool) {
        Some(true) => {}
        _ => {
            return Err(reply
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the hook failed without an error message")
                .to_string());
        }
    }

    let Some(result) = reply.get("result") else {
        return Ok(HookEffect::default());
    };
    if result.is_null() {
        return Ok(HookEffect::default());
    }

    Ok(HookEffect {
        stop: result
            .get("stop")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        updated_input: string_field(result, "updatedInput"),
        updated_tool_output: string_field(result, "updatedToolOutput"),
        watch_paths: result
            .get("watchPaths")
            .and_then(serde_json::Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|path| path.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn string_field(value: &serde_json::Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Read a string property from a JavaScript value.
fn js_string_field(value: &JsValue, name: &str, context: &mut Context) -> Option<String> {
    let object = value.as_object()?;
    let field = object.get(JsString::from(name), context).ok()?;
    field.as_string().map(JsString::to_std_string_escaped)
}

fn apply_limits(context: &mut Context, limits: &ModLimits) {
    let mut engine_limits = context.runtime_limits();
    engine_limits.set_loop_iteration_limit(limits.max_loop_iterations);
    engine_limits.set_recursion_limit(limits.recursion_limit);
    engine_limits.set_stack_size_limit(limits.stack_size_limit);
    context.set_runtime_limits(engine_limits);
}

/// Put `$` and `on` into the realm before the mod runs.
fn install_host_api(context: &mut Context, session: &ModSession, name: &str) -> JsResult<()> {
    context.register_global_property(
        js_string!("__coderExModName"),
        JsString::from(name),
        Attribute::READONLY,
    )?;

    let session_object = ObjectInitializer::new(context)
        .property(
            js_string!("cwd"),
            JsString::from(session.cwd.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("model"),
            JsString::from(session.model.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("permission_mode"),
            JsString::from(session.permission_mode.clone()),
            Attribute::READONLY,
        )
        .build();

    let dollar = ObjectInitializer::new(context)
        .property(js_string!("session"), session_object, Attribute::READONLY)
        .function(NativeFunction::from_fn_ptr(mod_log), js_string!("log"), 1)
        .function(NativeFunction::from_fn_ptr(mod_env), js_string!("env"), 1)
        .build();
    context.register_global_property(
        js_string!("$"),
        dollar,
        Attribute::READONLY | Attribute::NON_ENUMERABLE,
    )?;
    context.eval(Source::from_bytes(
        "Object.freeze($.session); Object.freeze($);",
    ))?;

    let on = NativeFunction::from_fn_ptr(on_hook).to_js_function(context.realm());
    context.register_global_property(
        js_string!("on"),
        on,
        Attribute::READONLY | Attribute::NON_ENUMERABLE,
    )?;
    Ok(())
}

/// The main realm's harness: the per-mod registry and the hook dispatcher.
///
/// The dispatcher runs in the main realm but calls the hook function and passes
/// it the `$` captured in its own realm, so every mod keeps its realm (and its
/// globals) to itself.
const MAIN_HARNESS: &str = r"
'use strict';
globalThis.__coderExMods = [];

globalThis.__coderExNormalize = function (value) {
  if (value === undefined || value === null || typeof value !== 'object') return null;
  const normalized = {};
  if (value.stop === true) normalized.stop = true;
  const updatedInput = value.updatedInput !== undefined ? value.updatedInput : value.updated_input;
  if (typeof updatedInput === 'string') normalized.updatedInput = updatedInput;
  const updatedToolOutput = value.updatedToolOutput !== undefined ? value.updatedToolOutput : value.updated_tool_output;
  if (typeof updatedToolOutput === 'string') normalized.updatedToolOutput = updatedToolOutput;
  const watchPaths = value.watchPaths !== undefined ? value.watchPaths : value.watch_paths;
  if (Array.isArray(watchPaths)) {
    normalized.watchPaths = watchPaths.filter(function (path) { return typeof path === 'string'; });
  }
  return normalized;
};

globalThis.__coderExCall = function (modIndex, hookIndex, event, input, contextJson, nextFn) {
  const module = __coderExMods[modIndex];
  const registration = module !== undefined ? module[hookIndex] : undefined;
  if (!registration) {
    return JSON.stringify({ ok: false, error: 'hook registration ' + modIndex + '/' + hookIndex + ' is missing' });
  }

  let eventObject = {};
  try {
    const parsed = JSON.parse(input);
    if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
      eventObject = parsed;
    }
  } catch (_) {
    eventObject = {};
  }
  eventObject.event = event;
  eventObject.input = input;

  try {
    const parsedContext = JSON.parse(contextJson);
    if (parsedContext !== null && typeof parsedContext === 'object') {
      for (const key of Object.keys(parsedContext)) {
        if (key === 'tool_input') {
          try {
            eventObject[key] = JSON.parse(parsedContext[key]);
            continue;
          } catch (_) {
            // Not JSON; fall through and expose the raw string.
          }
        }
        eventObject[key] = parsedContext[key];
      }
    }
  } catch (_) {
    // A malformed context is simply not exposed.
  }

  try {
    const value = registration.hook(registration.host, eventObject, nextFn);
    if (value !== null && typeof value === 'object' && typeof value.then === 'function') {
      return JSON.stringify({ ok: false, error: 'async hooks are not supported yet; return a value synchronously' });
    }
    return JSON.stringify({ ok: true, result: __coderExNormalize(value) });
  } catch (error) {
    const message = (error && error.message) ? String(error.message) : String(error);
    return JSON.stringify({ ok: false, error: message });
  }
};
";

/// The per-mod harness: `on` and the registration array it fills.
fn install_mod_harness(context: &mut Context) -> JsResult<()> {
    let max_hooks = MAX_HOOKS_PER_MOD;
    context.eval(Source::from_bytes(
        format!(
            r"
'use strict';
globalThis.__coderExRegs = [];
globalThis.__coderExOn = function (event, matcher, hook) {{
  if (typeof event !== 'string' || event.trim().length === 0) {{
    throw new TypeError('on(event, matcher, hook): event must be a non-empty string');
  }}
  if (typeof matcher === 'function' && (hook === undefined || hook === null)) {{
    hook = matcher;
    matcher = '*';
  }}
  if (typeof matcher !== 'string' || matcher.trim().length === 0) {{
    throw new TypeError('on(event, matcher, hook): matcher must be a non-empty string');
  }}
  if (typeof hook !== 'function') {{
    throw new TypeError('on(event, matcher, hook): hook must be a function');
  }}
  if (__coderExRegs.length >= {max_hooks}) {{
    throw new RangeError('on(event, matcher, hook): a mod may register at most {max_hooks} hooks');
  }}
  __coderExRegs.push({{ event: event, matcher: matcher, hook: hook, host: $ }});
  return undefined;
}};
",
        )
        .as_bytes(),
    ))?;
    Ok(())
}

fn on_hook(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    call_helper(context, "__coderExOn", args)
}

fn mod_log(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let message = match args.first() {
        Some(value) => value.to_string(context)?.to_std_string_escaped(),
        None => String::new(),
    };
    let module = global_string(context, "__coderExModName").unwrap_or_default();
    tracing::info!(target: "coderex_mods", module = %module, "{message}");
    Ok(JsValue::undefined())
}

#[allow(clippy::unnecessary_wraps)] // `NativeFunction` requires a `JsResult`.
fn mod_env(_this: &JsValue, args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let Some(name) = args.first().and_then(JsValue::as_string) else {
        return Ok(JsValue::null());
    };
    let name = name.to_std_string_escaped();
    if !ALLOWED_ENV_VARS.contains(&name.as_str()) {
        return Ok(JsValue::null());
    }
    std::env::var(&name).map_or_else(
        |_| Ok(JsValue::null()),
        |value| Ok(JsString::from(value).into()),
    )
}

fn call_helper(context: &mut Context, name: &str, args: &[JsValue]) -> JsResult<JsValue> {
    let helper = context.global_object().get(JsString::from(name), context)?;
    let function = helper.as_callable().ok_or_else(|| {
        boa_engine::JsNativeError::typ()
            .with_message(format!("the runtime helper {name} is missing"))
    })?;
    function.call(&JsValue::undefined(), args, context)
}

fn global_string(context: &mut Context, name: &str) -> Option<String> {
    let value = context
        .global_object()
        .get(JsString::from(name), context)
        .ok()?;
    Some(value.to_string(context).ok()?.to_std_string_escaped())
}

fn js_string_literal(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn event_matches(configured: &str, fired: &str) -> bool {
    let configured = configured.trim();
    configured == "*" || configured.eq_ignore_ascii_case(fired.trim())
}

fn matcher_matches(matcher: &str, input: &str) -> bool {
    matcher.split('|').any(|alternative| {
        let alternative = alternative.trim();
        alternative == "*" || alternative.eq_ignore_ascii_case(input)
    })
}

pub fn is_gating_event(event: &str) -> bool {
    use coderex_core::hook_events::{PRE_TOOL_USE, USER_PROMPT_SUBMIT};
    event.eq_ignore_ascii_case(PRE_TOOL_USE) || event.eq_ignore_ascii_case(USER_PROMPT_SUBMIT)
}

pub fn dedupe_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(paths.len());
    for path in paths {
        if seen.insert(path.clone()) {
            unique.push(path);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct RecordingInner {
        calls: Arc<Mutex<Vec<(String, String)>>>,
        result: Arc<Mutex<HookFireResult>>,
    }

    impl RecordingInner {
        fn answering(result: HookFireResult) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                result: Arc::new(Mutex::new(result)),
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl HookExecutor for RecordingInner {
        async fn fire_hook(
            &self,
            event_name: &str,
            input: &str,
            _context: &HashMap<String, String>,
        ) -> HookFireResult {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event_name.to_string(), input.to_string()));
            self.result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[derive(Clone, Default)]
    struct ContextInner {
        contexts: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }

    #[async_trait::async_trait]
    impl HookExecutor for ContextInner {
        async fn fire_hook(
            &self,
            _event_name: &str,
            _input: &str,
            context: &HashMap<String, String>,
        ) -> HookFireResult {
            self.contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context.clone());
            HookFireResult::default()
        }
    }

    struct SlowInner;

    #[async_trait::async_trait]
    impl HookExecutor for SlowInner {
        async fn fire_hook(
            &self,
            _event_name: &str,
            _input: &str,
            _context: &HashMap<String, String>,
        ) -> HookFireResult {
            tokio::time::sleep(Duration::from_secs(3)).await;
            HookFireResult::default()
        }
    }

    fn session() -> ModSession {
        ModSession {
            cwd: "/work".to_string(),
            model: "test-model".to_string(),
            permission_mode: "default".to_string(),
        }
    }

    fn load(source: &str) -> ModRuntime {
        load_with(source, ModLimits::default())
    }

    fn load_with(source: &str, limits: ModLimits) -> ModRuntime {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("test.js"), source).unwrap();
        ModRuntime::load_dir_with_limits(directory.path(), session(), limits).unwrap()
    }

    fn fire_with(
        runtime: &ModRuntime,
        event: &str,
        input: &str,
        inner: Arc<dyn HookExecutor>,
    ) -> HookFireResult {
        runtime.fire_with_inner(event, input, &HashMap::new(), Some(inner))
    }

    #[test]
    fn next_runs_the_inner_executor_and_returns_its_result() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            updated_input: Some("inner-input".to_string()),
            ..HookFireResult::default()
        }));
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             const downstream = next(); \
             return { updatedInput: 'js(' + downstream.updatedInput + ')' }; }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(
            inner.calls(),
            vec![("PostToolUse".to_string(), "Bash".to_string())],
            "next() must run the inner executor with the same event and input"
        );
        assert_eq!(result.updated_input.as_deref(), Some("js(inner-input)"));
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn a_hook_that_calls_next_overrides_the_downstream_result() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            stop: true,
            updated_tool_output: Some("inner-output".to_string()),
            ..HookFireResult::default()
        }));
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             const downstream = next(); \
             return { updatedToolOutput: downstream.stop ? 'override' : 'saw-no-stop' }; }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(
            result.updated_tool_output.as_deref(),
            Some("override"),
            "the hook's non-empty field must win over the downstream value"
        );
        assert!(
            result.stop,
            "a downstream stop must survive the hook's overriding result"
        );
        assert_eq!(inner.calls().len(), 1);
    }

    #[test]
    fn calling_next_twice_returns_an_error_to_javascript() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult::default()));
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             next(); \
             try { next(); return { updatedInput: 'second call succeeded' }; } \
             catch (error) { return { updatedInput: String(error.message) }; } }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(
            inner.calls().len(),
            1,
            "a second next() must not run the downstream chain again"
        );
        assert!(
            result
                .updated_input
                .as_deref()
                .is_some_and(|message| message.contains("once")),
            "{result:?}"
        );
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn an_uncaught_second_next_is_a_hook_error_but_the_inner_ran_once() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult::default()));
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             next(); next(); }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(inner.calls().len(), 1);
        assert!(result.failed, "{result:?}");
        assert!(
            result.errors.iter().any(|error| error.contains("once")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn next_combines_and_dedupes_watch_paths() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            watch_paths: vec!["/tmp/a".to_string(), "/tmp/b".to_string()],
            ..HookFireResult::default()
        }));
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             next(); return { watchPaths: ['/tmp/b', '/tmp/c'] }; }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        let mut paths = result.watch_paths.clone();
        paths.sort();
        assert_eq!(paths, vec!["/tmp/a", "/tmp/b", "/tmp/c"]);
        assert_eq!(
            result.watch_paths.len(),
            3,
            "the shared path must appear once: {:?}",
            result.watch_paths
        );
    }

    #[test]
    fn a_downstream_updated_tool_output_is_visible_to_the_hook() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            updated_tool_output: Some("from-downstream".to_string()),
            ..HookFireResult::default()
        }));
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             const downstream = next(); \
             return { updatedToolOutput: 'hook saw ' + downstream.updatedToolOutput }; }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(
            result.updated_tool_output.as_deref(),
            Some("hook saw from-downstream")
        );
    }

    #[test]
    fn next_without_an_inner_executor_returns_an_empty_result() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             const downstream = next(); \
             return { updatedInput: [downstream.stop, downstream.failed, \
             downstream.watchPaths.length, String(downstream.updatedInput)].join('|') }; }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert_eq!(
            result.updated_input.as_deref(),
            Some("false|false|0|null"),
            "{result:?}"
        );
    }

    #[test]
    fn next_forwards_a_modified_input_and_context() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult::default()));
        let context_inner = Arc::new(ContextInner::default());
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             next({ input: 'rewritten', context: { mode: 'review' } }); return {}; }); }",
        );

        let result = runtime.fire_with_inner(
            "PostToolUse",
            "Bash",
            &HashMap::new(),
            Some(Arc::clone(&inner) as _),
        );

        assert_eq!(
            inner.calls(),
            vec![("PostToolUse".to_string(), "rewritten".to_string())]
        );
        assert!(!result.failed, "{result:?}");

        let result = runtime.fire_with_inner(
            "PostToolUse",
            "Bash",
            &HashMap::from([("mode".to_string(), "original".to_string())]),
            Some(Arc::clone(&context_inner) as _),
        );

        assert_eq!(
            context_inner
                .contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[HashMap::from([("mode".to_string(), "review".to_string())])],
            "next()'s context must replace the request context"
        );
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn the_whole_chain_timeout_includes_the_downstream_run() {
        let limits = ModLimits {
            hook_timeout: Duration::from_millis(100),
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { next(); }); }",
            limits,
        );

        let started = std::time::Instant::now();
        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::new(SlowInner) as _);
        let elapsed = started.elapsed();

        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("wall-clock budget")),
            "{:?}",
            result.errors
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    // ── Phase-1 behaviour, preserved on the worker model ────────────────────

    #[test]
    fn a_hook_can_stop_a_tool_and_matching_ignores_case() {
        let runtime = load(
            "function register(on) { on('PreToolUse', 'Bash|Write', function () { return { stop: true }; }); }",
        );
        assert_eq!(runtime.hook_count(), 1);

        let result = runtime.fire("pretooluse", "bash", &HashMap::new());
        assert!(result.stop, "{result:?}");
        assert!(!result.failed, "{result:?}");
        assert!(result.errors.is_empty(), "{result:?}");
    }

    #[test]
    fn a_hook_only_runs_for_its_event_and_matcher() {
        let runtime = load(
            "function register(on) { on('PreToolUse', 'Bash', function () { return { stop: true }; }); }",
        );

        assert!(!runtime.fire("PreToolUse", "Write", &HashMap::new()).stop);
        assert!(!runtime.fire("PostToolUse", "Bash", &HashMap::new()).stop);
        assert!(runtime.fire("PreToolUse", "Bash", &HashMap::new()).stop);
    }

    #[test]
    fn a_hook_can_replace_the_tool_output() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e) { return { updatedToolOutput: 'redacted:' + e.input }; }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());
        assert_eq!(result.updated_tool_output.as_deref(), Some("redacted:Read"));
        assert!(!result.stop);
    }

    #[test]
    fn a_hook_reads_the_session_snapshot() {
        let runtime = load(
            "function register(on) { on('SessionStart', '*', function ($) { \
             return { updatedInput: $.session.cwd + '|' + $.session.model + '|' + $.session.permission_mode }; }); }",
        );

        let result = runtime.fire("SessionStart", "", &HashMap::new());
        assert_eq!(
            result.updated_input.as_deref(),
            Some("/work|test-model|default")
        );
    }

    #[test]
    fn the_host_api_is_read_only_and_has_no_tools_or_files() {
        let runtime = load(
            "function register(on) { on('SessionStart', '*', function ($) { \
             try { $.session.cwd = '/changed'; } catch (_) {} \
             return { updatedInput: $.session.cwd + ':' + typeof $.tools + ':' + typeof $.files + ':' + typeof $.__engine }; }); }",
        );

        let result = runtime.fire("SessionStart", "", &HashMap::new());
        assert_eq!(
            result.updated_input.as_deref(),
            Some("/work:undefined:undefined:undefined")
        );
    }

    #[test]
    fn env_only_returns_allowlisted_names() {
        let runtime = load(
            "function register(on) { on('SessionStart', '*', function ($) { \
             return { updatedInput: [String($.env('HOME')), String($.env('PATH')), \
             String($.env('ANTHROPIC_API_KEY'))].join(',') }; }); }",
        );

        let result = runtime.fire("SessionStart", "", &HashMap::new());
        assert_eq!(
            result.updated_input.as_deref(),
            Some("null,null,null"),
            "the allowlist must not include home, PATH or a credential"
        );
        assert!(!ALLOWED_ENV_VARS.contains(&"HOME"));
        assert!(!ALLOWED_ENV_VARS.contains(&"PATH"));
        assert!(!ALLOWED_ENV_VARS.contains(&"ANTHROPIC_API_KEY"));
    }

    #[test]
    fn the_event_input_and_context_are_available_to_the_hook() {
        let runtime = load(
            "function register(on) { on('PreToolUse', '*', function ($, e) { \
             return { updatedInput: e.event + '|' + e.input + '|' + e.tool_input.command + '|' + e.session_id }; }); }",
        );

        let mut context = HashMap::new();
        context.insert(
            "tool_input".to_string(),
            r#"{"command":"git status"}"#.to_string(),
        );
        context.insert("session_id".to_string(), "s-1".to_string());

        let result = runtime.fire("PreToolUse", "Bash", &context);
        assert_eq!(
            result.updated_input.as_deref(),
            Some("PreToolUse|Bash|git status|s-1")
        );
    }

    #[test]
    fn an_infinite_loop_hits_the_engine_limit_and_is_reported() {
        let limits = ModLimits {
            max_loop_iterations: 100_000,
            hook_timeout: Duration::from_secs(5),
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { on('PreToolUse', 'Bash', function () { while (true) {} }); }",
            limits,
        );

        let started = std::time::Instant::now();
        let result = runtime.fire("PreToolUse", "Bash", &HashMap::new());
        let elapsed = started.elapsed();

        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("loop iteration limit")),
            "{:?}",
            result.errors
        );
        assert!(
            result.stop,
            "a failure on a gating event must fail closed: {result:?}"
        );
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    }

    #[test]
    fn a_hook_over_its_wall_clock_budget_is_reported() {
        // No engine loop cap: this exercises the watchdog path specifically.
        // The worker is detached when the budget expires and the test process
        // kills it on exit.
        let limits = ModLimits {
            max_loop_iterations: u64::MAX,
            hook_timeout: Duration::from_millis(50),
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { on('PreToolUse', 'Bash', function () { while (true) {} }); }",
            limits,
        );

        let started = std::time::Instant::now();
        let result = runtime.fire("PreToolUse", "Bash", &HashMap::new());
        let elapsed = started.elapsed();

        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("wall-clock budget")),
            "{:?}",
            result.errors
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn a_syntax_error_mod_is_skipped_and_the_rest_still_load() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("broken.js"),
            "function register(on) { this is not JavaScript }",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("good.js"),
            "function register(on) { on('PreToolUse', 'Bash', function () { return { stop: true }; }); }",
        )
        .unwrap();

        let runtime = ModRuntime::load_dir(directory.path(), session()).unwrap();

        assert_eq!(
            runtime.load_errors().len(),
            1,
            "{:?}",
            runtime.load_errors()
        );
        assert!(runtime.load_errors()[0].contains("broken.js"));
        assert_eq!(runtime.hook_count(), 1);
        assert!(runtime.fire("PreToolUse", "Bash", &HashMap::new()).stop);
    }

    #[test]
    fn a_mod_without_register_is_skipped() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("empty.js"), "const value = 1;").unwrap();

        let runtime = ModRuntime::load_dir(directory.path(), session()).unwrap();

        assert_eq!(runtime.load_errors().len(), 1);
        assert!(runtime.load_errors()[0].contains("register(on, options)"));
        assert_eq!(runtime.hook_count(), 0);
    }

    #[test]
    fn a_missing_directory_is_an_empty_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ModRuntime::load_dir(&directory.path().join("missing"), session()).unwrap();
        assert_eq!(runtime.hook_count(), 0);
        assert_eq!(runtime.load_errors().len(), 0);
    }

    #[test]
    fn option_snake_case_is_accepted() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function () { \
             return { updated_tool_output: 'snake', watch_paths: ['/tmp/a', '/tmp/a', '/tmp/b'] }; }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());
        assert_eq!(result.updated_tool_output.as_deref(), Some("snake"));
        assert_eq!(result.watch_paths, vec!["/tmp/a", "/tmp/b"]);
    }

    #[test]
    fn an_exception_is_an_error_not_a_crash() {
        let runtime = load(
            "function register(on) { on('PreToolUse', 'Bash', function () { throw new Error('nope'); }); }",
        );

        let result = runtime.fire("PreToolUse", "Bash", &HashMap::new());

        assert!(result.failed);
        assert!(result.errors.iter().any(|error| error.contains("nope")));
        assert!(result.stop);
    }

    #[test]
    fn an_async_hook_is_reported_rather_than_silently_ignored() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', async function () { return { stop: true }; }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert!(result.failed);
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("async hooks are not supported")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn a_hook_registered_on_every_event_matches() {
        let runtime = load(
            "function register(on) { on('*', '*', function ($, e) { return { updatedInput: e.event }; }); }",
        );

        assert_eq!(
            runtime
                .fire("FileChanged", "/tmp/x", &HashMap::new())
                .updated_input
                .as_deref(),
            Some("FileChanged")
        );
    }

    #[test]
    fn two_mods_keep_their_own_realm_and_run_in_file_order() {
        // Both mods declare the same top-level binding. A shared realm would
        // reject the second declaration; each mod having a realm of its own
        // also means their state is independent.
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("a.js"),
            "let seen = 'a'; \
             function register(on) { on('PostToolUse', '*', function () { \
             return { updatedInput: seen }; }); }",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("b.js"),
            "let seen = 'b'; \
             function register(on) { on('PostToolUse', '*', function () { \
             seen += '!'; return { updatedInput: seen }; }); }",
        )
        .unwrap();

        let runtime = ModRuntime::load_dir(directory.path(), session()).unwrap();
        assert_eq!(runtime.hook_count(), 2, "{:?}", runtime.load_errors());

        assert_eq!(
            runtime
                .fire("PostToolUse", "Bash", &HashMap::new())
                .updated_input
                .as_deref(),
            Some("b!"),
            "the later file runs later and wins the field"
        );
        assert_eq!(
            runtime
                .fire("PostToolUse", "Bash", &HashMap::new())
                .updated_input
                .as_deref(),
            Some("b!!"),
            "the second mod's realm state persists between calls"
        );
    }

    #[test]
    fn top_level_state_persists_across_hook_calls_in_one_mod() {
        // The worker keeps one realm for the runtime's lifetime, so module
        // state is no longer re-created for every call.
        let runtime = load(
            "let calls = 0; \
             function register(on) { on('PostToolUse', '*', function () { \
             calls += 1; return { updatedInput: String(calls) }; }); }",
        );

        assert_eq!(
            runtime
                .fire("PostToolUse", "Bash", &HashMap::new())
                .updated_input
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            runtime
                .fire("PostToolUse", "Bash", &HashMap::new())
                .updated_input
                .as_deref(),
            Some("2")
        );
    }
}
