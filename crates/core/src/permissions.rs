//! Permission system — modes, rules, path matching, and policy evaluation.

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

/// Permission mode controlling how tool approvals work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PermissionMode {
    /// Default: ask before dangerous operations.
    #[default]
    Default,
    /// Accept edits without asking, but still approve dangerous tools.
    AcceptEdits,
    /// Bypass all permission checks.
    BypassPermissions,
    /// Don't ask — auto-deny anything that would require approval.
    DontAsk,
    /// Plan mode: read-only, no mutations.
    Plan,
    /// Auto: [`PermissionMode::Default`] plus the auto-classifier hook.
    ///
    /// Inside this checker the mode is indistinguishable from `Default` — it
    /// has no auto behaviour of its own and auto-approves nothing. What makes
    /// it "auto" lives outside the checker: the CLI registers
    /// [`AutoClassifierHook`](crate::permission_engine::AutoClassifierHook),
    /// which is active only in this mode and can short-circuit an operation
    /// the classifier judges safe. Without that hook, selecting `Auto` changes
    /// nothing.
    Auto,
}

impl PermissionMode {
    /// Returns true if this mode is fully read-only.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Plan)
    }

    /// Returns true if this mode auto-approves all operations.
    #[must_use]
    pub const fn bypasses_all(self) -> bool {
        matches!(self, Self::BypassPermissions)
    }

    /// Returns true if this mode auto-denies all operations.
    #[must_use]
    pub const fn denies_all(self) -> bool {
        matches!(self, Self::DontAsk)
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept-edits",
            Self::BypassPermissions => "bypass-permissions",
            Self::DontAsk => "dont-ask",
            Self::Plan => "plan",
            Self::Auto => "auto",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "default" => Ok(Self::Default),
            "accept-edits" => Ok(Self::AcceptEdits),
            "bypass-permissions" => Ok(Self::BypassPermissions),
            "dont-ask" => Ok(Self::DontAsk),
            "plan" => Ok(Self::Plan),
            "auto" => Ok(Self::Auto),
            _ => Err(format!("unknown permission mode: {s}")),
        }
    }
}

/// The result of a permission check.
///
/// A session-wide approval is not a result of its own: once the user answers
/// with `ConfirmationResponse::ApproveSession`, the checker records it with
/// [`PermissionChecker::record_session_approval`] and later checks return
/// [`Self::Allow`]. In other words, the result always describes the current
/// call and never caches a future decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult {
    /// The operation is allowed.
    Allow,
    /// The operation requires user confirmation.
    Ask {
        /// Human-readable reason for the prompt.
        reason: String,
    },
    /// The operation is denied.
    Deny {
        /// Human-readable reason for denial.
        reason: String,
    },
    /// The operation is allowed only for the given path patterns.
    AllowForPath {
        /// Glob patterns the tool is allowed to operate on.
        patterns: Vec<String>,
    },
    /// The operation requires elevated privileges (e.g. sudo).
    Elevated {
        /// Human-readable reason for elevation.
        reason: String,
    },
}

impl PermissionResult {
    /// Message for a tool result when an `Ask` reached the caller unresolved.
    ///
    /// [`PermissionEngine::evaluate`](crate::permission_engine::PermissionEngine::evaluate)
    /// routes `Ask` through the confirmation bus when one is configured and
    /// returns the user's actual answer. So an `Ask` that arrives at a caller
    /// means no interactive channel was available — reporting it as "denied by
    /// user" claims a decision nobody made, and hides the real cause from a
    /// headless or SDK caller.
    #[must_use]
    pub fn unapproved_tool_message(tool_name: &str, reason: &str) -> String {
        format!(
            "tool '{tool_name}' requires approval but no confirmation channel is available: {reason}"
        )
    }
}

impl PermissionResult {
    /// Returns true if the operation can proceed without user input.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow | Self::AllowForPath { .. })
    }
}

/// A single permission rule entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Tool name this rule applies to (empty = all tools).
    pub tool_name: String,
    /// Whether to allow, deny, or ask.
    pub action: PermissionAction,
    /// Glob patterns for path-based rules.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
}

/// A parsed Claude-style permission rule.
///
/// Settings store the raw strings (`allow: ["Bash(git diff:*)"]`); this is the
/// parsed form. It is kept separate from [`PermissionRule`] so the legacy
/// programmatic rule API — and every struct literal that uses it — keeps
/// working unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRuleSpec {
    /// Tool the rule applies to.
    pub tool_name: String,
    /// Whether the rule allows, asks, or denies.
    pub action: PermissionAction,
    /// What the rule matches.
    pub matcher: RuleMatcher,
    /// The raw rule text, used in user-facing reasons.
    pub description: String,
}

/// What a parsed rule matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleMatcher {
    /// `Tool` — every call to the tool.
    WholeTool,
    /// `Tool(glob)` — path globs, matched against rule subjects.
    ///
    /// `*` does not cross a `/`; `**` does (`src/**` matches the whole tree).
    Glob {
        /// User-supplied patterns, kept for sub-agent recompilation.
        patterns: Vec<String>,
    },
    /// `Bash(prefix:*)` — the command starts with `prefix` at a word boundary.
    BashPrefix {
        /// Command prefix.
        prefix: String,
    },
    /// `Bash(glob)` — a glob over the whole command (`Bash(*)`, `Bash(ls *.rs)`).
    BashGlob {
        /// Command glob.
        pattern: String,
    },
}

/// One thing a permission rule is judged against for a call.
///
/// A call has one subject per command segment or file. Each subject carries the
/// candidate spellings of that same thing (the argument as typed, its absolute
/// form, its canonical form). Deny/ask rules match when **any** subject
/// matches; an allow rule must cover **every** subject, so a compound command
/// or a multi-file write is never auto-approved by a rule that only covers one
/// of its parts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleSubject {
    /// Candidate spellings of this subject.
    pub forms: Vec<String>,
    /// Why this call cannot be fully modelled, if it cannot.
    ///
    /// An opaque call is never auto-approved by an allow rule; it asks (or is
    /// denied when no prompt is available). Deny and ask rules still match the
    /// segments that were recovered.
    pub opaque: Option<String>,
}

impl From<&str> for RuleSubject {
    fn from(value: &str) -> Self {
        Self::single(value)
    }
}

impl From<String> for RuleSubject {
    fn from(value: String) -> Self {
        Self::single(value)
    }
}

impl RuleSubject {
    /// Build a subject from its candidate spellings.
    #[must_use]
    pub fn new(forms: impl IntoIterator<Item = String>) -> Self {
        Self {
            forms: forms.into_iter().collect(),
            opaque: None,
        }
    }

    /// A subject with exactly one spelling.
    #[must_use]
    pub fn single(value: impl Into<String>) -> Self {
        Self {
            forms: vec![value.into()],
            opaque: None,
        }
    }

    /// Mark the call as not fully analysable, with the reason.
    #[must_use]
    pub fn with_opaque(mut self, reason: impl Into<String>) -> Self {
        self.opaque = Some(reason.into());
        self
    }
}

/// Errors from parsing a Claude-style permission rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PermissionRuleParseError {
    /// The rule is empty.
    #[error("permission rule is empty")]
    Empty,
    /// The tool name is empty.
    #[error("permission rule tool name is empty")]
    MissingToolName,
    /// The rule has an unmatched parenthesis.
    #[error("permission rule {0:?} has an unbalanced '('")]
    UnbalancedParentheses(String),
    /// The specifier is empty.
    #[error("permission rule {0:?} has an empty specifier")]
    EmptySpecifier(String),
    /// A Bash prefix is empty.
    #[error("permission rule {0:?} has an empty command prefix")]
    EmptyBashPrefix(String),
    /// A glob did not compile.
    #[error("permission rule {0:?} has an invalid glob: {1}")]
    InvalidGlob(String, String),
}

/// Current directory and home used when compiling path rules.
#[derive(Debug, Clone, Copy)]
pub struct RuleCompileContext<'a> {
    /// Session working directory for relative paths.
    pub cwd: &'a Path,
    /// Home directory for `~` patterns, when known.
    pub home: Option<&'a Path>,
}

impl PermissionRuleSpec {
    /// Parse one Claude-style rule string.
    ///
    /// Accepted forms:
    /// - `Tool`
    /// - `Tool(glob)` for path tools, e.g. `Read(./.env)`, `Edit(src/**)`
    /// - `Bash(prefix:*)` for a command prefix
    /// - `Bash(glob)` for a command glob, e.g. `Bash(*)`, `Bash(ls *.rs)`
    pub fn parse(raw: &str, action: PermissionAction) -> Result<Self, PermissionRuleParseError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(PermissionRuleParseError::Empty);
        }

        let (tool_name, specifier) = match raw.find('(') {
            None => (raw, None),
            Some(open) => {
                if !raw.ends_with(')') {
                    return Err(PermissionRuleParseError::UnbalancedParentheses(
                        raw.to_string(),
                    ));
                }
                let close = raw.len() - 1;
                (
                    raw[..open].trim(),
                    Some(raw[open + 1..close].trim().to_string()),
                )
            }
        };

        if tool_name.is_empty() {
            return Err(PermissionRuleParseError::MissingToolName);
        }
        if tool_name.chars().any(char::is_whitespace) || tool_name.contains(')') {
            return Err(PermissionRuleParseError::UnbalancedParentheses(
                raw.to_string(),
            ));
        }

        let matcher = match specifier {
            None => RuleMatcher::WholeTool,
            Some(specifier) if specifier.is_empty() => {
                return Err(PermissionRuleParseError::EmptySpecifier(raw.to_string()));
            }
            Some(specifier) if is_shell_tool(tool_name) => parse_shell_matcher(&specifier, raw)?,
            Some(specifier) => {
                validate_path_glob(&specifier, raw)?;
                RuleMatcher::Glob {
                    patterns: vec![specifier],
                }
            }
        };

        Ok(Self {
            tool_name: tool_name.to_string(),
            action,
            matcher,
            description: raw.to_string(),
        })
    }
}

/// A tool whose specifier is a command, not a path.
fn is_shell_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "powershell" | "pwsh" | "sh" | "zsh" | "shell"
    )
}

fn parse_shell_matcher(
    specifier: &str,
    raw: &str,
) -> Result<RuleMatcher, PermissionRuleParseError> {
    if let Some(prefix) = specifier.strip_suffix(":*") {
        let prefix = prefix.trim_end();
        if prefix.is_empty() {
            return Err(PermissionRuleParseError::EmptyBashPrefix(raw.to_string()));
        }
        return Ok(RuleMatcher::BashPrefix {
            prefix: prefix.to_string(),
        });
    }
    Glob::new(specifier)
        .map_err(|e| PermissionRuleParseError::InvalidGlob(raw.to_string(), e.to_string()))?;
    Ok(RuleMatcher::BashGlob {
        pattern: specifier.to_string(),
    })
}

fn validate_path_glob(pattern: &str, raw: &str) -> Result<(), PermissionRuleParseError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|_| ())
        .map_err(|e| PermissionRuleParseError::InvalidGlob(raw.to_string(), e.to_string()))
}

/// Per-tool exemptions from whole-tool allow rules.
///
/// A whole-tool allow rule names a tool and gives no path patterns: it says
/// "this tool, anywhere, is fine". For most tools that is the point. For a
/// *runner* — a tool that executes other work on the model's behalf — it is a
/// way to approve once and get anything, so the tool itself refuses the
/// shortcut.
///
/// These narrow **allow** rules only. A deny rule is evaluated before any
/// allow rule and is unaffected: a tool carrying either flag is still denied
/// by a deny rule.
///
/// Both flags are a property of the *call*, not only of the tool: the artifact
/// tool refuses the shortcut for its mutating actions and honours it for its
/// reads. Whoever resolves them therefore answers per input — a tool answers
/// `rule_exemptions_for(input)` in `coderex-tools`, and a session hands the
/// engine a resolver that consults the registry (see
/// [`PermissionEngine::set_rule_exemptions_resolver`](crate::permission_engine::PermissionEngine::set_rule_exemptions_resolver)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRuleExemptions {
    /// A whole-tool allow rule does not approve this tool — the rule is not
    /// honoured and the decision continues down the ordinary path.
    ///
    /// Only that: it neutralises a rule that matched. A tool carrying it that
    /// no rule matched still gets the ordinary mode default, so the flag is not
    /// "always prompt" (a mode that bypasses checks still allows) and being a
    /// no-op when there is nothing to refuse is intended.
    #[serde(default)]
    pub ignores_whole_tool_allow_rule: bool,
    /// The "always allow" choice is not offered for this tool, so an approval
    /// cannot be turned into a session-wide one.
    #[serde(default)]
    pub suppresses_always_allow_rule: bool,
}

impl ToolRuleExemptions {
    /// No exemptions — the state of every tool that declares none.
    pub const NONE: Self = Self {
        ignores_whole_tool_allow_rule: false,
        suppresses_always_allow_rule: false,
    };

    /// Whether this tool carries no exemptions at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.ignores_whole_tool_allow_rule && !self.suppresses_always_allow_rule
    }
}

/// Action a rule takes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PermissionAction {
    /// Allow the operation.
    Allow,
    /// Ask for confirmation.
    Ask,
    /// Deny the operation.
    Deny,
}

/// Errors from the permission checker.
#[derive(Debug, Error)]
pub enum PermissionError {
    /// Invalid glob pattern in a rule.
    #[error("invalid glob pattern: {pattern}")]
    InvalidGlob {
        /// The pattern that failed to compile.
        pattern: String,
        /// The underlying error.
        source: globset::Error,
    },
}

/// Pre-compiled matcher for a rule.
enum CompiledMatcher {
    /// Every call to the tool.
    WholeTool,
    /// Path globs; at least one must match a subject.
    Path(Vec<GlobMatcher>),
    /// Bash command prefix at a word boundary.
    BashPrefix { prefix: String },
    /// Bash command glob.
    BashGlob(GlobMatcher),
}

/// One rule ready for evaluation.
struct CompiledRule {
    tool_name: String,
    action: PermissionAction,
    matcher: CompiledMatcher,
    description: String,
    /// Parsed form, kept so sub-agents can recompile against their own cwd.
    spec: PermissionRuleSpec,
}

/// Compile one parsed rule into its matcher.
fn compile_spec(
    spec: &PermissionRuleSpec,
    ctx: &RuleCompileContext<'_>,
) -> Result<CompiledRule, PermissionError> {
    let matcher = match &spec.matcher {
        RuleMatcher::WholeTool => CompiledMatcher::WholeTool,
        RuleMatcher::BashPrefix { prefix } => CompiledMatcher::BashPrefix {
            prefix: prefix.clone(),
        },
        RuleMatcher::BashGlob { pattern } => {
            let glob = Glob::new(pattern).map_err(|e| PermissionError::InvalidGlob {
                pattern: pattern.clone(),
                source: e,
            })?;
            CompiledMatcher::BashGlob(glob.compile_matcher())
        }
        RuleMatcher::Glob { patterns } => {
            let mut matchers = Vec::new();
            for pattern in patterns {
                for variant in path_pattern_variants(pattern, ctx) {
                    let glob = GlobBuilder::new(&variant)
                        .literal_separator(true)
                        .build()
                        .map_err(|e| PermissionError::InvalidGlob {
                            pattern: pattern.clone(),
                            source: e,
                        })?;
                    matchers.push(glob.compile_matcher());
                }
            }
            CompiledMatcher::Path(matchers)
        }
    };

    Ok(CompiledRule {
        tool_name: spec.tool_name.clone(),
        action: spec.action,
        matcher,
        description: spec.description.clone(),
        spec: spec.clone(),
    })
}

/// Normalize separators so the same rule works on every platform.
fn normalize_separators(value: &str) -> String {
    value.replace('\\', "/")
}

/// The pattern forms a path rule is matched against.
///
/// Both the user spelling and an absolute form are compiled: the subject list
/// contains the raw argument and a cwd/home-resolved absolute path, and either
/// may be the one the user meant.
#[allow(clippy::option_if_let_else)]
fn path_pattern_variants(pattern: &str, ctx: &RuleCompileContext<'_>) -> Vec<String> {
    let pattern = normalize_separators(pattern.trim());
    let mut variants = vec![pattern.clone()];

    let relative = pattern.strip_prefix("./").unwrap_or(&pattern);
    if relative != pattern && !relative.is_empty() {
        variants.push(relative.to_string());
    }

    let absolute = if let Some(rest) = pattern.strip_prefix("//") {
        Some(format!("/{rest}"))
    } else if pattern == "~" {
        ctx.home
            .map(|home| globset::escape(&normalize_separators(&home.to_string_lossy())))
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        ctx.home.map(|home| {
            format!(
                "{}/{}",
                globset::escape(&normalize_separators(&home.to_string_lossy()))
                    .trim_end_matches('/'),
                rest
            )
        })
    } else if pattern.starts_with('/') {
        Some(pattern.clone())
    } else {
        let cwd = normalize_separators(&ctx.cwd.to_string_lossy());
        if cwd.is_empty() {
            None
        } else {
            Some(format!(
                "{}/{}",
                globset::escape(cwd.trim_end_matches('/')),
                relative
            ))
        }
    };

    if let Some(absolute) = absolute {
        if let Some(canonical) = canonical_pattern_variant(&absolute)
            && !variants.contains(&canonical)
        {
            variants.push(canonical);
        }
        if !variants.contains(&absolute) {
            variants.push(absolute);
        }
    }
    variants
}

/// Resolve the longest existing, glob-free prefix of a pattern.
///
/// A rule such as `Read(./link/**)` must match a file reached through a
/// symlink by its real location. The literal directory before the first glob
/// is canonicalized; the glob suffix is appended unchanged. A path that is a
/// symlink but whose target changes between the check and the tool call is a
/// residual TOCTOU risk no userspace matcher can remove.
fn canonical_pattern_variant(pattern: &str) -> Option<String> {
    let glob_at = pattern.find(['*', '?', '[', '{']);
    if glob_at.is_none() {
        let path = std::path::Path::new(pattern);
        let canonical = canonicalize_lenient(path)?;
        return Some(globset::escape(&normalize_separators(
            &canonical.to_string_lossy(),
        )));
    }

    let glob_at = glob_at.unwrap_or(pattern.len());
    let literal = &pattern[..glob_at];
    let rest = &pattern[glob_at..];
    let directory_end = literal.rfind('/').map_or(0, |index| index + 1);
    if directory_end == 0 {
        return None;
    }
    let directory = &literal[..directory_end];
    let partial = &literal[directory_end..];
    let canonical = canonicalize_lenient(std::path::Path::new(directory))?;
    let mut out = globset::escape(&normalize_separators(&canonical.to_string_lossy()));
    if !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(partial);
    out.push_str(rest);
    Some(out)
}

/// Resolve symlinks when possible; keep the path otherwise.
fn canonicalize_lenient(path: &Path) -> Option<std::path::PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(strip_verbatim_prefix(canonical));
    }

    // A not-yet-created target: find the nearest existing ancestor, then
    // re-attach the missing components.
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path;
    loop {
        let name = current.file_name()?;
        suffix.push(name.to_os_string());
        let parent = current.parent()?;
        if parent.as_os_str().is_empty() {
            return None;
        }
        if let Ok(mut base) = std::fs::canonicalize(parent) {
            for component in suffix.iter().rev() {
                base.push(component);
            }
            return Some(strip_verbatim_prefix(base));
        }
        current = parent;
    }
}

/// Windows canonical paths carry a `\\?\` prefix; rules do not.
fn strip_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    text.strip_prefix(r"\\?\")
        .map_or(path, std::path::PathBuf::from)
}

/// Whether a command is the configured prefix at a word boundary.
///
/// Leading `NAME=value` environment assignments are skipped for matching, so a
/// `Bash(rm:*)` rule still covers `FOO=1 rm -rf /`. The rule text itself may
/// still include assignments if the caller wants them.
fn bash_prefix_matches(command: &str, prefix: &str) -> bool {
    let command = command.trim_start();
    let stripped = crate::shell_security::strip_leading_env_assignments(command);
    [command, stripped]
        .into_iter()
        .any(|candidate| command_starts_with_prefix(candidate, prefix))
}

fn command_starts_with_prefix(command: &str, prefix: &str) -> bool {
    command.strip_prefix(prefix).is_some_and(|rest| {
        rest.is_empty()
            || rest.starts_with(|c: char| {
                c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')')
            })
    })
}

/// A Bash glob also matches once leading environment assignments are removed.
fn bash_glob_matches(matcher: &GlobMatcher, form: &str) -> bool {
    matcher.is_match(form)
        || matcher.is_match(crate::shell_security::strip_leading_env_assignments(form))
}

/// Checks permissions based on mode and configured rules.
#[allow(missing_debug_implementations)]
pub struct PermissionChecker {
    mode: PermissionMode,
    /// Saved mode for prePlanMode save/restore.
    saved_mode: Option<PermissionMode>,
    rules: Vec<CompiledRule>,
    /// Session-level cached approvals: tool_name -> whether approved.
    session_approvals: HashMap<String, bool>,
    /// Tools known to be dangerous (always require approval).
    dangerous_tools: Vec<String>,
    /// Per-tool rule exemptions, keyed by lowercased tool name.
    ///
    /// The checker cannot see tools, so whoever owns both a registry and a
    /// checker has to introduce them; until then a tool's declared exemptions
    /// are inert.
    tool_exemptions: HashMap<String, ToolRuleExemptions>,
}

impl PermissionChecker {
    /// Create a new permission checker in the given mode.
    #[must_use]
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            saved_mode: None,
            rules: Vec::new(),
            session_approvals: HashMap::new(),
            dangerous_tools: vec![
                "bash".into(),
                "write".into(),
                "edit".into(),
                "multi_edit".into(),
                "notebook_edit".into(),
            ],
            tool_exemptions: HashMap::new(),
        }
    }

    /// Declare a tool's exemptions from whole-tool allow rules.
    ///
    /// Passing [`ToolRuleExemptions::NONE`] clears any earlier declaration.
    pub fn set_tool_exemptions(&mut self, tool_name: &str, exemptions: ToolRuleExemptions) {
        let key = tool_name.to_lowercase();
        if exemptions.is_empty() {
            self.tool_exemptions.remove(&key);
        } else {
            self.tool_exemptions.insert(key, exemptions);
        }
    }

    /// A tool's declared exemptions, or [`ToolRuleExemptions::NONE`].
    #[must_use]
    pub fn tool_exemptions(&self, tool_name: &str) -> ToolRuleExemptions {
        self.tool_exemptions
            .get(&tool_name.to_lowercase())
            .copied()
            .unwrap_or(ToolRuleExemptions::NONE)
    }

    /// Set the permission mode.
    pub const fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Save the current mode and switch to Plan mode.
    ///
    /// Use before entering PlanMode; pair with `exit_plan_mode()` to restore.
    pub const fn enter_plan_mode(&mut self) {
        self.saved_mode = Some(self.mode);
        self.mode = PermissionMode::Plan;
    }

    /// Restore the mode saved before entering Plan mode.
    ///
    /// Returns the restored mode, or `None` if no mode was saved.
    pub const fn exit_plan_mode(&mut self) -> Option<PermissionMode> {
        let prev = self.saved_mode.take();
        if let Some(saved) = prev {
            self.mode = saved;
        }
        prev
    }

    /// Get the current permission mode.
    #[must_use]
    pub const fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Add a permission rule.
    ///
    /// This is the legacy programmatic API: patterns are matched verbatim, as
    /// before. Compiled Claude-style rules are added with [`Self::add_spec`].
    pub fn add_rule(&mut self, rule: PermissionRule) -> Result<(), PermissionError> {
        let mut path_matchers = Vec::new();
        for pattern in &rule.paths {
            let glob = Glob::new(pattern)
                .map_err(|e| PermissionError::InvalidGlob {
                    pattern: pattern.clone(),
                    source: e,
                })?
                .compile_matcher();
            path_matchers.push(glob);
        }

        let spec = PermissionRuleSpec {
            tool_name: rule.tool_name.clone(),
            action: rule.action,
            matcher: if rule.paths.is_empty() {
                RuleMatcher::WholeTool
            } else {
                RuleMatcher::Glob {
                    patterns: rule.paths.clone(),
                }
            },
            description: rule.description.clone(),
        };
        self.rules.push(CompiledRule {
            tool_name: rule.tool_name,
            action: rule.action,
            matcher: if path_matchers.is_empty() {
                CompiledMatcher::WholeTool
            } else {
                CompiledMatcher::Path(path_matchers)
            },
            description: rule.description,
            spec,
        });
        Ok(())
    }

    /// Add a parsed Claude-style rule, compiled against `ctx`.
    pub fn add_spec(
        &mut self,
        spec: &PermissionRuleSpec,
        ctx: &RuleCompileContext<'_>,
    ) -> Result<(), PermissionError> {
        let compiled = compile_spec(spec, ctx)?;
        self.rules.push(compiled);
        Ok(())
    }

    /// Add several parsed rules atomically: all compile or none are installed.
    pub fn add_specs(
        &mut self,
        specs: &[PermissionRuleSpec],
        ctx: &RuleCompileContext<'_>,
    ) -> Result<(), PermissionError> {
        let mut compiled = Vec::with_capacity(specs.len());
        for spec in specs {
            compiled.push(compile_spec(spec, ctx)?);
        }
        self.rules.extend(compiled);
        Ok(())
    }

    /// The parsed rules installed in this checker.
    ///
    /// Sub-agents use this to inherit rules and recompile relative patterns
    /// against their own working directory.
    #[must_use]
    pub fn rule_specs(&self) -> Vec<PermissionRuleSpec> {
        self.rules.iter().map(|rule| rule.spec.clone()).collect()
    }

    /// Check if a tool operation is allowed.
    ///
    /// Evaluates in order: mode shortcuts → deny → plan/accept-edits → session
    /// cache → ask → allow → dangerous-tool default.
    ///
    /// A tool's rule exemptions are taken from this checker's static table —
    /// see [`Self::check_with_exemptions`] for a caller that can answer them
    /// per call.
    #[must_use]
    pub fn check(&self, tool_name: &str, path: Option<&Path>) -> PermissionResult {
        self.check_with_exemptions(tool_name, path, None)
    }

    /// Check a call whose rule exemptions are already known.
    ///
    /// The flags are a property of the *call* — the artifact tool refuses
    /// whole-tool allow rules for its mutating actions only — so a caller that
    /// has the call's arguments (the permission engine) resolves them and
    /// passes them here. `None` means "use this checker's static table", which
    /// is what a caller with only a tool name can do.
    #[must_use]
    pub fn check_with_exemptions(
        &self,
        tool_name: &str,
        path: Option<&Path>,
        exemptions: Option<ToolRuleExemptions>,
    ) -> PermissionResult {
        let subjects: Vec<RuleSubject> = path
            .map(|path| vec![RuleSubject::single(path.to_string_lossy().to_string())])
            .unwrap_or_default();
        self.check_with_subjects(tool_name, &subjects, exemptions)
    }

    /// Check a call against explicit rule subjects.
    ///
    /// Subjects are what a rule is judged against: the Bash command, the
    /// file path(s), or whatever a tool declares in its `RuleContentField`.
    /// The engine obtains them from its [`ToolExecutor`](crate::tool_executor::ToolExecutor).
    /// A rule with a specifier matches when **any** subject matches; a
    /// whole-tool rule matches regardless.
    #[must_use]
    pub fn check_with_subjects(
        &self,
        tool_name: &str,
        subjects: &[RuleSubject],
        exemptions: Option<ToolRuleExemptions>,
    ) -> PermissionResult {
        // Mode-level shortcuts. Bypass and dont-ask are the deliberate hard
        // overrides; a configured deny still applies in plan/accept-edits so
        // a rule cannot be sidestepped by switching mode.
        if self.mode.bypasses_all() {
            return PermissionResult::Allow;
        }
        if self.mode.denies_all() {
            return PermissionResult::Deny {
                reason: "permission checks disabled — auto-deny".into(),
            };
        }

        let exemptions = exemptions.unwrap_or_else(|| self.tool_exemptions(tool_name));
        let opaque_reason = subjects.iter().find_map(|subject| subject.opaque.clone());

        // Deny rules first: deny always wins over ask and allow.
        for rule in &self.rules {
            if rule.action == PermissionAction::Deny
                && Self::rule_matches_any(rule, tool_name, subjects)
            {
                return PermissionResult::Deny {
                    reason: Self::rule_reason(rule, tool_name),
                };
            }
        }

        if self.mode.is_read_only() {
            if self.is_tool_read_only(tool_name) {
                return PermissionResult::Allow;
            }
            return PermissionResult::Deny {
                reason: format!("tool '{tool_name}' is not allowed in plan mode"),
            };
        }
        if self.mode == PermissionMode::AcceptEdits && self.is_edit_tool(tool_name) {
            return PermissionResult::Allow;
        }

        // Session-level cache.
        if self
            .session_approvals
            .get(tool_name)
            .is_some_and(|approved| *approved)
        {
            return PermissionResult::Allow;
        }

        // Ask before allow: an explicit ask must not be defeated by an allow.
        for rule in &self.rules {
            if rule.action == PermissionAction::Ask
                && Self::rule_matches_any(rule, tool_name, subjects)
            {
                return PermissionResult::Ask {
                    reason: Self::rule_reason(rule, tool_name),
                };
            }
        }

        // Explicit whole-tool allows. A runner that refuses the shortcut
        // does not get it; the specifier rules below, or the default, decide.
        for rule in &self.rules {
            let name_matches =
                rule.tool_name.is_empty() || rule.tool_name.eq_ignore_ascii_case(tool_name);
            if rule.action == PermissionAction::Allow
                && name_matches
                && matches!(rule.matcher, CompiledMatcher::WholeTool)
                && opaque_reason.is_none()
                && !exemptions.ignores_whole_tool_allow_rule
            {
                return PermissionResult::Allow;
            }
        }

        // Specifier allows compose: a compound command or a multi-file write
        // is auto-approved only when *every* subject is covered by at least
        // one allow rule. A rule covering one part is not enough.
        if !subjects.is_empty() && opaque_reason.is_none() {
            let mut path_patterns: Vec<String> = Vec::new();
            let mut all_covered = true;
            for subject in subjects {
                let mut covered = false;
                for rule in &self.rules {
                    if rule.action != PermissionAction::Allow
                        || matches!(rule.matcher, CompiledMatcher::WholeTool)
                    {
                        continue;
                    }
                    if Self::rule_matches_segment(rule, tool_name, subject) {
                        covered = true;
                        if let RuleMatcher::Glob { patterns } = &rule.spec.matcher {
                            for pattern in patterns {
                                if !path_patterns.contains(pattern) {
                                    path_patterns.push(pattern.clone());
                                }
                            }
                        }
                    }
                }
                if !covered {
                    all_covered = false;
                    break;
                }
            }
            if all_covered {
                return if path_patterns.is_empty() {
                    PermissionResult::Allow
                } else {
                    PermissionResult::AllowForPath {
                        patterns: path_patterns,
                    }
                };
            }
        }

        // A command the static analysis cannot fully model is never
        // auto-approved; it asks even when the tool would otherwise be safe.
        if let Some(reason) = opaque_reason {
            return PermissionResult::Ask {
                reason: format!(
                    "command requires approval because it cannot be fully analyzed: {reason}"
                ),
            };
        }

        // Default: dangerous tools require approval, safe tools allowed.
        if self.is_dangerous_tool(tool_name) {
            PermissionResult::Ask {
                reason: format!("tool '{tool_name}' requires approval (dangerous operation)"),
            }
        } else {
            PermissionResult::Allow
        }
    }

    /// Record a session-level approval.
    pub fn record_session_approval(&mut self, tool_name: String, approved: bool) {
        self.session_approvals.insert(tool_name, approved);
    }

    /// Clear session-level approvals.
    pub fn clear_session_approvals(&mut self) {
        self.session_approvals.clear();
    }

    /// Strip dangerous tool calls from a list, returning allowed and denied.
    #[must_use]
    pub fn strip_dangerous_tools<'a>(
        &self,
        tool_names: impl IntoIterator<Item = &'a str>,
    ) -> (Vec<&'a str>, Vec<(&'a str, String)>) {
        let mut allowed = Vec::new();
        let mut denied = Vec::new();

        for name in tool_names {
            match self.check(name, None) {
                PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                    allowed.push(name);
                }
                other => {
                    let reason = match other {
                        PermissionResult::Deny { reason }
                        | PermissionResult::Ask { reason }
                        | PermissionResult::Elevated { reason } => reason,
                        PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                            unreachable!()
                        }
                    };
                    denied.push((name, reason));
                }
            }
        }
        (allowed, denied)
    }

    /// Check if a tool is known to be dangerous.
    #[must_use]
    pub fn is_dangerous_tool(&self, tool_name: &str) -> bool {
        let lower = tool_name.to_lowercase();
        self.dangerous_tools
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&lower))
    }

    /// Check if a tool edits local files.
    #[must_use]
    pub fn is_edit_tool(&self, tool_name: &str) -> bool {
        matches!(
            tool_name.to_lowercase().as_str(),
            "write" | "edit" | "multi_edit" | "notebook_edit"
        )
    }

    /// Check if a tool is read-only (safe for plan mode).
    #[must_use]
    pub fn is_tool_read_only(&self, tool_name: &str) -> bool {
        matches!(
            tool_name.to_lowercase().as_str(),
            "read"
                | "ls"
                | "listdir"
                | "currentdir"
                | "grep"
                | "glob"
                | "web_search"
                | "web_fetch"
                | "grep_code"
                | "file_search"
                | "notebook_read"
                | "toolsearch"
                | "lsp"
        )
    }

    /// Whether a rule matches one subject of a call.
    fn rule_matches_segment(rule: &CompiledRule, tool_name: &str, subject: &RuleSubject) -> bool {
        let name_matches =
            rule.tool_name.is_empty() || rule.tool_name.eq_ignore_ascii_case(tool_name);
        if !name_matches {
            return false;
        }

        match &rule.matcher {
            CompiledMatcher::WholeTool => true,
            CompiledMatcher::BashPrefix { prefix } => subject
                .forms
                .iter()
                .any(|form| bash_prefix_matches(form, prefix)),
            CompiledMatcher::BashGlob(matcher) => subject
                .forms
                .iter()
                .any(|form| bash_glob_matches(matcher, form)),
            CompiledMatcher::Path(matchers) => subject
                .forms
                .iter()
                .any(|form| matchers.iter().any(|matcher| matcher.is_match(form))),
        }
    }

    /// Whether a rule matches any subject of a call (deny/ask semantics).
    fn rule_matches_any(rule: &CompiledRule, tool_name: &str, subjects: &[RuleSubject]) -> bool {
        let name_matches =
            rule.tool_name.is_empty() || rule.tool_name.eq_ignore_ascii_case(tool_name);
        if !name_matches {
            return false;
        }
        if matches!(rule.matcher, CompiledMatcher::WholeTool) {
            return true;
        }
        subjects
            .iter()
            .any(|subject| Self::rule_matches_segment(rule, tool_name, subject))
    }

    /// Generate a human-readable reason from a rule.
    fn rule_reason(rule: &CompiledRule, tool_name: &str) -> String {
        if !rule.description.is_empty() {
            rule.description.clone()
        } else if rule.tool_name.is_empty() {
            "all tools".into()
        } else {
            format!("tool '{tool_name}'")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_subjects_keep_their_form() {
        let subject = RuleSubject::from("Read(./.env)".to_string());
        assert_eq!(subject.forms, vec!["Read(./.env)"]);
        assert!(subject.opaque.is_none());
        assert_ne!(subject, RuleSubject::default());
    }

    #[test]
    fn exemptions_report_whether_they_are_empty() {
        assert!(ToolRuleExemptions::NONE.is_empty());
        assert!(
            !ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                suppresses_always_allow_rule: false,
            }
            .is_empty()
        );
        assert!(
            !ToolRuleExemptions {
                ignores_whole_tool_allow_rule: false,
                suppresses_always_allow_rule: true,
            }
            .is_empty()
        );
    }

    #[test]
    fn setting_the_mode_replaces_it() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.set_mode(PermissionMode::Plan);
        assert_eq!(checker.mode(), PermissionMode::Plan);
    }

    #[test]
    fn only_editing_tools_are_edit_tools() {
        let checker = PermissionChecker::new(PermissionMode::Default);
        for tool in ["write", "edit", "multi_edit", "notebook_edit", "Write"] {
            assert!(checker.is_edit_tool(tool), "{tool}");
        }
        for tool in ["read", "grep", "bash", "glob", "list_dir"] {
            assert!(!checker.is_edit_tool(tool), "{tool}");
        }
    }

    #[test]
    fn a_dot_prefixed_pattern_has_both_spellings() {
        let cwd = Path::new("/proj");
        let context = RuleCompileContext { cwd, home: None };
        let variants = path_pattern_variants("./docs/**", &context);
        assert!(variants.contains(&"./docs/**".to_string()), "{variants:?}");
        assert!(variants.contains(&"docs/**".to_string()), "{variants:?}");
        assert!(
            variants.iter().any(|variant| variant.starts_with("/proj/")),
            "{variants:?}"
        );
        assert!(
            path_pattern_variants("/", &context).len() == 1,
            "a bare slash must not be rewritten"
        );
    }

    #[test]
    fn add_spec_installs_the_rule() {
        let cwd = Path::new("/proj");
        let context = RuleCompileContext { cwd, home: None };
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        let mut add = |spec: PermissionRuleSpec| {
            checker.add_spec(&spec, &context).unwrap();
        };
        add(PermissionRuleSpec::parse("Bash(echo:*)", PermissionAction::Allow).unwrap());
        assert_eq!(checker.rule_specs().len(), 1);
        assert!(matches!(
            checker.check_with_subjects("Bash", &[RuleSubject::single("echo hi")], None),
            PermissionResult::Allow
        ));
    }

    #[test]
    fn add_specs_is_atomic() {
        let cwd = Path::new("/proj");
        let context = RuleCompileContext { cwd, home: None };
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        let good = PermissionRuleSpec::parse("Bash(echo:*)", PermissionAction::Allow).unwrap();
        // A glob that cannot compile makes the whole batch fail.
        let bad = PermissionRuleSpec {
            tool_name: "Read".into(),
            action: PermissionAction::Allow,
            matcher: RuleMatcher::Glob {
                patterns: vec!["[".into()],
            },
            description: "Read([)".into(),
        };
        assert!(checker.add_specs(&[good, bad], &context).is_err());
        assert!(
            checker.rule_specs().is_empty(),
            "partial batch was installed"
        );
    }

    #[test]
    fn a_whole_tool_allow_still_fails_closed_on_an_opaque_command() {
        let cwd = Path::new("/proj");
        let context = RuleCompileContext { cwd, home: None };
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker
            .add_spec(
                &PermissionRuleSpec::parse("Bash", PermissionAction::Allow).unwrap(),
                &context,
            )
            .unwrap();
        assert!(matches!(
            checker.check_with_subjects("Bash", &[RuleSubject::single("echo hi")], None),
            PermissionResult::Allow
        ));
        let opaque = RuleSubject::single("sudo rm -rf /").with_opaque("wrapped");
        assert!(matches!(
            checker.check_with_subjects("Bash", std::slice::from_ref(&opaque), None),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn an_exempt_tool_cannot_use_a_whole_tool_rule_to_cover_segments() {
        let cwd = Path::new("/proj");
        let context = RuleCompileContext { cwd, home: None };
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker
            .add_spec(
                &PermissionRuleSpec::parse("Bash", PermissionAction::Allow).unwrap(),
                &context,
            )
            .unwrap();
        let exemptions = ToolRuleExemptions {
            ignores_whole_tool_allow_rule: true,
            suppresses_always_allow_rule: false,
        };
        assert!(matches!(
            checker.check_with_subjects(
                "Bash",
                &[RuleSubject::single("echo hi")],
                Some(exemptions)
            ),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn bypass_mode_allows_everything() {
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Allow
        ));
        assert!(matches!(
            checker.check("write", None),
            PermissionResult::Allow
        ));
    }

    #[test]
    fn is_allowed_only_for_preapproved_results() {
        // The two genuinely pre-approved results are allowed.
        assert!(PermissionResult::Allow.is_allowed());
        assert!(
            PermissionResult::AllowForPath {
                patterns: vec!["/tmp/**".into()],
            }
            .is_allowed()
        );

        // The remaining results all mean "cannot proceed unattended".
        assert!(
            !PermissionResult::Ask {
                reason: "needs approval".into()
            }
            .is_allowed()
        );
        assert!(
            !PermissionResult::Deny {
                reason: "no".into()
            }
            .is_allowed()
        );
        assert!(
            !PermissionResult::Elevated {
                reason: "runs as another user".into()
            }
            .is_allowed()
        );
    }

    #[test]
    fn dontask_mode_denies_everything() {
        let checker = PermissionChecker::new(PermissionMode::DontAsk);
        assert!(matches!(
            checker.check("read", None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn plan_mode_allows_only_readonly_tools() {
        let checker = PermissionChecker::new(PermissionMode::Plan);
        assert!(matches!(
            checker.check("read", None),
            PermissionResult::Allow
        ));
        assert!(matches!(
            checker.check("grep", None),
            PermissionResult::Allow
        ));
        // Directory inspection is read-only and is what a plan-mode model
        // reaches for first.
        assert!(matches!(
            checker.check("CurrentDir", None),
            PermissionResult::Allow
        ));
        assert!(matches!(
            checker.check("ListDir", None),
            PermissionResult::Allow
        ));
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Deny { .. }
        ));
        assert!(matches!(
            checker.check("write", None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn dangerous_tools_require_approval_in_default_mode() {
        let checker = PermissionChecker::new(PermissionMode::Default);
        assert!(matches!(
            checker.check("read", None),
            PermissionResult::Allow
        ));
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
        assert!(matches!(
            checker.check("write", None),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn path_allowlist_allows_matching_paths() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker
            .add_rule(PermissionRule {
                tool_name: "write".into(),
                action: PermissionAction::Allow,
                paths: vec!["/tmp/**".into()],
                description: "allow writes to /tmp".into(),
            })
            .unwrap();

        assert!(matches!(
            checker.check("write", Some(Path::new("/tmp/foo.txt"))),
            PermissionResult::AllowForPath { .. }
        ));
        assert!(matches!(
            checker.check("write", Some(Path::new("/etc/passwd"))),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn path_deny_rules_have_priority() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker
            .add_rule(PermissionRule {
                tool_name: "bash".into(),
                action: PermissionAction::Deny,
                paths: vec!["**/node_modules/**".into()],
                description: "no bash in node_modules".into(),
            })
            .unwrap();

        assert!(matches!(
            checker.check("bash", Some(Path::new("/proj/node_modules/foo"))),
            PermissionResult::Deny { .. }
        ));
        // Outside node_modules, bash still needs approval (dangerous)
        assert!(matches!(
            checker.check("bash", Some(Path::new("/proj/src"))),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn session_approvals_cached() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.record_session_approval("bash".into(), true);
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Allow
        ));

        checker.clear_session_approvals();
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn strip_dangerous_tools_removes_blocked() {
        let checker = PermissionChecker::new(PermissionMode::Plan);
        let tools = ["read", "grep", "bash", "write"];
        let (allowed, denied) = checker.strip_dangerous_tools(tools.iter().map(|s| *s));

        assert_eq!(allowed, vec!["read", "grep"]);
        assert_eq!(denied.len(), 2);
        assert!(denied.iter().any(|(name, _)| *name == "bash"));
        assert!(denied.iter().any(|(name, _)| *name == "write"));
    }

    #[test]
    fn glob_pattern_error_for_invalid_syntax() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        let err = checker
            .add_rule(PermissionRule {
                tool_name: "test".into(),
                action: PermissionAction::Allow,
                paths: vec!["[invalid".into()],
                description: String::new(),
            })
            .unwrap_err();
        assert!(matches!(err, PermissionError::InvalidGlob { .. }));
    }

    #[test]
    fn default_mode_auto_for_safe_tools() {
        let checker = PermissionChecker::new(PermissionMode::Default);
        // Safe tools should be allowed
        assert!(matches!(
            checker.check("read", None),
            PermissionResult::Allow
        ));
        assert!(matches!(checker.check("ls", None), PermissionResult::Allow));
        assert!(matches!(
            checker.check("grep", None),
            PermissionResult::Allow
        ));
    }

    #[test]
    fn auto_mode_asks_for_dangerous_tools() {
        let checker = PermissionChecker::new(PermissionMode::Auto);
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
    }

    /// `Auto` is not a behaviour in this checker: it is `Default` plus the
    /// `AutoClassifierHook` the CLI registers. If the checker ever grows an
    /// auto behaviour of its own, this equality breaks — which is the point.
    #[test]
    fn auto_mode_is_indistinguishable_from_default_in_the_checker() {
        let auto = PermissionChecker::new(PermissionMode::Auto);
        let default = PermissionChecker::new(PermissionMode::Default);
        for tool in [
            "read",
            "ls",
            "grep",
            "bash",
            "write",
            "edit",
            "multi_edit",
            "notebook_edit",
        ] {
            assert_eq!(
                auto.check(tool, None),
                default.check(tool, None),
                "checker must treat Auto and Default identically for '{tool}'"
            );
        }
    }

    #[test]
    fn tool_specific_rule_overrides_general() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        // General deny for all tools on certain path
        checker
            .add_rule(PermissionRule {
                tool_name: String::new(),
                action: PermissionAction::Deny,
                paths: vec!["/etc/**".into()],
                description: "block /etc access".into(),
            })
            .unwrap();
        // Specific allow for read
        checker
            .add_rule(PermissionRule {
                tool_name: "read".into(),
                action: PermissionAction::Allow,
                paths: vec!["/etc/passwd".into()],
                description: String::new(),
            })
            .unwrap();

        // Deny has priority, so even read to /etc/passwd should be denied
        // because the deny rule is checked first
        assert!(matches!(
            checker.check("read", Some(Path::new("/etc/passwd"))),
            PermissionResult::Deny { .. }
        ));
    }

    /// A whole-tool allow rule normally approves the tool outright.
    fn whole_tool_allow(tool: &str) -> PermissionRule {
        PermissionRule {
            tool_name: tool.into(),
            action: PermissionAction::Allow,
            paths: Vec::new(),
            description: String::new(),
        }
    }

    /// A tool refuses the blanket allow: the rule is not honoured and the
    /// decision continues down the ordinary path — here, to the ask rule that
    /// sits behind it.
    #[test]
    fn a_tool_that_ignores_whole_tool_allow_rules_is_not_approved_by_one() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        // A dangerous tool, so refusing the blanket allow falls through to the
        // ordinary "requires approval" default that the test can observe.
        checker.add_rule(whole_tool_allow("bash")).unwrap();

        // Without the flag the blanket allow stands.
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Allow
        ));

        checker.set_tool_exemptions(
            "bash",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                ..ToolRuleExemptions::NONE
            },
        );
        assert!(
            matches!(checker.check("bash", None), PermissionResult::Ask { .. }),
            "a blanket allow approved a tool that refuses blanket allows"
        );
    }

    /// The flag's only job is to neutralise an allow rule. A tool that carries
    /// it and finds no whole-tool allow rule to refuse gets exactly the default
    /// an ordinary tool gets — for a safe tool, "allow".
    ///
    /// This is the pin on the branch that used to sit at the mode default and
    /// answer `Ask` for any exempt tool. That branch made the flag "never
    /// auto-approved", which the reference does not do: there, `bypassPermissions`
    /// and the ordinary default are reached before — and regardless of — the flag.
    #[test]
    fn an_exempt_tool_with_no_matching_allow_rule_gets_the_ordinary_mode_default() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.set_tool_exemptions(
            "artifact",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                ..ToolRuleExemptions::NONE
            },
        );

        assert!(
            matches!(checker.check("artifact", None), PermissionResult::Allow),
            "an exempt tool nothing refused must still get the ordinary default: {:?}",
            checker.check("artifact", None)
        );

        // A dangerous tool keeps the ordinary dangerous default, with the
        // ordinary reason rather than one invented for the exemption.
        checker.set_tool_exemptions(
            "bash",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                ..ToolRuleExemptions::NONE
            },
        );
        match checker.check("bash", None) {
            PermissionResult::Ask { reason } => {
                assert!(reason.contains("dangerous operation"), "{reason}");
            }
            other => panic!("expected the ordinary approval question, got {other:?}"),
        }
    }

    /// The mode bypass runs before the rules and before the exemptions, so an
    /// exempt tool is still approved wholesale in `bypassPermissions`. The
    /// exemption is not a floor under the mode.
    #[test]
    fn mode_bypass_is_checked_before_exemptions() {
        let mut checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        checker.set_tool_exemptions(
            "artifact",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                suppresses_always_allow_rule: true,
            },
        );
        assert!(matches!(
            checker.check("artifact", None),
            PermissionResult::Allow
        ));
    }

    /// A caller holding the call's arguments answers the flags itself, and what
    /// it passes wins over the static table — which is how a per-input answer
    /// reaches the checker.
    ///
    /// The observable difference is which rules a call reaches: a refused
    /// allow rule falls through to the ask rule behind it. (Without that second
    /// rule both answers end at the same default, since "artifact" is not a
    /// dangerous name and the ordinary default is "allow".)
    #[test]
    fn exemptions_resolved_for_a_call_override_the_static_table() {
        let exemptions = ToolRuleExemptions {
            ignores_whole_tool_allow_rule: true,
            ..ToolRuleExemptions::NONE
        };
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.add_rule(whole_tool_allow("bash")).unwrap();

        // This call carries nothing, so the whole-tool allow stands.
        assert!(matches!(
            checker.check_with_exemptions("bash", None, Some(ToolRuleExemptions::NONE)),
            PermissionResult::Allow
        ));
        // This call refuses the allow; the dangerous default decides.
        match checker.check_with_exemptions("bash", None, Some(exemptions)) {
            PermissionResult::Ask { reason } => {
                assert!(reason.contains("requires approval"), "{reason}");
            }
            other => panic!("a refused allow rule must not approve the call, got {other:?}"),
        }

        // Nothing resolved for it: the static table is used, and it declares
        // nothing — which is the pre-resolution behaviour.
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Allow
        ));

        // Now declare it statically: the plain check refuses the rule, and the
        // per-call answer overrides that back to "nothing to refuse".
        checker.set_tool_exemptions("bash", exemptions);
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
        assert!(matches!(
            checker.check_with_exemptions("bash", None, Some(ToolRuleExemptions::NONE)),
            PermissionResult::Allow
        ));
    }

    /// Deny keeps winning. The exemptions narrow allow rules only.
    #[test]
    fn an_exempt_tool_cannot_escape_a_deny() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.add_rule(whole_tool_allow("agent")).unwrap();
        checker
            .add_rule(PermissionRule {
                tool_name: "agent".into(),
                action: PermissionAction::Deny,
                paths: Vec::new(),
                description: "no sub-agents here".into(),
            })
            .unwrap();
        checker.set_tool_exemptions(
            "agent",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                suppresses_always_allow_rule: true,
            },
        );

        match checker.check("agent", None) {
            PermissionResult::Deny { reason } => assert!(reason.contains("sub-agents"), "{reason}"),
            other => panic!("deny must win over exemptions, got {other:?}"),
        }
    }

    /// A path-scoped allow is not a blanket allow: it still applies, because
    /// the flag is about approving a tool *anywhere*.
    #[test]
    fn an_exempt_tool_is_still_covered_by_a_path_scoped_allow() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker
            .add_rule(PermissionRule {
                tool_name: "agent".into(),
                action: PermissionAction::Allow,
                paths: vec!["/tmp/**".into()],
                description: String::new(),
            })
            .unwrap();
        checker.set_tool_exemptions(
            "agent",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                ..ToolRuleExemptions::NONE
            },
        );

        assert!(matches!(
            checker.check("agent", Some(Path::new("/tmp/work"))),
            PermissionResult::AllowForPath { .. }
        ));
    }

    #[test]
    fn exemptions_are_cleared_by_declaring_none() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.set_tool_exemptions(
            "agent",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                ..ToolRuleExemptions::NONE
            },
        );
        assert!(
            checker
                .tool_exemptions("Agent")
                .ignores_whole_tool_allow_rule
        );

        checker.set_tool_exemptions("agent", ToolRuleExemptions::NONE);
        assert_eq!(checker.tool_exemptions("agent"), ToolRuleExemptions::NONE);
        // A tool that declares nothing is unaffected, by name or by case.
        assert_eq!(checker.tool_exemptions("bash"), ToolRuleExemptions::NONE);
    }

    #[test]
    fn enter_plan_mode_saves_and_restores_mode() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.enter_plan_mode();
        assert!(matches!(checker.mode(), PermissionMode::Plan));
        // Plan mode denies write/bash
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Deny { .. }
        ));

        let restored = checker.exit_plan_mode();
        assert!(restored.is_some());
        assert!(matches!(restored.unwrap(), PermissionMode::Default));
        // Restored mode: dangerous tools require approval
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn exit_plan_mode_without_enter_returns_none() {
        let mut checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let restored = checker.exit_plan_mode();
        assert!(restored.is_none());
        // Mode unchanged
        assert!(matches!(checker.mode(), PermissionMode::BypassPermissions));
    }

    #[test]
    fn enter_plan_mode_preserves_original_mode() {
        let mut checker = PermissionChecker::new(PermissionMode::Auto);
        checker.enter_plan_mode();
        assert!(matches!(checker.mode(), PermissionMode::Plan));
        checker.exit_plan_mode();
        assert!(matches!(checker.mode(), PermissionMode::Auto));
    }

    #[test]
    fn permission_mode_from_str() {
        assert!(matches!(
            "default".parse::<PermissionMode>().unwrap(),
            PermissionMode::Default
        ));
        assert!(matches!(
            "accept-edits".parse::<PermissionMode>().unwrap(),
            PermissionMode::AcceptEdits
        ));
        assert!(matches!(
            "bypass-permissions".parse::<PermissionMode>().unwrap(),
            PermissionMode::BypassPermissions
        ));
        assert!(matches!(
            "dont-ask".parse::<PermissionMode>().unwrap(),
            PermissionMode::DontAsk
        ));
        assert!(matches!(
            "plan".parse::<PermissionMode>().unwrap(),
            PermissionMode::Plan
        ));
        assert!(matches!(
            "auto".parse::<PermissionMode>().unwrap(),
            PermissionMode::Auto
        ));
        assert!("invalid".parse::<PermissionMode>().is_err());
    }

    #[test]
    fn permission_mode_display() {
        assert_eq!(PermissionMode::Default.to_string(), "default");
        assert_eq!(PermissionMode::AcceptEdits.to_string(), "accept-edits");
        assert_eq!(
            PermissionMode::BypassPermissions.to_string(),
            "bypass-permissions"
        );
        assert_eq!(PermissionMode::DontAsk.to_string(), "dont-ask");
        assert_eq!(PermissionMode::Plan.to_string(), "plan");
        assert_eq!(PermissionMode::Auto.to_string(), "auto");
    }

    #[test]
    fn permission_mode_roundtrip() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
            PermissionMode::Plan,
            PermissionMode::Auto,
        ] {
            let s = mode.to_string();
            let parsed: PermissionMode = s.parse().unwrap();
            assert_eq!(parsed, mode);
        }
    }
}

#[cfg(test)]
mod unapproved_tool_message_tests {
    use super::PermissionResult;

    #[test]
    fn message_names_the_tool_and_the_missing_channel() {
        let msg = PermissionResult::unapproved_tool_message("Bash", "writes outside the worktree");
        assert!(msg.contains("Bash"), "{msg}");
        assert!(msg.contains("no confirmation channel"), "{msg}");
        assert!(msg.contains("writes outside the worktree"), "{msg}");
    }

    /// The distinction this message exists to preserve: when the user really
    /// was asked and refused, the engine resolves the ask itself and says
    /// "denied by user". An unresolved ask must not borrow that wording.
    #[test]
    fn does_not_claim_a_denial_by_a_person() {
        let msg = PermissionResult::unapproved_tool_message("Bash", "needs approval");
        assert!(
            !msg.contains("denied by user"),
            "an unasked tool must not report a user denial: {msg}"
        );
    }
}

#[cfg(test)]
mod rule_spec_tests {
    use super::*;

    fn ctx<'a>(cwd: &'a Path, home: Option<&'a Path>) -> RuleCompileContext<'a> {
        RuleCompileContext { cwd, home }
    }

    fn parsed(raw: &str, action: PermissionAction) -> PermissionRuleSpec {
        PermissionRuleSpec::parse(raw, action).unwrap_or_else(|error| panic!("{raw}: {error}"))
    }

    fn checker_with(
        specs: &[PermissionRuleSpec],
        context: &RuleCompileContext<'_>,
    ) -> PermissionChecker {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.add_specs(specs, context).unwrap();
        checker
    }

    #[test]
    fn parses_whole_tool_and_bash_forms() {
        assert_eq!(
            parsed("Bash", PermissionAction::Allow).matcher,
            RuleMatcher::WholeTool
        );
        assert_eq!(
            parsed("Bash(git diff:*)", PermissionAction::Deny).matcher,
            RuleMatcher::BashPrefix {
                prefix: "git diff".into()
            }
        );
        assert!(
            PermissionRuleSpec::parse("Bash()", PermissionAction::Allow).is_err(),
            "an empty specifier is not a whole-tool rule"
        );
        assert!(
            PermissionRuleSpec::parse("Bash( )", PermissionAction::Allow).is_err(),
            "a whitespace-only specifier is empty after trimming"
        );
        assert!(
            PermissionRuleSpec::parse("Bad Tool(./x)", PermissionAction::Allow).is_err(),
            "a tool name with whitespace is not a rule"
        );
        assert!(
            PermissionRuleSpec::parse("Bad)(./x)", PermissionAction::Allow).is_err(),
            "a tool name with a closing parenthesis is not a rule"
        );
        assert_eq!(
            parsed("Bash(*)", PermissionAction::Allow).matcher,
            RuleMatcher::BashGlob {
                pattern: "*".into()
            }
        );
        assert_eq!(
            parsed("Bash(ls *.rs)", PermissionAction::Allow).matcher,
            RuleMatcher::BashGlob {
                pattern: "ls *.rs".into()
            }
        );
        assert_eq!(
            parsed("Read(./.env)", PermissionAction::Deny).matcher,
            RuleMatcher::Glob {
                patterns: vec!["./.env".into()]
            }
        );
        assert_eq!(
            PermissionRuleSpec::parse("Bash(git diff:*)", PermissionAction::Ask)
                .unwrap()
                .description,
            "Bash(git diff:*)"
        );
    }

    #[test]
    fn powershell_rules_use_command_semantics() {
        assert_eq!(
            parsed("PowerShell(Get-ChildItem:*)", PermissionAction::Allow).matcher,
            RuleMatcher::BashPrefix {
                prefix: "Get-ChildItem".into()
            }
        );
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(
            &[parsed(
                "PowerShell(Get-ChildItem:*)",
                PermissionAction::Allow,
            )],
            &context,
        );
        assert!(matches!(
            checker.check_with_subjects(
                "PowerShell",
                &[RuleSubject::single("Get-ChildItem C:\\work")],
                None
            ),
            PermissionResult::Allow
        ));

        let opaque = RuleSubject::single("Invoke-Expression $payload")
            .with_opaque("PowerShell cmdlet can execute arbitrary commands");
        assert!(matches!(
            checker.check_with_subjects("PowerShell", std::slice::from_ref(&opaque), None),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn rejects_malformed_rules() {
        for raw in [
            "",
            "   ",
            "(x)",
            "Bash(",
            "Bash()",
            "Bash(:*)",
            "Read([)",
            "Read(src/[)",
        ] {
            assert!(
                PermissionRuleSpec::parse(raw, PermissionAction::Allow).is_err(),
                "{raw:?} must be rejected"
            );
        }
    }

    #[test]
    fn bash_prefix_stops_at_a_word_boundary() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(
            &[parsed("Bash(git diff:*)", PermissionAction::Deny)],
            &context,
        );
        for command in ["git diff", "git diff --stat", "git diff && echo done"] {
            assert!(
                matches!(
                    checker.check_with_subjects("Bash", &[command.into()], None),
                    PermissionResult::Deny { .. }
                ),
                "{command} should match"
            );
        }
        for command in ["git differences", "git status"] {
            assert!(
                !matches!(
                    checker.check_with_subjects("Bash", &[command.into()], None),
                    PermissionResult::Deny { .. }
                ),
                "{command} should not match"
            );
        }
    }

    #[test]
    fn bash_glob_matches_the_whole_command() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(&[parsed("Bash(*)", PermissionAction::Allow)], &context);
        assert!(matches!(
            checker.check_with_subjects("Bash", &["rm -rf /tmp/x".into()], None),
            PermissionResult::Allow
        ));

        let exact = checker_with(
            &[parsed("Bash(git diff)", PermissionAction::Deny)],
            &context,
        );
        assert!(matches!(
            exact.check_with_subjects("Bash", &["git diff".into()], None),
            PermissionResult::Deny { .. }
        ));
        assert!(!matches!(
            exact.check_with_subjects("Bash", &["git diff --stat".into()], None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn path_rules_normalize_relative_home_and_absolute_forms() {
        let context = ctx(Path::new("/proj"), Some(Path::new("/home/u")));

        for (rule, subject) in [
            ("Read(./src/**)", "src/lib.rs"),
            ("Read(./src/**)", "/proj/src/lib.rs"),
            ("Read(src/**)", "src/a/b.rs"),
            ("Read(src/**)", "/proj/src/a/b.rs"),
            ("Read(~/secrets/**)", "/home/u/secrets/token"),
            ("Read(//tmp/**)", "/tmp/x"),
            ("Read(/etc/**)", "/etc/hosts"),
        ] {
            let checker = checker_with(&[parsed(rule, PermissionAction::Deny)], &context);
            assert!(
                matches!(
                    checker.check_with_subjects("Read", &[subject.into()], None),
                    PermissionResult::Deny { .. }
                ),
                "{rule} should match {subject}"
            );
        }
    }

    #[test]
    fn path_star_stops_at_a_separator_and_double_star_crosses_it() {
        let context = ctx(Path::new("/proj"), None);
        let single = checker_with(&[parsed("Read(src/*)", PermissionAction::Deny)], &context);
        assert!(matches!(
            single.check_with_subjects("Read", &["src/lib.rs".into()], None),
            PermissionResult::Deny { .. }
        ));
        assert!(!matches!(
            single.check_with_subjects("Read", &["src/a/lib.rs".into()], None),
            PermissionResult::Deny { .. }
        ));

        let recursive = checker_with(&[parsed("Read(src/**)", PermissionAction::Deny)], &context);
        assert!(matches!(
            recursive.check_with_subjects("Read", &["src/a/lib.rs".into()], None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn a_cwd_with_glob_metacharacters_still_compiles() {
        let context = ctx(Path::new("/pro[ject]*"), None);
        let checker = checker_with(&[parsed("Read(./x)", PermissionAction::Deny)], &context);
        assert!(matches!(
            checker.check_with_subjects("Read", &["/pro[ject]*/x".into()], None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn any_subject_may_match_a_rule() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(
            &[parsed("Write(./a.txt)", PermissionAction::Deny)],
            &context,
        );
        assert!(matches!(
            checker.check_with_subjects("Write", &["b.txt".into(), "/proj/a.txt".into()], None,),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn ask_beats_allow_and_deny_beats_both() {
        let context = ctx(Path::new("/proj"), None);
        let allow = parsed("Bash(echo:*)", PermissionAction::Allow);
        let ask = parsed("Bash(echo hi:*)", PermissionAction::Ask);
        let deny = parsed("Bash(echo hi:*)", PermissionAction::Deny);

        let ask_over_allow = checker_with(&[allow.clone(), ask], &context);
        assert!(matches!(
            ask_over_allow.check_with_subjects("Bash", &["echo hi".into()], None),
            PermissionResult::Ask { .. }
        ));

        let deny_over_all = checker_with(&[allow, deny], &context);
        assert!(matches!(
            deny_over_all.check_with_subjects("Bash", &["echo hi".into()], None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn a_deny_rule_applies_in_plan_and_accept_edits_modes() {
        let context = ctx(Path::new("/proj"), None);
        let mut checker = PermissionChecker::new(PermissionMode::Plan);
        checker
            .add_specs(&[parsed("Read(./.env)", PermissionAction::Deny)], &context)
            .unwrap();
        assert!(matches!(
            checker.check_with_subjects("Read", &[".env".into()], None),
            PermissionResult::Deny { .. }
        ));

        let mut edits = PermissionChecker::new(PermissionMode::AcceptEdits);
        edits
            .add_specs(
                &[parsed("Write(./secret.txt)", PermissionAction::Deny)],
                &context,
            )
            .unwrap();
        assert!(matches!(
            edits.check_with_subjects("Write", &["./secret.txt".into()], None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn a_whole_tool_allow_only_matches_its_tool() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(&[parsed("Artifact", PermissionAction::Allow)], &context);
        assert!(
            !checker
                .check_with_subjects("Write", &[RuleSubject::single("x")], None)
                .is_allowed(),
            "an allow for one tool approved another"
        );
    }

    #[test]
    fn an_opaque_command_is_never_auto_approved() {
        let context = ctx(Path::new("/proj"), None);
        let opaque = RuleSubject::single("sudo rm -rf /tmp/x").with_opaque("sudo wrapper");

        let whole = checker_with(&[parsed("Bash", PermissionAction::Allow)], &context);
        assert!(
            matches!(
                whole.check_with_subjects("Bash", std::slice::from_ref(&opaque), None),
                PermissionResult::Ask { .. }
            ),
            "a whole-tool allow approved an opaque command"
        );

        let specifier = checker_with(&[parsed("Bash(sudo:*)", PermissionAction::Allow)], &context);
        match specifier.check_with_subjects("Bash", std::slice::from_ref(&opaque), None) {
            PermissionResult::Ask { reason } => {
                assert!(reason.contains("cannot be fully analyzed"), "{reason}");
            }
            other => panic!("an opaque command was auto-approved: {other:?}"),
        }

        let deny = checker_with(&[parsed("Bash(sudo:*)", PermissionAction::Deny)], &context);
        assert!(matches!(
            deny.check_with_subjects("Bash", std::slice::from_ref(&opaque), None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn a_whole_tool_deny_applies_even_without_subjects() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(&[parsed("Bash", PermissionAction::Deny)], &context);
        assert!(matches!(
            checker.check("Bash", None),
            PermissionResult::Deny { .. }
        ));
        let ask = checker_with(&[parsed("Bash", PermissionAction::Ask)], &context);
        assert!(matches!(
            ask.check("Bash", None),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn an_allow_rule_must_cover_every_command_segment() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(&[parsed("Bash(git:*)", PermissionAction::Allow)], &context);

        let both_git = [
            RuleSubject::single("git status"),
            RuleSubject::single("git diff"),
        ];
        assert!(matches!(
            checker.check_with_subjects("Bash", &both_git, None),
            PermissionResult::Allow
        ));

        let mixed = [
            RuleSubject::single("git status"),
            RuleSubject::single("rm -rf /tmp/x"),
        ];
        assert!(
            !checker
                .check_with_subjects("Bash", &mixed, None)
                .is_allowed(),
            "a rule covering only one segment approved the whole compound command"
        );
    }

    #[test]
    fn leading_environment_assignments_do_not_hide_the_command() {
        let context = ctx(Path::new("/proj"), None);
        let prefix = checker_with(&[parsed("Bash(rm:*)", PermissionAction::Deny)], &context);
        assert!(matches!(
            prefix.check_with_subjects("Bash", &["FOO=1 rm -rf /tmp/x".into()], None),
            PermissionResult::Deny { .. }
        ));

        let exact = checker_with(
            &[parsed("Bash(rm -rf /tmp/x)", PermissionAction::Deny)],
            &context,
        );
        assert!(matches!(
            exact.check_with_subjects("Bash", &["FOO=1 rm -rf /tmp/x".into()], None),
            PermissionResult::Deny { .. }
        ));

        assert!(matches!(
            prefix.check_with_subjects("Bash", &[r#"FOO="a b" rm -rf /tmp/x"#.into()], None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn separate_allow_rules_can_cover_a_compound_command() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(
            &[
                parsed("Bash(cd:*)", PermissionAction::Allow),
                parsed("Bash(echo:*)", PermissionAction::Allow),
            ],
            &context,
        );
        let subjects = [
            RuleSubject::single("cd /tmp"),
            RuleSubject::single("echo chained-ok"),
        ];
        assert!(matches!(
            checker.check_with_subjects("Bash", &subjects, None),
            PermissionResult::Allow
        ));
    }

    #[test]
    fn a_deny_or_ask_rule_matches_any_segment() {
        let context = ctx(Path::new("/proj"), None);
        let deny = checker_with(&[parsed("Bash(rm:*)", PermissionAction::Deny)], &context);
        let subjects = [
            RuleSubject::single("git status"),
            RuleSubject::single("rm -rf /tmp/x"),
        ];
        assert!(matches!(
            deny.check_with_subjects("Bash", &subjects, None),
            PermissionResult::Deny { .. }
        ));

        let ask = checker_with(&[parsed("Bash(rm:*)", PermissionAction::Ask)], &context);
        assert!(matches!(
            ask.check_with_subjects("Bash", &subjects, None),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn a_path_allow_must_cover_every_file() {
        let context = ctx(Path::new("/proj"), None);
        let checker = checker_with(
            &[parsed("Write(./src/**)", PermissionAction::Allow)],
            &context,
        );

        assert!(matches!(
            checker.check_with_subjects("Write", &[RuleSubject::single("/proj/src/a.rs")], None,),
            PermissionResult::AllowForPath { .. }
        ));
        assert!(
            !checker
                .check_with_subjects(
                    "Write",
                    &[
                        RuleSubject::single("/proj/src/a.rs"),
                        RuleSubject::single("/etc/passwd"),
                    ],
                    None,
                )
                .is_allowed(),
            "a rule covering one file approved writing another"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_path_is_matched_by_its_real_location() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let link = root.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("secret.txt"), "hidden").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let context = ctx(root.path(), None);
        let checker = checker_with(
            &[parsed("Read(./real/**)", PermissionAction::Deny)],
            &context,
        );
        let canonical = std::fs::canonicalize(&real)
            .unwrap()
            .join("secret.txt")
            .to_string_lossy()
            .to_string();
        let subject = RuleSubject::new(vec![
            link.join("secret.txt").to_string_lossy().to_string(),
            canonical,
        ]);
        assert!(
            matches!(
                checker.check_with_subjects("Read", &[subject], None),
                PermissionResult::Deny { .. }
            ),
            "a symlinked path bypassed the deny rule"
        );
    }

    #[test]
    fn rule_specs_round_trip_for_sub_agents() {
        let context = ctx(Path::new("/proj"), None);
        let specs = vec![
            parsed("Bash(rm:*)", PermissionAction::Deny),
            parsed("Read(./x)", PermissionAction::Ask),
        ];
        let checker = checker_with(&specs, &context);
        assert_eq!(checker.rule_specs(), specs);
    }
}
