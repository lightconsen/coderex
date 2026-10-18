//! Permission system — modes, rules, path matching, and policy evaluation.

use globset::{Glob, GlobMatcher};
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
    /// The operation requires user confirmation, but the answer
    /// should be cached for the rest of the session.
    ///
    /// Nothing in [`PermissionChecker`] constructs this variant: a
    /// session-wide approval is recorded with
    /// [`PermissionChecker::record_session_approval`] once the user has
    /// answered. It remains part of the public result type because callers
    /// still carry it through their match arms. It always means "no answer
    /// yet", so [`Self::is_allowed`] is false for it.
    AskForSession {
        /// Human-readable reason for the prompt.
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
    ///
    /// `AskForSession` is deliberately not allowed: it means "ask, then cache
    /// the answer", so no answer exists yet. Treating it as allowed would let
    /// a caller that trusts this predicate skip the prompt entirely.
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

/// Pre-compiled path matcher for a rule.
struct CompiledRule {
    tool_name: String,
    action: PermissionAction,
    path_matchers: Vec<GlobMatcher>,
    description: String,
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
    pub fn enter_plan_mode(&mut self) {
        self.saved_mode = Some(self.mode);
        self.mode = PermissionMode::Plan;
    }

    /// Restore the mode saved before entering Plan mode.
    ///
    /// Returns the restored mode, or `None` if no mode was saved.
    pub fn exit_plan_mode(&mut self) -> Option<PermissionMode> {
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

        self.rules.push(CompiledRule {
            tool_name: rule.tool_name,
            action: rule.action,
            path_matchers,
            description: rule.description,
        });
        Ok(())
    }

    /// Check if a tool operation is allowed.
    ///
    /// Evaluates in order: mode → session cache → rules → dangerous tool defaults.
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
        // Mode-level shortcuts
        if self.mode.bypasses_all() {
            return PermissionResult::Allow;
        }
        if self.mode.denies_all() {
            return PermissionResult::Deny {
                reason: "permission checks disabled — auto-deny".into(),
            };
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

        // Session-level cache
        if let Some(&approved) = self.session_approvals.get(tool_name) {
            if approved {
                return PermissionResult::Allow;
            }
        }

        // Rule matching: most specific (tool+path) wins, then tool-only, then default
        let path_str = path.map(|p| p.to_string_lossy().to_string());
        let exemptions = exemptions.unwrap_or_else(|| self.tool_exemptions(tool_name));

        // Check for explicit deny rules first (deny takes priority). Nothing
        // below — including a tool's own exemptions — can turn a deny into an
        // allow, which is why this loop runs before any of them.
        for rule in &self.rules {
            if rule.action == PermissionAction::Deny
                && Self::rule_matches(rule, tool_name, path_str.as_deref())
            {
                return PermissionResult::Deny {
                    reason: Self::rule_reason(rule, tool_name),
                };
            }
        }

        // Check for explicit allow rules
        for rule in &self.rules {
            if rule.action == PermissionAction::Allow
                && Self::rule_matches(rule, tool_name, path_str.as_deref())
            {
                if rule.path_matchers.is_empty() {
                    // A whole-tool allow. A runner that refuses the shortcut
                    // does not get it, and the next rule — or the default
                    // below — decides instead.
                    if exemptions.ignores_whole_tool_allow_rule {
                        continue;
                    }
                    return PermissionResult::Allow;
                }
                return PermissionResult::AllowForPath {
                    patterns: rule
                        .path_matchers
                        .iter()
                        .map(|m| m.glob().to_string())
                        .collect(),
                };
            }
        }

        // Check for ask rules
        for rule in &self.rules {
            if rule.action == PermissionAction::Ask
                && Self::rule_matches(rule, tool_name, path_str.as_deref())
            {
                return PermissionResult::Ask {
                    reason: Self::rule_reason(rule, tool_name),
                };
            }
        }

        // Default: dangerous tools require approval, safe tools allowed.
        //
        // `Auto` takes this same path. The checker has no auto behaviour of its
        // own — see `PermissionMode::Auto`; anything "auto" about the mode comes
        // from the classifier hook the CLI registers ahead of the checker.
        //
        // A tool's exemptions are not consulted here. Their only job is to
        // refuse a whole-tool allow rule that matched (above); a tool that
        // carries them and finds no such rule reaches this ordinary default
        // like any other tool. Asking here instead would make the flag mean
        // "never auto-approved", which is not what it is — being a no-op when
        // no rule matched is the intended design.
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
                        | PermissionResult::AskForSession { reason }
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
                | "grep"
                | "glob"
                | "web_search"
                | "web_fetch"
                | "grep_code"
                | "file_search"
                | "notebook_read"
        )
    }

    /// Check if a rule matches the given tool and path.
    fn rule_matches(rule: &CompiledRule, tool_name: &str, path: Option<&str>) -> bool {
        let name_matches =
            rule.tool_name.is_empty() || rule.tool_name.eq_ignore_ascii_case(tool_name);
        if !name_matches {
            return false;
        }

        // If the rule has path patterns, at least one must match
        if rule.path_matchers.is_empty() {
            return true;
        }

        let Some(path) = path else {
            return false;
        };

        rule.path_matchers
            .iter()
            .any(|matcher| matcher.is_match(path))
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
    fn ask_for_session_is_not_allowed_until_answered() {
        // `AskForSession` means "ask and cache the answer", not "approved".
        // A caller trusting `is_allowed` must not skip the prompt.
        assert!(!PermissionResult::AskForSession {
            reason: "needs approval".into()
        }
        .is_allowed());

        // The two genuinely pre-approved results stay allowed.
        assert!(PermissionResult::Allow.is_allowed());
        assert!(PermissionResult::AllowForPath {
            patterns: vec!["/tmp/**".into()],
        }
        .is_allowed());

        // The remaining results all mean "cannot proceed unattended".
        assert!(!PermissionResult::Ask {
            reason: "needs approval".into()
        }
        .is_allowed());
        assert!(!PermissionResult::Deny {
            reason: "no".into()
        }
        .is_allowed());
        assert!(!PermissionResult::Elevated {
            reason: "runs as another user".into()
        }
        .is_allowed());
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
        let tools = vec!["read", "grep", "bash", "write"];
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
            "read", "ls", "grep", "bash", "write", "edit", "multi_edit", "notebook_edit",
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
        checker.add_rule(whole_tool_allow("artifact")).unwrap();
        checker
            .add_rule(PermissionRule {
                tool_name: "artifact".into(),
                action: PermissionAction::Ask,
                paths: Vec::new(),
                description: "artifact needs a look".into(),
            })
            .unwrap();

        // Without the flag the blanket allow stands.
        assert!(matches!(
            checker.check("artifact", None),
            PermissionResult::Allow
        ));

        checker.set_tool_exemptions(
            "artifact",
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                ..ToolRuleExemptions::NONE
            },
        );
        assert!(
            matches!(checker.check("artifact", None), PermissionResult::Ask { .. }),
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
        checker.add_rule(whole_tool_allow("artifact")).unwrap();
        checker
            .add_rule(PermissionRule {
                tool_name: "artifact".into(),
                action: PermissionAction::Ask,
                paths: Vec::new(),
                description: "artifact needs a look".into(),
            })
            .unwrap();

        // This call is a read: it carries nothing, so the whole-tool allow stands.
        assert!(matches!(
            checker.check_with_exemptions("artifact", None, Some(ToolRuleExemptions::NONE)),
            PermissionResult::Allow
        ));
        // This call is a publish: it refuses the allow, so the ask behind it decides.
        match checker.check_with_exemptions("artifact", None, Some(exemptions)) {
            PermissionResult::Ask { reason } => assert!(reason.contains("needs a look"), "{reason}"),
            other => panic!("a refused allow rule must not approve the call, got {other:?}"),
        }

        // Nothing resolved for it: the static table is used, and it declares
        // nothing — which is the pre-resolution behaviour.
        assert!(matches!(
            checker.check("artifact", None),
            PermissionResult::Allow
        ));

        // Now declare it statically: the plain check refuses the rule, and the
        // per-call answer overrides that back to "nothing to refuse".
        checker.set_tool_exemptions("artifact", exemptions);
        assert!(matches!(
            checker.check("artifact", None),
            PermissionResult::Ask { .. }
        ));
        assert!(matches!(
            checker.check_with_exemptions("artifact", None, Some(ToolRuleExemptions::NONE)),
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
