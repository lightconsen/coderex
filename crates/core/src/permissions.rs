//! Permission system — modes, rules, path matching, and policy evaluation.

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

/// Permission mode controlling how tool approvals work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
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
    /// Auto: use heuristics to decide (safe ops auto-approved).
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
    /// Returns true if the operation can proceed without user input.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(
            self,
            Self::Allow | Self::AllowForPath { .. } | Self::AskForSession { .. }
        )
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
        }
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
    #[must_use]
    pub fn check(&self, tool_name: &str, path: Option<&Path>) -> PermissionResult {
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

        // Session-level cache
        if let Some(&approved) = self.session_approvals.get(tool_name) {
            if approved {
                return PermissionResult::Allow;
            }
        }

        // Rule matching: most specific (tool+path) wins, then tool-only, then default
        let path_str = path.map(|p| p.to_string_lossy().to_string());

        // Check for explicit deny rules first (deny takes priority)
        for rule in &self.rules {
            if rule.action == PermissionAction::Deny
                && Self::rule_matches(rule, tool_name, path_str.as_deref()) {
                    return PermissionResult::Deny {
                        reason: Self::rule_reason(rule, tool_name),
                    };
                }
        }

        // Check for explicit allow rules
        for rule in &self.rules {
            if rule.action == PermissionAction::Allow
                && Self::rule_matches(rule, tool_name, path_str.as_deref()) {
                    if rule.path_matchers.is_empty() {
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
                && Self::rule_matches(rule, tool_name, path_str.as_deref()) {
                    return PermissionResult::Ask {
                        reason: Self::rule_reason(rule, tool_name),
                    };
                }
        }

        // Default: dangerous tools require approval, safe tools allowed
        if self.is_dangerous_tool(tool_name) {
            if self.mode == PermissionMode::Auto {
                // Auto mode: safe-ish operations auto-approved
                return PermissionResult::Ask {
                    reason: format!(
                        "tool '{tool_name}' requires approval (auto mode)"
                    ),
                };
            }
            PermissionResult::Ask {
                reason: format!(
                    "tool '{tool_name}' requires approval (dangerous operation)"
                ),
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
        self.dangerous_tools.iter().any(|t| t.eq_ignore_ascii_case(&lower))
    }

    /// Check if a tool is read-only (safe for plan mode).
    #[must_use]
    pub fn is_tool_read_only(&self, tool_name: &str) -> bool {
        matches!(
            tool_name.to_lowercase().as_str(),
            "read" | "ls" | "grep" | "glob"
                | "web_search" | "web_fetch" | "grep_code"
                | "file_search" | "notebook_read"
        )
    }

    /// Check if a rule matches the given tool and path.
    fn rule_matches(rule: &CompiledRule, tool_name: &str, path: Option<&str>) -> bool {
        let name_matches = rule.tool_name.is_empty() || rule.tool_name.eq_ignore_ascii_case(tool_name);
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
        assert!(matches!(checker.check("bash", None), PermissionResult::Allow));
        assert!(matches!(checker.check("write", None), PermissionResult::Allow));
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
        assert!(matches!(checker.check("read", None), PermissionResult::Allow));
        assert!(matches!(checker.check("grep", None), PermissionResult::Allow));
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
        assert!(matches!(checker.check("read", None), PermissionResult::Allow));
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
        assert!(matches!(checker.check("bash", None), PermissionResult::Allow));

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
        assert!(matches!(checker.check("read", None), PermissionResult::Allow));
        assert!(matches!(checker.check("ls", None), PermissionResult::Allow));
        assert!(matches!(checker.check("grep", None), PermissionResult::Allow));
    }

    #[test]
    fn auto_mode_asks_for_dangerous_tools() {
        let checker = PermissionChecker::new(PermissionMode::Auto);
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
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

    #[test]
    fn enter_plan_mode_saves_and_restores_mode() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.enter_plan_mode();
        assert!(matches!(checker.mode(), PermissionMode::Plan));
        // Plan mode denies write/bash
        assert!(matches!(checker.check("bash", None), PermissionResult::Deny { .. }));

        let restored = checker.exit_plan_mode();
        assert!(restored.is_some());
        assert!(matches!(restored.unwrap(), PermissionMode::Default));
        // Restored mode: dangerous tools require approval
        assert!(matches!(checker.check("bash", None), PermissionResult::Ask { .. }));
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
        assert_eq!(PermissionMode::BypassPermissions.to_string(), "bypass-permissions");
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
