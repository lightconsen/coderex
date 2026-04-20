//! Permission manager — rule management layer on top of [`PermissionEngine`].
//!
//! The [`PermissionManager`] provides:
//! - Rule persistence (load/save rules from/to files)
//! - Rule groups and priorities
//! - Rule validation and conflict detection
//! - Integration with the [`ConfirmationBus`](crate::confirmation_bus::ConfirmationBus)
//!
//! Built on top of the existing [`PermissionChecker`](crate::permissions::PermissionChecker)
//! and [`PermissionEngine`](crate::permission_engine::PermissionEngine) foundations.

use crate::permissions::{PermissionAction, PermissionChecker, PermissionMode, PermissionResult, PermissionRule};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

/// Errors from the permission manager.
#[derive(Debug, Error)]
pub enum PermissionManagerError {
    /// Failed to load rules from file.
    #[error("failed to load rules: {0}")]
    LoadFailed(String),
    /// Failed to save rules to file.
    #[error("failed to save rules: {0}")]
    SaveFailed(String),
    /// Invalid rule configuration.
    #[error("invalid rule: {0}")]
    InvalidRule(String),
    /// Rule conflict detected.
    #[error("rule conflict: {0}")]
    RuleConflict(String),
}

/// A named group of permission rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleGroup {
    /// Group name (e.g., "project", "safe", "strict").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Priority (higher = evaluated first).
    pub priority: i32,
    /// Rules in this group.
    pub rules: Vec<PermissionRule>,
    /// Whether this group is active.
    pub enabled: bool,
}

/// Persistent configuration for the permission manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionManagerConfig {
    /// Default permission mode.
    pub default_mode: PermissionMode,
    /// Named rule groups.
    pub groups: Vec<RuleGroup>,
    /// Flat list of rules (for simple configs without groups).
    pub rules: Vec<PermissionRule>,
    /// Dangerous tools list.
    pub dangerous_tools: Vec<String>,
    /// Read-only tools list.
    pub read_only_tools: Vec<String>,
}

impl Default for PermissionManagerConfig {
    fn default() -> Self {
        Self {
            default_mode: PermissionMode::Default,
            groups: Vec::new(),
            rules: Vec::new(),
            dangerous_tools: vec![
                "bash".into(),
                "write".into(),
                "edit".into(),
                "multi_edit".into(),
                "notebook_edit".into(),
            ],
            read_only_tools: vec![
                "read".into(),
                "ls".into(),
                "grep".into(),
                "glob".into(),
                "web_search".into(),
                "web_fetch".into(),
                "grep_code".into(),
                "file_search".into(),
                "notebook_read".into(),
            ],
        }
    }
}

/// Manages permission rules with persistence and conflict detection.
///
/// The manager sits above [`PermissionChecker`] and provides:
/// - Rule CRUD operations
/// - Group-based organization
/// - Persistence to/from TOML/JSON files
/// - Conflict detection between rules
pub struct PermissionManager {
    config: PermissionManagerConfig,
    /// Compiled checker built from current config.
    checker: PermissionChecker,
    /// Config file path for persistence.
    config_path: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for PermissionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionManager")
            .field("config", &self.config)
            .field("config_path", &self.config_path)
            .finish()
    }
}

impl PermissionManager {
    /// Create a new permission manager with default settings.
    #[must_use]
    pub fn new() -> Self {
        let config = PermissionManagerConfig::default();
        let checker = Self::build_checker(&config);
        Self {
            config,
            checker,
            config_path: None,
        }
    }

    /// Create from a configuration.
    #[must_use]
    pub fn from_config(config: PermissionManagerConfig) -> Self {
        let checker = Self::build_checker(&config);
        Self {
            config,
            checker,
            config_path: None,
        }
    }

    /// Set the config file path for persistence.
    pub fn set_config_path(&mut self, path: impl Into<std::path::PathBuf>) {
        self.config_path = Some(path.into());
    }

    /// Get the current permission mode.
    #[must_use]
    pub fn mode(&self) -> PermissionMode {
        self.checker.mode()
    }

    /// Set the permission mode.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.config.default_mode = mode;
        self.checker.set_mode(mode);
    }

    /// Get the underlying permission checker.
    #[must_use]
    pub fn checker(&self) -> &PermissionChecker {
        &self.checker
    }

    /// Get a mutable reference to the underlying checker.
    pub fn checker_mut(&mut self) -> &mut PermissionChecker {
        &mut self.checker
    }

    /// Add a single rule to the default group.
    ///
    /// Returns an error if the rule has an invalid glob pattern.
    pub fn add_rule(&mut self, rule: PermissionRule) -> Result<(), PermissionManagerError> {
        self.config.rules.push(rule.clone());
        self.checker
            .add_rule(rule)
            .map_err(|e| PermissionManagerError::InvalidRule(e.to_string()))?;
        Ok(())
    }

    /// Remove a rule by matching its tool name and paths.
    pub fn remove_rule(&mut self, tool_name: &str, path_pattern: Option<&str>) {
        self.config.rules.retain(|r| {
            if r.tool_name != tool_name {
                return true;
            }
            if let Some(pattern) = path_pattern {
                !r.paths.iter().any(|p| p == pattern)
            } else {
                false
            }
        });
        // Rebuild checker from config
        self.checker = Self::build_checker(&self.config);
    }

    /// Add a rule group.
    pub fn add_group(&mut self, group: RuleGroup) {
        self.config.groups.push(group);
        // Rebuild checker
        self.checker = Self::build_checker(&self.config);
    }

    /// Remove a rule group by name.
    pub fn remove_group(&mut self, name: &str) {
        self.config.groups.retain(|g| g.name != name);
        self.checker = Self::build_checker(&self.config);
    }

    /// Enable or disable a rule group.
    pub fn set_group_enabled(&mut self, name: &str, enabled: bool) {
        if let Some(group) = self.config.groups.iter_mut().find(|g| g.name == name) {
            group.enabled = enabled;
            self.checker = Self::build_checker(&self.config);
        }
    }

    /// Get all rule groups.
    #[must_use]
    pub fn groups(&self) -> &[RuleGroup] {
        &self.config.groups
    }

    /// Get all flat rules (outside groups).
    #[must_use]
    pub fn rules(&self) -> &[PermissionRule] {
        &self.config.rules
    }

    /// Load configuration from a TOML file.
    ///
    /// If the file doesn't exist, the current config is kept.
    pub fn load_from_toml(&mut self, path: &Path) -> Result<(), PermissionManagerError> {
        if !path.exists() {
            return Ok(());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| PermissionManagerError::LoadFailed(e.to_string()))?;
        let config: PermissionManagerConfig = toml::from_str(&content)
            .map_err(|e| PermissionManagerError::LoadFailed(e.to_string()))?;
        self.config = config;
        self.checker = Self::build_checker(&self.config);
        self.config_path = Some(path.to_path_buf());
        Ok(())
    }

    /// Save configuration to a TOML file.
    pub fn save_to_toml(&self, path: &Path) -> Result<(), PermissionManagerError> {
        let content = toml::to_string_pretty(&self.config)
            .map_err(|e| PermissionManagerError::SaveFailed(e.to_string()))?;
        std::fs::write(path, content)
            .map_err(|e| PermissionManagerError::SaveFailed(e.to_string()))?;
        Ok(())
    }

    /// Persist current config to the configured path.
    pub fn persist(&self) -> Result<(), PermissionManagerError> {
        if let Some(ref path) = self.config_path {
            self.save_to_toml(path)
        } else {
            Err(PermissionManagerError::SaveFailed(
                "no config path set".into(),
            ))
        }
    }

    /// Check for conflicting rules.
    ///
    /// Returns a list of conflict descriptions. Two rules conflict if:
    /// - They have the same tool name and overlapping path patterns
    /// - One is Allow and the other is Deny
    #[must_use]
    pub fn detect_conflicts(&self) -> Vec<String> {
        let mut conflicts = Vec::new();
        let all_rules = self.all_rules();

        for (i, r1) in all_rules.iter().enumerate() {
            for r2 in all_rules.iter().skip(i + 1) {
                if r1.tool_name == r2.tool_name && r1.action != r2.action {
                    // Check for path overlap
                    let paths_overlap = if r1.paths.is_empty() || r2.paths.is_empty() {
                        true // Empty paths = applies everywhere
                    } else {
                        r1.paths.iter().any(|p1| r2.paths.iter().any(|p2| p1 == p2))
                    };

                    if paths_overlap {
                        conflicts.push(format!(
                            "rule conflict: '{}' has both {:?} and {:?} for paths",
                            r1.tool_name, r1.action, r2.action
                        ));
                    }
                }
            }
        }

        conflicts
    }

    /// Get all rules (from groups + flat), sorted by priority.
    #[must_use]
    fn all_rules(&self) -> Vec<PermissionRule> {
        let mut all = Vec::new();

        // Add rules from enabled groups (sorted by priority)
        let mut groups: Vec<_> = self.config.groups.iter().filter(|g| g.enabled).collect();
        groups.sort_by_key(|g| -g.priority);
        for group in groups {
            all.extend(group.rules.iter().cloned());
        }

        // Add flat rules
        all.extend(self.config.rules.iter().cloned());
        all
    }

    /// Build a PermissionChecker from config.
    fn build_checker(config: &PermissionManagerConfig) -> PermissionChecker {
        let mut checker = PermissionChecker::new(config.default_mode);

        // Add rules from enabled groups (sorted by priority, highest first)
        let mut groups: Vec<_> = config.groups.iter().filter(|g| g.enabled).collect();
        groups.sort_by_key(|g| -g.priority);
        for group in groups {
            for rule in &group.rules {
                let _ = checker.add_rule(rule.clone());
            }
        }

        // Add flat rules
        for rule in &config.rules {
            let _ = checker.add_rule(rule.clone());
        }

        checker
    }

    /// Create a "safe" preset configuration.
    ///
    /// All dangerous tools require approval. Network access is allowed.
    #[must_use]
    pub fn safe_preset() -> Self {
        let config = PermissionManagerConfig::default();
        Self::from_config(config)
    }

    /// Create a "strict" preset configuration.
    ///
    /// Only read-only tools are allowed. Everything else is denied.
    #[must_use]
    pub fn strict_preset() -> Self {
        let mut config = PermissionManagerConfig::default();
        // Plan mode naturally allows read-only tools and denies everything else
        config.default_mode = PermissionMode::Plan;
        Self::from_config(config)
    }

    /// Create a "permissive" preset configuration.
    ///
    /// Most operations are allowed. Only the most dangerous ones require approval.
    #[must_use]
    pub fn permissive_preset() -> Self {
        let mut config = PermissionManagerConfig::default();
        config.default_mode = PermissionMode::AcceptEdits;

        // Allow all dangerous tools except bash (which still requires approval)
        for tool in &config.dangerous_tools {
            if tool == "bash" {
                continue;
            }
            config.rules.push(PermissionRule {
                tool_name: tool.clone(),
                action: PermissionAction::Allow,
                paths: Vec::new(),
                description: format!("allow tool in permissive mode: {tool}"),
            });
        }

        // Only bash requires approval
        config.rules.push(PermissionRule {
            tool_name: "bash".into(),
            action: PermissionAction::Ask,
            paths: Vec::new(),
            description: "bash commands require approval".into(),
        });

        Self::from_config(config)
    }
}

impl Default for PermissionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionResult;
    use std::path::Path;

    #[test]
    fn default_mode_is_default() {
        let mgr = PermissionManager::new();
        assert!(matches!(mgr.mode(), PermissionMode::Default));
    }

    #[test]
    fn safe_preset_blocks_dangerous_tools() {
        let mgr = PermissionManager::safe_preset();
        let checker = mgr.checker();
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
        assert!(matches!(checker.check("read", None), PermissionResult::Allow));
    }

    #[test]
    fn strict_preset_only_allows_readonly() {
        let mgr = PermissionManager::strict_preset();
        let checker = mgr.checker();
        assert!(matches!(checker.check("read", None), PermissionResult::Allow));
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn permissive_preset_allows_most_tools() {
        let mgr = PermissionManager::permissive_preset();
        let checker = mgr.checker();
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Ask { .. }
        ));
        assert!(matches!(checker.check("write", None), PermissionResult::Allow));
    }

    #[test]
    fn add_rule_works() {
        let mut mgr = PermissionManager::new();
        mgr.add_rule(PermissionRule {
            tool_name: "write".into(),
            action: PermissionAction::Allow,
            paths: vec!["/tmp/**".into()],
            description: "allow writes to /tmp".into(),
        })
        .unwrap();

        let checker = mgr.checker();
        assert!(matches!(
            checker.check("write", Some(Path::new("/tmp/foo.txt"))),
            PermissionResult::AllowForPath { .. }
        ));
    }

    #[test]
    fn remove_rule_works() {
        let mut mgr = PermissionManager::new();
        mgr.add_rule(PermissionRule {
            tool_name: "write".into(),
            action: PermissionAction::Allow,
            paths: vec!["/tmp/**".into()],
            description: String::new(),
        })
        .unwrap();

        mgr.remove_rule("write", Some("/tmp/**"));
        let checker = mgr.checker();
        assert!(matches!(
            checker.check("write", Some(Path::new("/tmp/foo.txt"))),
            PermissionResult::Ask { .. }
        ));
    }

    #[test]
    fn group_priority_ordering() {
        let mut mgr = PermissionManager::new();
        mgr.add_group(RuleGroup {
            name: "high".into(),
            description: String::new(),
            priority: 100,
            rules: vec![PermissionRule {
                tool_name: "bash".into(),
                action: PermissionAction::Deny,
                paths: Vec::new(),
                description: String::new(),
            }],
            enabled: true,
        });
        mgr.add_group(RuleGroup {
            name: "low".into(),
            description: String::new(),
            priority: 10,
            rules: vec![PermissionRule {
                tool_name: "bash".into(),
                action: PermissionAction::Allow,
                paths: Vec::new(),
                description: String::new(),
            }],
            enabled: true,
        });

        // High priority group should be evaluated first
        let checker = mgr.checker();
        // Since deny rules have priority in PermissionChecker, bash should be denied
        assert!(matches!(
            checker.check("bash", None),
            PermissionResult::Deny { .. }
        ));
    }

    #[test]
    fn detect_conflicts_finds_allow_deny_overlap() {
        let mut mgr = PermissionManager::new();
        mgr.add_rule(PermissionRule {
            tool_name: "write".into(),
            action: PermissionAction::Allow,
            paths: vec!["/tmp/**".into()],
            description: String::new(),
        })
        .unwrap();
        mgr.add_rule(PermissionRule {
            tool_name: "write".into(),
            action: PermissionAction::Deny,
            paths: vec!["/tmp/**".into()],
            description: String::new(),
        })
        .unwrap();

        let conflicts = mgr.detect_conflicts();
        assert!(!conflicts.is_empty());
        assert!(conflicts[0].contains("write"));
    }

    #[test]
    fn load_save_toml_roundtrip() {
        let mut mgr = PermissionManager::new();
        mgr.set_mode(PermissionMode::Auto);
        mgr.add_rule(PermissionRule {
            tool_name: "bash".into(),
            action: PermissionAction::Ask,
            paths: Vec::new(),
            description: "ask for bash".into(),
        })
        .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("permissions.toml");

        mgr.save_to_toml(&path).unwrap();

        let mut mgr2 = PermissionManager::new();
        mgr2.load_from_toml(&path).unwrap();

        assert!(matches!(mgr2.mode(), PermissionMode::Auto));
        assert_eq!(mgr2.rules().len(), 1);
        assert_eq!(mgr2.rules()[0].tool_name, "bash");
    }

    #[test]
    fn load_nonexistent_file_is_ok() {
        let mut mgr = PermissionManager::new();
        let path = std::path::PathBuf::from("/nonexistent/path/permissions.toml");
        // Should not panic or error
        mgr.load_from_toml(&path).unwrap();
    }
}
