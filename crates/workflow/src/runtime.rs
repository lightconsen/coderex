//! The workflow runtime — running a script, and keeping it honest.
//!
//! # How a script runs
//!
//! The script is plain JavaScript evaluated in a realm that has only what this
//! module puts there: no filesystem, no network, no `require`. It is written in
//! the reference's shape — `agent()` returns a promise, `parallel()` awaits
//! many, `pipeline()` chains them — so the script is genuinely asynchronous
//! while the *host* stays simple: JavaScript runs until it is waiting on
//! agents, the host then runs every agent it owes concurrently, hands back the
//! results, and lets JavaScript continue. That loop is the whole runtime.
//!
//! # What the runtime refuses
//!
//! `Date.now()` and `Math.random()` are replaced with functions that throw,
//! because a resumed run replays what it already did: a script that reads the
//! clock produces different work the second time. A call sequence that would
//! exceed the agent cap, the token budget, or the fan-out limit throws with an
//! explanation rather than quietly doing something smaller.

use boa_engine::{
    Context, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source, js_string,
    object::ObjectInitializer, property::Attribute,
};
use std::path::PathBuf;
use std::sync::Arc;

use crate::journal::{CallOptions, Entry, Journal, ROOT_KEY, key_for};
use crate::meta::Meta;

/// Most agent calls one run may make.
pub const MAX_AGENT_CALLS: usize = 1000;

/// Most items one `parallel()` or `pipeline()` may be given.
pub const MAX_ITEMS_PER_CALL: usize = 4096;

/// Most `log()` lines kept.
pub const MAX_LOG_LINES: usize = 1000;

/// Longest a single log line may be before it is trimmed.
pub const MAX_LOG_LINE_CHARS: usize = 400;

/// The concurrency gate: never more than this many agents at once.
#[must_use]
pub fn default_max_concurrent() -> usize {
    let parallelism = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    parallelism.saturating_sub(2).clamp(2, 16)
}

/// Something a workflow needs from its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    /// What the agent is asked to do.
    pub prompt: String,
    /// The label the script gave it, for progress.
    pub label: Option<String>,
    /// The phase it belongs to.
    pub phase: Option<String>,
    /// A model to use instead of the session's.
    pub model: Option<String>,
    /// A schema the answer must match.
    pub schema: Option<serde_json::Value>,
    /// Whether it runs in its own worktree.
    pub isolation: Option<String>,
    /// The kind of agent.
    pub agent_type: Option<String>,
    /// Tools it may not use.
    pub disallowed_tools: Vec<String>,
}

impl AgentRequest {
    /// The request as an identity: what the agent is asked, not how it is
    /// labelled.
    #[must_use]
    pub fn call_options(&self) -> CallOptions {
        CallOptions {
            schema: self.schema.as_ref().map(ToString::to_string),
            model: self.model.clone(),
            effort: None,
            isolation: self.isolation.clone(),
            agent_type: self.agent_type.clone(),
            disallowed_tools: self.disallowed_tools.clone(),
            bash_clamp: None,
        }
    }
}

/// What an agent produced: its text (if any) and the output tokens it spent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentOutput {
    /// The agent's text, or `None` when it produced nothing — skipped, or it
    /// gave up — which the script sees as `null`.
    pub text: Option<String>,
    /// Output tokens the agent consumed, for the run's token budget.
    pub tokens: u64,
}

impl AgentOutput {
    /// An answer with no token accounting.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            tokens: 0,
        }
    }

    /// An answer and the tokens it spent.
    #[must_use]
    pub fn with_tokens(text: impl Into<String>, tokens: u64) -> Self {
        Self {
            text: Some(text.into()),
            tokens,
        }
    }

    /// No answer.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            text: None,
            tokens: 0,
        }
    }
}

/// What runs the agents a workflow asks for.
#[async_trait::async_trait]
pub trait AgentRunner: Send + Sync {
    /// Run one agent. `text: None` means it produced nothing — skipped, or it
    /// gave up — which the script sees as `null`. `tokens` feeds the run's
    /// budget.
    async fn run(&self, request: AgentRequest) -> Result<AgentOutput, String>;
}

/// What happened while a workflow ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Lines the script logged.
    pub log: Vec<String>,
    /// Phases the script opened, in order.
    pub phases: Vec<String>,
    /// How many agents ran (not counting replayed ones).
    pub agents_run: usize,
    /// How many were replayed from the journal.
    pub agents_replayed: usize,
    /// How many returned nothing.
    pub agents_empty: usize,
}

/// Limits a run is held to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Most agent calls in one run.
    pub max_agents: usize,
    /// Most agents at once.
    pub max_concurrent: usize,
    /// Most items one fan-out may take.
    pub max_items: usize,
    /// Most log lines kept.
    pub max_log_lines: usize,
    /// A hard ceiling on output tokens, if the turn has one.
    pub token_budget: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_agents: MAX_AGENT_CALLS,
            max_concurrent: default_max_concurrent(),
            max_items: MAX_ITEMS_PER_CALL,
            max_log_lines: MAX_LOG_LINES,
            token_budget: None,
        }
    }
}

/// Something that stopped a run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    /// The script or its metadata is unusable.
    #[error("{0}")]
    Script(#[from] crate::meta::MetaError),
    /// The script threw.
    #[error("the workflow script failed: {0}")]
    Failed(String),
    /// The run hit a limit.
    #[error("{0}")]
    Limit(String),
    /// The journal could not be written or read.
    #[error("the workflow journal failed: {0}")]
    Journal(String),
    /// The run produced no result.
    #[error("the workflow finished without producing a result")]
    NoResult,
}

/// What a run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The run's id.
    pub run_id: String,
    /// What the script returned, as JSON.
    pub result: Option<serde_json::Value>,
    /// What happened along the way.
    pub progress: Progress,
    /// The keys the run used, in order.
    pub keys: Vec<String>,
}

/// Run a workflow script.
///
/// `run_dir` is where the journal lives; a run that finds one there replays
/// what it holds instead of running those agents again.
///
/// # Errors
///
/// Fails if the script cannot be read or run, or if it exceeds a limit.
pub async fn run_script(
    script: &str,
    args: Option<serde_json::Value>,
    runner: Arc<dyn AgentRunner>,
    run_dir: PathBuf,
    limits: Limits,
) -> Result<Outcome, WorkflowError> {
    let (meta, body) = crate::meta::read(script)?;
    let journal = Journal::open(&run_dir);
    let previous = journal
        .load()
        .map_err(|e| WorkflowError::Journal(e.to_string()))?;

    // Every agent call gets a key chained onto the call before it, so a
    // resume replays the longest unchanged prefix of the run.
    let run_id = new_run_id();
    let mut state = State {
        runner,
        journal,
        limits,
        progress: Progress::default(),
        previous,
        keys: Vec::new(),
        last_key: ROOT_KEY.to_string(),
        agent_count: 0,
        tokens_used: 0,
        pending: Vec::new(),
    };

    let body = body.clone();
    let args = args.unwrap_or(serde_json::Value::Null);
    let meta_phases: Vec<String> = meta.phases.iter().map(|p| p.title.clone()).collect();
    let run_id_for_thread = run_id.clone();

    // The realm is built and driven on one thread: a `Context` is not `Send`,
    // and every await the script makes is served by the host in rounds below.
    let handle = tokio::runtime::Handle::current();
    let mut outcome =
        tokio::task::spawn_blocking(move || drive(&mut state, &body, &args, &handle, &meta_phases))
            .await
            .map_err(|error| {
                WorkflowError::Failed(format!("the workflow thread panicked: {error}"))
            })??;

    outcome.run_id = run_id_for_thread;
    Ok(outcome)
}

/// A run's id: short, and shaped so it is recognisable in a file name.
#[must_use]
pub fn new_run_id() -> String {
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    format!("wf_{}-{}", &uuid[..8], &uuid[8..11])
}

/// Everything a run carries between rounds.
struct State {
    runner: Arc<dyn AgentRunner>,
    journal: Journal,
    limits: Limits,
    progress: Progress,
    previous: crate::journal::Loaded,
    keys: Vec<String>,
    last_key: String,
    agent_count: usize,
    /// Output tokens spent by agents that actually ran.
    tokens_used: u64,
    pending: Vec<(String, AgentRequest)>,
}

fn drive(
    state: &mut State,
    body: &str,
    args: &serde_json::Value,
    handle: &tokio::runtime::Handle,
    meta_phases: &[String],
) -> Result<Outcome, WorkflowError> {
    let mut context = Context::default();
    install_shims(&mut context).map_err(|e| WorkflowError::Failed(e.to_string()))?;

    let prelude = build_prelude();
    context
        .eval(Source::from_bytes(&prelude))
        .map_err(|e| WorkflowError::Failed(format!("the runtime prelude failed: {e}")))?;
    set_budget_globals(&mut context, state)?;

    // The arguments the script sees, and the calls it can make.
    let args_value = json_to_js(&mut context, args);
    context
        .register_global_property(
            js_string!("args"),
            args_value,
            Attribute::READONLY | Attribute::NON_ENUMERABLE,
        )
        .map_err(|e| WorkflowError::Failed(e.to_string()))?;

    register_host_functions(&mut context).map_err(|e| WorkflowError::Failed(e.to_string()))?;

    // The body is wrapped so top-level `await` works the way the reference's
    // scripts use it, and so a throw lands in a place the host can read
    // rather than becoming an unhandled rejection.
    let program = format!(
        "(async () => {{\n           try {{\n             __workflow_result_value = await (async () => {{\n{body}\n}})();\n           }} catch (error) {{\n             __workflow_failure = (error && error.message) ? String(error.message) : String(error);\n           }} finally {{\n             __workflow_done = true;\n           }}\n         }})()"
    );
    context
        .eval(Source::from_bytes(&program))
        .map_err(|error| WorkflowError::Failed(readable(&error)))?;

    // Rounds: let JavaScript run, run every agent it is waiting on, hand the
    // results back, and let it continue. The script is asynchronous; the host
    // is a loop.
    let mut rounds = 0usize;
    loop {
        context
            .run_jobs()
            .map_err(|error| WorkflowError::Failed(readable(&error)))?;
        state.pending = pending_requests(&mut context);
        if state.pending.is_empty() {
            break;
        }
        if rounds > state.limits.max_agents + 1 {
            return Err(WorkflowError::Limit(
                "the workflow did not finish after every agent call it was allowed".to_string(),
            ));
        }
        rounds += 1;

        let pending = std::mem::take(&mut state.pending);
        let results = handle.block_on(run_agents(state, pending))?;

        for (id, result) in results {
            let value = result.as_ref().map_or_else(JsValue::null, |text| {
                json_to_js(&mut context, &serde_json::Value::String(text.clone()))
            });
            resolve_agent(&mut context, &id, value)?;
        }
        // The next round's `budget.remaining()` must see what this round
        // spent.
        set_budget_globals(&mut context, state)?;
    }
    context
        .run_jobs()
        .map_err(|error| WorkflowError::Failed(readable(&error)))?;

    if let Some(failure) = read_string(&mut context, "__workflow_failure") {
        if !failure.is_empty() && failure != "null" {
            return Err(WorkflowError::Failed(failure));
        }
    }

    state.progress.phases = read_strings(&mut context, "__phases");
    state.progress.log = read_strings(&mut context, "__log");
    if !meta_phases.is_empty() {
        for declared in meta_phases {
            if !state.progress.phases.iter().any(|phase| phase == declared) {
                tracing::debug!(phase = %declared, "declared in meta but never opened");
            }
        }
    }

    let result = eval_json(&mut context, "__workflow_result_value");

    Ok(Outcome {
        run_id: String::new(),
        result: Some(result),
        progress: state.progress.clone(),
        keys: state.keys.clone(),
    })
}

/// Read a string global out of the realm.
fn read_string(context: &mut Context, name: &str) -> Option<String> {
    let value = context.eval(Source::from_bytes(name.as_bytes())).ok()?;
    Some(value.to_string(context).ok()?.to_std_string_escaped())
}

/// Read an array-of-strings global out of the realm.
fn read_strings(context: &mut Context, name: &str) -> Vec<String> {
    eval_json(context, name)
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One completed agent call: its id, journal key, label, and outcome.
type JournalEntry = (String, String, Option<String>, Result<AgentOutput, String>);

/// Run the agents a script is waiting on, at the concurrency it is allowed.
async fn run_agents(
    state: &mut State,
    pending: Vec<(String, AgentRequest)>,
) -> Result<Vec<(String, Option<String>)>, WorkflowError> {
    use futures::stream::{self, StreamExt};

    let mut results = Vec::with_capacity(pending.len());
    let mut to_run = Vec::new();

    // Do not start a round with the budget already spent. The check after the
    // round catches the last batch; this one refuses the next.
    if state
        .limits
        .token_budget
        .is_some_and(|budget| state.tokens_used >= budget)
    {
        return Err(WorkflowError::Limit(format!(
            "the workflow reached its token budget of {} output tokens",
            state.limits.token_budget.unwrap_or_default()
        )));
    }

    for (id, request) in pending {
        state.agent_count += 1;
        if state.agent_count > state.limits.max_agents {
            return Err(WorkflowError::Limit(format!(
                "the workflow reached its cap of {} agent calls. This usually means a loop \
                 whose condition never becomes false; give it a hard iteration limit.",
                state.limits.max_agents
            )));
        }

        let key = key_for(&state.last_key, &request.prompt, &request.call_options());
        state.last_key.clone_from(&key);
        state.keys.push(key.clone());

        // A replay: the key is in the journal, so the agent does not run.
        if let Some(previous) = state.previous.results.get(&key) {
            state.progress.agents_replayed += 1;
            results.push((id, previous.clone()));
            continue;
        }

        state
            .journal
            .append(&Entry::Started {
                key: key.clone(),
                agent_id: id.clone(),
                label: request.label.clone(),
                phase: request.phase.clone(),
            })
            .map_err(|e| WorkflowError::Journal(e.to_string()))?;

        to_run.push((id, key, request));
    }

    let runner = Arc::clone(&state.runner);
    let limit = state.limits.max_concurrent;
    let jounal_entries: Vec<JournalEntry> =
        stream::iter(to_run.into_iter().map(|(id, key, request)| {
            let runner = Arc::clone(&runner);
            async move {
                let outcome = runner.run(request).await;
                (id, key, None, outcome)
            }
        }))
        .buffer_unordered(limit)
        .collect::<Vec<_>>()
        .await;

    for (id, key, _, outcome) in jounal_entries {
        match outcome {
            Ok(output) => {
                state.tokens_used = state.tokens_used.saturating_add(output.tokens);
                let value = output.text;
                if value.is_none() {
                    state.progress.agents_empty += 1;
                }
                state.progress.agents_run += 1;
                state
                    .journal
                    .append(&Entry::Result {
                        key,
                        agent_id: id.clone(),
                        result: value.clone(),
                    })
                    .map_err(|e| WorkflowError::Journal(e.to_string()))?;
                results.push((id, value));
            }
            Err(error) => {
                state
                    .journal
                    .append(&Entry::Failed {
                        key,
                        agent_id: id.clone(),
                    })
                    .map_err(|e| WorkflowError::Journal(e.to_string()))?;
                // A failed agent is a `null` the script can see, not a dead
                // run: a workflow that can react to one agent failing should
                // be able to.
                tracing::warn!(agent = %id, "workflow agent failed: {error}");
                state.progress.agents_empty += 1;
                results.push((id, None));
            }
        }
    }

    if let Some(budget) = state.limits.token_budget {
        if state.tokens_used > budget {
            return Err(WorkflowError::Limit(format!(
                "the workflow used {} output tokens, past its budget of {budget}",
                state.tokens_used
            )));
        }
    }

    Ok(results)
}

/// The JavaScript the runtime runs in.
///
/// Kept as one string because it is JavaScript: the fan-out helpers are
/// promise combinators, and writing them in the language the script is written
/// in keeps one implementation of the semantics rather than two.
fn build_prelude() -> String {
    format!(
        r"
'use strict';
const __pending = new Map();
let __workflow_done = false;
let __workflow_failure = null;
let __workflow_result_value = null;

function __to_text(value) {{
  if (value === undefined || value === null) return '';
  if (typeof value === 'string') return value;
  try {{ return JSON.stringify(value); }} catch (error) {{ return String(value); }}
}}

function __to_options(value) {{
  return (value && typeof value === 'object') ? value : {{}};
}}

function __log_push(message) {{
  if (__log.length < {MAX_LOG_LINES}) {{
    __log.push(__to_text(message));
  }}
  return undefined;
}}

function __phase_push(title) {{
  __current_phase = __to_text(title);
  __phases.push(__current_phase);
  return undefined;
}}

function __agent_call(prompt, options) {{
  const id = 'a' + __pending.size + '-' + Math.floor(__next_id++);
  return new Promise((resolve, reject) => {{
    __pending.set(id, {{ resolve, reject, prompt, options }});
  }});
}}

let __next_id = 0;
let __log = [];


// The host reads this between rounds.
globalThis.__pending_requests = () => {{
  const out = [];
  for (const [id, entry] of __pending) {{
    out.push({{
      id,
      prompt: String(entry.prompt ?? ''),
      label: entry.options && entry.options.label !== undefined ? String(entry.options.label) : null,
      phase: entry.options && entry.options.phase !== undefined ? String(entry.options.phase) : (__current_phase ?? null),
      model: entry.options && entry.options.model !== undefined ? String(entry.options.model) : null,
      schema: entry.options && entry.options.schema !== undefined ? entry.options.schema : null,
      isolation: entry.options && entry.options.isolation !== undefined ? String(entry.options.isolation) : null,
      agentType: entry.options && entry.options.agentType !== undefined ? String(entry.options.agentType) : null,
      disallowedTools: entry.options && Array.isArray(entry.options.disallowedTools) ? entry.options.disallowedTools.map(String) : []
    }});
  }}
  return out;
}};

globalThis.__resolve_agent = (id, value) => {{
  const entry = __pending.get(id);
  if (!entry) return;
  __pending.delete(id);
  entry.resolve(value);
}};

globalThis.__workflow_finished = () => __workflow_done;

// `parallel` waits for all of them; a thunk that throws becomes null in its
// own slot and the call itself never rejects.
async function parallel(thunks) {{
  if (!Array.isArray(thunks)) throw new Error('parallel() takes an array of functions');
  if (thunks.length > {MAX_ITEMS_PER_CALL}) throw new Error('parallel() takes at most {MAX_ITEMS_PER_CALL} items, got ' + thunks.length);
  const started = thunks.map((thunk) => {{
    try {{ return Promise.resolve(thunk()).catch(() => null); }}
    catch (error) {{ return Promise.resolve(null); }}
  }});
  return Promise.all(started);
}}

// `pipeline` does not wait for a stage to finish before starting the next
// item: each item walks the stages on its own.
async function pipeline(items, ...stages) {{
  if (!Array.isArray(items)) throw new Error('pipeline() takes an array of items');
  if (items.length > {MAX_ITEMS_PER_CALL}) throw new Error('pipeline() takes at most {MAX_ITEMS_PER_CALL} items, got ' + items.length);
  const chains = items.map(async (item, index) => {{
    let value = item;
    for (const stage of stages) {{
      try {{ value = await stage(value, item, index); }}
      catch (error) {{ return null; }}
    }}
    return value;
  }});
  return Promise.all(chains);
}}

async function workflow(name) {{
  throw new Error('running another workflow from inside one is not supported (' + name + ')');
}}
"
    )
}

/// Put the callable surface into the realm.
///
/// Each native function is a thin door onto a JavaScript helper: the helper
/// owns the behaviour, and the door exists because the realm is what decides
/// what a script can reach.
fn register_host_functions(context: &mut Context) -> Result<(), boa_engine::JsError> {
    context.register_global_builtin_callable(
        js_string!("agent"),
        2,
        NativeFunction::from_fn_ptr(agent_call),
    )?;
    context.register_global_builtin_callable(
        js_string!("log"),
        1,
        NativeFunction::from_fn_ptr(log_push),
    )?;
    context.register_global_builtin_callable(
        js_string!("phase"),
        1,
        NativeFunction::from_fn_ptr(phase_push),
    )?;

    // A script can read what is left of the budget; it cannot change it.
    let budget = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(budget_total),
            js_string!("total"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(budget_remaining),
            js_string!("remaining"),
            0,
        )
        .build();
    context.register_global_property(js_string!("budget"), budget, Attribute::READONLY)?;

    Ok(())
}

fn agent_call(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    call_helper(context, "__agent_call", args)
}

fn log_push(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    call_helper(context, "__log_push", args)
}

fn phase_push(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    call_helper(context, "__phase_push", args)
}

fn budget_total(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    context.eval(Source::from_bytes("globalThis.__budget_total"))
}

fn budget_remaining(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    context.eval(Source::from_bytes("globalThis.__budget_remaining"))
}

/// Publish the run's token budget to the script.
///
/// The prelude initialises the globals to "no budget"; the host replaces them
/// with the real figures before the body runs and after every round, so
/// `budget.total()` and `budget.remaining()` report the budget actually in
/// force instead of a constant.
fn set_budget_globals(context: &mut Context, state: &State) -> Result<(), WorkflowError> {
    let total = state
        .limits
        .token_budget
        .map_or_else(|| "null".to_string(), |budget| budget.to_string());
    let remaining = state.limits.token_budget.map_or_else(
        || "Infinity".to_string(),
        |budget| budget.saturating_sub(state.tokens_used).to_string(),
    );
    let script = format!(
        "globalThis.__budget_total = {total}; globalThis.__budget_remaining = {remaining};"
    );
    context.eval(Source::from_bytes(&script)).map_err(|error| {
        WorkflowError::Failed(format!(
            "could not set the workflow budget globals: {error}"
        ))
    })?;
    Ok(())
}

/// Call one of the realm's helpers with the arguments a script passed.
fn call_helper(context: &mut Context, name: &str, args: &[JsValue]) -> JsResult<JsValue> {
    let helper = context.global_object().get(JsString::from(name), context)?;
    let function = helper.as_callable().ok_or_else(|| {
        JsNativeError::typ().with_message(format!("the runtime helper {name} is missing"))
    })?;
    function.call(&JsValue::undefined(), args, context)
}

/// Replace the ways a script could read the world outside itself.
fn install_shims(context: &mut Context) -> Result<(), boa_engine::JsError> {
    context.eval(Source::from_bytes(
        r"
'use strict';
const __real_date = Date;
Math.random = function random() {
  throw new Error('Math.random() is unavailable in workflow scripts (it breaks resume). ' +
                  'Include the index in the agent label or prompt instead.');
};
const __shim_date = function Date(...args) {
  if (!new.target) throw new Error('Date is unavailable in workflow scripts (it breaks resume).');
  if (args.length === 0) throw new Error('new Date() is unavailable in workflow scripts (it breaks resume).');
  return Reflect.construct(__real_date, args, new.target);
};
__shim_date.now = function now() {
  throw new Error('Date.now() is unavailable in workflow scripts (it breaks resume). ' +
                  'Stamp results after the workflow returns, or pass timestamps in args.');
};
__shim_date.parse = __real_date.parse;
__shim_date.UTC = __real_date.UTC;
__shim_date.prototype = __real_date.prototype;
__real_date.prototype.constructor = __shim_date;
Object.freeze(__real_date);
globalThis.Date = __shim_date;
globalThis.__current_phase = null;
globalThis.__phases = [];
// No token budget is attached yet; the host sets these when one is.
globalThis.__budget_total = null;
globalThis.__budget_remaining = Infinity;
",
    ))?;
    Ok(())
}

/// Hand one agent's result back to the script.
fn resolve_agent(context: &mut Context, id: &str, value: JsValue) -> Result<(), WorkflowError> {
    context
        .register_global_property(js_string!("__agent_value"), value, Attribute::all())
        .map_err(|e| WorkflowError::Failed(e.to_string()))?;
    context
        .eval(Source::from_bytes(
            format!(
                "__resolve_agent({}, __agent_value)",
                serde_json::to_string(id).unwrap_or_default()
            )
            .as_bytes(),
        ))
        .map_err(|error| WorkflowError::Failed(readable(&error)))?;
    Ok(())
}

/// Read the pending agent requests out of the realm.
#[must_use]
pub fn pending_requests(context: &mut Context) -> Vec<(String, AgentRequest)> {
    let json = eval_json(context, "__pending_requests()");
    let Some(items) = json.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            Some((
                id,
                AgentRequest {
                    prompt: item.get("prompt")?.as_str()?.to_string(),
                    label: item
                        .get("label")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    phase: item
                        .get("phase")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    model: item
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    schema: item.get("schema").filter(|v| !v.is_null()).cloned(),
                    isolation: item
                        .get("isolation")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    agent_type: item
                        .get("agentType")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    disallowed_tools: item
                        .get("disallowedTools")
                        .and_then(|v| v.as_array())
                        .map(|tools| {
                            tools
                                .iter()
                                .filter_map(|t| t.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            ))
        })
        .collect()
}

fn json_to_js(context: &mut Context, value: &serde_json::Value) -> JsValue {
    let source = format!("({value})");
    context
        .eval(Source::from_bytes(source.as_bytes()))
        .unwrap_or(JsValue::undefined())
}

/// Evaluate an expression and read it as JSON.
///
/// The value is stringified inside the realm rather than rendered here: a
/// `display()` of an object is not JSON, and every value crossing this boundary
/// is JSON-shaped by construction.
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

/// A thrown value as a person would read it.
fn readable(error: &boa_engine::JsError) -> String {
    error.to_string()
}

/// What a script declared, for a caller that wants to show it.
#[must_use]
pub fn describe(meta: &Meta) -> String {
    format!("{} — {}", meta.name, meta.description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An agent runner that answers without a model.
    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<AgentRequest>>,
        answer: Mutex<Option<String>>,
    }

    impl FakeRunner {
        fn answering(text: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                answer: Mutex::new(Some(text.to_string())),
            }
        }

        fn calls(&self) -> Vec<AgentRequest> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentRunner for FakeRunner {
        async fn run(&self, request: AgentRequest) -> Result<AgentOutput, String> {
            let prompt = request.prompt.clone();
            self.calls.lock().unwrap().push(request);
            let answer = self.answer.lock().unwrap().clone();
            Ok(AgentOutput::text(
                answer.unwrap_or_else(|| format!("answered {prompt}")),
            ))
        }
    }

    fn script(body: &str) -> String {
        format!("export const meta = {{ name: 'test', description: 'a test workflow' }}\n{body}")
    }

    async fn run_with(body: &str, runner: Arc<dyn AgentRunner>) -> Result<Outcome, WorkflowError> {
        run_with_limits(body, runner, Limits::default()).await
    }

    async fn run_with_limits(
        body: &str,
        runner: Arc<dyn AgentRunner>,
        limits: Limits,
    ) -> Result<Outcome, WorkflowError> {
        run_script(
            &script(body),
            None,
            runner,
            std::env::temp_dir().join(new_run_id()),
            limits,
        )
        .await
    }

    /// The budget the host configured is what the script sees, rather than
    /// the prelude's `null` / `Infinity` placeholders.
    #[tokio::test]
    async fn the_budget_globals_report_the_configured_budget() {
        let outcome = run_with_limits(
            "return [budget.total(), budget.remaining()]",
            Arc::new(FakeRunner::answering("unused")),
            Limits {
                token_budget: Some(10),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.result, Some(serde_json::json!([10, 10])));
    }

    /// The budget is a ceiling, not advice: a run that spends past it stops
    /// with a limit error instead of continuing.
    #[tokio::test]
    async fn a_run_stops_when_it_spends_its_token_budget() {
        struct Spending;

        #[async_trait::async_trait]
        impl AgentRunner for Spending {
            async fn run(&self, _request: AgentRequest) -> Result<AgentOutput, String> {
                Ok(AgentOutput::with_tokens("ok", 100))
            }
        }

        let error = run_with_limits(
            "await agent('one')\nawait agent('two')\nreturn 'done'",
            Arc::new(Spending),
            Limits {
                token_budget: Some(150),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(error, WorkflowError::Limit(_)), "{error}");
    }

    /// `budget.remaining()` reflects what earlier agents spent.
    #[tokio::test]
    async fn the_remaining_budget_shrinks_after_agents_run() {
        struct Spending;

        #[async_trait::async_trait]
        impl AgentRunner for Spending {
            async fn run(&self, _request: AgentRequest) -> Result<AgentOutput, String> {
                Ok(AgentOutput::with_tokens("ok", 40))
            }
        }

        let outcome = run_with_limits(
            "await agent('one')\nreturn budget.remaining()",
            Arc::new(Spending),
            Limits {
                token_budget: Some(100),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.result, Some(serde_json::json!(60)));
    }

    #[tokio::test]
    async fn a_script_can_call_an_agent_and_use_its_answer() {
        let runner = Arc::new(FakeRunner::answering("done"));
        let outcome = run_with(
            "const a = await agent('do the thing')\nreturn a",
            runner.clone(),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome.result,
            Some(serde_json::Value::String("done".into()))
        );
        assert_eq!(outcome.progress.agents_run, 1);
        assert_eq!(runner.calls()[0].prompt, "do the thing");
    }

    #[tokio::test]
    async fn parallel_runs_every_thunk_and_waits_for_all() {
        let runner = Arc::new(FakeRunner::default());
        let outcome = run_with(
            "const out = await parallel([() => agent('one'), () => agent('two'), () => agent('three')])\nreturn out",
            runner.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.progress.agents_run, 3);
        assert_eq!(outcome.result.unwrap().as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_thunk_that_throws_becomes_null_without_failing_the_call() {
        let runner = Arc::new(FakeRunner::default());
        let outcome = run_with(
            "const out = await parallel([() => { throw new Error('boom') }, () => agent('fine')])\nreturn out",
            runner,
        )
        .await
        .unwrap();

        let result = outcome.result.unwrap();
        assert_eq!(result[0], serde_json::Value::Null);
        assert!(result[1].is_string());
    }

    #[tokio::test]
    async fn pipeline_walks_every_item_through_every_stage() {
        let runner = Arc::new(FakeRunner::default());
        let outcome = run_with(
            "const out = await pipeline([1, 2],\n  (item) => agent('first ' + item),\n  (previous, item) => agent('second ' + item + ' after ' + previous))\nreturn out",
            runner.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.progress.agents_run, 4);
        let prompts: Vec<String> = runner.calls().iter().map(|c| c.prompt.clone()).collect();
        assert!(prompts.contains(&"first 1".to_string()), "{prompts:?}");
        assert!(
            prompts.contains(&"second 2 after answered first 2".to_string()),
            "{prompts:?}"
        );
    }

    #[tokio::test]
    async fn a_pipeline_stage_that_throws_skips_that_items_remaining_stages() {
        let runner = Arc::new(FakeRunner::default());
        let outcome = run_with(
            "const out = await pipeline([1, 2],\n  (item) => { if (item === 1) throw new Error('nope'); return item },\n  (item) => agent('stage two ' + item))\nreturn out",
            runner.clone(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.result.unwrap()[0], serde_json::Value::Null);
        assert_eq!(outcome.progress.agents_run, 1);
        assert_eq!(runner.calls()[0].prompt, "stage two 2");
    }

    #[tokio::test]
    async fn the_clock_and_random_are_unavailable() {
        for body in [
            "await agent('x'); return Date.now()",
            "await agent('x'); return Math.random()",
            "await agent('x'); return new Date()",
        ] {
            let error = run_with(body, Arc::new(FakeRunner::default()))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("breaks resume"), "{body}");
        }
    }

    #[tokio::test]
    async fn parsing_a_fixed_date_is_still_allowed() {
        let outcome = run_with(
            "await agent('x'); return Date.parse('2026-01-15T00:00:00Z') > 0",
            Arc::new(FakeRunner::default()),
        )
        .await
        .unwrap();
        assert_eq!(outcome.result, Some(serde_json::Value::Bool(true)));
    }

    #[tokio::test]
    async fn phases_and_logs_are_recorded() {
        let outcome = run_with(
            "phase('Review')\nlog('starting')\nawait agent('look')\nphase('Verify')\nawait agent('check')\nreturn 1",
            Arc::new(FakeRunner::default()),
        )
        .await
        .unwrap();

        assert_eq!(outcome.progress.phases, vec!["Review", "Verify"]);
        assert_eq!(outcome.progress.log, vec!["starting"]);
        assert_eq!(outcome.progress.agents_run, 2);
    }

    #[tokio::test]
    async fn an_agent_belongs_to_the_phase_it_was_called_in() {
        let runner = Arc::new(FakeRunner::default());
        run_with(
            "phase('Review')\nawait agent('look')\nphase('Verify')\nawait agent('check')",
            runner.clone(),
        )
        .await
        .unwrap();

        let calls = runner.calls();
        assert_eq!(calls[0].phase.as_deref(), Some("Review"));
        assert_eq!(calls[1].phase.as_deref(), Some("Verify"));
    }

    #[tokio::test]
    async fn arguments_reach_the_script_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_script(
            &script("return args.items.map(i => i.name)"),
            Some(serde_json::json!({"items": [{"name": "a"}, {"name": "b"}]})),
            Arc::new(FakeRunner::default()),
            dir.path().to_path_buf(),
            Limits::default(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.result, Some(serde_json::json!(["a", "b"])));
    }

    #[tokio::test]
    async fn the_agent_cap_stops_a_runaway_loop_with_an_explanation() {
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            max_agents: 5,
            ..Limits::default()
        };

        let error = run_script(
            &script("let i = 0\nwhile (true) { await agent('loop ' + i); i++ }\nreturn i"),
            None,
            Arc::new(FakeRunner::default()),
            dir.path().to_path_buf(),
            limits,
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("cap of 5"), "{message}");
        assert!(message.contains("iteration limit"), "{message}");
    }

    #[tokio::test]
    async fn a_resumed_run_replays_the_agents_it_already_ran() {
        let dir = tempfile::tempdir().unwrap();
        let body = "const a = await agent('one')\nconst b = await agent('two')\nreturn [a, b]";

        let first = run_script(
            &script(body),
            None,
            Arc::new(FakeRunner::answering("first answer")),
            dir.path().to_path_buf(),
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(first.progress.agents_run, 2);
        assert_eq!(first.progress.agents_replayed, 0);

        // The same script against the same journal runs nothing.
        let again = run_script(
            &script(body),
            None,
            Arc::new(FakeRunner::answering("a different answer")),
            dir.path().to_path_buf(),
            Limits::default(),
        )
        .await
        .unwrap();

        assert_eq!(again.progress.agents_run, 0);
        assert_eq!(again.progress.agents_replayed, 2);
        assert_eq!(again.result, first.result);
    }

    #[tokio::test]
    async fn changing_the_second_call_replays_the_first_and_reruns_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(FakeRunner::default());

        run_script(
            &script("await agent('one')\nawait agent('two')\nreturn 1"),
            None,
            runner.clone(),
            dir.path().to_path_buf(),
            Limits::default(),
        )
        .await
        .unwrap();

        let changed = run_script(
            &script("await agent('one')\nawait agent('two, but different')\nreturn 1"),
            None,
            runner.clone(),
            dir.path().to_path_buf(),
            Limits::default(),
        )
        .await
        .unwrap();

        assert_eq!(changed.progress.agents_replayed, 1);
        assert_eq!(changed.progress.agents_run, 1);
        let prompts: Vec<String> = runner.calls().iter().map(|c| c.prompt.clone()).collect();
        assert_eq!(
            prompts,
            vec![
                "one".to_string(),
                "two".to_string(),
                "two, but different".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn an_agent_that_fails_is_null_to_the_script() {
        struct FailingRunner;

        #[async_trait::async_trait]
        impl AgentRunner for FailingRunner {
            async fn run(&self, _request: AgentRequest) -> Result<AgentOutput, String> {
                Err("the model refused".to_string())
            }
        }

        let outcome = run_with(
            "const a = await agent('one')\nreturn a === null",
            Arc::new(FailingRunner),
        )
        .await
        .unwrap();

        assert_eq!(outcome.result, Some(serde_json::Value::Bool(true)));
        assert_eq!(outcome.progress.agents_empty, 1);
    }

    #[tokio::test]
    async fn the_realm_has_no_filesystem_or_network() {
        for name in [
            "require",
            "process",
            "fetch",
            "XMLHttpRequest",
            "globalThis.module",
        ] {
            let outcome = run_with(
                &format!("await agent('x'); return typeof {name}"),
                Arc::new(FakeRunner::default()),
            )
            .await
            .unwrap();
            assert_eq!(
                outcome.result,
                Some(serde_json::Value::String("undefined".into())),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn a_script_that_throws_reports_what_it_said() {
        let error = run_with(
            "throw new Error('the script gave up')",
            Arc::new(FakeRunner::default()),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("the script gave up"), "{error}");
    }

    #[tokio::test]
    async fn a_script_without_meta_is_refused_before_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let error = run_script(
            "console.log('no meta here')",
            None,
            Arc::new(FakeRunner::default()),
            dir.path().to_path_buf(),
            Limits::default(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, WorkflowError::Script(_)), "{error:?}");
    }

    #[test]
    fn the_concurrency_gate_stays_between_two_and_sixteen() {
        let gate = default_max_concurrent();
        assert!((2..=16).contains(&gate), "{gate}");
    }

    #[test]
    fn a_run_id_is_shaped_like_one() {
        let id = new_run_id();
        assert!(id.starts_with("wf_"), "{id}");
        assert!(id.len() > 8, "{id}");
        assert_ne!(id, new_run_id());
    }
}
