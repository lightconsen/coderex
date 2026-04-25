//! Organization-scoped remote policy — enterprise-level policy enforcement with org boundaries.
//!
//! Policies are tied to an organization ID and include permission scoping,
//! model allowlists/denylists, and tool access restrictions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Errors from org policy operations.
#[derive(Debug, thiserror::Error)]
pub enum OrgPolicyError {
    /// Policy not found for the given org.
    #[error("no policy found for org {org_id}")]
    NotFound { org_id: String },
    /// Policy violation — action not allowed.
    #[error("policy violation: {reason}")]
    Violation { reason: String },
    /// Not authorized for this org.
    #[error("not authorized for org {org_id}")]
    NotAuthorized { org_id: String },
}

/// An organization-scoped remote policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgPolicy {
    /// Organization identifier.
    pub org_id: String,
    /// Models that are allowed for this org. If empty, all models allowed.
    pub allowed_models: HashSet<String>,
    /// Models that are explicitly denied for this org.
    pub denied_models: HashSet<String>,
    /// Permission modes that org members are allowed to use.
    pub allowed_permission_modes: HashSet<String>,
    /// Tools that are blocked for this org.
    pub blocked_tools: HashSet<String>,
    /// Maximum session budget in USD.
    pub max_session_budget_usd: Option<f64>,
    /// Whether team memory sync is enabled for this org.
    pub team_memory_enabled: bool,
    /// Whether trusted device verification is required.
    pub require_trusted_device: bool,
    /// When this policy was created.
    pub created_at: DateTime<Utc>,
    /// When this policy was last updated.
    pub updated_at: DateTime<Utc>,
    /// Policy version (monotonically increasing).
    pub version: u32,
}

impl Default for OrgPolicy {
    fn default() -> Self {
        Self {
            org_id: String::new(),
            allowed_models: HashSet::new(),
            denied_models: HashSet::new(),
            allowed_permission_modes: HashSet::new(),
            blocked_tools: HashSet::new(),
            max_session_budget_usd: None,
            team_memory_enabled: true,
            require_trusted_device: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            version: 1,
        }
    }
}

impl OrgPolicy {
    /// Create a new policy for the given org.
    #[must_use]
    pub fn new(org_id: String) -> Self {
        Self {
            org_id,
            ..Default::default()
        }
    }

    /// Check if a model is allowed under this policy.
    #[must_use]
    pub fn is_model_allowed(&self, model: &str) -> bool {
        if self.denied_models.contains(model) {
            return false;
        }
        if self.allowed_models.is_empty() {
            return true; // No allowlist means all allowed
        }
        self.allowed_models.contains(model)
    }

    /// Check if a permission mode is allowed.
    #[must_use]
    pub fn is_permission_mode_allowed(&self, mode: &str) -> bool {
        if self.allowed_permission_modes.is_empty() {
            return true;
        }
        self.allowed_permission_modes.contains(mode)
    }

    /// Check if a tool is allowed.
    #[must_use]
    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        !self.blocked_tools.contains(tool_name)
    }

    /// Check if a budget would exceed the session limit.
    #[must_use]
    pub fn is_within_budget(&self, amount: f64) -> bool {
        match self.max_session_budget_usd {
            Some(limit) => amount <= limit,
            None => true, // No budget cap
        }
    }

    /// Merge this org policy into settings, applying org-level restrictions.
    #[must_use]
    pub fn apply_to_settings(
        &self,
        settings: crate::settings::Settings,
    ) -> crate::settings::Settings {
        let mut merged = settings;

        // Restrict model if not allowed
        if let Some(ref model) = merged.model {
            if !self.is_model_allowed(model) {
                // Fall back to first allowed model or default
                merged.model = self.allowed_models.iter().next().cloned();
            }
        }

        // Restrict permission mode if not allowed
        if let Some(ref mode) = merged.permission_mode {
            if !self.is_permission_mode_allowed(mode) {
                merged.permission_mode = self.allowed_permission_modes.iter().next().cloned();
            }
        }

        // Apply budget cap if lower than current
        if let Some(org_budget) = self.max_session_budget_usd {
            if let Some(current) = merged.max_budget_usd {
                if org_budget < current {
                    merged.max_budget_usd = Some(org_budget);
                }
            } else {
                merged.max_budget_usd = Some(org_budget);
            }
        }

        merged
    }

    /// Validate that a requested action is allowed under this policy.
    pub fn validate(
        &self,
        model: Option<&str>,
        mode: Option<&str>,
        tool: Option<&str>,
    ) -> Result<(), OrgPolicyError> {
        if let Some(m) = model {
            if !self.is_model_allowed(m) {
                return Err(OrgPolicyError::Violation {
                    reason: format!("model '{m}' is not allowed for org '{}'", self.org_id),
                });
            }
        }
        if let Some(m) = mode {
            if !self.is_permission_mode_allowed(m) {
                return Err(OrgPolicyError::Violation {
                    reason: format!(
                        "permission mode '{m}' is not allowed for org '{}'",
                        self.org_id
                    ),
                });
            }
        }
        if let Some(t) = tool {
            if !self.is_tool_allowed(t) {
                return Err(OrgPolicyError::Violation {
                    reason: format!("tool '{t}' is blocked for org '{}'", self.org_id),
                });
            }
        }
        Ok(())
    }
}

/// Org policy store — manages multiple org policies with caching.
#[derive(Debug, Default)]
pub struct OrgPolicyStore {
    /// Map of org_id -> policy.
    policies: std::collections::HashMap<String, OrgPolicy>,
    /// Currently active org.
    active_org_id: Option<String>,
}

impl OrgPolicyStore {
    /// Create a new empty policy store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the active org.
    pub fn set_active_org(&mut self, org_id: String) {
        self.active_org_id = Some(org_id);
    }

    /// Get the active org ID.
    #[must_use]
    pub fn active_org_id(&self) -> Option<&str> {
        self.active_org_id.as_deref()
    }

    /// Add or update a policy for an org.
    pub fn upsert_policy(&mut self, policy: OrgPolicy) {
        let org_id = policy.org_id.clone();
        if let Some(existing) = self.policies.get_mut(&org_id) {
            existing.version += 1;
            existing.allowed_models = policy.allowed_models.clone();
            existing.denied_models = policy.denied_models.clone();
            existing.allowed_permission_modes = policy.allowed_permission_modes.clone();
            existing.blocked_tools = policy.blocked_tools.clone();
            existing.max_session_budget_usd = policy.max_session_budget_usd;
            existing.team_memory_enabled = policy.team_memory_enabled;
            existing.require_trusted_device = policy.require_trusted_device;
            existing.updated_at = Utc::now();
        } else {
            self.policies.insert(org_id, policy);
        }
    }

    /// Get the policy for an specific org.
    pub fn get_policy(&self, org_id: &str) -> Option<&OrgPolicy> {
        self.policies.get(org_id)
    }

    /// Get the active org's policy.
    pub fn active_policy(&self) -> Option<&OrgPolicy> {
        self.active_org_id
            .as_ref()
            .and_then(|id| self.policies.get(id))
    }

    /// Validate a request against the active org policy.
    pub fn validate_active(
        &self,
        model: Option<&str>,
        mode: Option<&str>,
        tool: Option<&str>,
    ) -> Result<(), OrgPolicyError> {
        match self.active_policy() {
            Some(policy) => policy.validate(model, mode, tool),
            None => Ok(()), // No policy = no restrictions
        }
    }

    /// Apply the active org policy to settings.
    pub fn apply_active_to_settings(
        &self,
        settings: crate::settings::Settings,
    ) -> crate::settings::Settings {
        match self.active_policy() {
            Some(policy) => policy.apply_to_settings(settings),
            None => settings,
        }
    }

    /// Remove a policy.
    pub fn remove_policy(&mut self, org_id: &str) -> bool {
        self.policies.remove(org_id).is_some()
    }

    /// List all org IDs with policies.
    #[must_use]
    pub fn list_orgs(&self) -> Vec<&str> {
        self.policies.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    fn test_policy() -> OrgPolicy {
        let mut policy = OrgPolicy::new("acme-corp".to_string());
        policy.allowed_models = HashSet::from([
            "claude-sonnet-4-6".to_string(),
            "claude-opus-4-6".to_string(),
        ]);
        policy.denied_models = HashSet::from(["claude-haiku-4-5-20251001".to_string()]);
        policy.allowed_permission_modes =
            HashSet::from(["default".to_string(), "accept-edits".to_string()]);
        policy.blocked_tools = HashSet::from(["web_browser".to_string()]);
        policy.max_session_budget_usd = Some(50.0);
        policy
    }

    #[test]
    fn model_allowlist() {
        let policy = test_policy();
        assert!(policy.is_model_allowed("claude-sonnet-4-6"));
        assert!(policy.is_model_allowed("claude-opus-4-6"));
        assert!(!policy.is_model_allowed("claude-haiku-4-5-20251001")); // denied
        assert!(!policy.is_model_allowed("other-model")); // not in allowlist
    }

    #[test]
    fn empty_allowlist_means_all_allowed() {
        let policy = OrgPolicy::new("open-org".to_string());
        assert!(policy.is_model_allowed("any-model"));
        assert!(policy.is_tool_allowed("any-tool"));
        assert!(policy.is_permission_mode_allowed("any-mode"));
    }

    #[test]
    fn denied_overrides_allowed() {
        let policy = test_policy();
        // haiku is in denied_models — even if also in allowed, denied wins
        assert!(!policy.is_model_allowed("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn tool_blocking() {
        let policy = test_policy();
        assert!(!policy.is_tool_allowed("web_browser"));
        assert!(policy.is_tool_allowed("read"));
        assert!(policy.is_tool_allowed("bash"));
    }

    #[test]
    fn budget_check() {
        let policy = test_policy();
        assert!(policy.is_within_budget(25.0));
        assert!(policy.is_within_budget(50.0));
        assert!(!policy.is_within_budget(50.01));
    }

    #[test]
    fn no_budget_means_unlimited() {
        let policy = OrgPolicy::new("free-org".to_string());
        assert!(policy.is_within_budget(1_000_000.0));
    }

    #[test]
    fn validate_success() {
        let policy = test_policy();
        policy
            .validate(Some("claude-sonnet-4-6"), Some("default"), Some("read"))
            .unwrap();
    }

    #[test]
    fn validate_model_violation() {
        let policy = test_policy();
        let err = policy.validate(Some("claude-haiku-4-5-20251001"), None, None);
        assert!(matches!(err, Err(OrgPolicyError::Violation { .. })));
    }

    #[test]
    fn validate_tool_violation() {
        let policy = test_policy();
        let err = policy.validate(None, None, Some("web_browser"));
        assert!(matches!(err, Err(OrgPolicyError::Violation { .. })));
    }

    #[test]
    fn apply_to_settings_restricts_model() {
        let policy = test_policy();
        let settings = Settings {
            model: Some("claude-haiku-4-5-20251001".into()),
            permission_mode: Some("default".into()),
            ..Settings::defaults()
        };
        let merged = policy.apply_to_settings(settings);
        // haiku is denied, should fall back to an allowed model
        assert!(merged.model.is_some());
        assert!(policy.is_model_allowed(merged.model.as_ref().unwrap()));
    }

    #[test]
    fn apply_to_settings_applies_budget_cap() {
        let policy = test_policy();
        let settings = Settings {
            model: Some("claude-sonnet-4-6".into()),
            permission_mode: Some("default".into()),
            max_budget_usd: Some(100.0),
            ..Settings::defaults()
        };
        let merged = policy.apply_to_settings(settings);
        assert_eq!(merged.max_budget_usd, Some(50.0)); // org cap is lower
    }

    #[test]
    fn org_policy_store_active_policy() {
        let mut store = OrgPolicyStore::new();
        let policy = test_policy();
        store.upsert_policy(policy);
        store.set_active_org("acme-corp".to_string());

        let active = store.active_policy().unwrap();
        assert_eq!(active.org_id, "acme-corp");
        assert!(active.is_model_allowed("claude-sonnet-4-6"));
    }

    #[test]
    fn org_policy_store_validate_active() {
        let mut store = OrgPolicyStore::new();
        store.upsert_policy(test_policy());
        store.set_active_org("acme-corp".to_string());

        assert!(
            store
                .validate_active(Some("claude-sonnet-4-6"), None, None)
                .is_ok()
        );
        assert!(
            store
                .validate_active(Some("claude-haiku-4-5-20251001"), None, None)
                .is_err()
        );
    }

    #[test]
    fn org_policy_store_no_active_org() {
        let store = OrgPolicyStore::new();
        // No active org = no restrictions
        let settings = Settings {
            model: Some("any-model".into()),
            max_budget_usd: Some(1_000_000.0),
            ..Default::default()
        };
        let merged = store.apply_active_to_settings(settings);
        assert_eq!(merged.model, Some("any-model".into()));
    }

    #[test]
    fn policy_version_increments_on_update() {
        let mut store = OrgPolicyStore::new();
        let policy = OrgPolicy::new("v1-org".to_string());
        store.upsert_policy(policy);
        assert_eq!(store.get_policy("v1-org").unwrap().version, 1);

        // Upsert again
        let mut policy2 = OrgPolicy::new("v1-org".to_string());
        policy2.blocked_tools = HashSet::from(["bash".to_string()]);
        store.upsert_policy(policy2);
        assert_eq!(store.get_policy("v1-org").unwrap().version, 2);
    }

    #[test]
    fn remove_policy() {
        let mut store = OrgPolicyStore::new();
        store.upsert_policy(test_policy());
        assert!(store.remove_policy("acme-corp"));
        assert!(store.get_policy("acme-corp").is_none());
        assert!(!store.remove_policy("nonexistent"));
    }
}
