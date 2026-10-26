//! Installing settings-defined permission rules into a checker.

use crate::settings::{ConfigError, PermissionsSettings};
use coderex_core::permissions::{
    PermissionAction, PermissionChecker, PermissionRuleSpec, RuleCompileContext,
};
use std::path::Path;

/// Parse and compile every rule in `permissions` into `checker`.
///
/// All rules are parsed and compiled before any is installed, so a malformed
/// rule cannot leave a partially applied policy. Relative path patterns are
/// resolved against `cwd`; `home` falls back to the process home directory.
///
/// # Errors
///
/// Returns [`ConfigError::Validation`] when a rule cannot be parsed or a glob
/// cannot be compiled.
pub fn install_settings_rules(
    checker: &mut PermissionChecker,
    permissions: Option<&PermissionsSettings>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<usize, ConfigError> {
    let Some(permissions) = permissions else {
        return Ok(0);
    };

    let mut specs = Vec::new();
    for (field, action, rules) in [
        ("allow", PermissionAction::Allow, &permissions.allow),
        ("deny", PermissionAction::Deny, &permissions.deny),
        ("ask", PermissionAction::Ask, &permissions.ask),
    ] {
        for (index, raw) in rules.iter().enumerate() {
            let spec = PermissionRuleSpec::parse(raw, action).map_err(|error| {
                ConfigError::Validation {
                    field: format!("permissions.{field}[{index}]"),
                    message: format!("invalid permission rule {raw:?}: {error}"),
                }
            })?;
            specs.push(spec);
        }
    }
    if specs.is_empty() {
        return Ok(0);
    }

    let home = home.map(Path::to_path_buf).or_else(home::home_dir);
    let ctx = RuleCompileContext {
        cwd,
        home: home.as_deref(),
    };
    checker
        .add_specs(&specs, &ctx)
        .map_err(|error| ConfigError::Validation {
            field: "permissions".to_string(),
            message: error.to_string(),
        })?;
    Ok(specs.len())
}
