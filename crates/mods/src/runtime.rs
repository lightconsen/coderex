//! Loading mods and firing their hooks on a long-lived worker.
//!
//! A QuickJS runtime and its contexts are confined to one thread, and `next(e)`
//! needs the same realm to resume in after the downstream chain has run. Each
//! [`ModRuntime`] therefore owns one worker thread, one QuickJS runtime, and
//! one [`Context`] (a realm) per loaded mod for as long as it lives.
//! [`ModRuntime::fire`] sends a request over a channel, waits for the reply
//! under the wall-clock budget, and never touches the engine directly.
//!
//! Unlike boa, QuickJS lets the host cap the heap and interrupt the engine from
//! inside it: the runtime has a memory limit, a maximum stack size and an
//! interrupt handler that runs periodically while JavaScript executes. The
//! handler is driven by a per-request deadline, so a runaway hook is stopped
//! at the wall-clock budget instead of being left to run.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coderex_core::hook_executor::{HookExecutor, HookFireResult};
use coderex_core::ui_protocol::{UiElement, UiStyle, parse_ui_elements};
use rquickjs::function::Opt;
use rquickjs::object::Property;
use rquickjs::{
    Array, Context, Ctx, Function, IntoJs, Object, Persistent, Runtime as QuickJsRuntime, Value,
};
use sha2::{Digest, Sha256};

use crate::transform;

/// Environment variable that opts the in-process runtime in.
pub const MODS_ENV: &str = "CODEREX_MODS";

/// Environment variable that overrides the mods directory.
pub const MODS_DIR_ENV: &str = "CODEREX_MODS_DIR";

/// Environment variable that overrides the timeout circuit-breaker threshold.
///
/// The value is a non-negative integer: how many wall-clock timeouts are
/// tolerated before mods are disabled for the rest of the session. An invalid
/// value falls back to [`DEFAULT_TIMEOUT_BREAKER_LIMIT`]; `0` disables the
/// breaker (timeouts never disable mods).
pub const MODS_TIMEOUT_LIMIT_ENV: &str = "CODEREX_MODS_TIMEOUT_LIMIT";

/// Environment variable that overrides the JavaScript heap cap.
///
/// The value is a byte count with an optional binary suffix (`k`, `m`, `g`,
/// each optionally followed by `i` and/or `b`, case-insensitively), for
/// example `33554432`, `32m` or `32MiB`. Values below
/// [`MIN_MEMORY_LIMIT`] are clamped up to it. An unparseable value falls back
/// to [`DEFAULT_MEMORY_LIMIT`].
pub const MODS_MEMORY_LIMIT_ENV: &str = "CODEREX_MODS_MEMORY_LIMIT";

/// The default JavaScript heap cap: 64 MiB.
pub const DEFAULT_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// The smallest JavaScript heap cap that still leaves room for a runtime, its
/// per-mod realms and the harness (4 MiB).
pub const MIN_MEMORY_LIMIT: usize = 4 * 1024 * 1024;

/// The smallest engine stack accepted (256 KiB, QuickJS's own default).
const MIN_STACK_SIZE: usize = 256 * 1024;

/// The largest engine stack accepted (8 MiB), leaving room on the worker's
/// 16 MiB native stack for Rust frames around the interpreter.
const MAX_STACK_SIZE: usize = 8 * 1024 * 1024;

/// How many wall-clock timeouts disable mods by default.
pub const DEFAULT_TIMEOUT_BREAKER_LIMIT: usize = 3;

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

/// Most `$.session.submit` calls one hook invocation may make.
///
/// A call beyond the cap is dropped and counted; the hook result carries one
/// dim notice saying how many were lost.
pub const MAX_SUBMISSIONS_PER_HOOK_CALL: usize = 2;

/// Most queued user messages a single turn may leave for the driver to drain.
///
/// `$.session.submit(text)` queues a message for after the current turn. A
/// turn can fire many hooks (and each hook call may submit twice), so this
/// bounds what one turn can accumulate. Submissions beyond the cap are
/// dropped, counted, and reported as one dim notice in the hook result's UI.
pub const MAX_PENDING_PROMPTS_PER_TURN: usize = 8;

/// Most nested `next(e)` calls one [`ModRuntime::fire`] may perform.
///
/// Each `next(e)` continues the chain after the calling hook, so the depth is
/// the number of chained hooks plus the final one. Without this bound a mod
/// could build an unbounded `next` recursion; exceeding it is a hook error.
const MAX_CHAIN_DEPTH: usize = 64;

/// Extra time the calling thread waits past the engine's deadline, so an
/// interrupted hook can report its error before the worker is detached.
const TIMEOUT_REPLY_GRACE: Duration = Duration::from_millis(100);

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

/// A cloneable handle to the user messages mods queued with
/// `$.session.submit(text)`.
///
/// The queue belongs to the [`ModRuntime`], not to a worker, so a fresh worker
/// started after a timeout still shares the same pending messages. The runtime
/// fills it after each hook call; the caller that owns the turn loop (the CLI)
/// takes them once the turn is idle. Taking is the turn boundary: the queue is
/// capped at [`MAX_PENDING_PROMPTS_PER_TURN`] while it waits.
#[derive(Clone, Debug, Default)]
pub struct ModSubmitQueue {
    pending: Arc<Mutex<Vec<String>>>,
}

impl ModSubmitQueue {
    /// Take every queued message, in submission order, leaving the queue empty.
    #[must_use]
    pub fn take(&self) -> Vec<String> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *pending)
    }

    /// Whether no message is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many messages are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Append messages and return how many were dropped by the per-turn cap.
    fn push_all(&self, prompts: &[String]) -> usize {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut dropped = 0;
        for prompt in prompts {
            if pending.len() >= MAX_PENDING_PROMPTS_PER_TURN {
                dropped += 1;
            } else {
                pending.push(prompt.clone());
            }
        }
        dropped
    }
}

/// Limits every hook call is held to.
///
/// All three engine limits are installed on the QuickJS runtime the worker
/// owns, so they apply to every realm and every `next(e)` level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModLimits {
    /// Wall-clock budget for one [`ModRuntime::fire`] call.
    ///
    /// The budget covers the whole chain: every matching JavaScript hook and,
    /// when a hook calls `next`, the downstream (inner) executor as well.
    /// QuickJS's interrupt handler is driven by this budget, so a JavaScript
    /// loop that never returns is stopped at the deadline rather than left to
    /// run. The calling thread keeps a small grace period as a backstop for
    /// work the engine cannot interrupt (a downstream executor blocked in a
    /// native call); if that backstop fires, the worker is detached.
    pub hook_timeout: Duration,
    /// Most heap bytes the QuickJS runtime may allocate.
    ///
    /// Enforced by `JS_SetMemoryLimit`. An allocation past the cap throws an
    /// out-of-memory exception, which a hook sees as an ordinary error.
    /// [`DEFAULT_MEMORY_LIMIT`] unless overridden with
    /// [`MODS_MEMORY_LIMIT_ENV`]; never below [`MIN_MEMORY_LIMIT`].
    pub memory_limit: usize,
    /// Most stack bytes the engine may use.
    ///
    /// Enforced by `JS_SetMaxStackSize`; a runaway recursion throws a
    /// `RangeError` instead of overflowing the native stack. One `next(e)`
    /// level uses roughly four JavaScript frames, so this must leave room for
    /// [`MAX_CHAIN_DEPTH`] levels. Clamped to 256 KiB..=8 MiB; the worker
    /// thread's native stack is 16 MiB.
    pub max_stack_size: usize,
    /// Heap growth between automatic garbage collections.
    ///
    /// Enforced by `JS_SetGCThreshold`; clamped below `memory_limit`.
    pub gc_threshold: usize,
}

impl Default for ModLimits {
    fn default() -> Self {
        Self {
            hook_timeout: Duration::from_secs(5),
            memory_limit: configured_memory_limit(),
            // QuickJS defaults to a 256 KiB stack. A full `next` chain is
            // deeply nested JavaScript (the harness chain runner, the hook and
            // its `next` closure at every level), so 4 MiB leaves room for all
            // 64 links while still failing a runaway recursion quickly.
            // QuickJS treats values above 16 MiB as "unlimited".
            max_stack_size: 4 * 1024 * 1024,
            gc_threshold: 2 * 1024 * 1024,
        }
    }
}

/// Resolve the heap cap from [`MODS_MEMORY_LIMIT_ENV`], clamped to the sane
/// minimum. An unset or unparseable value yields [`DEFAULT_MEMORY_LIMIT`].
fn configured_memory_limit() -> usize {
    std::env::var(MODS_MEMORY_LIMIT_ENV)
        .ok()
        .and_then(|value| parse_memory_limit(&value))
        .unwrap_or(DEFAULT_MEMORY_LIMIT)
        .max(MIN_MEMORY_LIMIT)
}

/// Parse a byte count with an optional binary suffix.
///
/// Accepts `33554432`, `32768k`, `32m`, `1g`, and the `KiB`/`MiB`/`GiB`
/// spellings. Returns `None` for an empty, malformed or overflowing value.
fn parse_memory_limit(value: &str) -> Option<usize> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    // Longest spelling first: `mib` before `mb` and `m`.
    let (digits, multiplier) = [
        ("kib", 1024usize),
        ("kb", 1024),
        ("k", 1024),
        ("mib", 1024 * 1024),
        ("mb", 1024 * 1024),
        ("m", 1024 * 1024),
        ("gib", 1024 * 1024 * 1024),
        ("gb", 1024 * 1024 * 1024),
        ("g", 1024 * 1024 * 1024),
        ("b", 1),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        value
            .strip_suffix(suffix)
            .map(|digits| (digits, multiplier))
    })
    .unwrap_or((value.as_str(), 1));
    let digits: usize = digits.trim().parse().ok()?;
    digits.checked_mul(multiplier)
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

/// What the runtime knows about one file in the mods directory.
///
/// A report entry exists for every `.js`, `.jsx`, `.ts` and `.tsx` file found,
/// including the ones that were skipped. `loaded` means the file evaluated,
/// defined `register(on, options)`, and registered without error; `errors`
/// says why a skipped file was skipped. An empty `sha256` means the file could
/// not be read, so there were no bytes to hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModInfo {
    /// The file name, as loaded (`options.name`).
    pub name: String,
    /// The full path the file was read from.
    pub path: PathBuf,
    /// Lowercase hex SHA-256 of the **original file bytes**, before any
    /// TypeScript/JSX transform. Informational only; see `docs/mods.md`.
    pub sha256: String,
    /// Whether the mod evaluated and registered.
    pub loaded: bool,
    /// Why the mod was skipped; empty when it loaded.
    pub errors: Vec<String>,
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
    /// Every load error, formatted as `<path>: <reason>`.
    load_errors: Vec<String>,
    /// Per-mod load errors, aligned with [`WorkerConfig::mods`]: `None` when
    /// that mod loaded, `Some(reason)` when it was skipped.
    mod_errors: Vec<Option<String>>,
}

/// A request from the caller to the worker.
struct Request {
    event: String,
    input: String,
    context: HashMap<String, String>,
    inner: Option<Arc<dyn HookExecutor>>,
    reply: mpsc::SyncSender<Reply>,
}

/// A UI activation request from the caller to the worker.
struct ActivateRequest {
    id: String,
    reply: mpsc::SyncSender<ActivateReply>,
}

/// A worker result plus whether the engine, not the caller's backstop, stopped
/// the call at the wall-clock deadline.
///
/// The distinction matters for the circuit breaker: every timeout counts, but
/// only a timeout that the engine could not interrupt (for example a
/// downstream executor blocked in a native call) forces the caller to detach
/// the worker.
struct Reply {
    result: HookFireResult,
    timed_out: bool,
}

/// The activation counterpart of [`Reply`].
struct ActivateReply {
    result: Result<Vec<UiElement>, String>,
    timed_out: bool,
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
/// QuickJS runtime and one context (realm) per mod for the lifetime of the
/// runtime. What remains here is strings, configuration and a channel, which
/// is what lets [`JsHookExecutor`](crate::JsHookExecutor) be `Send + Sync`.
#[derive(Debug)]
pub struct ModRuntime {
    dir: PathBuf,
    session: ModSession,
    limits: ModLimits,
    hook_count: usize,
    load_errors: Vec<String>,
    /// One entry per mod file found, including skipped files.
    report: Vec<ModInfo>,
    /// Wall-clock timeouts tolerated before the breaker trips. `0` means the
    /// breaker is disabled.
    timeout_breaker_limit: usize,
    /// Hook calls that exceeded the wall-clock budget, whether the engine
    /// interrupted them or the caller's backstop detached the worker.
    timeout_abandonments: AtomicUsize,
    /// Whether mods have been disabled for the rest of the session.
    disabled: AtomicBool,
    /// Messages mods queued with `$.session.submit(text)`.
    ///
    /// Filled after every hook call and drained by the caller that owns the
    /// turn loop. The engine never reads it.
    submissions: ModSubmitQueue,
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
        let mut report = Vec::new();
        // Report indexes of the files that reached the worker, so the worker's
        // per-mod outcome can be folded into the report afterwards.
        let mut worker_report_indices = Vec::new();
        for (name, file_path, kind) in files {
            let (source, sha256) = match read_source(&file_path) {
                Ok(loaded) => loaded,
                Err(error) => {
                    record_load_error(&mut load_errors, &file_path, &error);
                    report.push(ModInfo {
                        name,
                        path: file_path,
                        sha256: String::new(),
                        loaded: false,
                        errors: vec![error],
                    });
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
                        report.push(ModInfo {
                            name,
                            path: file_path,
                            sha256,
                            loaded: false,
                            errors: vec![error],
                        });
                        continue;
                    }
                }
            } else {
                source
            };
            worker_report_indices.push(report.len());
            report.push(ModInfo {
                name: name.clone(),
                path: file_path.clone(),
                sha256,
                loaded: false,
                errors: Vec::new(),
            });
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

        for (worker_index, report_index) in worker_report_indices.iter().enumerate() {
            let info = &mut report[*report_index];
            match outcome
                .mod_errors
                .get(worker_index)
                .and_then(Option::as_ref)
            {
                Some(error) => info.errors.push(error.clone()),
                None => info.loaded = true,
            }
        }

        Ok(Self {
            dir: path.to_path_buf(),
            session,
            limits,
            hook_count: outcome.hooks.len(),
            load_errors,
            report,
            timeout_breaker_limit: configured_timeout_breaker_limit(),
            timeout_abandonments: AtomicUsize::new(0),
            disabled: AtomicBool::new(false),
            submissions: ModSubmitQueue::default(),
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

    /// A handle to the messages mods queued for after the current turn.
    #[must_use]
    pub fn submit_queue(&self) -> ModSubmitQueue {
        self.submissions.clone()
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

    /// One entry per mod file found, loaded or skipped.
    ///
    /// `sha256` hashes the original file bytes (not the transformed source)
    /// and is informational only: there is no signature or trust store.
    #[must_use]
    pub fn load_report(&self) -> &[ModInfo] {
        &self.report
    }

    /// Whether the timeout circuit breaker has disabled mods for the rest of
    /// this session.
    ///
    /// Once this is `true` it stays `true`: no worker is started again, and
    /// every gating `fire` fails closed until a new runtime (a new process or
    /// session) is created.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    /// How many hook calls have exceeded the wall-clock budget.
    ///
    /// Ordinary hook errors are not counted; only timeouts are. The name is
    /// historical: since the engine now interrupts itself, most timeouts stop
    /// the JavaScript and leave the worker usable.
    #[must_use]
    pub fn timeout_abandonments(&self) -> usize {
        self.timeout_abandonments.load(Ordering::SeqCst)
    }

    /// The configured breaker threshold. `0` means the breaker is disabled.
    #[must_use]
    pub const fn timeout_breaker_limit(&self) -> usize {
        self.timeout_breaker_limit
    }

    /// Why mods are disabled, or `None` while they are still enabled.
    #[must_use]
    pub fn disabled_reason(&self) -> Option<String> {
        self.is_disabled().then(|| {
            format!(
                "mods are disabled for the rest of this session: {} hook call(s) exceeded the \
                 {} ms wall-clock budget (threshold {}); restart CodeRex to re-enable mods",
                self.timeout_abandonments(),
                self.limits.hook_timeout.as_millis(),
                self.timeout_breaker_limit,
            )
        })
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
    ///
    /// Once the timeout circuit breaker has tripped (see
    /// [`is_disabled`](Self::is_disabled)), no worker is contacted. The call
    /// records an error and fails closed on a gating event; on a non-gating
    /// event it still delegates to `inner`, so settings hooks keep working
    /// while mods are off.
    #[must_use]
    pub fn fire_with_inner(
        &self,
        event: &str,
        input: &str,
        context: &HashMap<String, String>,
        inner: Option<Arc<dyn HookExecutor>>,
    ) -> HookFireResult {
        let result = if let Some(reason) = self.disabled_reason() {
            let mut result = failure_result(event, reason);
            // A gating event is already denied; do not run the downstream
            // hooks behind a closed gate. For anything else, behave as if the
            // mods were not installed: run the inner executor unchanged.
            if !result.stop {
                if let Some(inner) = inner {
                    let inner_result = run_inner_blocking(&inner, event, input, context);
                    result = merge_with_inner(result, inner_result);
                }
            }
            result
        } else {
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
        };
        self.stage_prompts(result)
    }

    /// Queue the messages a hook call submitted and report any drops.
    ///
    /// The queue, not this call's result, is what the CLI drains after the
    /// turn. A drop notice is appended to the result's UI so the person sees
    /// why a submission did not arrive. The engine ignores
    /// [`HookFireResult::pending_prompts`] itself.
    fn stage_prompts(&self, mut result: HookFireResult) -> HookFireResult {
        let dropped = self.submissions.push_all(&result.pending_prompts);
        if dropped > 0 {
            result.ui.push(UiElement::Text {
                text: format!(
                    "[{dropped} mod submission(s) dropped: at most \
                     {MAX_PENDING_PROMPTS_PER_TURN} queued messages per turn]"
                ),
                style: UiStyle::Dim,
            });
        }
        result
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
        if let Some(reason) = self.disabled_reason() {
            return Err(reason);
        }
        let (reply, reply_receiver) = mpsc::sync_channel(1);
        let sender = self.worker_sender()?;
        sender
            .send(WorkerRequest::Activate(ActivateRequest {
                id: id.to_string(),
                reply,
            }))
            .map_err(|_| "the mod worker stopped before the activation could run".to_string())?;

        match reply_receiver.recv_timeout(self.reply_budget()) {
            Ok(reply) => {
                if reply.timed_out {
                    self.record_timeout();
                }
                reply.result
            }
            Err(RecvTimeoutError::Timeout) => {
                self.record_timeout();
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

    /// How long the calling thread waits for the worker's reply.
    ///
    /// The engine is given the full `hook_timeout` and stops JavaScript from
    /// inside its own interrupt handler. The extra grace lets that interrupted
    /// call travel back over the channel before the caller gives up on the
    /// worker. The caller-side deadline remains the backstop for work QuickJS
    /// cannot interrupt, such as a downstream executor blocked in a native
    /// call.
    const fn reply_budget(&self) -> Duration {
        self.limits.hook_timeout.saturating_add(TIMEOUT_REPLY_GRACE)
    }

    fn dispatch(
        &self,
        request: Request,
        reply_receiver: &mpsc::Receiver<Reply>,
    ) -> Result<HookFireResult, String> {
        let sender = self.worker_sender()?;
        sender
            .send(WorkerRequest::Fire(request))
            .map_err(|_| "the mod worker stopped before the hook could run".to_string())?;

        match reply_receiver.recv_timeout(self.reply_budget()) {
            Ok(reply) => {
                if reply.timed_out {
                    self.record_timeout();
                }
                Ok(reply.result)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.record_timeout();
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
        // Read the breaker under the worker lock: once it trips, no caller may
        // start a replacement worker.
        if let Some(reason) = self.disabled_reason() {
            return Err(reason);
        }
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

    /// Count one wall-clock timeout and trip the breaker at the threshold.
    ///
    /// Only timeouts call this, whether the engine interrupted the hook or the
    /// caller's backstop detached the worker. An ordinary hook error, a
    /// disconnected worker or a memory/stack error does not. A threshold of
    /// `0` disables the breaker.
    fn record_timeout(&self) {
        if self.timeout_breaker_limit == 0 {
            return;
        }
        let count = self.timeout_abandonments.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= self.timeout_breaker_limit && !self.disabled.swap(true, Ordering::SeqCst) {
            tracing::error!(
                timeouts = count,
                threshold = self.timeout_breaker_limit,
                "mods disabled for the rest of the session after repeated hook timeouts"
            );
        }
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

/// Resolve the breaker threshold from the environment, falling back to the
/// default. A value of `0` disables the breaker.
fn configured_timeout_breaker_limit() -> usize {
    std::env::var(MODS_TIMEOUT_LIMIT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_BREAKER_LIMIT)
}

/// Budget for the downstream executor when no call deadline is in force.
///
/// This is the path where mods are disabled and the caller runs the inner
/// executor directly; it still must not hang forever.
const DOWNSTREAM_BUDGET_FALLBACK: Duration = Duration::from_secs(5);

/// Run the inner executor, bounded by `budget`.
///
/// QuickJS interrupts JavaScript at the deadline, but a native `block_on` for
/// the downstream executor is outside the engine. Bounding it here means the
/// worker returns at the deadline (with a failed result) instead of leaving
/// the caller to abandon the thread.
///
/// The timeout needs the executor's future to yield. An executor that blocks
/// the worker thread inside a single poll cannot be cut off here; that case
/// still falls back to the caller's reply budget, which abandons the worker.
fn run_inner_bounded(
    runtime: &tokio::runtime::Runtime,
    budget: Duration,
    inner: &Arc<dyn HookExecutor>,
    event: &str,
    input: &str,
    context: &HashMap<String, String>,
) -> HookFireResult {
    // Build the timer inside `block_on`: `tokio::time::timeout` registers with
    // the runtime's timer driver as soon as it is constructed.
    runtime
        .block_on(async {
            tokio::time::timeout(budget, inner.fire_hook(event, input, context)).await
        })
        .unwrap_or_else(|_| HookFireResult {
            failed: true,
            errors: vec![
                "the downstream hook executor did not finish within the wall-clock budget"
                    .to_string(),
            ],
            ..HookFireResult::default()
        })
}

/// Run the downstream executor once while mods are disabled.
///
/// The normal path runs the inner executor on the worker's own current-thread
/// runtime. With the worker gone, the calling thread supplies one instead. The
/// callers of [`ModRuntime::fire_with_inner`] are blocking contexts (the async
/// adapter uses `spawn_blocking`), so this does not nest inside a runtime.
fn run_inner_blocking(
    inner: &Arc<dyn HookExecutor>,
    event: &str,
    input: &str,
    context: &HashMap<String, String>,
) -> HookFireResult {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => run_inner_bounded(
            &runtime,
            DOWNSTREAM_BUDGET_FALLBACK,
            inner,
            event,
            input,
            context,
        ),
        Err(error) => HookFireResult {
            failed: true,
            errors: vec![format!(
                "mods are disabled and the downstream hooks could not be run: {error}"
            )],
            ..HookFireResult::default()
        },
    }
}

/// Builds a runtime with an explicit session and limits.
#[derive(Debug, Clone)]
pub struct ModRuntimeBuilder {
    session: ModSession,
    limits: ModLimits,
}

impl ModRuntimeBuilder {
    /// Start with the given session snapshot and the default limits.
    #[must_use]
    pub fn new(session: ModSession) -> Self {
        Self {
            session,
            limits: ModLimits::default(),
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

/// Read one mod file and hash its original bytes.
///
/// Returns the source and the lowercase hex SHA-256 of the bytes on disk. The
/// hash is taken before any TypeScript/JSX lowering, so it identifies the file
/// the person installed rather than the runtime's transformed copy.
fn read_source(path: &Path) -> Result<(String, String), String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("could not inspect the file: {error}"))?;
    if metadata.len() > MAX_MOD_BYTES {
        return Err(format!(
            "the file is larger than the {MAX_MOD_BYTES}-byte limit"
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("could not read the file: {error}"))?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let source = String::from_utf8(bytes)
        .map_err(|_| "could not read the file: the file is not valid UTF-8".to_string())?;
    Ok((source, sha256))
}

/// Start a worker thread and wait until it has loaded the mods.
fn spawn_worker(
    config: Arc<WorkerConfig>,
) -> Result<(mpsc::Sender<WorkerRequest>, InitOutcome), String> {
    let (sender, receiver) = mpsc::channel();
    let (init_sender, init_receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("coderex-mods".to_string())
        // The engine's stack limit is checked against the thread's stack, so
        // the thread must be comfortably larger than `max_stack_size` (4 MiB
        // by default). Rust's 2 MiB default would abort before QuickJS could
        // raise its catchable "stack size exceeded" error.
        .stack_size(16 * 1024 * 1024)
        .spawn(move || worker_main(config, &receiver, &init_sender))
        .map_err(|error| format!("could not start the mod worker: {error}"))?;

    match init_receiver.recv() {
        Ok(Ok(outcome)) => Ok((sender, outcome)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("the mod worker stopped during loading".to_string()),
    }
}

fn worker_main(
    config: Arc<WorkerConfig>,
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
        mod_errors: state.mod_errors.clone(),
    };
    if init.send(Ok(outcome)).is_err() {
        return;
    }

    while let Ok(request) = receiver.recv() {
        match request {
            WorkerRequest::Fire(request) => {
                let reply = state.fire(&request);
                let _ = request.reply.send(reply);
            }
            WorkerRequest::Activate(request) => {
                let reply = state.activate(&request.id);
                let _ = request.reply.send(reply);
            }
        }
    }
}

/// The interrupt deadline shared by the caller and the engine.
///
/// The worker sets it before each call and clears it afterwards. While it is
/// set, QuickJS calls [`Deadline::expired`] periodically; the first call that
/// is past the deadline makes QuickJS raise an uncatchable exception and
/// return control to the worker, which reports the timeout.
#[derive(Debug, Clone, Default)]
struct Deadline(Arc<Mutex<DeadlineState>>);

#[derive(Debug, Default)]
struct DeadlineState {
    deadline: Option<Instant>,
    /// Set by the interrupt handler; read after the engine returns.
    interrupted: bool,
}

impl Deadline {
    /// Start a call with `budget` of wall-clock time.
    fn begin(&self, budget: Duration) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.deadline = Instant::now().checked_add(budget);
        state.interrupted = false;
    }

    /// The interrupt handler's check. Returns `true` once the deadline passes.
    fn expired(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired = state
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        if expired {
            state.interrupted = true;
        }
        expired
    }

    /// Time left before the deadline, or `None` when no call is in flight.
    ///
    /// Used to bound the native hop into the downstream executor: QuickJS can
    /// interrupt JavaScript, but not a Rust `block_on`, so that hop gets the
    /// same budget and returns at the deadline instead of being abandoned.
    fn remaining(&self) -> Option<Duration> {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Finish a call and report whether the engine interrupted it.
    fn end(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.deadline = None;
        std::mem::take(&mut state.interrupted)
    }
}

/// The engine (one QuickJS runtime and one realm per mod) confined to the
/// worker thread.
///
/// Fields drop in declaration order, so the contexts (and the values they
/// own) are released before the runtime that created them.
struct Engine {
    main: Context,
    /// Keeps each mod's realm alive: the main harness stores functions that
    /// were created in those realms.
    #[allow(dead_code)]
    mods: Vec<Context>,
    /// Owns the runtime. The contexts hold clones, so this field is what keeps
    /// it alive until they have all been dropped.
    #[allow(dead_code)]
    js: QuickJsRuntime,
}

/// Everything a successful load produced.
struct EngineLoad {
    engine: Engine,
    hooks: Vec<WorkerHook>,
    load_errors: Vec<String>,
    mod_errors: Vec<Option<String>>,
    mod_names: Vec<String>,
}

impl Engine {
    /// Create the runtime, apply its limits, and load every mod in its own
    /// realm.
    ///
    /// A per-mod failure is reported in [`EngineLoad`] and does not fail the
    /// whole load. Fatal failures (the runtime cannot start, the main harness
    /// cannot be installed, or a registration array cannot be stored) abort.
    fn build(config: &WorkerConfig, deadline: &Deadline) -> Result<EngineLoad, String> {
        let js = QuickJsRuntime::new()
            .map_err(|error| format!("could not start the JavaScript runtime: {error}"))?;
        // The heap cap is the one limit the engine enforces on every
        // allocation, including a single large native one.
        let memory_limit = config.limits.memory_limit.max(MIN_MEMORY_LIMIT);
        js.set_memory_limit(memory_limit);
        // Keep the engine's stack well below the worker thread's native stack
        // (16 MiB). QuickJS treats a limit above 16 MiB as "unlimited".
        let stack = config
            .limits
            .max_stack_size
            .clamp(MIN_STACK_SIZE, MAX_STACK_SIZE);
        js.set_max_stack_size(stack);
        if config.limits.gc_threshold > 0 {
            // Keep the GC threshold below the heap cap, or the engine could hit
            // the cap between collections.
            let gc = config
                .limits
                .gc_threshold
                .min(memory_limit / 2)
                .max(64 * 1024);
            js.set_gc_threshold(gc);
        }
        let interrupt = deadline.clone();
        js.set_interrupt_handler(Some(Box::new(move || interrupt.expired())));

        let main = Context::full(&js)
            .map_err(|error| format!("could not create the JavaScript context: {error}"))?;
        main.with(|ctx| install_main_harness(&ctx))
            .map_err(|error| format!("could not install the runtime harness: {error}"))?;

        let mut mods = Vec::new();
        let mut hooks = Vec::new();
        let mut load_errors = Vec::new();
        let mut mod_errors = Vec::new();
        let mut mod_names = Vec::new();

        for source in &config.mods {
            let mod_context =
                Context::full(&js).map_err(|error| format!("could not create a realm: {error}"))?;
            // Loading runs mod top-level code, so it gets the same wall-clock
            // protection as a hook. A top-level infinite loop is skipped as a
            // load error instead of hanging the session.
            deadline.begin(config.limits.hook_timeout);
            let loaded = mod_context.with(|ctx| load_mod_in_realm(&ctx, source, &config.session));
            let load_timed_out = deadline.end();
            let loaded = if load_timed_out {
                Err(budget_error(&config.limits, "mod load"))
            } else {
                loaded
            };
            match loaded {
                Ok(loaded) => {
                    let mod_index = mod_names.len();
                    push_registrations(&main, &loaded)?;
                    // `hook_index` is the index within the mod's registration
                    // array, which the harness uses to look the hook up again.
                    for (hook_index, (event, matcher)) in loaded.hooks.into_iter().enumerate() {
                        hooks.push(WorkerHook {
                            mod_index,
                            hook_index,
                            event,
                            matcher,
                        });
                    }
                    mod_names.push(source.name.clone());
                    mod_errors.push(None);
                    mods.push(mod_context);
                }
                Err(error) => {
                    load_errors.push(format!("{}: {error}", source.path.display()));
                    mod_errors.push(Some(error));
                }
            }
        }

        Ok(EngineLoad {
            engine: Self { main, mods, js },
            hooks,
            load_errors,
            mod_errors,
            mod_names,
        })
    }
}

/// Store one mod's registration and activation arrays in the main realm's
/// registry.
fn push_registrations(main: &Context, loaded: &LoadedMod) -> Result<(), String> {
    main.with(|ctx| {
        let registry: Array = ctx
            .globals()
            .get("__coderExMods")
            .map_err(|error| format!("could not read the runtime harness: {error}"))?;
        let registrations = loaded
            .registrations
            .clone()
            .restore(&ctx)
            .map_err(|error| format!("could not register a mod: {error}"))?;
        registry
            .set(registry.len(), registrations)
            .map_err(|error| format!("could not register a mod: {error}"))?;

        let activation_registry: Array = ctx
            .globals()
            .get("__coderExActivations")
            .map_err(|error| format!("could not read the runtime activation registry: {error}"))?;
        let activations = loaded
            .activations
            .clone()
            .restore(&ctx)
            .map_err(|error| format!("could not register a mod's UI handlers: {error}"))?;
        activation_registry
            .set(activation_registry.len(), activations)
            .map_err(|error| format!("could not register a mod's UI handlers: {error}"))?;
        Ok(())
    })
}

/// The worker thread's engine state.
struct WorkerState {
    config: Arc<WorkerConfig>,
    runtime: Arc<tokio::runtime::Runtime>,
    deadline: Deadline,
    /// `None` after the engine was interrupted (or failed to rebuild), so the
    /// next request starts from a fresh runtime and reloads the mods.
    engine: Option<Engine>,
    hooks: Vec<WorkerHook>,
    load_errors: Vec<String>,
    /// Per-mod load errors, aligned with the configured mod order.
    mod_errors: Vec<Option<String>>,
    mod_names: Vec<String>,
}

impl WorkerState {
    /// Build the runtime, one realm per mod, and the flattened hook list.
    fn load(
        config: Arc<WorkerConfig>,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, String> {
        let deadline = Deadline::default();
        let loaded = Engine::build(&config, &deadline)?;
        Ok(Self {
            config,
            runtime,
            deadline,
            engine: Some(loaded.engine),
            hooks: loaded.hooks,
            load_errors: loaded.load_errors,
            mod_errors: loaded.mod_errors,
            mod_names: loaded.mod_names,
        })
    }

    /// Make sure an engine is installed, rebuilding it after an interrupt.
    ///
    /// QuickJS can be reused after an interrupt, but the interrupted hook may
    /// have left its own realm half-mutated. Rebuilding gives the next call a
    /// clean heap and fresh mod state, and makes the recovery explicit. The
    /// loaded hook metadata is unchanged: the same files are loaded from the
    /// same configuration.
    fn ensure_engine(&mut self) -> Result<(), String> {
        if self.engine.is_none() {
            let loaded = Engine::build(&self.config, &self.deadline)?;
            self.engine = Some(loaded.engine);
        }
        Ok(())
    }

    /// Run every matching JavaScript hook for one request.
    fn fire(&mut self, request: &Request) -> Reply {
        if let Err(error) = self.ensure_engine() {
            return Reply {
                result: failure_result(&request.event, error),
                timed_out: false,
            };
        }

        let context_json = serde_json::to_string(&request.context).unwrap_or_else(|_| "{}".into());
        let chain = self.chain_json(request);
        let inner = Arc::new(InnerCall {
            runtime: Arc::clone(&self.runtime),
            inner: request.inner.clone(),
            deadline: self.deadline.clone(),
            called: AtomicBool::new(false),
        });

        self.deadline.begin(self.config.limits.hook_timeout);
        let outcome = {
            let engine = self.engine.as_ref().expect("engine was just ensured");
            engine
                .main
                .with(|ctx| run_chain(&ctx, &chain, &context_json, request, &inner))
        };
        let timed_out = self.deadline.end();
        if timed_out {
            self.engine = None;
        }

        match outcome {
            Ok(mut result) => {
                if result.failed && is_gating_event(&request.event) {
                    result.stop = true;
                }
                Reply { result, timed_out }
            }
            Err(error) => {
                let message = if timed_out {
                    budget_error(&self.config.limits, "hook")
                } else {
                    error
                };
                let mut result = failure_result(&request.event, message);
                // An engine error aborts the whole harness call, so the chain
                // may not have reached its inner-executor step. Preserve the
                // old behaviour for a non-gating event: run the downstream
                // executor once with the request input when it never ran.
                // A timeout does not: the downstream work is what may have
                // outlived the budget, and re-running it would ignore that.
                if !timed_out
                    && !result.stop
                    && !inner.called.load(Ordering::SeqCst)
                    && let Some(executor) = &request.inner
                {
                    let budget = self
                        .deadline
                        .remaining()
                        .unwrap_or(DOWNSTREAM_BUDGET_FALLBACK);
                    let inner_result = run_inner_bounded(
                        &self.runtime,
                        budget,
                        executor,
                        &request.event,
                        &request.input,
                        &request.context,
                    );
                    result = merge_with_inner(result, inner_result);
                }
                Reply { result, timed_out }
            }
        }
    }

    /// Run the handler registered for one button id.
    fn activate(&mut self, id: &str) -> ActivateReply {
        if let Err(error) = self.ensure_engine() {
            return ActivateReply {
                result: Err(error),
                timed_out: false,
            };
        }

        self.deadline.begin(self.config.limits.hook_timeout);
        let outcome = {
            let engine = self.engine.as_ref().expect("engine was just ensured");
            engine.main.with(|ctx| run_activation(&ctx, id))
        };
        let timed_out = self.deadline.end();
        if timed_out {
            self.engine = None;
        }

        let result = match outcome {
            // `Ok(Err(..))` is the handler's own error message, already
            // formatted by the harness.
            Ok(result) => result,
            Err(error) => Err(if timed_out {
                budget_error(&self.config.limits, "activation handler")
            } else {
                error
            }),
        };
        ActivateReply { result, timed_out }
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
}

/// What one evaluated mod left behind in its realm.
struct LoadedMod {
    /// The mod's `on(...)` registrations.
    registrations: Persistent<Array<'static>>,
    /// The mod's `$.ui.onActivate(...)` registrations.
    activations: Persistent<Array<'static>>,
    /// `(event, matcher)` per registration, in registration order.
    hooks: Vec<(String, String)>,
}

/// Install the main harness in the main realm.
fn install_main_harness(ctx: &Ctx<'_>) -> Result<(), String> {
    let harness = MAIN_HARNESS
        .replace("@@MAX_CHAIN_DEPTH@@", &MAX_CHAIN_DEPTH.to_string())
        .replace(
            "@@MAX_SUBMISSIONS_PER_CALL@@",
            &MAX_SUBMISSIONS_PER_HOOK_CALL.to_string(),
        )
        .replace(
            "@@MAX_PENDING_PROMPTS@@",
            &MAX_PENDING_PROMPTS_PER_TURN.to_string(),
        );
    ctx.eval::<(), _>(harness.as_str())
        .map_err(|error| js_error(ctx, error))?;
    let _: Array = ctx
        .globals()
        .get("__coderExMods")
        .map_err(|error| js_error(ctx, error))?;
    let _: Array = ctx
        .globals()
        .get("__coderExActivations")
        .map_err(|error| js_error(ctx, error))?;
    Ok(())
}

/// Evaluate one mod in its own realm and collect its registrations.
fn load_mod_in_realm(
    ctx: &Ctx<'_>,
    source: &ModSource,
    session: &ModSession,
) -> Result<LoadedMod, String> {
    install_host_api(ctx, session, &source.name)
        .map_err(|error| format!("could not install the host API: {error}"))?;

    // Boa evaluated mod files as sloppy-mode scripts (a mod's own
    // `'use strict'` directive still applies); keep that.
    let mut options = rquickjs::context::EvalOptions::default();
    options.strict = false;
    ctx.eval_with_options::<(), _>(source.source.as_str(), options)
        .map_err(|error| format!("could not evaluate the mod: {}", js_error(ctx, error)))?;

    install_mod_harness(ctx)
        .map_err(|error| format!("could not install the runtime harness: {error}"))?;

    let has_register: bool = ctx
        .eval("typeof register === 'function'")
        .map_err(|error| format!("could not inspect the mod: {}", js_error(ctx, error)))?;
    if !has_register {
        return Err("the mod does not define a global register(on, options) function".to_string());
    }

    let options = js_string_literal(&source.name);
    ctx.eval::<(), _>(format!("register(on, Object.freeze({{ name: {options} }}))").as_str())
        .map_err(|error| format!("register() failed: {}", js_error(ctx, error)))?;

    let registrations: Array = ctx.globals().get("__coderExRegs").map_err(|error| {
        format!(
            "register() did not leave readable hook registrations: {}",
            js_error(ctx, error)
        )
    })?;
    let activations: Array = ctx.globals().get("__coderExHandlers").map_err(|error| {
        format!(
            "register() did not leave readable activation registrations: {}",
            js_error(ctx, error)
        )
    })?;

    let mut hooks = Vec::new();
    for index in 0..registrations.len() {
        let Ok(entry) = registrations.get::<Object>(index) else {
            continue;
        };
        let (Some(event), Some(matcher)) = (
            string_property(&entry, "event"),
            string_property(&entry, "matcher"),
        ) else {
            continue;
        };
        hooks.push((event, matcher));
    }

    Ok(LoadedMod {
        registrations: Persistent::save(ctx, registrations),
        activations: Persistent::save(ctx, activations),
        hooks,
    })
}

/// Read a string property, or `None` when it is missing or not a string.
fn string_property(object: &Object<'_>, name: &str) -> Option<String> {
    let value = object.get::<_, Option<Value>>(name).ok().flatten()?;
    value.as_string().and_then(|string| string.to_string().ok())
}

/// The inner executor the JavaScript chain runs at its end.
struct InnerCall {
    runtime: Arc<tokio::runtime::Runtime>,
    inner: Option<Arc<dyn HookExecutor>>,
    /// The call's shared deadline, so the native hop is bounded like the JS.
    deadline: Deadline,
    /// Whether the executor was already reached, so a fatal harness error
    /// cannot make [`WorkerState::fire`] run it a second time.
    called: AtomicBool,
}

/// The body of the chain's final step: run the inner executor synchronously
/// and return its result as JSON for the harness to merge.
fn inner_entry(event: &str, input: &str, context_json: &str, call: &Arc<InnerCall>) -> String {
    let inner_context: HashMap<String, String> =
        serde_json::from_str(context_json).unwrap_or_default();
    call.called.store(true, Ordering::SeqCst);
    let budget = call
        .deadline
        .remaining()
        .unwrap_or(DOWNSTREAM_BUDGET_FALLBACK);
    let result = call
        .inner
        .as_ref()
        .map_or_else(HookFireResult::default, |inner| {
            run_inner_bounded(&call.runtime, budget, inner, event, input, &inner_context)
        });
    serde_json::json!({
        "stop": result.stop,
        "updatedInput": result.updated_input,
        "updatedToolOutput": result.updated_tool_output,
        "watchPaths": result.watch_paths,
        "errors": result.errors,
        "failed": result.failed,
        "ui": result.ui,
        "pendingPrompts": result.pending_prompts,
    })
    .to_string()
}

/// Call the harness chain runner with one native inner-executor function.
fn run_chain(
    ctx: &Ctx<'_>,
    chain: &str,
    context_json: &str,
    request: &Request,
    inner_call: &Arc<InnerCall>,
) -> Result<HookFireResult, String> {
    let captured = Arc::clone(inner_call);
    let inner = Function::new(
        ctx.clone(),
        move |event: String, input: String, context_json: String| -> String {
            inner_entry(&event, &input, &context_json, &captured)
        },
    )
    .map_err(|error| {
        format!(
            "could not install the downstream executor: {}",
            js_error(ctx, error)
        )
    })?;

    let runner: Function = ctx.globals().get("__coderExRun").map_err(|error| {
        format!(
            "could not read the runtime harness: {}",
            js_error(ctx, error)
        )
    })?;
    let text: String = runner
        .call((
            chain,
            request.event.clone(),
            request.input.clone(),
            context_json,
            inner,
            is_gating_event(&request.event),
        ))
        .map_err(|error| {
            format!(
                "the hook chain could not be called: {}",
                js_error(ctx, error)
            )
        })?;
    parse_chain_result(&text)
}

/// Run the handler registered for one button id.
///
/// The outer `Result` is a harness/engine failure; the inner one is the
/// handler's own error message, which is not a failure of the worker.
fn run_activation(ctx: &Ctx<'_>, id: &str) -> Result<Result<Vec<UiElement>, String>, String> {
    let activate: Function = ctx.globals().get("__coderExActivate").map_err(|error| {
        format!(
            "could not read the runtime harness: {}",
            js_error(ctx, error)
        )
    })?;
    let event = serde_json::json!({ "id": id }).to_string();
    let text: String = activate.call((id, event)).map_err(|error| {
        format!(
            "the activation handler could not be called: {}",
            js_error(ctx, error)
        )
    })?;
    parse_activation_result(&text)
}

/// Read the harness chain's merged result back.
fn parse_chain_result(text: &str) -> Result<HookFireResult, String> {
    let reply: serde_json::Value = serde_json::from_str(text)
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
        pending_prompts: strings("pendingPrompts"),
    })
}

/// Read the harness activation result back.
fn parse_activation_result(text: &str) -> Result<Result<Vec<UiElement>, String>, String> {
    let reply: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("the activation handler returned unreadable output: {error}"))?;

    if reply.get("found").and_then(serde_json::Value::as_bool) == Some(false) {
        return Ok(Ok(Vec::new()));
    }
    if reply.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Ok(Err(string_field(&reply, "error")
            .unwrap_or_else(|| "the activation handler failed".to_string())));
    }
    match reply.get("ui") {
        None | Some(serde_json::Value::Null) => Ok(Ok(Vec::new())),
        Some(value) => parse_ui_elements(value)
            .map(Ok)
            .map_err(|error| format!("the activation handler returned invalid UI: {error}")),
    }
}

/// The wall-clock error text shared by the hook and activation paths.
fn budget_error(limits: &ModLimits, what: &str) -> String {
    format!(
        "the {what} exceeded its {} ms wall-clock budget",
        limits.hook_timeout.as_millis()
    )
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
    let mut pending_prompts = js.pending_prompts;
    pending_prompts.extend(inner.pending_prompts);

    HookFireResult {
        stop: js.stop || inner.stop,
        updated_input: inner.updated_input.or(js.updated_input),
        updated_tool_output: inner.updated_tool_output.or(js.updated_tool_output),
        errors,
        failed: js.failed || inner.failed,
        watch_paths: dedupe_paths(watch_paths),
        ui,
        pending_prompts,
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

/// Format a QuickJS error, reading the pending exception when there is one.
///
/// Taking the error by value keeps the common
/// `.map_err(|error| js_error(ctx, error))` call sites direct; the value is
/// only inspected.
#[allow(clippy::needless_pass_by_value)]
fn js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    if matches!(error, rquickjs::Error::Exception) {
        let value = ctx.catch();
        if let Some(exception) = value.as_exception() {
            return exception
                .message()
                .unwrap_or_else(|| "JavaScript exception".to_string());
        }
        return value
            .into_string()
            .and_then(|text| text.to_string().ok())
            .unwrap_or_else(|| "JavaScript exception".to_string());
    }
    error.to_string()
}

/// Put `$` and `on` into the realm before the mod runs.
///
/// `$` and `$.session` are ordinary objects whose properties are installed
/// read-only; `HOST_UI_API` adds `$.ui`, `notify` and `submit` and then freezes
/// both objects, so a mod cannot replace them.
fn install_host_api<'js>(ctx: &Ctx<'js>, session: &ModSession, name: &str) -> Result<(), String> {
    let globals = ctx.globals();
    let module_name =
        rquickjs::String::from_str(ctx.clone(), name).map_err(|error| js_error(ctx, error))?;
    globals
        .prop("__coderExModName", Property::from(module_name))
        .map_err(|error| js_error(ctx, error))?;

    let session_object = Object::new(ctx.clone()).map_err(|error| js_error(ctx, error))?;
    for (key, value) in [
        ("cwd", session.cwd.as_str()),
        ("model", session.model.as_str()),
        ("permission_mode", session.permission_mode.as_str()),
    ] {
        session_object
            .prop(key, Property::from(value))
            .map_err(|error| js_error(ctx, error))?;
    }

    let dollar = Object::new(ctx.clone()).map_err(|error| js_error(ctx, error))?;
    dollar
        .prop("session", Property::from(session_object))
        .map_err(|error| js_error(ctx, error))?;

    let module = name.to_string();
    let log = Function::new(ctx.clone(), move |message: Opt<Value>| {
        let message = message.0.map_or_else(String::new, |value| {
            value
                .into_string()
                .and_then(|text| text.to_string().ok())
                .unwrap_or_default()
        });
        tracing::info!(target: "coderex_mods", module = %module, "{message}");
    })
    .map_err(|error| js_error(ctx, error))?;
    dollar
        .set("log", log)
        .map_err(|error| js_error(ctx, error))?;

    let env = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, name: Opt<Value<'js>>| -> Value<'js> {
            let Some(name) = name.0 else {
                return Value::new_null(ctx);
            };
            let Some(name) = name.as_string().and_then(|string| string.to_string().ok()) else {
                return Value::new_null(ctx);
            };
            if !ALLOWED_ENV_VARS.contains(&name.as_str()) {
                return Value::new_null(ctx);
            }
            match std::env::var(&name) {
                Ok(value) => value.into_js(&ctx).unwrap_or_else(|_| Value::new_null(ctx)),
                Err(_) => Value::new_null(ctx),
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    dollar
        .set("env", env)
        .map_err(|error| js_error(ctx, error))?;

    globals
        .prop("$", Property::from(dollar))
        .map_err(|error| js_error(ctx, error))?;

    let host_api = HOST_UI_API.replace(
        "@@MAX_SUBMISSIONS_PER_CALL@@",
        &MAX_SUBMISSIONS_PER_HOOK_CALL.to_string(),
    );
    ctx.eval::<(), _>(host_api.as_str())
        .map_err(|error| js_error(ctx, error))?;
    ctx.eval::<(), _>(JSX_RUNTIME)
        .map_err(|error| js_error(ctx, error))?;
    Ok(())
}

/// The `$.ui` host API and `$.session.notify`, installed in each mod's realm
/// before the mod runs.
///
/// The constructors return plain, JSON-shaped descriptors; `render` stages
/// them and the main-realm dispatcher drains the staging area after each hook
/// call. Validation happens here (so the mod gets a JavaScript error at the
/// call site) and again in [`parse_ui_elements`] at the boundary.
///
/// `notify` stages a [`UiStyle::Notice`](coderex_core::ui_protocol::UiStyle)
/// text element in a separate list. The drain prepends that list to the
/// rendered elements, so notices always appear before `$.ui.render` output
/// from the same hook call.
const HOST_UI_API: &str = r#"
'use strict';
(function () {
  const STYLES = ['plain', 'bold', 'dim', 'warning', 'error', 'notice'];
  // `$.session.notify` limits. The text is capped before it is staged; the
  // per-call cap keeps a loop from flooding the transcript. Further calls are
  // dropped, and one summary element reports how many were lost.
  const MAX_NOTICE_LENGTH = 2048;
  const MAX_NOTICES_PER_CALL = 16;
  // `$.session.submit` limits. The text is capped before it is staged; the
  // per-call cap keeps one hook from flooding the next turn. Further calls are
  // dropped, and one summary element reports how many were lost. The runtime
  // additionally caps what a whole turn may leave queued.
  const MAX_SUBMISSION_LENGTH = 8192;
  const MAX_SUBMISSIONS_PER_CALL = @@MAX_SUBMISSIONS_PER_CALL@@;
  const TRUNCATION_MARKER = '…';
  const staging = [];
  const notices = [];
  // One array per hook call. A hook that calls `next()` runs later hooks
  // synchronously inside its own call, so a nested call pushes a fresh frame
  // and restores the outer one on return; without that, the inner call's reset
  // would discard what the outer hook had already submitted.
  let submissions = [];
  const submissionFrames = [];
  let droppedNotices = 0;
  let droppedSubmissions = 0;

  function notify(text) {
    if (typeof text !== 'string') {
      throw new TypeError('$.session.notify(text): text must be a string');
    }
    if (text.length === 0) {
      throw new TypeError('$.session.notify(text): text must be a non-empty string');
    }
    if (notices.length >= MAX_NOTICES_PER_CALL) {
      droppedNotices += 1;
      return undefined;
    }
    const value = text.length > MAX_NOTICE_LENGTH
      ? text.slice(0, MAX_NOTICE_LENGTH) + TRUNCATION_MARKER
      : text;
    notices.push({ type: 'text', text: value, style: 'notice' });
    return undefined;
  }

  // Queue a user message for after the current turn. This never touches the
  // engine: it only stages text here. The turn loop drains it when idle and
  // submits it as an ordinary user turn, so it goes through the same
  // permission pipeline as anything a person types.
  function submit(text) {
    if (typeof text !== 'string') {
      throw new TypeError('$.session.submit(text): text must be a string');
    }
    if (text.length === 0) {
      throw new TypeError('$.session.submit(text): text must be a non-empty string');
    }
    if (submissions.length >= MAX_SUBMISSIONS_PER_CALL) {
      droppedSubmissions += 1;
      return undefined;
    }
    const value = text.length > MAX_SUBMISSION_LENGTH
      ? text.slice(0, MAX_SUBMISSION_LENGTH) + TRUNCATION_MARKER
      : text;
    submissions.push(value);
    return undefined;
  }

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
    __resetUi: function () {
      staging.length = 0;
      notices.length = 0;
      droppedNotices = 0;
    },
    // Start a hook call's submission frame. `__resetUi` deliberately leaves
    // submissions alone: a call nested by `next(e)` must not clear its parent.
    __beginCall: function () {
      submissionFrames.push({ prompts: submissions, dropped: droppedSubmissions });
      submissions = [];
      droppedSubmissions = 0;
    },
    // The dispatcher takes submissions separately from rendered elements: they
    // are not displayed, they are handed to the turn loop. Taking also closes
    // the frame and restores the caller's submissions.
    __takeSubmissions: function () {
      const taken = { prompts: submissions.slice(), dropped: droppedSubmissions };
      const parent = submissionFrames.pop();
      if (parent) {
        submissions = parent.prompts;
        droppedSubmissions = parent.dropped;
      } else {
        submissions = [];
        droppedSubmissions = 0;
      }
      return taken;
    },
    // Notices are prepended in emission order, so `$.session.notify` output
    // always appears before anything the same hook call staged with
    // `$.ui.render`.
    __takeUi: function () {
      const taken = notices.concat(staging);
      notices.length = 0;
      staging.length = 0;
      if (droppedNotices > 0) {
        const dropped = droppedNotices;
        droppedNotices = 0;
        taken.push({
          type: 'text',
          text: '[' + dropped + ' notification(s) dropped: at most '
            + MAX_NOTICES_PER_CALL + ' per hook call]',
          style: 'dim'
        });
      }
      return taken;
    }
  };

  Object.defineProperty($.session, 'notify', {
    value: notify,
    writable: false,
    enumerable: true,
    configurable: false
  });

  Object.defineProperty($.session, 'submit', {
    value: submit,
    writable: false,
    enumerable: true,
    configurable: false
  });

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
  if (ui && typeof ui.__beginCall === 'function') ui.__beginCall();

  try {
    const value = registration.hook(host, eventObject, nextFn);
    // Take whatever `$.ui.render` staged, even when the hook returns nothing:
    // rendering is itself the result. Submissions live beside the UI staging
    // area: they are not rendered, they cross the worker boundary as text for
    // the turn loop to drain. A call that went over the per-hook cap gets one
    // dim notice, as `notify` does for its own overflow.
    const staged = (ui && typeof ui.__takeUi === 'function') ? ui.__takeUi() : [];
    const submitted = (ui && typeof ui.__takeSubmissions === 'function')
      ? ui.__takeSubmissions()
      : { prompts: [], dropped: 0 };
    if (submitted.dropped > 0) {
      staged.push({
        type: 'text',
        text: '[' + submitted.dropped + ' mod submission(s) dropped: at most '
          + __coderExMaxSubmissionsPerCall + ' per hook call]',
        style: 'dim'
      });
    }
    if (value !== null && typeof value === 'object' && typeof value.then === 'function') {
      return JSON.stringify({ ok: false, error: 'async hooks are not supported yet; return a value synchronously' });
    }
    return JSON.stringify({
      ok: true,
      result: __coderExNormalize(value),
      ui: staged,
      submissions: submitted.prompts
    });
  } catch (error) {
    if (ui && typeof ui.__takeSubmissions === 'function') ui.__takeSubmissions();
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
  // An activation handler cannot submit a prompt, but it can call submit();
  // the frame exists so those calls are discarded rather than leaking into the
  // next hook call.
  if (ui && typeof ui.__beginCall === 'function') ui.__beginCall();

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
    // Close the submission frame; an activation cannot start a turn.
    if (ui && typeof ui.__takeSubmissions === 'function') ui.__takeSubmissions();
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
    if (ui && typeof ui.__takeSubmissions === 'function') ui.__takeSubmissions();
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
    ui: [],
    pendingPrompts: []
  };
};

// Submissions merge in call order. Identical adjacent messages are folded:
// two hooks that independently queue the same text should submit it once.
globalThis.__coderExDedupePrompts = function (prompts) {
  const unique = [];
  for (let index = 0; index < prompts.length; index++) {
    if (unique.length === 0 || unique[unique.length - 1] !== prompts[index]) {
      unique.push(prompts[index]);
    }
  }
  return unique;
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
// The calling hook's submissions happened before it called `next`, so they
// precede everything the downstream chain queued.
globalThis.__coderExMergeEffect = function (effect, ui, submissions, downstream) {
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
    ui: ui.concat(downstream.ui),
    pendingPrompts: submissions.concat(downstream.pendingPrompts)
  };
};

globalThis.__coderExApplyEffect = function (combined, effect, ui, submissions) {
  if (effect !== null) {
    if (effect.stop === true) combined.stop = true;
    if (effect.updatedInput !== undefined) combined.updatedInput = effect.updatedInput;
    if (effect.updatedToolOutput !== undefined) combined.updatedToolOutput = effect.updatedToolOutput;
    if (Array.isArray(effect.watchPaths)) combined.watchPaths = combined.watchPaths.concat(effect.watchPaths);
  }
  combined.ui = combined.ui.concat(ui);
  combined.pendingPrompts = combined.pendingPrompts.concat(submissions);
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
  combined.pendingPrompts = combined.pendingPrompts.concat(result.pendingPrompts);
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
    ui: js.ui.concat(inner.ui),
    pendingPrompts: js.pendingPrompts.concat(inner.pendingPrompts)
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
      const submissions = Array.isArray(reply.submissions) ? reply.submissions : [];
      if (downstream !== null) {
        __coderExApplyResult(
          combined,
          __coderExMergeEffect(reply.result, reply.ui, submissions, downstream)
        );
      } else {
        __coderExApplyEffect(combined, reply.result, reply.ui, submissions);
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
  result.pendingPrompts = __coderExDedupePrompts(result.pendingPrompts);
  // Only the outermost call finalizes: a deeper level would otherwise add the
  // same drop notice again after the outer level merges it back in.
  if (depth === 0 && result.pendingPrompts.length > __coderExMaxPendingPrompts) {
    const dropped = result.pendingPrompts.length - __coderExMaxPendingPrompts;
    result.pendingPrompts = result.pendingPrompts.slice(0, __coderExMaxPendingPrompts);
    result.ui.push({
      type: 'text',
      text: '[' + dropped + ' mod submission(s) dropped: at most '
        + __coderExMaxPendingPrompts + ' queued messages per hook firing]',
      style: 'dim'
    });
  }
  return result;
};

globalThis.__coderExRun = function (chainJson, event, input, contextJson, innerFn, gating) {
  return JSON.stringify(
    __coderExChain(JSON.parse(chainJson), 0, event, input, contextJson, innerFn, 0, gating === true)
  );
};

globalThis.__coderExMaxChainDepth = @@MAX_CHAIN_DEPTH@@;
globalThis.__coderExMaxSubmissionsPerCall = @@MAX_SUBMISSIONS_PER_CALL@@;
globalThis.__coderExMaxPendingPrompts = @@MAX_PENDING_PROMPTS@@;
"#;

/// The per-mod harness: `on` and the registration array it fills.
fn install_mod_harness(ctx: &Ctx<'_>) -> Result<(), String> {
    let max_hooks = MAX_HOOKS_PER_MOD;
    let max_activations = MAX_ACTIVATIONS_PER_MOD;
    ctx.eval::<(), _>(
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
Object.defineProperty(globalThis, 'on', {{
  value: function (event, matcher, hook) {{
    return __coderExOn(event, matcher, hook);
  }},
  writable: false,
  enumerable: false,
  configurable: false
}});
",
        )
        .as_str(),
    )
    .map_err(|error| js_error(ctx, error))?;
    Ok(())
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

    #[test]
    fn a_cut_off_downstream_run_does_not_abandon_the_worker() {
        let limits = ModLimits {
            hook_timeout: Duration::from_millis(100),
            ..ModLimits::default()
        };
        // `calls` lives in the mod's realm: a rebuilt worker would start at 1,
        // so the follow-up call also proves the realm survived.
        let runtime = load_with(
            "let calls = 0; \
             function register(on) { on('PostToolUse', '*', function ($, e, next) { \
               calls += 1; const downstream = next(); \
               return { updatedInput: calls + ':' + downstream.updatedInput }; }); }",
            limits,
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", Arc::new(SlowInner) as _);
        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.contains("wall-clock budget")),
            "{:?}",
            result.errors
        );
        assert_eq!(
            runtime.timeout_abandonments(),
            0,
            "the bounded downstream hop returns to the caller; it must not be abandoned"
        );

        // The same worker and realm keep serving calls, and the downstream
        // executor still runs on the next one.
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            updated_input: Some("inner".to_string()),
            ..HookFireResult::default()
        }));
        let follow_up = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);
        assert!(!follow_up.failed, "{follow_up:?}");
        assert_eq!(inner.calls().len(), 1);
        assert_eq!(
            follow_up.updated_input.as_deref(),
            Some("2:inner"),
            "the mod state from the first call must survive"
        );
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
    fn an_endless_loop_is_interrupted_at_the_wall_clock() {
        // The interrupt handler stops the interpreter itself. The caller gets
        // the error well before its own backstop, the engine stays usable,
        // and the timeout still counts toward the breaker.
        let limits = ModLimits {
            hook_timeout: Duration::from_millis(100),
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { \
             on('PreToolUse', 'Bash', function () { while (true) {} }); \
             on('SessionStart', '*', function () { return { updatedInput: 'alive' }; }); }",
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
        assert!(
            result.stop,
            "a failure on a gating event must fail closed: {result:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
        assert_eq!(
            runtime.timeout_abandonments(),
            1,
            "an engine-interrupted timeout is still a timeout"
        );
        assert!(
            !runtime.is_disabled(),
            "one timeout is below the default breaker threshold"
        );

        // The interrupted engine was discarded, so the next request starts a
        // fresh runtime and reloads the mods. The worker itself is still
        // usable.
        let recovered = runtime.fire("SessionStart", "", &HashMap::new());
        assert!(!recovered.failed, "{recovered:?}");
        assert_eq!(recovered.updated_input.as_deref(), Some("alive"));
    }

    #[test]
    fn a_memory_bomb_hits_the_heap_limit() {
        // QuickJS accounts every allocation, including the backing store of a
        // single ArrayBuffer, against the runtime's memory limit.
        let limits = ModLimits {
            memory_limit: 8 * 1024 * 1024,
            hook_timeout: Duration::from_secs(5),
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { on('PostToolUse', '*', function () { \
             const chunks = []; \
             while (true) { chunks.push(new ArrayBuffer(1024 * 1024)); } }); }",
            limits,
        );

        let started = std::time::Instant::now();
        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());
        let elapsed = started.elapsed();

        assert!(result.failed, "{result:?}");
        assert!(
            result
                .errors
                .iter()
                .any(|error| error.to_lowercase().contains("memory")),
            "the heap limit must be reported: {:?}",
            result.errors
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "the heap limit must fire before the wall clock; took {elapsed:?}"
        );
        assert_eq!(
            runtime.timeout_abandonments(),
            0,
            "a heap limit is not a wall-clock timeout"
        );
        assert!(!runtime.is_disabled());
    }

    #[test]
    fn a_hook_over_its_wall_clock_budget_is_reported() {
        let limits = ModLimits {
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
    fn the_timeout_breaker_trips_at_the_threshold_and_then_stops_the_worker() {
        let limits = ModLimits {
            // Only the wall clock can stop this hook.
            hook_timeout: Duration::from_millis(200),
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { on('*', '*', function () { while (true) {} }); }",
            limits,
        );

        for attempt in 1..=DEFAULT_TIMEOUT_BREAKER_LIMIT {
            let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());
            assert!(result.failed, "attempt {attempt}: {result:?}");
            assert!(!result.stop, "PostToolUse is not a gating event");
            assert!(
                result
                    .errors
                    .iter()
                    .any(|error| error.contains("wall-clock budget")),
                "attempt {attempt}: {:?}",
                result.errors
            );
            assert_eq!(runtime.timeout_abandonments(), attempt);
            assert_eq!(
                runtime.is_disabled(),
                attempt == DEFAULT_TIMEOUT_BREAKER_LIMIT,
                "the breaker must trip exactly at the threshold"
            );
        }

        // Disabled: no worker is started, so this returns without waiting for
        // the infinite loop's budget. The downstream executor still runs on a
        // non-gating event, so settings hooks keep working.
        let inner = Arc::new(RecordingInner::answering(HookFireResult {
            updated_input: Some("from-inner".to_string()),
            ..HookFireResult::default()
        }));
        let started = std::time::Instant::now();
        let post = fire_with(&runtime, "PostToolUse", "Bash", Arc::clone(&inner) as _);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(150),
            "a disabled runtime must not start a worker; took {elapsed:?}"
        );
        assert_eq!(inner.calls().len(), 1, "the downstream executor still runs");
        assert_eq!(post.updated_input.as_deref(), Some("from-inner"));
        assert!(post.failed, "{post:?}");
        assert!(!post.stop, "PostToolUse is not a gating event");
        assert!(
            post.errors
                .iter()
                .any(|error| error.contains("disabled for the rest of this session")),
            "{:?}",
            post.errors
        );

        let inner = Arc::new(RecordingInner::answering(HookFireResult::default()));
        let pre = fire_with(&runtime, "PreToolUse", "Bash", Arc::clone(&inner) as _);
        assert!(pre.stop, "a gating event fails closed: {pre:?}");
        assert_eq!(
            inner.calls().len(),
            0,
            "the downstream hooks must not run behind a closed gate"
        );
        assert!(
            pre.errors.iter().any(|error| error.contains("disabled")),
            "{:?}",
            pre.errors
        );
    }

    #[test]
    fn ordinary_hook_errors_do_not_count_toward_the_breaker() {
        let runtime = load(
            "function register(on) { on('PreToolUse', 'Bash', function () { \
             throw new Error('broken'); }); }",
        );

        for _ in 0..(DEFAULT_TIMEOUT_BREAKER_LIMIT + 2) {
            let result = runtime.fire("PreToolUse", "Bash", &HashMap::new());
            assert!(result.failed && result.stop, "{result:?}");
        }
        assert_eq!(runtime.timeout_abandonments(), 0);
        assert!(!runtime.is_disabled());
    }

    #[test]
    fn heap_limit_errors_do_not_count_toward_the_breaker() {
        let limits = ModLimits {
            memory_limit: 8 * 1024 * 1024,
            ..ModLimits::default()
        };
        let runtime = load_with(
            "function register(on) { on('PostToolUse', '*', function () { \
             return { updatedInput: new ArrayBuffer(64 * 1024 * 1024) ? 'allocated' : 'no' }; }); }",
            limits,
        );

        for _ in 0..=DEFAULT_TIMEOUT_BREAKER_LIMIT {
            let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());
            assert!(result.failed, "{result:?}");
            assert!(
                result
                    .errors
                    .iter()
                    .any(|error| error.to_lowercase().contains("memory")),
                "{:?}",
                result.errors
            );
        }
        assert_eq!(runtime.timeout_abandonments(), 0);
        assert!(!runtime.is_disabled());
    }

    #[test]
    fn the_load_report_hashes_the_original_file_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let original =
            "function register(on: unknown) { on('PostToolUse', '*', function () {}); }\n";
        std::fs::write(directory.path().join("typed.ts"), original).unwrap();

        let runtime = ModRuntime::load_dir(directory.path(), session()).unwrap();
        let report = runtime.load_report();

        assert_eq!(report.len(), 1, "{report:?}");
        let info = &report[0];
        assert_eq!(info.name, "typed.ts");
        assert!(info.loaded, "{info:?}");
        assert!(info.errors.is_empty(), "{info:?}");
        assert_eq!(
            info.sha256,
            hex::encode(Sha256::digest(original.as_bytes())),
            "the hash must cover the bytes on disk, not the transformed source"
        );
    }

    #[test]
    fn mod_hashes_are_stable_and_change_with_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.js");
        std::fs::write(&path, "function register(on) {}").unwrap();

        let first = ModRuntime::load_dir(directory.path(), session()).unwrap();
        let first_hash = first.load_report()[0].sha256.clone();
        let second = ModRuntime::load_dir(directory.path(), session()).unwrap();
        assert_eq!(
            first_hash,
            second.load_report()[0].sha256,
            "the same bytes must hash the same"
        );
        assert_eq!(first_hash.len(), 64, "SHA-256 is 32 bytes of hex");

        std::fs::write(
            &path,
            "function register(on) { on('PostToolUse', '*', function () {}); }",
        )
        .unwrap();
        let changed = ModRuntime::load_dir(directory.path(), session()).unwrap();
        assert_ne!(
            first_hash,
            changed.load_report()[0].sha256,
            "different bytes must hash differently"
        );
    }

    #[test]
    fn the_load_report_records_skipped_mods_and_their_errors() {
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
        let report = runtime.load_report();

        assert_eq!(report.len(), 2, "{report:?}");
        let broken = report
            .iter()
            .find(|info| info.name == "broken.js")
            .expect("the skipped file is in the report");
        assert!(!broken.loaded, "{broken:?}");
        assert!(!broken.errors.is_empty(), "{broken:?}");
        assert_eq!(broken.sha256.len(), 64, "readable bytes are still hashed");

        let good = report
            .iter()
            .find(|info| info.name == "good.js")
            .expect("the loaded file is in the report");
        assert!(good.loaded, "{good:?}");
        assert!(good.errors.is_empty(), "{good:?}");
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
    fn the_memory_limit_accepts_byte_counts_and_binary_suffixes() {
        assert_eq!(parse_memory_limit("67108864"), Some(64 * 1024 * 1024));
        assert_eq!(parse_memory_limit("64k"), Some(64 * 1024));
        assert_eq!(parse_memory_limit("64KiB"), Some(64 * 1024));
        assert_eq!(parse_memory_limit("64mb"), Some(64 * 1024 * 1024));
        assert_eq!(parse_memory_limit("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_limit(""), None);
        assert_eq!(parse_memory_limit("lots"), None);
        assert_eq!(parse_memory_limit("12x"), None);
    }

    #[test]
    fn a_top_level_endless_loop_is_skipped_at_the_wall_clock() {
        let limits = ModLimits {
            hook_timeout: Duration::from_millis(100),
            ..ModLimits::default()
        };

        let started = std::time::Instant::now();
        let runtime = load_with("while (true) {}", limits);
        let elapsed = started.elapsed();

        assert_eq!(runtime.hook_count(), 0);
        assert_eq!(
            runtime.load_errors().len(),
            1,
            "{:?}",
            runtime.load_errors()
        );
        assert!(
            runtime.load_errors()[0].contains("wall-clock budget"),
            "{:?}",
            runtime.load_errors()
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
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

    // ── Notifications ───────────────────────────────────────────────────────

    #[test]
    fn a_hook_that_only_notifies_still_contributes_ui() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.session.notify('heads up'); }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert!(!result.failed, "{result:?}");
        assert_eq!(
            result.ui,
            vec![UiElement::Text {
                text: "heads up".to_string(),
                style: UiStyle::Notice,
            }]
        );
    }

    #[test]
    fn notices_are_staged_before_rendered_elements_in_emission_order() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.ui.render($.ui.text('rendered')); \
             $.session.notify('first'); \
             $.session.notify('second'); }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert_eq!(
            result.ui,
            vec![
                UiElement::Text {
                    text: "first".to_string(),
                    style: UiStyle::Notice,
                },
                UiElement::Text {
                    text: "second".to_string(),
                    style: UiStyle::Notice,
                },
                UiElement::Text {
                    text: "rendered".to_string(),
                    style: UiStyle::Plain,
                },
            ]
        );
    }

    #[test]
    fn the_notice_cap_drops_further_calls_and_reports_how_many() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             for (let index = 0; index < 20; index++) { \
               $.session.notify('n' + index); } }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert!(!result.failed, "{result:?}");
        assert_eq!(result.ui.len(), 17, "16 notices plus one drop summary");
        let notices: Vec<_> = result.ui[..16]
            .iter()
            .map(|element| match element {
                UiElement::Text { text, style } => {
                    assert_eq!(*style, UiStyle::Notice);
                    text.clone()
                }
                other => panic!("expected a notice, got {other:?}"),
            })
            .collect();
        assert_eq!(
            notices,
            (0..16).map(|index| format!("n{index}")).collect::<Vec<_>>()
        );
        assert_eq!(
            result.ui[16],
            UiElement::Text {
                text: "[4 notification(s) dropped: at most 16 per hook call]".to_string(),
                style: UiStyle::Dim,
            }
        );
    }

    #[test]
    fn notify_rejects_non_string_and_empty_text() {
        let cases = [
            ("$.session.notify(42);", "text must be a string"),
            ("$.session.notify('');", "text must be a non-empty string"),
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
            assert!(result.ui.is_empty(), "{statement} staged something");
        }
    }

    #[test]
    fn an_oversized_notice_is_truncated_with_a_marker() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.session.notify('x'.repeat(3000)); }); }",
        );

        let result = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert!(!result.failed, "{result:?}");
        let [UiElement::Text { text, style }] = result.ui.as_slice() else {
            panic!("expected one notice, got {:?}", result.ui);
        };
        assert_eq!(*style, UiStyle::Notice);
        assert_eq!(text.chars().count(), 2049, "2048 chars plus the marker");
        assert!(text.ends_with('…'), "{text:?}");
    }

    #[test]
    fn notice_staging_is_cleared_between_hook_calls() {
        let runtime = load(
            "let calls = 0; \
             function register(on) { on('PostToolUse', '*', function ($) { \
             if (calls++ === 0) { $.session.notify('once'); } }); }",
        );

        let first = runtime.fire("PostToolUse", "Read", &HashMap::new());
        let second = runtime.fire("PostToolUse", "Read", &HashMap::new());

        assert_eq!(first.ui.len(), 1, "the first call notified once");
        assert!(second.ui.is_empty(), "the second call notified nothing");
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

    #[test]
    fn submit_stages_messages_in_order_and_caps_a_hook_at_two() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.session.submit('first'); $.session.submit('second'); \
             $.session.submit('third'); }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert_eq!(result.pending_prompts, vec!["first", "second"]);
        assert!(
            result.ui.iter().any(|element| matches!(
                element,
                UiElement::Text { text, style: UiStyle::Dim } if text.contains("1 mod submission(s) dropped")
            )),
            "the over-cap call is reported: {:?}",
            result.ui
        );
    }

    #[test]
    fn submit_truncates_long_text_with_a_marker() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.session.submit('x'.repeat(9000)); }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert_eq!(result.pending_prompts.len(), 1);
        let prompt = &result.pending_prompts[0];
        assert_eq!(prompt.chars().count(), 8193, "8192 chars plus the marker");
        assert!(prompt.ends_with('…'));
    }

    #[test]
    fn submit_rejects_an_empty_string_and_a_non_string() {
        for call in ["$.session.submit('')", "$.session.submit(42)"] {
            let runtime = load(&format!(
                "function register(on) {{ on('PostToolUse', '*', function ($) {{ {call}; }}); }}"
            ));

            let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

            assert!(result.failed, "{call} should fail the hook");
            assert_eq!(result.pending_prompts, Vec::<String>::new());
        }
    }

    #[test]
    fn the_chain_merges_submissions_in_order_and_dedupes_adjacent_ones() {
        let runtime = load(
            "function register(on) { \
             on('PostToolUse', 'Bash', function ($) { \
               $.session.submit('one'); $.session.submit('two'); }); \
             on('PostToolUse', 'Bash', function ($) { \
               $.session.submit('two'); $.session.submit('three'); }); }",
        );

        let result = runtime.fire("PostToolUse", "Bash", &HashMap::new());

        assert_eq!(result.pending_prompts, vec!["one", "two", "three"]);
    }

    #[test]
    fn the_runtime_queue_caps_a_turn_at_eight_and_reports_the_drop() {
        let runtime = load(
            "function register(on) { on('PostToolUse', '*', function ($) { \
             $.session.submit('a'); $.session.submit('b'); }); }",
        );

        let mut last = HookFireResult::default();
        for _ in 0..5 {
            last = runtime.fire("PostToolUse", "Bash", &HashMap::new());
        }
        let queued = runtime.submit_queue().take();

        assert_eq!(queued.len(), MAX_PENDING_PROMPTS_PER_TURN);
        assert_eq!(queued[0], "a");
        assert_eq!(queued[MAX_PENDING_PROMPTS_PER_TURN - 1], "b");
        assert!(
            last.ui.iter().any(|element| matches!(
                element,
                UiElement::Text { text, style: UiStyle::Dim } if text.contains("dropped")
            )),
            "the fifth call's two messages are dropped and reported: {:?}",
            last.ui
        );
        assert!(runtime.submit_queue().is_empty(), "taking drains the queue");
    }

    #[test]
    fn the_chain_carries_submissions_across_next() {
        let inner = Arc::new(RecordingInner::answering(HookFireResult::default()));
        let runtime = load(
            "function register(on) { \
             on('PostToolUse', '*', function ($, e, next) { \
               $.session.submit('outer'); next(); }); \
             on('PostToolUse', '*', function ($) { $.session.submit('inner'); }); }",
        );

        let result = fire_with(&runtime, "PostToolUse", "Bash", inner);

        assert_eq!(result.pending_prompts, vec!["outer", "inner"]);
    }
}
