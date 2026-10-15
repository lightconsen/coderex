//! Path rules for the hill-climb loop.
//!
//! Every id in a flow — the flow name, the variant, a case id — ends up in a
//! file name, and the flow directory is inside a repository that the thing
//! being measured can write to. So a name that can climb (`..`), start an
//! absolute path (`/`), or hide itself (`.`) is refused rather than sanitised:
//! a silently-renamed case is a case whose results land somewhere unexpected.

/// Something wrong with a flow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HarnessError {
    /// A name cannot be used in a path.
    #[error("{0}")]
    UnsafeName(String),
    /// The harness does not make sense.
    #[error("{0}")]
    Invalid(String),
    /// The harness file could not be read.
    #[error("cannot read {path}: {message}")]
    Read {
        /// The file that could not be read.
        path: String,
        /// Why.
        message: String,
    },
    /// The harness file could not be parsed.
    #[error("cannot parse {path}: {message}")]
    Parse {
        /// The file that could not be parsed.
        path: String,
        /// Why.
        message: String,
    },
    /// The harness changed since it was approved.
    #[error(
        "harness changed since last approved run (files: {files}) — review the change, then run \
         with --approve-harness to approve it"
    )]
    HarnessChanged {
        /// The files that make up the harness.
        files: String,
    },
    /// The run itself failed.
    #[error("{0}")]
    Run(String),
}

/// Check a name is usable as one path segment.
pub fn path_safe_id(id: &str) -> Result<&str, HarnessError> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(HarnessError::UnsafeName("an id is empty".to_string()));
    }
    if trimmed.len() > 128 {
        return Err(HarnessError::UnsafeName(format!(
            "the id '{trimmed}' is longer than 128 characters"
        )));
    }
    if trimmed == "." || trimmed == ".." {
        return Err(HarnessError::UnsafeName(format!(
            "'{trimmed}' is not a usable id"
        )));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(HarnessError::UnsafeName(format!(
            "'{trimmed}' has characters an id cannot carry; allowed are letters, digits, \
             - _ and ."
        )));
    }
    if trimmed.starts_with('.') {
        return Err(HarnessError::UnsafeName(format!(
            "'{trimmed}' starts with a dot, which hides it"
        )));
    }
    Ok(trimmed)
}

/// Check a name is one segment, without any path separators.
pub fn assert_in_flow(name: &str) -> Result<&str, HarnessError> {
    if name.contains('/') || name.contains('\\') {
        return Err(HarnessError::UnsafeName(format!(
            "'{name}' carries a path separator"
        )));
    }
    path_safe_id(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_ids_are_accepted() {
        for id in ["smoke", "case-1", "case_2", "v1.baseline"] {
            assert_eq!(path_safe_id(id).unwrap(), id);
        }
    }

    #[test]
    fn climbing_and_absolute_ids_are_refused() {
        for id in [
            "",
            "..",
            ".",
            "../etc",
            "/etc/passwd",
            "a/b",
            "a\\b",
            "case 1",
            ".hidden",
        ] {
            assert!(path_safe_id(id).is_err(), "{id} was accepted");
        }
    }

    #[test]
    fn an_id_longer_than_a_name_should_be_is_refused() {
        assert!(path_safe_id(&"x".repeat(129)).is_err());
        assert!(path_safe_id(&"x".repeat(128)).is_ok());
    }

    #[test]
    fn assert_in_flow_refuses_separators() {
        assert!(assert_in_flow("plain").is_ok());
        assert!(assert_in_flow("../up").is_err());
        assert!(assert_in_flow("a/b").is_err());
        assert!(assert_in_flow("a\\b").is_err());
    }
}
