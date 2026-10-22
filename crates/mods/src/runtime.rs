//! Loading mods and firing their hooks with a bounded JavaScript realm.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use boa_engine::{
    Context, JsResult, JsString, JsValue, NativeFunction, Source, js_string,
    object::ObjectInitializer, property::Attribute,
};
use coderex_core::hook_executor::HookFireResult;

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
    /// Wall-clock budget for one hook call.
    ///
    /// `boa_engine` 0.20 has no interrupt API, so this is enforced by the
    /// calling thread: when the budget expires the call is reported as an
    /// error and the worker is left to unwind on its own. The engine limits
    /// below are what guarantee that it does.
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

/// A mod file that loaded and registered its hooks.
#[derive(Debug, Clone)]
struct LoadedMod {
    name: String,
    source: String,
}

/// One registration, pointing at the hook function in a loaded mod.
#[derive(Debug, Clone)]
struct HookRegistration {
    mod_index: usize,
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

/// Loads mods and fires their hooks.
///
/// A runtime holds only strings and configuration: the JavaScript realms are
/// created per hook call, on the thread that runs it. That is what lets
/// [`JsHookExecutor`](crate::JsHookExecutor) be `Send + Sync` even though a
/// `boa_engine::Context` is neither.
#[derive(Debug)]
pub struct ModRuntime {
    dir: PathBuf,
    mods: Vec<LoadedMod>,
    hooks: Vec<HookRegistration>,
    session: ModSession,
    limits: ModLimits,
    load_errors: Vec<String>,
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
    /// Fails when the directory exists but cannot be listed.
    pub fn load_dir(path: &Path, session: ModSession) -> Result<Self, String> {
        Self::load_dir_with_limits(path, session, ModLimits::default())
    }

    /// Load with explicit limits. See [`load_dir`](Self::load_dir).
    ///
    /// # Errors
    ///
    /// Fails when the directory exists but cannot be listed.
    pub fn load_dir_with_limits(
        path: &Path,
        session: ModSession,
        limits: ModLimits,
    ) -> Result<Self, String> {
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

        let mut mods = Vec::new();
        let mut hooks = Vec::new();
        let mut load_errors = Vec::new();

        for (name, file_path) in files {
            let source = match read_source(&file_path) {
                Ok(source) => source,
                Err(error) => {
                    let message = format!("{}: {error}", file_path.display());
                    tracing::warn!(path = %file_path.display(), "{error}");
                    load_errors.push(message);
                    continue;
                }
            };

            match evaluate_and_register(&source, &name, &session, &limits) {
                Ok(registrations) => {
                    let mod_index = mods.len();
                    for (hook_index, (event, matcher)) in registrations.into_iter().enumerate() {
                        hooks.push(HookRegistration {
                            mod_index,
                            hook_index,
                            event,
                            matcher,
                        });
                    }
                    mods.push(LoadedMod { name, source });
                }
                Err(error) => {
                    let message = format!("{}: {error}", file_path.display());
                    tracing::warn!(path = %file_path.display(), "mod was skipped: {error}");
                    load_errors.push(message);
                }
            }
        }

        Ok(Self {
            dir: path.to_path_buf(),
            mods,
            hooks,
            session,
            limits,
            load_errors,
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
    pub fn hook_count(&self) -> usize {
        self.hooks.len()
    }

    /// Files that were skipped at load, with the reason. Each appears once.
    #[must_use]
    pub fn load_errors(&self) -> &[String] {
        &self.load_errors
    }

    /// Fire `event` against every matching hook and merge what they return.
    ///
    /// Hooks run in load order (files sorted by name, then registration
    /// order). An exception, timeout or engine limit becomes an entry in
    /// [`HookFireResult::errors`] and sets
    /// [`failed`](HookFireResult::failed); other hooks still run. On a gating
    /// event (`PreToolUse`, `UserPromptSubmit`) a failure also sets `stop`, so
    /// a broken hook cannot read as an allow.
    #[must_use]
    pub fn fire(
        &self,
        event: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult {
        let mut combined = HookFireResult::default();

        for registration in &self.hooks {
            if !event_matches(&registration.event, event)
                || !matcher_matches(&registration.matcher, input)
            {
                continue;
            }
            let loaded = &self.mods[registration.mod_index];
            match call_hook(
                loaded,
                registration.hook_index,
                event,
                input,
                context,
                &self.session,
                &self.limits,
            ) {
                Ok(effect) => {
                    combined.stop |= effect.stop;
                    if effect.updated_input.is_some() {
                        combined.updated_input = effect.updated_input;
                    }
                    if effect.updated_tool_output.is_some() {
                        combined.updated_tool_output = effect.updated_tool_output;
                    }
                    combined.watch_paths.extend(effect.watch_paths);
                }
                Err(error) => {
                    combined.failed = true;
                    combined
                        .errors
                        .push(format!("mod `{}`: {error}", loaded.name));
                }
            }
        }

        if combined.failed && is_gating_event(event) {
            combined.stop = true;
        }
        combined.watch_paths = dedupe_paths(combined.watch_paths);
        combined
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
    /// Fails when the directory exists but cannot be listed.
    pub fn load_dir(self, path: &Path) -> Result<ModRuntime, String> {
        ModRuntime::load_dir_with_limits(path, self.session, self.limits)
    }
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

/// Evaluate one mod and read back the registrations its `register` makes.
fn evaluate_and_register(
    source: &str,
    name: &str,
    session: &ModSession,
    limits: &ModLimits,
) -> Result<Vec<(String, String)>, String> {
    let mut context = Context::default();
    apply_limits(&mut context, limits);
    install_host_api(&mut context, session, name)
        .map_err(|error| format!("could not install the host API: {error}"))?;

    context
        .eval(Source::from_bytes(source))
        .map_err(|error| format!("could not evaluate the mod: {error}"))?;

    install_harness(&mut context)
        .map_err(|error| format!("could not install the runtime harness: {error}"))?;

    let has_register = context
        .eval(Source::from_bytes("typeof register === 'function'"))
        .map_err(|error| format!("could not inspect the mod: {error}"))?
        .to_boolean();
    if !has_register {
        return Err("the mod does not define a global register(on, options) function".to_string());
    }

    let options = js_string_literal(name);
    context
        .eval(Source::from_bytes(
            format!("register(on, Object.freeze({{ name: {options} }}))").as_bytes(),
        ))
        .map_err(|error| format!("register() failed: {error}"))?;

    let value = eval_json(
        &mut context,
        "__coderExRegs.map(function (registration) { \
         return { event: registration.event, matcher: registration.matcher }; })",
    );
    let Some(registrations) = value.as_array() else {
        return Err("register() did not leave readable hook registrations".to_string());
    };

    Ok(registrations
        .iter()
        .filter_map(|registration| {
            Some((
                registration.get("event")?.as_str()?.to_string(),
                registration.get("matcher")?.as_str()?.to_string(),
            ))
        })
        .collect())
}

/// Run one hook on its own thread, bounded by the engine limits and a wall
/// clock.
fn call_hook(
    loaded: &LoadedMod,
    hook_index: usize,
    event: &str,
    input: &str,
    context: &HashMap<String, String>,
    session: &ModSession,
    limits: &ModLimits,
) -> Result<HookEffect, String> {
    let source = loaded.source.clone();
    let name = loaded.name.clone();
    let session = session.clone();
    let limits = *limits;
    let event = event.to_string();
    let input = input.to_string();
    let context_json = serde_json::to_string(context)
        .map_err(|error| format!("could not serialize the hook context: {error}"))?;

    let (send, receive) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name(format!("coderex-mod-{name}"))
        .spawn(move || {
            let reply = execute_hook(
                &source,
                &name,
                hook_index,
                &event,
                &input,
                &context_json,
                &session,
                &limits,
            );
            let _ = send.send(reply);
        })
        .map_err(|error| format!("could not start a hook thread: {error}"))?;

    match receive.recv_timeout(limits.hook_timeout) {
        Ok(reply) => {
            let _ = thread.join();
            reply
        }
        Err(RecvTimeoutError::Timeout) => Err(format!(
            "the hook exceeded its {} ms wall-clock budget",
            limits.hook_timeout.as_millis()
        )),
        Err(RecvTimeoutError::Disconnected) => {
            let _ = thread.join();
            Err("the hook thread stopped without reporting a result".to_string())
        }
    }
}

/// The body of a hook thread: a fresh realm, the same registration, one call.
#[allow(clippy::too_many_arguments)]
fn execute_hook(
    source: &str,
    name: &str,
    hook_index: usize,
    event: &str,
    input: &str,
    context_json: &str,
    session: &ModSession,
    limits: &ModLimits,
) -> Result<HookEffect, String> {
    let mut context = Context::default();
    apply_limits(&mut context, limits);
    install_host_api(&mut context, session, name)
        .map_err(|error| format!("could not install the host API: {error}"))?;

    context
        .eval(Source::from_bytes(source))
        .map_err(|error| format!("could not evaluate the mod: {error}"))?;
    install_harness(&mut context)
        .map_err(|error| format!("could not install the runtime harness: {error}"))?;

    let options = js_string_literal(name);
    context
        .eval(Source::from_bytes(
            format!("register(on, Object.freeze({{ name: {options} }}))").as_bytes(),
        ))
        .map_err(|error| format!("register() failed: {error}"))?;

    let count = eval_json(&mut context, "__coderExRegs.length")
        .as_u64()
        .unwrap_or_default();
    if u64::try_from(hook_index).unwrap_or(u64::MAX) >= count {
        return Err(format!(
            "hook registration {hook_index} is no longer present (the mod now registers {count})"
        ));
    }

    let call = format!(
        "__coderExCall({hook_index}, {}, {}, {})",
        js_string_literal(event),
        js_string_literal(input),
        js_string_literal(context_json)
    );
    let value = context
        .eval(Source::from_bytes(call.as_bytes()))
        .map_err(|error| format!("the hook could not be called: {error}"))?;
    let text = value
        .to_string(&mut context)
        .map_err(|error| format!("could not read the hook result: {error}"))?
        .to_std_string_escaped();
    let reply: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("the hook returned unreadable output: {error}"))?;
    parse_reply(&reply)
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

/// Install the JavaScript side of `on`/`register`, with fresh registration
/// storage. Run after the mod source so a mod cannot shadow the helpers.
fn install_harness(context: &mut Context) -> JsResult<()> {
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
  __coderExRegs.push({{ event: event, matcher: matcher, hook: hook }});
  return undefined;
}};

globalThis.__coderExNormalize = function (value) {{
  if (value === undefined || value === null || typeof value !== 'object') return null;
  const normalized = {{}};
  if (value.stop === true) normalized.stop = true;
  const updatedInput = value.updatedInput !== undefined ? value.updatedInput : value.updated_input;
  if (typeof updatedInput === 'string') normalized.updatedInput = updatedInput;
  const updatedToolOutput = value.updatedToolOutput !== undefined ? value.updatedToolOutput : value.updated_tool_output;
  if (typeof updatedToolOutput === 'string') normalized.updatedToolOutput = updatedToolOutput;
  const watchPaths = value.watchPaths !== undefined ? value.watchPaths : value.watch_paths;
  if (Array.isArray(watchPaths)) {{
    normalized.watchPaths = watchPaths.filter(function (path) {{ return typeof path === 'string'; }});
  }}
  return normalized;
}};

globalThis.__coderExCall = function (index, event, input, contextJson) {{
  const registration = __coderExRegs[index];
  if (!registration) {{
    return JSON.stringify({{ ok: false, error: 'hook registration ' + index + ' is missing' }});
  }}

  let eventObject = {{}};
  try {{
    const parsed = JSON.parse(input);
    if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {{
      eventObject = parsed;
    }}
  }} catch (_) {{
    eventObject = {{}};
  }}
  eventObject.event = event;
  eventObject.input = input;

  try {{
    const parsedContext = JSON.parse(contextJson);
    if (parsedContext !== null && typeof parsedContext === 'object') {{
      for (const key of Object.keys(parsedContext)) {{
        if (key === 'tool_input') {{
          try {{
            eventObject[key] = JSON.parse(parsedContext[key]);
            continue;
          }} catch (_) {{
            // Not JSON; fall through and expose the raw string.
          }}
        }}
        eventObject[key] = parsedContext[key];
      }}
    }}
  }} catch (_) {{
    // A malformed context is simply not exposed.
  }}

  try {{
    const value = registration.hook($, eventObject);
    if (value !== null && typeof value === 'object' && typeof value.then === 'function') {{
      return JSON.stringify({{ ok: false, error: 'async hooks are not supported yet; return a value synchronously' }});
    }}
    return JSON.stringify({{ ok: true, result: __coderExNormalize(value) }});
  }} catch (error) {{
    const message = (error && error.message) ? String(error.message) : String(error);
    return JSON.stringify({{ ok: false, error: message }});
  }}
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

fn eval_json(context: &mut Context, expression: &str) -> serde_json::Value {
    let source = format!("JSON.stringify({expression})");
    let Ok(value) = context.eval(Source::from_bytes(source.as_bytes())) else {
        return serde_json::Value::Null;
    };
    let Ok(text) = value.to_string(context) else {
        return serde_json::Value::Null;
    };
    serde_json::from_str(&text.to_std_string_escaped()).unwrap_or(serde_json::Value::Null)
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
}
