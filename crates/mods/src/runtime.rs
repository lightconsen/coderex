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
    js_string, object::ObjectInitializer, object::builtins::JsArray, property::Attribute,
};
use coderex_core::hook_executor::{HookExecutor, HookFireResult};
use coderex_core::ui_protocol::{UiElement, parse_ui_elements};

use crate::transform;

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

/// Most UI activation handlers a single mod may register.
const MAX_ACTIVATIONS_PER_MOD: usize = 256;

/// Most nested `next(e)` calls one [`ModRuntime::fire`] may perform.
///
/// Each `next(e)` continues the chain after the calling hook, so the depth is
/// the number of chained hooks plus the final one. Without this bound a mod
/// could build an unbounded `next` recursion; exceeding it is a hook error.
const MAX_CHAIN_DEPTH: usize = 64;

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
    ///
    /// One `next(e)` level uses about four frames, so this must stay above
    /// [`MAX_CHAIN_DEPTH`]'s frames for a bounded chain to reach the chain's
    /// own error rather than an engine runtime-limit error.
    pub recursion_limit: usize,
    /// Most values one JavaScript stack may hold.
    pub stack_size_limit: usize,
}

impl Default for ModLimits {
    fn default() -> Self {
        Self {
            hook_timeout: Duration::from_secs(5),
            max_loop_iterations: 10_000_000,
            // One `next` level costs roughly four JavaScript frames
            // (`__coderExChain` → `__coderExCall` → hook → `next`), so the
            // engine ceiling must stay above [`MAX_CHAIN_DEPTH`]'s frames or
            // a bounded chain would die as a runtime-limit error before the
            // chain's own limit fires.
            recursion_limit: 512,
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

/// A UI activation request from the caller to the worker.
struct ActivateRequest {
    id: String,
    reply: mpsc::SyncSender<Result<Vec<UiElement>, String>>,
}

/// Any work the worker accepts, in the order it arrives.
enum WorkerRequest {
    Fire(Request),
    Activate(ActivateRequest),
}

/// The caller's handle to the worker thread.
#[derive(Debug)]
struct WorkerHandle {
    sender: mpsc::Sender<WorkerRequest>,
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
    /// Load every `.js`, `.jsx`, `.ts` and `.tsx` file in `path`.
    ///
    /// A missing directory is an empty runtime: opting in before creating the
    /// directory is not an error. A file that cannot be read, is too large,
    /// fails to transform, or fails to evaluate or register is reported once
    /// through `tracing::warn` and in [`load_errors`](Self::load_errors), and
    /// is skipped; the rest still load.
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
        for (name, file_path, kind) in files {
            let source = match read_source(&file_path) {
                Ok(source) => source,
                Err(error) => {
                    record_load_error(&mut load_errors, &file_path, &error);
                    continue;
                }
            };
            // `.js` is evaluated as it is. The other extensions are lowered
            // here, at load, so a transform failure is reported once and the
            // broken mod is skipped while the rest still load.
            let source = if kind.needs_transform() {
                match transform::transform(&source, &file_path) {
                    Ok(source) => source,
                    Err(error) => {
                        record_load_error(&mut load_errors, &file_path, &error);
                        continue;
                    }
                }
            } else {
                source
            };
            mods.push(ModSource {
                name,
                path: file_path,
                source,
            });
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
    /// downstream executor behind `next(e)`.
    ///
    /// Hooks run in load order (files sorted by name, then registration
    /// order). A hook that does not call `next` behaves exactly as before: its
    /// return value is merged into the result. A hook that calls `next`
    /// continues the chain: the hooks registered after it run in order and
    /// then, unless a hook stopped the chain or a deeper `next` already ran
    /// it, `inner`. The caller receives that combined result and its own
    /// non-empty fields win over it; `stop` is OR-ed and watch paths are
    /// deduplicated. When no hook called `next`, `inner` runs once after the
    /// JavaScript hooks with the possibly updated input, exactly like a plain
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

        match self.dispatch(request, &reply_receiver) {
            Ok(result) => result,
            Err(error) => failure_result(event, error),
        }
    }

    /// Deliver a button activation to the worker and return the elements its
    /// handler rendered.
    ///
    /// The handler runs on the same worker thread and realm as the mod's
    /// hooks, under the same wall-clock budget and engine limits. An id with
    /// no registered handler is ignored: it logs a warning and yields no
    /// elements.
    ///
    /// # Errors
    ///
    /// Returns the handler's error message when it throws, returns invalid
    /// elements, or exceeds the wall-clock budget.
    pub fn activate(&self, id: &str) -> Result<Vec<UiElement>, String> {
        let (reply, reply_receiver) = mpsc::sync_channel(1);
        let sender = self.worker_sender()?;
        sender
            .send(WorkerRequest::Activate(ActivateRequest {
                id: id.to_string(),
                reply,
            }))
            .map_err(|_| "the mod worker stopped before the activation could run".to_string())?;

        match reply_receiver.recv_timeout(self.limits.hook_timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.abandon_worker();
                Err(format!(
                    "the activation handler exceeded its {} ms wall-clock budget",
                    self.limits.hook_timeout.as_millis()
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.abandon_worker();
                Err("the mod worker stopped without reporting an activation result".to_string())
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
            .send(WorkerRequest::Fire(request))
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
    fn worker_sender(&self) -> Result<mpsc::Sender<WorkerRequest>, String> {
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
                recursion_limit: 512,
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

/// The source languages a mod file may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModKind {
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
}

impl ModKind {
    /// The kind for a file extension, case-insensitively.
    fn from_extension(extension: &OsStr) -> Option<Self> {
        match extension.to_str()?.to_ascii_lowercase().as_str() {
            "js" => Some(Self::JavaScript),
            "jsx" => Some(Self::Jsx),
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    /// Whether the source must be lowered before evaluation.
    ///
    /// `.js` keeps the original behaviour; every other extension is parsed and
    /// printed by [`crate::transform`].
    const fn needs_transform(self) -> bool {
        !matches!(self, Self::JavaScript)
    }
}

/// List the mod files directly in `path`, sorted by file name.
fn collect_mod_files(path: &Path) -> Result<Vec<(String, PathBuf, ModKind)>, String> {
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
                let Some(kind) = file_path.extension().and_then(ModKind::from_extension) else {
                    continue;
                };
                let name = file_path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("<unnamed>")
                    .to_string();
                files.push((name, file_path, kind));
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

/// Report one skipped mod once: in the log and in the runtime's error list.
fn record_load_error(errors: &mut Vec<String>, path: &Path, error: &str) {
    let message = format!("{}: {error}", path.display());
    tracing::warn!("{message}");
    errors.push(message);
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
fn spawn_worker(
    config: Arc<WorkerConfig>,
) -> Result<(mpsc::Sender<WorkerRequest>, InitOutcome), String> {
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
    receiver: &mpsc::Receiver<WorkerRequest>,
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
        match request {
            WorkerRequest::Fire(request) => {
                let result = state.fire(&request);
                let _ = request.reply.send(result);
            }
            WorkerRequest::Activate(request) => {
                let result = state.activate(&request.id);
                let _ = request.reply.send(result);
            }
        }
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
        let harness = MAIN_HARNESS.replace("@@MAX_CHAIN_DEPTH@@", &MAX_CHAIN_DEPTH.to_string());
        context
            .eval(Source::from_bytes(harness.as_bytes()))
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

        let activation_registry = context
            .global_object()
            .get(js_string!("__coderExActivations"), &mut context)
            .map_err(|error| format!("could not read the runtime harness: {error}"))?;
        let activation_registry = activation_registry
            .as_object()
            .cloned()
            .and_then(|object| JsArray::from_object(object).ok())
            .ok_or_else(|| "the runtime activation registry is missing".to_string())?;

        let mut hooks = Vec::new();
        let mut load_errors = Vec::new();
        let mut mod_names = Vec::new();

        for source in &config.mods {
            match load_mod(&mut context, source, &config.session) {
                Ok(loaded) => {
                    registry
                        .push(loaded.registrations.clone(), &mut context)
                        .map_err(|error| {
                            format!("could not register {}: {error}", source.path.display())
                        })?;
                    activation_registry
                        .push(loaded.activations.clone(), &mut context)
                        .map_err(|error| {
                            format!(
                                "could not register {}'s UI handlers: {error}",
                                source.path.display()
                            )
                        })?;
                    let array = loaded
                        .registrations
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
    ///
    /// The chain itself lives in the main-realm harness: each hook's `next(e)`
    /// calls back into it for the hooks registered after that hook, then the
    /// inner executor. All this function does is hand the matching hooks to
    /// [`__coderExRun`] and read the merged result back.
    fn fire(&mut self, request: &Request) -> HookFireResult {
        let context_json = serde_json::to_string(&request.context).unwrap_or_else(|_| "{}".into());
        let chain = self.chain_json(request);
        let inner = Arc::new(InnerCall {
            runtime: Arc::clone(&self.runtime),
            inner: request.inner.clone(),
            called: AtomicBool::new(false),
        });
        match self.run_chain(&chain, &context_json, request, &inner) {
            Ok(mut result) => {
                if result.failed && is_gating_event(&request.event) {
                    result.stop = true;
                }
                result
            }
            Err(error) => {
                let mut result = failure_result(&request.event, error);
                // An engine-limit error aborts the whole harness call, so the
                // chain could not reach its inner-executor step. Preserve the
                // old behaviour for a non-gating event: when the inner
                // executor never ran, run it once with the request input.
                if !result.stop && !inner.called.load(Ordering::SeqCst) {
                    if let Some(executor) = &request.inner {
                        let inner_result = self.runtime.block_on(executor.fire_hook(
                            &request.event,
                            &request.input,
                            &request.context,
                        ));
                        result = merge_with_inner(result, inner_result);
                    }
                }
                result
            }
        }
    }

    /// Run the handler registered for one button id.
    ///
    /// An id with no handler is not an error: the handler registry is looked
    /// up in the main realm, and a miss is reported as a warning so a stale
    /// button cannot make a session fail.
    fn activate(&mut self, id: &str) -> Result<Vec<UiElement>, String> {
        let activate = self
            .context
            .global_object()
            .get(js_string!("__coderExActivate"), &mut self.context)
            .map_err(|error| format!("could not read the runtime harness: {error}"))?;
        let activate = activate
            .as_callable()
            .cloned()
            .ok_or_else(|| "the runtime harness is missing".to_string())?;

        let event = serde_json::json!({ "id": id }).to_string();
        let args = [JsString::from(id).into(), JsString::from(event).into()];
        let value = activate
            .call(&JsValue::undefined(), &args, &mut self.context)
            .map_err(|error| format!("the activation handler could not be called: {error}"))?;
        let text = value
            .to_string(&mut self.context)
            .map_err(|error| format!("could not read the activation result: {error}"))?
            .to_std_string_escaped();
        let reply: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
            format!("the activation handler returned unreadable output: {error}")
        })?;

        if reply.get("found").and_then(serde_json::Value::as_bool) == Some(false) {
            tracing::warn!(id = %id, "no onActivate handler is registered for this UI button");
            return Ok(Vec::new());
        }
        if reply.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(string_field(&reply, "error")
                .unwrap_or_else(|| "the activation handler failed".to_string()));
        }
        match reply.get("ui") {
            None | Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(value) => parse_ui_elements(value)
                .map_err(|error| format!("the activation handler returned invalid UI: {error}")),
        }
    }

    /// The matching hooks, in registration order, as the harness reads them.
    fn chain_json(&self, request: &Request) -> String {
        let hooks: Vec<serde_json::Value> = self
            .hooks
            .iter()
            .filter(|hook| {
                event_matches(&hook.event, &request.event)
                    && matcher_matches(&hook.matcher, &request.input)
            })
            .map(|hook| {
                serde_json::json!({
                    "m": hook.mod_index,
                    "h": hook.hook_index,
                    "name": self
                        .mod_names
                        .get(hook.mod_index)
                        .map_or("<unknown>", String::as_str),
                })
            })
            .collect();
        serde_json::to_string(&hooks).unwrap_or_else(|_| "[]".to_string())
    }

    /// Call the harness chain runner with one native inner-executor function.
    fn run_chain(
        &mut self,
        chain: &str,
        context_json: &str,
        request: &Request,
        inner_call: &Arc<InnerCall>,
    ) -> Result<HookFireResult, String> {
        // The chain calls this at its end; it owns only Rust state, which is
        // why the capture can leave it untraced.
        let inner = NativeFunction::from_copy_closure_with_captures(
            inner_entry,
            InnerCapture {
                call: Arc::clone(inner_call),
            },
        );
        let realm = self.context.realm().clone();
        let inner = JsValue::from(JsObject::from(inner.to_js_function(&realm)));

        let runner = self
            .context
            .global_object()
            .get(js_string!("__coderExRun"), &mut self.context)
            .map_err(|error| format!("could not read the runtime harness: {error}"))?;
        let runner = runner
            .as_callable()
            .cloned()
            .ok_or_else(|| "the runtime harness is missing".to_string())?;

        let args = [
            JsString::from(chain).into(),
            JsString::from(request.event.clone()).into(),
            JsString::from(request.input.clone()).into(),
            JsString::from(context_json).into(),
            inner,
            JsValue::from(is_gating_event(&request.event)),
        ];
        let value = runner
            .call(&JsValue::undefined(), &args, &mut self.context)
            .map_err(|error| format!("the hook chain could not be called: {error}"))?;
        parse_chain_result(&value, &mut self.context)
    }
}

/// What one evaluated mod left behind in its realm.
struct LoadedMod {
    /// The mod's `on(...)` registrations.
    registrations: JsValue,
    /// The mod's `$.ui.onActivate(...)` registrations.
    activations: JsValue,
}

/// Evaluate one mod in its own realm and return its registrations.
fn load_mod(
    context: &mut Context,
    source: &ModSource,
    session: &ModSession,
) -> Result<LoadedMod, String> {
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
) -> Result<LoadedMod, String> {
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

    let registrations = context
        .eval(Source::from_bytes("__coderExRegs"))
        .map_err(|error| {
            format!("register() did not leave readable hook registrations: {error}")
        })?;
    let activations = context
        .eval(Source::from_bytes("__coderExHandlers"))
        .map_err(|error| {
            format!("register() did not leave readable activation registrations: {error}")
        })?;
    Ok(LoadedMod {
        registrations,
        activations,
    })
}

/// The inner executor the JavaScript chain runs at its end.
struct InnerCall {
    runtime: Arc<tokio::runtime::Runtime>,
    inner: Option<Arc<dyn HookExecutor>>,
    /// Whether the executor was already reached, so a fatal harness error
    /// cannot make [`WorkerState::fire`] run it a second time.
    called: AtomicBool,
}

/// The capture stored in the native inner-executor function.
///
/// The chain is JavaScript; only the host executor is native. The capture holds
/// Rust-owned state (an `Arc`), never a garbage-collected handle, so it is safe
/// to leave untraced.
#[derive(Finalize, Trace)]
struct InnerCapture {
    #[unsafe_ignore_trace]
    call: Arc<InnerCall>,
}

/// The body of the chain's final step: run the inner executor synchronously
/// and return its result as JSON for the harness to merge.
#[allow(clippy::unnecessary_wraps)] // `NativeFunction` requires a `JsResult`.
fn inner_entry(
    _this: &JsValue,
    args: &[JsValue],
    capture: &InnerCapture,
    _context: &mut Context,
) -> JsResult<JsValue> {
    let event = string_arg(args, 0);
    let input = string_arg(args, 1);
    let context_json = string_arg(args, 2);
    let inner_context: HashMap<String, String> =
        serde_json::from_str(&context_json).unwrap_or_default();
    capture.call.called.store(true, Ordering::SeqCst);
    let result = capture
        .call
        .inner
        .as_ref()
        .map_or_else(HookFireResult::default, |inner| {
            capture
                .call
                .runtime
                .block_on(inner.fire_hook(&event, &input, &inner_context))
        });
    let value = serde_json::json!({
        "stop": result.stop,
        "updatedInput": result.updated_input,
        "updatedToolOutput": result.updated_tool_output,
        "watchPaths": result.watch_paths,
        "errors": result.errors,
        "failed": result.failed,
        "ui": result.ui,
    });
    Ok(JsString::from(value.to_string()).into())
}

fn string_arg(args: &[JsValue], index: usize) -> String {
    args.get(index)
        .and_then(JsValue::as_string)
        .map_or_else(String::new, JsString::to_std_string_escaped)
}

/// Read the harness chain's merged result back.
fn parse_chain_result(value: &JsValue, context: &mut Context) -> Result<HookFireResult, String> {
    let text = value
        .to_string(context)
        .map_err(|error| format!("could not read the hook chain result: {error}"))?
        .to_std_string_escaped();
    let reply: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("the hook chain returned unreadable output: {error}"))?;

    let ui = match reply.get("ui") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(value) => parse_ui_elements(value)
            .map_err(|error| format!("the hook chain returned invalid UI: {error}"))?,
    };
    let strings = |name: &str| {
        reply
            .get(name)
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    Ok(HookFireResult {
        stop: reply
            .get("stop")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        updated_input: string_field(&reply, "updatedInput"),
        updated_tool_output: string_field(&reply, "updatedToolOutput"),
        watch_paths: strings("watchPaths"),
        errors: strings("errors"),
        failed: reply
            .get("failed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        ui,
    })
}

/// A result that reports `error` as a hook failure, fail-closed on a gate.
fn failure_result(event: &str, error: String) -> HookFireResult {
    let mut result = HookFireResult {
        failed: true,
        errors: vec![error],
        ..HookFireResult::default()
    };
    if is_gating_event(event) {
        result.stop = true;
    }
    result
}

/// Merge a failure with the inner executor's result.
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
    let mut ui = js.ui;
    ui.extend(inner.ui);

    HookFireResult {
        stop: js.stop || inner.stop,
        updated_input: inner.updated_input.or(js.updated_input),
        updated_tool_output: inner.updated_tool_output.or(js.updated_tool_output),
        errors,
        failed: js.failed || inner.failed,
        watch_paths: dedupe_paths(watch_paths),
        ui,
    }
}

fn dedupe_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(paths.len());
    for path in paths {
        if seen.insert(path.clone()) {
            unique.push(path);
        }
    }
    unique
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
    context.eval(Source::from_bytes(HOST_UI_API.as_bytes()))?;
    context.eval(Source::from_bytes(JSX_RUNTIME.as_bytes()))?;

    let on = NativeFunction::from_fn_ptr(on_hook).to_js_function(context.realm());
    context.register_global_property(
        js_string!("on"),
        on,
        Attribute::READONLY | Attribute::NON_ENUMERABLE,
    )?;
    Ok(())
}

/// The `$.ui` host API, installed in each mod's realm before the mod runs.
///
/// The constructors return plain, JSON-shaped descriptors; `render` stages
/// them and the main-realm dispatcher drains the staging area after each hook
/// call. Validation happens here (so the mod gets a JavaScript error at the
/// call site) and again in [`parse_ui_elements`] at the boundary.
const HOST_UI_API: &str = r#"
'use strict';
(function () {
  const STYLES = ['plain', 'bold', 'dim', 'warning', 'error'];
  const staging = [];

  function styleName(style) {
    if (style === undefined || style === null) return 'plain';
    if (typeof style !== 'string') {
      throw new TypeError('$.ui.text(text, style): style must be a string');
    }
    const normalized = style.toLowerCase();
    if (STYLES.indexOf(normalized) === -1) {
      throw new TypeError("$.ui.text(text, style): unknown style '" + style + "'; expected one of " + STYLES.join(', '));
    }
    return normalized;
  }

  function assertElement(element, depth) {
    if (depth > 32) {
      throw new RangeError('$.ui.render: UI elements may nest at most 32 levels');
    }
    if (element === null || typeof element !== 'object' || Array.isArray(element)) {
      throw new TypeError('$.ui.render: every element must be an object created by $.ui.text(), $.ui.box() or $.ui.button()');
    }
    if (element.type === 'text') {
      if (typeof element.text !== 'string') {
        throw new TypeError('$.ui.text(text, style): text must be a string');
      }
      styleName(element.style);
      return;
    }
    if (element.type === 'button') {
      if (typeof element.label !== 'string' || element.label.length === 0) {
        throw new TypeError('$.ui.button(label, id): label must be a non-empty string');
      }
      if (typeof element.id !== 'string' || element.id.length === 0) {
        throw new TypeError('$.ui.button(label, id): id must be a non-empty string');
      }
      return;
    }
    if (element.type === 'box') {
      if (element.title !== null && element.title !== undefined && typeof element.title !== 'string') {
        throw new TypeError('$.ui.box({title}, children...): title must be a string');
      }
      if (!Array.isArray(element.children)) {
        throw new TypeError('$.ui.box({title}, children...): children must be an array');
      }
      for (let index = 0; index < element.children.length; index++) {
        assertElement(element.children[index], depth + 1);
      }
      return;
    }
    throw new TypeError("$.ui.render: unknown element type '" + String(element.type) + "'");
  }

  const ui = {
    text: function (text, style) {
      if (typeof text !== 'string') {
        throw new TypeError('$.ui.text(text, style): text must be a string');
      }
      return { type: 'text', text: text, style: styleName(style) };
    },
    box: function (options) {
      let title = null;
      let start = 0;
      if (options !== undefined && options !== null) {
        if (typeof options !== 'object' || Array.isArray(options)) {
          throw new TypeError('$.ui.box({title}, children...): the first argument must be an options object');
        }
        if (options.title !== undefined && options.title !== null) {
          if (typeof options.title !== 'string') {
            throw new TypeError('$.ui.box({title}, children...): title must be a string');
          }
          title = options.title;
        }
        start = 1;
      }
      const children = [];
      for (let index = start; index < arguments.length; index++) {
        const child = arguments[index];
        if (Array.isArray(child)) {
          for (let inner = 0; inner < child.length; inner++) {
            assertElement(child[inner], 1);
            children.push(child[inner]);
          }
        } else {
          assertElement(child, 1);
          children.push(child);
        }
      }
      return { type: 'box', title: title, children: children };
    },
    button: function (label, id) {
      if (typeof label !== 'string' || label.length === 0) {
        throw new TypeError('$.ui.button(label, id): label must be a non-empty string');
      }
      if (typeof id !== 'string' || id.length === 0) {
        throw new TypeError('$.ui.button(label, id): id must be a non-empty string');
      }
      return { type: 'button', label: label, id: id };
    },
    onActivate: function (id, handler) {
      if (typeof id !== 'string' || id.length === 0) {
        throw new TypeError('$.ui.onActivate(id, handler): id must be a non-empty string');
      }
      if (typeof handler !== 'function') {
        throw new TypeError('$.ui.onActivate(id, handler): handler must be a function');
      }
      __coderExOnActivate(id, handler);
      return undefined;
    },
    render: function (element) {
      if (arguments.length === 0) {
        throw new TypeError('$.ui.render(element(s)): at least one element is required');
      }
      const elements = arguments.length > 1
        ? Array.prototype.slice.call(arguments)
        : (Array.isArray(element) ? element : [element]);
      for (let index = 0; index < elements.length; index++) {
        assertElement(elements[index], 0);
        staging.push(elements[index]);
      }
      return undefined;
    },
    // Host plumbing: the dispatcher clears and drains this area around each
    // hook call. Mods have no reason to call these.
    __resetUi: function () { staging.length = 0; },
    __takeUi: function () { const taken = staging.slice(); staging.length = 0; return taken; }
  };

  Object.freeze(ui);
  $.ui = ui;
})();
Object.freeze($.session);
Object.freeze($);
"#;

/// The JSX classic runtime, installed in each mod's realm after `$.ui`.
///
/// The transform emits `h(type, props, ...children)` and `Fragment`.
/// `h` maps the three host element names straight onto `$.ui`, so the
/// descriptor shape is exactly what `$.ui` already produces. A function type
/// is called as a component with its props (including `children`); `Fragment`
/// returns its children as an array, which `$.ui.box` and `$.ui.render`
/// already flatten one level. Anything else throws, and the dispatcher turns
/// that into an ordinary hook error.
const JSX_RUNTIME: &str = r#"
'use strict';
(function () {
  const Fragment = Symbol('Fragment');

  function flatten(value, out) {
    if (value === null || value === undefined || value === false || value === true) {
      return;
    }
    if (Array.isArray(value)) {
      for (let index = 0; index < value.length; index++) {
        flatten(value[index], out);
      }
      return;
    }
    out.push(value);
  }

  function textOf(children, element) {
    let text = '';
    for (let index = 0; index < children.length; index++) {
      const child = children[index];
      if (typeof child === 'string') {
        text += child;
      } else if (typeof child === 'number' || typeof child === 'bigint') {
        text += String(child);
      } else {
        throw new TypeError("h('" + element + "', ...): children must be strings or numbers");
      }
    }
    return text;
  }

  function componentProps(props, children) {
    const result = {};
    if (props !== null && props !== undefined) {
      if (typeof props !== 'object') {
        throw new TypeError('h(Component, ...): props must be an object');
      }
      for (const key of Object.keys(props)) {
        result[key] = props[key];
      }
    }
    if (children.length === 1) {
      result.children = children[0];
    } else if (children.length > 1) {
      result.children = children;
    }
    return result;
  }

  function h(type, props, ...children) {
    const flat = [];
    for (let index = 0; index < children.length; index++) {
      flatten(children[index], flat);
    }

    if (type === Fragment) {
      return flat;
    }
    if (typeof type === 'function') {
      return type(componentProps(props, flat));
    }
    if (typeof type !== 'string') {
      throw new TypeError('h(type, ...): type must be an element name, Fragment or a component function');
    }

    const options = (props !== null && typeof props === 'object') ? props : {};

    if (type === 'text') {
      return $.ui.text(textOf(flat, 'text'), options.style);
    }
    if (type === 'box') {
      return $.ui.box({ title: options.title }, flat);
    }
    if (type === 'button') {
      if (flat.length === 0) {
        throw new TypeError("h('button', ...): a button needs a label child");
      }
      return $.ui.button(textOf(flat, 'button'), options.id);
    }
    throw new TypeError("h(type, ...): unknown element type '" + type + "'; expected 'text', 'box', 'button', Fragment or a component function");
  }

  Object.defineProperty(globalThis, 'h', {
    value: h, writable: false, enumerable: false, configurable: false
  });
  Object.defineProperty(globalThis, 'Fragment', {
    value: Fragment, writable: false, enumerable: false, configurable: false
  });
})();
"#;

/// The main realm's harness: the per-mod registry, the hook dispatcher and the
/// `next(e)` chain.
///
/// The dispatcher and the chain run in the main realm but call each hook
/// function with the `$` captured in that hook's own realm, so every mod keeps
/// its realm (and its globals) to itself. The chain is JavaScript because a
/// native `next` only has `&mut Context` while the hook is on the stack; the
/// one native step, `innerFn`, runs the host's downstream executor at the end
/// of a chain segment.
const MAIN_HARNESS: &str = r#"
'use strict';
globalThis.__coderExMods = [];
globalThis.__coderExActivations = [];

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
  // The surface a person is looking at. CodeRex ships one — the terminal — so
  // this is constant today; the field exists so a mod can branch on it without
  // changing when another surface is actually built.
  eventObject.surface = 'terminal';

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

  const host = registration.host;
  const ui = host && host.ui;
  if (ui && typeof ui.__resetUi === 'function') ui.__resetUi();

  try {
    const value = registration.hook(host, eventObject, nextFn);
    // Take whatever `$.ui.render` staged, even when the hook returns nothing:
    // rendering is itself the result.
    const staged = (ui && typeof ui.__takeUi === 'function') ? ui.__takeUi() : [];
    if (value !== null && typeof value === 'object' && typeof value.then === 'function') {
      return JSON.stringify({ ok: false, error: 'async hooks are not supported yet; return a value synchronously' });
    }
    return JSON.stringify({ ok: true, result: __coderExNormalize(value), ui: staged });
  } catch (error) {
    if (ui && typeof ui.__takeUi === 'function') ui.__takeUi();
    const message = (error && error.message) ? String(error.message) : String(error);
    return JSON.stringify({ ok: false, error: message });
  }
};

// A button activation looks up the last handler registered for the id across
// every mod (a later registration replaces an earlier one), calls it with its
// own realm's `$`, and returns whatever it staged plus whatever elements it
// returned. Validation of the returned tree happens at the Rust boundary.
globalThis.__coderExActivate = function (id, eventJson) {
  let chosen = null;
  for (let modIndex = 0; modIndex < __coderExActivations.length; modIndex++) {
    const handlers = __coderExActivations[modIndex];
    for (let index = 0; index < handlers.length; index++) {
      if (handlers[index] !== null && handlers[index].id === id) chosen = handlers[index];
    }
  }
  if (chosen === null) {
    return JSON.stringify({ found: false, ok: true, ui: [] });
  }

  const host = chosen.host;
  const ui = host && host.ui;
  if (ui && typeof ui.__resetUi === 'function') ui.__resetUi();

  try {
    let event = {};
    try {
      const parsed = JSON.parse(eventJson);
      if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
        event = parsed;
      }
    } catch (_) {
      event = {};
    }
    event.event = 'ModUiActivate';
    event.input = id;
    event.id = id;
    event.surface = 'terminal';

    const value = chosen.handler(host, event);
    const staged = (ui && typeof ui.__takeUi === 'function') ? ui.__takeUi() : [];
    if (value !== null && typeof value === 'object' && typeof value.then === 'function') {
      return JSON.stringify({
        found: true,
        ok: false,
        error: 'async activation handlers are not supported yet; return elements synchronously',
        ui: staged
      });
    }
    let returned = [];
    if (Array.isArray(value)) {
      returned = value;
    } else if (value !== null && typeof value === 'object') {
      returned = [value];
    }
    return JSON.stringify({ found: true, ok: true, ui: staged.concat(returned) });
  } catch (error) {
    if (ui && typeof ui.__takeUi === 'function') ui.__takeUi();
    const message = (error && error.message) ? String(error.message) : String(error);
    return JSON.stringify({ found: true, ok: false, error: message, ui: [] });
  }
};

// ── The chain ───────────────────────────────────────────────────────────────
//
// A merged result has exactly the shape of HookFireResult: `stop`,
// `updatedInput` and `updatedToolOutput` (string or null), `watchPaths`,
// `errors`, `failed`, and `ui`. `__coderExChain` runs `hooks[start..]` in
// order. If a hook calls `next`, that hook's closure recurses for the hooks
// after it, the recursion runs the inner executor when no deeper hook called
// `next`, and this level then merges the hook's own return value over that
// downstream result and stops: the remaining hooks already ran inside `next`.
// If no hook at this level calls `next`, the loop runs them all and then the
// inner executor once.

globalThis.__coderExEmptyResult = function () {
  return {
    stop: false,
    updatedInput: null,
    updatedToolOutput: null,
    watchPaths: [],
    errors: [],
    failed: false,
    ui: []
  };
};

globalThis.__coderExDedupe = function (paths) {
  // A null prototype keeps a path such as "__proto__" an ordinary entry.
  const seen = Object.create(null);
  const unique = [];
  for (let index = 0; index < paths.length; index++) {
    const path = paths[index];
    if (seen[path] !== true) {
      seen[path] = true;
      unique.push(path);
    }
  }
  return unique;
};

// What `next(e)` hands back to the hook: the public result, without the UI
// snapshot, and detached from the stored downstream result so a hook that
// mutates it cannot change what is merged.
globalThis.__coderExPublicResult = function (result) {
  return {
    stop: result.stop,
    updatedInput: result.updatedInput,
    updatedToolOutput: result.updatedToolOutput,
    watchPaths: result.watchPaths.slice(),
    errors: result.errors.slice(),
    failed: result.failed
  };
};

// The calling hook's non-null fields win; `stop` is OR-ed; watch paths are
// combined and deduplicated; downstream errors and failure state are carried.
globalThis.__coderExMergeEffect = function (effect, ui, downstream) {
  const ownWatchPaths = (effect !== null && Array.isArray(effect.watchPaths))
    ? effect.watchPaths
    : [];
  return {
    stop: (effect !== null && effect.stop === true) || downstream.stop,
    updatedInput: (effect !== null && effect.updatedInput !== undefined)
      ? effect.updatedInput
      : downstream.updatedInput,
    updatedToolOutput: (effect !== null && effect.updatedToolOutput !== undefined)
      ? effect.updatedToolOutput
      : downstream.updatedToolOutput,
    errors: downstream.errors.slice(),
    failed: downstream.failed,
    watchPaths: __coderExDedupe(ownWatchPaths.concat(downstream.watchPaths)),
    ui: ui.concat(downstream.ui)
  };
};

globalThis.__coderExApplyEffect = function (combined, effect, ui) {
  if (effect !== null) {
    if (effect.stop === true) combined.stop = true;
    if (effect.updatedInput !== undefined) combined.updatedInput = effect.updatedInput;
    if (effect.updatedToolOutput !== undefined) combined.updatedToolOutput = effect.updatedToolOutput;
    if (Array.isArray(effect.watchPaths)) combined.watchPaths = combined.watchPaths.concat(effect.watchPaths);
  }
  combined.ui = combined.ui.concat(ui);
};

globalThis.__coderExApplyResult = function (combined, result) {
  combined.stop = combined.stop || result.stop;
  if (result.updatedInput !== null && result.updatedInput !== undefined) {
    combined.updatedInput = result.updatedInput;
  }
  if (result.updatedToolOutput !== null && result.updatedToolOutput !== undefined) {
    combined.updatedToolOutput = result.updatedToolOutput;
  }
  combined.watchPaths = combined.watchPaths.concat(result.watchPaths);
  combined.failed = combined.failed || result.failed;
  combined.errors = combined.errors.concat(result.errors);
  combined.ui = combined.ui.concat(result.ui);
};

// The inner executor ran second, so it wins an updated-value conflict; a side
// that left a field unset does not erase what the other set.
globalThis.__coderExMergeWithInner = function (js, inner) {
  return {
    stop: js.stop || inner.stop,
    updatedInput: (inner.updatedInput !== null && inner.updatedInput !== undefined)
      ? inner.updatedInput
      : js.updatedInput,
    updatedToolOutput: (inner.updatedToolOutput !== null && inner.updatedToolOutput !== undefined)
      ? inner.updatedToolOutput
      : js.updatedToolOutput,
    errors: js.errors.concat(inner.errors),
    failed: js.failed || inner.failed,
    watchPaths: __coderExDedupe(js.watchPaths.concat(inner.watchPaths)),
    ui: js.ui.concat(inner.ui)
  };
};

globalThis.__coderExChain = function (hooks, start, event, input, contextJson, innerFn, depth, gating) {
  const combined = __coderExEmptyResult();
  let nextCalled = false;

  for (let index = start; index < hooks.length; index++) {
    const hook = hooks[index];
    let downstream = null;
    let used = false;
    let closed = false;

    const next = function (argument) {
      if (used || closed) {
        throw new TypeError('next(e) may only be called once per hook invocation');
      }
      used = true;
      if (depth + 1 > __coderExMaxChainDepth) {
        throw new RangeError('next(e) exceeded the chain depth limit of ' + __coderExMaxChainDepth);
      }

      let nextInput = input;
      let nextContext = contextJson;
      if (typeof argument === 'string') {
        nextInput = argument;
      } else if (argument !== null && typeof argument === 'object' && !Array.isArray(argument)) {
        if (typeof argument.input === 'string') {
          nextInput = argument.input;
        }
        if (argument.context !== null && typeof argument.context === 'object' && !Array.isArray(argument.context)) {
          const forwarded = {};
          for (const key of Object.keys(argument.context)) {
            if (typeof argument.context[key] === 'string') {
              forwarded[key] = argument.context[key];
            }
          }
          nextContext = JSON.stringify(forwarded);
        }
      }

      downstream = __coderExChain(hooks, index + 1, event, nextInput, nextContext, innerFn, depth + 1, gating);
      return __coderExPublicResult(downstream);
    };

    const reply = JSON.parse(__coderExCall(hook.m, hook.h, event, input, contextJson, next));
    closed = true;

    if (reply.ok) {
      if (downstream !== null) {
        __coderExApplyResult(combined, __coderExMergeEffect(reply.result, reply.ui, downstream));
      } else {
        __coderExApplyEffect(combined, reply.result, reply.ui);
      }
    } else {
      if (downstream !== null) {
        __coderExApplyResult(combined, downstream);
      }
      combined.failed = true;
      combined.errors.push('mod `' + hook.name + '`: ' + reply.error);
    }

    if (downstream !== null) {
      nextCalled = true;
      break;
    }
  }

  // A failed hook on a gating event fails closed, and the gate is closed
  // before the inner executor is reached, exactly as it was before the chain
  // became recursive.
  if (combined.failed && gating) combined.stop = true;

  let result = combined;
  if (!nextCalled && !combined.stop && typeof innerFn === 'function') {
    // The inner executor sees the input the JavaScript hooks settled on, as it
    // did before the chain existed.
    const innerInput = (combined.updatedInput !== null && combined.updatedInput !== undefined)
      ? combined.updatedInput
      : input;
    result = __coderExMergeWithInner(combined, JSON.parse(innerFn(event, innerInput, contextJson)));
  }
  result.watchPaths = __coderExDedupe(result.watchPaths);
  return result;
};

globalThis.__coderExRun = function (chainJson, event, input, contextJson, innerFn, gating) {
  return JSON.stringify(
    __coderExChain(JSON.parse(chainJson), 0, event, input, contextJson, innerFn, 0, gating === true)
  );
};

globalThis.__coderExMaxChainDepth = @@MAX_CHAIN_DEPTH@@;
"#;

/// The per-mod harness: `on` and the registration array it fills.
fn install_mod_harness(context: &mut Context) -> JsResult<()> {
    let max_hooks = MAX_HOOKS_PER_MOD;
    let max_activations = MAX_ACTIVATIONS_PER_MOD;
    context.eval(Source::from_bytes(
        format!(
            r"
'use strict';
globalThis.__coderExRegs = [];
globalThis.__coderExHandlers = [];
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
globalThis.__coderExOnActivate = function (id, handler) {{
  for (let index = 0; index < __coderExHandlers.length; index++) {{
    if (__coderExHandlers[index].id === id) {{
      __coderExHandlers[index] = {{ id: id, handler: handler, host: $ }};
      return undefined;
    }}
  }}
  if (__coderExHandlers.length >= {max_activations}) {{
    throw new RangeError('$.ui.onActivate: a mod may register at most {max_activations} handlers');
  }}
  __coderExHandlers.push({{ id: id, handler: handler, host: $ }});
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

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::ui_protocol::{UiElement, UiStyle};

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
        // Two chained hooks, then the slow inner executor: the budget covers
        // the JavaScript chain and the downstream run inside the same call.
        let runtime = load_with(
            "function register(on) { \
             on('PostToolUse', '*', function ($, e, next) { next(); }); \
             on('PostToolUse', '*', function ($, e, next) { next(); }); }",
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

    // ── Chaining: next runs the hooks after the current one ─────────────────

    #[test]
    fn next_runs_the_following_hook_and_returns_its_result() {
        let runtime = load(
            "function register(on) { \
             on('PostToolUse', '*', function ($, e, next) { \
               const downstream = next(); \
               return { updatedInput: 'A[' + downstream.updatedInput + ']' }; }); \
             on('PostToolUse', '*', function ($, e) { return { updatedInput: 'B' }; }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert_eq!(result.updated_input.as_deref(), Some("A[B]"), "{result:?}");
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn a_downstream_hooks_failure_is_carried_through_next() {
        let runtime = load(
            "function register(on) { \
             on('PostToolUse', '*', function ($, e, next) { \
               const downstream = next(); \
               return { updatedInput: downstream.failed ? 'saw-failure' : 'saw-success' }; }); \
             on('PostToolUse', '*', function () { throw new Error('downstream-broke'); }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert_eq!(result.updated_input.as_deref(), Some("saw-failure"));
        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("downstream-broke")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn a_later_hooks_next_runs_the_inner_executor() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            updated_input: Some("inner".to_string()),
            ..HookFireResult::default()
        }));
        let runtime = load(
            "function register(on) { \
             on('PostToolUse', '*', function ($, e) { return { updatedToolOutput: 'first' }; }); \
             on('PostToolUse', '*', function ($, e, next) { \
               const downstream = next(); \
               return { updatedToolOutput: 'second(' + downstream.updatedInput + ')' }; }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(
            inner.calls(),
            vec![("PostToolUse".to_string(), "Bash".to_string())],
            "the chain must reach the inner executor exactly once"
        );
        assert_eq!(result.updated_tool_output.as_deref(), Some("second(inner)"));
        assert_eq!(
            result.updated_input.as_deref(),
            Some("inner"),
            "a field the hook did not set comes from downstream"
        );
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn three_hooks_chain_in_registration_order() {
        // Each hook records its name before continuing. The nested return
        // value spells out the order the hooks actually ran in.
        let runtime = load(
            "function register(on) { \
             const order = []; \
             on('PostToolUse', '*', function ($, e, next) { \
               order.push('A'); \
               const downstream = next(); \
               return { updatedInput: 'A(' + downstream.updatedInput + ')' }; }); \
             on('PostToolUse', '*', function ($, e, next) { \
               order.push('B'); \
               const downstream = next(); \
               return { updatedInput: 'B(' + downstream.updatedInput + ')' }; }); \
             on('PostToolUse', '*', function ($, e) { \
               order.push('C'); \
               return { updatedInput: order.join('') }; }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert_eq!(
            result.updated_input.as_deref(),
            Some("A(B(ABC))"),
            "{result:?}"
        );
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn next_input_and_context_reach_the_following_hook() {
        let runtime = load(
            "function register(on) { \
             on('PostToolUse', '*', function ($, e, next) { \
               next({ input: 'rewritten', context: { mode: 'review' } }); return {}; }); \
             on('PostToolUse', '*', function ($, e) { \
               return { updatedInput: e.input + '|' + e.mode }; }); }",
        );

        let result = runtime.fire(
            "PostToolUse",
            "Bash",
            &HashMap::from([("mode".to_string(), "original".to_string())]),
        );

        assert_eq!(
            result.updated_input.as_deref(),
            Some("rewritten|review"),
            "{result:?}"
        );
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn hooks_that_do_not_call_next_run_once_and_then_the_inner() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            updated_tool_output: Some("inner".to_string()),
            ..HookFireResult::default()
        }));
        let runtime = load(
            "function register(on) { \
             on('PostToolUse', '*', function ($, e) { return { updatedInput: 'first' }; }); \
             on('PostToolUse', '*', function ($, e) { return { updatedInput: 'second', \
               updatedToolOutput: 'js' }; }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(
            inner.calls(),
            vec![("PostToolUse".to_string(), "second".to_string())],
            "the inner executor runs once, after the hooks and with their input"
        );
        assert_eq!(result.updated_input.as_deref(), Some("second"));
        assert_eq!(
            result.updated_tool_output.as_deref(),
            Some("inner"),
            "the inner executor ran later and wins the conflict"
        );
        assert!(!result.failed, "{result:?}");
    }

    #[test]
    fn a_gating_failure_keeps_the_inner_executor_from_running() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult::default()));
        let runtime = load(
            "function register(on) { on('PreToolUse', 'Bash', function () { \
             throw new Error('broken'); }); }",
        );

        let result = fire_with(&runtime, "PreToolUse", "Bash", Arc::clone(&inner) as _);

        assert!(result.failed, "{result:?}");
        assert!(result.stop, "a gating failure must fail closed: {result:?}");
        assert_eq!(
            inner.calls().len(),
            0,
            "the gate closed before the inner executor was reached"
        );
    }

    #[test]
    fn the_chain_depth_limit_is_a_hook_error() {
        let hooks = MAX_CHAIN_DEPTH + 6;
        let runtime = load(&format!(
            "function register(on) {{ for (let i = 0; i < {hooks}; i++) {{ \
             on('PostToolUse', '*', function ($, e, next) {{ next(); }}); }} }}"
        ));

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("chain depth limit")),
            "{:?}",
            result.errors
        );
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

    /// `e.surface` is exposed so a mod can branch on where it is running.
    /// CodeRex has one surface today, so it is always `terminal`.
    #[test]
    fn the_hook_sees_the_terminal_surface() {
        let runtime = load(
            "function register(on) { on('PreToolUse', '*', function ($, e) { \
             return { updatedInput: String(e.surface) }; }); }",
        );
        let result = runtime.fire("PreToolUse", "Bash", &HashMap::new());
        assert_eq!(result.updated_input.as_deref(), Some("terminal"));
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

    // ── UI elements ─────────────────────────────────────────────────────────

    #[test]
    fn ui_render_contributes_text_boxes_and_buttons_without_a_return_value() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.ui.render($.ui.box({ title: 'Report' }, \
             $.ui.text('hello', 'bold'), $.ui.button('Retry', 'retry'))); }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert!(!result.failed, "{result:?}");
        assert_eq!(
            result.ui,
            vec![UiElement::Box {
                title: Some("Report".to_string()),
                children: vec![
                    UiElement::Text {
                        text: "hello".to_string(),
                        style: UiStyle::Bold,
                    },
                    UiElement::Button {
                        label: "Retry".to_string(),
                        id: "retry".to_string(),
                    },
                ],
            }]
        );
    }

    #[test]
    fn ui_render_accumulates_every_element_in_call_order() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.ui.render($.ui.text('first')); \
             $.ui.render([$.ui.text('second'), $.ui.button('Third', 'third')]); }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert_eq!(
            result.ui,
            vec![
                UiElement::Text {
                    text: "first".to_string(),
                    style: UiStyle::Plain,
                },
                UiElement::Text {
                    text: "second".to_string(),
                    style: UiStyle::Plain,
                },
                UiElement::Button {
                    label: "Third".to_string(),
                    id: "third".to_string(),
                },
            ]
        );
    }

    #[test]
    fn ui_styles_accept_case_insensitive_names_and_default_to_plain() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.ui.render($.ui.text('a'), $.ui.text('b', 'WARNING'), \
             $.ui.text('c', 'Error'), $.ui.text('d', 'dim')); }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        let styles: Vec<_> = result
            .ui
            .iter()
            .map(|element| match element {
                UiElement::Text { style, .. } => *style,
                _ => panic!("expected text elements"),
            })
            .collect();
        assert_eq!(
            styles,
            vec![
                UiStyle::Plain,
                UiStyle::Warning,
                UiStyle::Error,
                UiStyle::Dim
            ]
        );
    }

    #[test]
    fn ui_staging_is_cleared_between_hook_calls() {
        let runtime = load(
            "let calls = 0; \
             function register(on) { on('PostToolUse', '*', function ($) { \
             if (calls++ === 0) { $.ui.render($.ui.text('once')); } }); }",
        );

        let first = runtime.fire("PostToolUse", "Read", &HashMap::new());
        let second = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert_eq!(first.ui.len(), 1, "the first call rendered one element");
        assert!(second.ui.is_empty(), "the second call rendered nothing");
    }

    #[test]
    fn invalid_ui_shapes_become_clear_hook_errors() {
        let cases = [
            ("$.ui.render($.ui.text(42));", "text must be a string"),
            (
                "$.ui.render($.ui.text('hi', 'loud'));",
                "unknown style 'loud'",
            ),
            (
                "$.ui.render($.ui.button('', 'retry'));",
                "label must be a non-empty string",
            ),
            (
                "$.ui.render({ type: 'blink' });",
                "unknown element type 'blink'",
            ),
        ];

        for (statement, expected) in cases {
            let runtime = load(&format!(
                "function register(on) {{ on('PostToolUse', '*', function ($) {{ {statement} }}); }}"
            ));
            let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

            assert!(result.failed, "{statement} was accepted: {result:?}");
            assert!(
                result.errors.iter().any(|error| error.contains(expected)),
                "{statement}: expected {expected:?} in {:?}",
                result.errors
            );
            assert!(result.ui.is_empty(), "{statement} kept a partial tree");
        }
    }

    #[test]
    fn ui_from_a_hook_and_from_next_is_combined() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            ui: vec![UiElement::Text {
                text: "downstream".to_string(),
                style: UiStyle::Dim,
            }],
            ..HookFireResult::default()
        }));
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($, e, next) { \
             $.ui.render($.ui.text('hook')); next(); }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);

        assert_eq!(
            result.ui,
            vec![
                UiElement::Text {
                    text: "hook".to_string(),
                    style: UiStyle::Plain,
                },
                UiElement::Text {
                    text: "downstream".to_string(),
                    style: UiStyle::Dim,
                },
            ]
        );
    }

    // ── UI activation ───────────────────────────────────────────────────────

    #[test]
    fn an_activation_handler_returns_the_elements_it_builds() {
        let runtime = load(
            "function register(on) { \
             $.ui.onActivate('retry', function ($, event) { \
               return [$.ui.text('ran ' + event.id)]; }); }",
        );

        let elements = runtime.activate("retry").expect("the handler succeeds");

        assert_eq!(
            elements,
            vec![UiElement::Text {
                text: "ran retry".to_string(),
                style: UiStyle::Plain,
            }]
        );
    }

    #[test]
    fn an_activation_handler_may_render_and_return_elements() {
        let runtime = load(
            "function register(on) { \
             $.ui.onActivate('go', function ($, event) { \
               $.ui.render($.ui.button('Again', 'again')); \
               return $.ui.text('the event is ' + event.event); }); }",
        );

        let elements = runtime.activate("go").expect("the handler succeeds");

        assert_eq!(
            elements,
            vec![
                UiElement::Button {
                    label: "Again".to_string(),
                    id: "again".to_string(),
                },
                UiElement::Text {
                    text: "the event is ModUiActivate".to_string(),
                    style: UiStyle::Plain,
                },
            ],
            "the staged tree comes first, then what the handler returned"
        );
    }

    #[test]
    fn activating_an_unknown_id_is_ignored() {
        let runtime = load("function register(on) {}");

        assert_eq!(
            runtime.activate("missing").expect("a miss is not an error"),
            Vec::<UiElement>::new()
        );
    }

    #[test]
    fn a_duplicate_activation_registration_replaces_the_earlier_handler() {
        let runtime = load(
            "function register(on) { \
             $.ui.onActivate('go', function () { return [$.ui.text('first')]; }); \
             $.ui.onActivate('go', function () { return [$.ui.text('second')]; }); }",
        );

        let elements = runtime.activate("go").expect("the handler succeeds");

        assert_eq!(
            elements,
            vec![UiElement::Text {
                text: "second".to_string(),
                style: UiStyle::Plain,
            }],
            "the later registration must replace the earlier one"
        );
    }

    #[test]
    fn an_activation_handler_error_is_returned() {
        let runtime = load(
            "function register(on) { \
             $.ui.onActivate('boom', function () { throw new Error('handler broke'); }); }",
        );

        let error = runtime.activate("boom").expect_err("the handler throws");

        assert!(error.contains("handler broke"), "{error}");
    }

    #[test]
    fn an_activation_handler_obeys_the_wall_clock_budget() {
        let limits = ModLimits {
            hook_timeout: Duration::from_millis(50),
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { \
             $.ui.onActivate('slow', function () { while (true) {} }); }",
            limits,
        );

        let started = std::time::Instant::now();
        let error = runtime
            .activate("slow")
            .expect_err("the handler never returns");
        let elapsed = started.elapsed();

        assert!(error.contains("wall-clock budget"), "{error}");
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
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

    // ── TypeScript and JSX ──────────────────────────────────────────────────

    /// Load every `(file name, source)` pair from one directory.
    fn load_files(files: &[(&str, &str)]) -> ModRuntime {
        let directory = tempfile::tempdir().unwrap();
        for (name, source) in files {
            std::fs::write(directory.path().join(name), source).unwrap();
        }
        ModRuntime::load_dir(directory.path(), session()).unwrap()
    }

    #[test]
    fn a_typescript_mod_loads_and_registers() {
        let runtime = load_files(&[(
            "typed.ts",
            "interface Shape { input: string }\n\
             const suffix: string = ' typed';\n\
             function register(on: (event: string, matcher: string, hook: (event: Shape) => unknown) => void): void {\n\
               on('PostToolUse', '*', function ($, e: Shape) {\n\
                 return { updatedInput: e.input + suffix };\n\
               });\n\
             }\n",
        )]);

        assert_eq!(runtime.load_errors(), &[] as &[String]);
        assert_eq!(runtime.hook_count(), 1);
        assert_eq!(
            runtime
                .fire("PostToolUse", "Bash", &HashMap::new())
                .updated_input
                .as_deref(),
            Some("Bash typed")
        );
    }

    #[test]
    fn a_tsx_mod_builds_ui_that_renders_through_the_existing_renderer() {
        let runtime = load_files(&[(
            "panel.tsx",
            "function Badge(props: { label: string }) {\n\
               return <text style=\"bold\">{props.label}</text>;\n\
             }\n\
             function register(on) {\n\
               on('PostToolUse', '*', function ($) {\n\
                 $.ui.render(\n\
                   <box title=\"Typed report\">\n\
                     <text style=\"dim\">hello</text>\n\
                     <Badge label=\"ok\" />\n\
                     <button id=\"retry\">Retry</button>\n\
                   </box>);\n\
               });\n\
             }\n",
        )]);

        assert_eq!(runtime.load_errors(), &[] as &[String]);
        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert!(!result.failed, "{result:?}");
        assert_eq!(
            result.ui,
            vec![UiElement::Box {
                title: Some("Typed report".to_string()),
                children: vec![
                    UiElement::Text {
                        text: "hello".to_string(),
                        style: UiStyle::Dim,
                    },
                    UiElement::Text {
                        text: "ok".to_string(),
                        style: UiStyle::Bold,
                    },
                    UiElement::Button {
                        label: "Retry".to_string(),
                        id: "retry".to_string(),
                    },
                ],
            }]
        );

        let rendered = coderex_ui::mod_elements::render_mod_ui_text(&result.ui, 60);
        for expected in ["Typed report", "hello", "ok", "[Retry]"] {
            assert!(
                rendered.contains(expected),
                "{expected:?} missing from:\n{rendered}"
            );
        }
    }

    #[test]
    fn a_fragment_and_a_component_return_array_are_flattened_into_the_box() {
        let runtime = load_files(&[(
            "fragment.tsx",
            "function Pair() {\n\
               return <><text>one</text><text>two</text></>;\n\
             }\n\
             function register(on) {\n\
               on('PostToolUse', '*', function ($) {\n\
                 $.ui.render(<box><Pair /><text>three</text></box>);\n\
               });\n\
             }\n",
        )]);

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());
        assert!(!result.failed, "{result:?}");
        assert_eq!(
            result.ui,
            vec![UiElement::Box {
                title: None,
                children: vec![
                    UiElement::Text {
                        text: "one".to_string(),
                        style: UiStyle::Plain,
                    },
                    UiElement::Text {
                        text: "two".to_string(),
                        style: UiStyle::Plain,
                    },
                    UiElement::Text {
                        text: "three".to_string(),
                        style: UiStyle::Plain,
                    },
                ],
            }]
        );
    }

    #[test]
    fn an_unknown_jsx_element_is_a_clear_hook_error() {
        let runtime = load_files(&[(
            "unknown.tsx",
            "function register(on) {\n\
               on('PostToolUse', '*', function () { return <div />; });\n\
             }\n",
        )]);

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("unknown element type 'div'")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn a_transform_error_is_reported_with_its_line_and_the_mod_is_skipped() {
        let runtime = load_files(&[
            (
                "broken.ts",
                "function register(on) {\n  this is not TypeScript\n}\n",
            ),
            (
                "good.js",
                "function register(on) { on('PreToolUse', 'Bash', function () { \
                 return { stop: true }; }); }",
            ),
        ]);

        assert_eq!(
            runtime.load_errors().len(),
            1,
            "{:?}",
            runtime.load_errors()
        );
        let error = &runtime.load_errors()[0];
        assert!(error.contains("broken.ts"), "{error}");
        assert!(error.contains("line 2"), "{error}");
        assert_eq!(runtime.hook_count(), 1);
        assert!(runtime.fire("PreToolUse", "Bash", &HashMap::new()).stop);
    }

    #[test]
    fn a_tsx_mod_without_register_is_still_skipped() {
        let runtime = load_files(&[(
            "component.tsx",
            "const label: string = 'hi';\n\
             function Badge() { return <text>{label}</text>; }\n",
        )]);

        assert_eq!(runtime.load_errors().len(), 1);
        assert!(runtime.load_errors()[0].contains("register(on, options)"));
        assert_eq!(runtime.hook_count(), 0);
    }

    #[test]
    fn a_jsx_mod_loads_and_a_plain_js_file_is_not_transformed() {
        let runtime = load_files(&[
            (
                "view.jsx",
                "function register(on) { on('PostToolUse', '*', function ($) { \
                 $.ui.render(<text>from jsx</text>); }); }",
            ),
            (
                "plain.js",
                // A string that only a JavaScript parse would accept: if `.js`
                // went through the JSX/TS transform, this would be a syntax
                // error instead of a registered hook.
                "function register(on) { on('PostToolUse', '*', function () { \
                 return { updatedInput: '<not jsx> !== type' }; }); }",
            ),
        ]);

        assert_eq!(runtime.load_errors(), &[] as &[String]);
        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert_eq!(
            result.ui,
            vec![UiElement::Text {
                text: "from jsx".to_string(),
                style: UiStyle::Plain,
            }]
        );
        assert_eq!(
            result.updated_input.as_deref(),
            Some("<not jsx> !== type"),
            "the .js file keeps its original source"
        );
    }
}
