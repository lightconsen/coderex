//! The memory tool family: `memory_list`, `memory_read`, `memory_write`.
//!
//! Three tools over [`MemoryService`], one per verb. They are split by name
//! rather than folded into one `Memory` tool with an `action` field because a
//! permission rule has to be able to allow reading memory while still asking
//! about writing it, and a rule matches on a tool name.
//!
//! # Why this is not an `impl coderex_tools::Tool`
//!
//! It cannot be. `coderex-tools` depends on `coderex-memory` (its `BriefTool`
//! reads transcripts), so `coderex-memory` depending on `coderex-tools` would
//! close a cycle, and Cargo rejects a cycle even when the dependency is
//! optional. So the family is written against plain JSON — the same shape the
//! `Tool` trait passes around, with no reference to it — and a crate that
//! depends on both wraps it:
//!
//! ```ignore
//! // In a crate that depends on both coderex-memory and coderex-tools.
//! impl coderex_tools::Tool for MemoryToolAdapter {
//!     fn descriptor(&self) -> coderex_tools::ToolDescriptor {
//!         coderex_tools::ToolDescriptor {
//!             name: self.tool.name().to_string(),
//!             tier: coderex_tools::ToolTier::Baseline,
//!             input_schema: self.tool.input_schema(),
//!             output_schema: self.tool.output_schema(),
//!             requires_approval: false,
//!             is_enabled: true,
//!             is_read_only: self.tool.is_read_only(),
//!             is_concurrency_safe: self.tool.is_read_only(),
//!             interrupt_behavior: coderex_tools::InterruptBehavior::RunToCompletion,
//!         }
//!     }
//!
//!     async fn execute(
//!         &self,
//!         input: coderex_tools::ToolInput,
//!         _ctx: coderex_tools::ToolContext<'_>,
//!     ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
//!         self.tool
//!             .call(&input.arguments)
//!             .await
//!             .map(|output| coderex_tools::ToolOutput { output, is_display: true })
//!             .map_err(|error| coderex_tools::ToolError::InvalidInput {
//!                 message: error.to_string(),
//!             })
//!     }
//! }
//! ```
//!
//! Registering the family is then the same one line the artifact family uses:
//!
//! ```ignore
//! for tool in coderex_memory::tools::MemoryTools::new(memory_service).all() {
//!     dispatcher.registry().register_dynamic(std::sync::Arc::new(MemoryToolAdapter::new(tool)));
//! }
//! ```
//!
//! # What a write reports
//!
//! A successful `memory_write` reports the new version, and `memory_read`
//! reports the version it read. That pair is the point: a caller that read at
//! version 3 passes `if_version: 3` to the write, and a write that would
//! clobber a change it never saw is refused with the version that is there
//! now. The convention is the artifact database's, not a second one.

use serde_json::{Value, json};
use std::sync::Arc;

use crate::service::{MemoryRecord, MemoryService, MemoryServiceError, MemoryStore};

/// The tool that lists what is in a store.
pub const MEMORY_LIST_TOOL: &str = "memory_list";
/// The tool that reads one memory.
pub const MEMORY_READ_TOOL: &str = "memory_read";
/// The tool that writes one memory.
pub const MEMORY_WRITE_TOOL: &str = "memory_write";

/// Every tool in the family.
pub const FAMILY: [&str; 3] = [MEMORY_LIST_TOOL, MEMORY_READ_TOOL, MEMORY_WRITE_TOOL];

/// Longest memory content `memory_read` hands back before truncating.
///
/// A memory is a note, not a file; one that has grown past this is truncated
/// with a note saying so rather than silently filling the context.
pub const MAX_CONTENT_CHARS: usize = 100_000;

/// Something wrong with a memory tool call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryToolError {
    /// The arguments do not name what the tool needs.
    #[error("{0}")]
    Invalid(String),
    /// The memory service refused or failed.
    #[error("{0}")]
    Failed(String),
}

impl From<MemoryServiceError> for MemoryToolError {
    fn from(error: MemoryServiceError) -> Self {
        match error {
            MemoryServiceError::Invalid(message) => Self::Invalid(message),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// The memory tool family over one [`MemoryService`].
#[derive(Debug, Clone)]
pub struct MemoryTools {
    service: Arc<MemoryService>,
}

impl MemoryTools {
    /// Build the family over a service.
    #[must_use]
    pub const fn new(service: Arc<MemoryService>) -> Self {
        Self { service }
    }

    /// The service the family was built over.
    #[must_use]
    pub const fn service(&self) -> &Arc<MemoryService> {
        &self.service
    }

    /// The name of every tool in the family.
    #[must_use]
    pub const fn names(&self) -> [&'static str; 3] {
        FAMILY
    }

    /// Whether `name` is one of this family's tools.
    #[must_use]
    pub fn handles(name: &str) -> bool {
        FAMILY.contains(&name)
    }

    /// Every tool in the family, ready to register.
    #[must_use]
    pub fn all(&self) -> Vec<MemoryTool> {
        FAMILY
            .iter()
            .map(|name| MemoryTool {
                service: Arc::clone(&self.service),
                name,
            })
            .collect()
    }

    /// One tool by name.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<MemoryTool> {
        FAMILY
            .iter()
            .find(|candidate| **candidate == name)
            .map(|name| MemoryTool {
                service: Arc::clone(&self.service),
                name,
            })
    }
}

/// One memory tool.
#[derive(Debug, Clone)]
pub struct MemoryTool {
    service: Arc<MemoryService>,
    name: &'static str,
}

impl MemoryTool {
    /// The tool's name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Whether the tool only reads.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.name != MEMORY_WRITE_TOOL
    }

    /// What the tool does, as the model is told it.
    #[must_use]
    pub fn description(&self) -> &'static str {
        match self.name {
            MEMORY_WRITE_TOOL => {
                "write a memory to a named store; a write to a shared or org \
                 store is classified before it leaves the machine"
            }
            MEMORY_READ_TOOL => "read one memory from a named store by its key",
            _ => "list the memories in a named store, with each one's version",
        }
    }

    /// The JSON schema for this tool's arguments.
    #[must_use]
    pub fn input_schema(&self) -> Value {
        match self.name {
            MEMORY_WRITE_TOOL => json!({
                "type": "object",
                "properties": {
                    "store": {
                        "type": "string",
                        "enum": ["personal", "project", "shared", "org"],
                        "description": "which store to write to; personal and project stay on this machine, shared and org are classified before they are sent"
                    },
                    "key": {
                        "type": "string",
                        "description": "what the memory is about; may be hierarchical ('backend/auth')"
                    },
                    "content": {
                        "type": "string",
                        "description": "the memory itself"
                    },
                    "if_version": {
                        "type": "integer",
                        "description": "the version this write replaces, from memory_read; omit to write regardless"
                    }
                },
                "required": ["store", "key", "content"]
            }),
            MEMORY_READ_TOOL => json!({
                "type": "object",
                "properties": {
                    "store": {
                        "type": "string",
                        "enum": ["personal", "project", "shared", "org"]
                    },
                    "key": { "type": "string" }
                },
                "required": ["store", "key"]
            }),
            _ => json!({
                "type": "object",
                "properties": {
                    "store": {
                        "type": "string",
                        "enum": ["personal", "project", "shared", "org"],
                        "description": "which store to list; omit to list every store"
                    }
                }
            }),
        }
    }

    /// The JSON schema for this tool's output.
    #[must_use]
    pub fn output_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "output": { "type": "string" } } })
    }

    /// Run the tool.
    ///
    /// # Errors
    ///
    /// [`MemoryToolError::Invalid`] when the arguments do not name a store and
    /// key; [`MemoryToolError::Failed`] when the service refuses or fails.
    pub async fn call(&self, arguments: &Value) -> Result<String, MemoryToolError> {
        match self.name {
            MEMORY_WRITE_TOOL => self.write(arguments).await,
            MEMORY_READ_TOOL => self.read(arguments),
            _ => self.list(arguments),
        }
    }

    fn list(&self, arguments: &Value) -> Result<String, MemoryToolError> {
        use std::fmt::Write as _;

        let stores: Vec<MemoryStore> = match string_arg(arguments, "store") {
            Some(name) => vec![store_arg(&name)?],
            None => MemoryStore::all().to_vec(),
        };

        let mut out = String::new();
        for store in stores {
            match self.service.list(store) {
                Ok(records) => {
                    let _ = writeln!(out, "{} ({} memories)", store, records.len());
                    for record in records {
                        let _ = writeln!(
                            out,
                            "  {}\tv{}\t{}",
                            record.key,
                            record.version,
                            record.updated_at.to_rfc3339()
                        );
                    }
                }
                // A store that is not configured is worth naming, but it is
                // not a reason to fail a listing of the stores that are.
                Err(MemoryServiceError::StoreUnavailable(_)) => {
                    let _ = writeln!(out, "{store} (not configured)");
                }
                Err(other) => return Err(other.into()),
            }
        }
        Ok(out.trim_end().to_string())
    }

    fn read(&self, arguments: &Value) -> Result<String, MemoryToolError> {
        let (store, key) = store_and_key(arguments)?;
        let record = self.service.read(store, &key)?;
        Ok(format_record(&record))
    }

    async fn write(&self, arguments: &Value) -> Result<String, MemoryToolError> {
        let (store, key) = store_and_key(arguments)?;
        let content = string_arg(arguments, "content")
            .ok_or_else(|| MemoryToolError::Invalid("'content' is required".into()))?;
        let if_version = match arguments.get("if_version") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                MemoryToolError::Invalid("'if_version' must be a non-negative integer".into())
            })?),
        };

        let record = self
            .service
            .write(store, &key, &content, if_version)
            .await?;
        Ok(format!(
            "Wrote {} '{}' at version {}.",
            record.store, record.key, record.version
        ))
    }
}

/// The store and key a call names, or why it does not name them.
fn store_and_key(arguments: &Value) -> Result<(MemoryStore, String), MemoryToolError> {
    let store = store_arg(
        &string_arg(arguments, "store")
            .ok_or_else(|| MemoryToolError::Invalid("'store' is required".into()))?,
    )?;
    let key = string_arg(arguments, "key")
        .ok_or_else(|| MemoryToolError::Invalid("'key' is required".into()))?;
    Ok((store, key))
}

/// The header and body `memory_read` reports.
///
/// The version is in the header because it is what a later write pins with
/// `if_version`; leaving it out would make the pin unreachable.
fn format_record(record: &MemoryRecord) -> String {
    format!(
        "{}/{} (version {}, updated {})\n{}",
        record.store,
        record.key,
        record.version,
        record.updated_at.to_rfc3339(),
        truncate(&record.content, MAX_CONTENT_CHARS)
    )
}

fn store_arg(name: &str) -> Result<MemoryStore, MemoryToolError> {
    MemoryStore::parse(name).ok_or_else(|| {
        MemoryToolError::Invalid(format!(
            "'{name}' is not a memory store; expected one of {}",
            MemoryStore::all()
                .iter()
                .map(|store| store.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

fn string_arg(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

/// Cut a string to at most `max` characters, on a character boundary.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n… truncated at {max} characters")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::service_over;

    fn family() -> (MemoryTools, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let service = service_over(&dir.path().join("long_term"), &dir.path().join("team"));
        (MemoryTools::new(Arc::new(service)), dir)
    }

    fn tool(tools: &MemoryTools, name: &str) -> MemoryTool {
        tools.tool(name).expect("tool is in the family")
    }

    #[test]
    fn the_family_is_three_tools_and_only_the_write_is_not_read_only() {
        let (tools, _dir) = family();
        let all = tools.all();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all.iter().map(MemoryTool::name).collect::<Vec<_>>(),
            FAMILY.to_vec()
        );
        for tool in &all {
            assert_eq!(
                tool.is_read_only(),
                tool.name() != MEMORY_WRITE_TOOL,
                "{}",
                tool.name()
            );
        }
        assert!(MemoryTools::handles("memory_write"));
        assert!(!MemoryTools::handles("memory_delete"));
    }

    #[tokio::test]
    async fn write_then_read_reports_the_version_to_pin() {
        let (tools, _dir) = family();
        let write = tool(&tools, MEMORY_WRITE_TOOL);

        let written = write
            .call(&json!({
                "store": "project",
                "key": "build",
                "content": "cargo test -p coderex-memory"
            }))
            .await
            .unwrap();
        assert!(written.contains("version 1"), "{written}");

        let read = tool(&tools, MEMORY_READ_TOOL);
        let read_back = read
            .call(&json!({ "store": "project", "key": "build" }))
            .await
            .unwrap();
        assert!(read_back.contains("project/build"), "{read_back}");
        assert!(read_back.contains("version 1"), "{read_back}");
        assert!(read_back.contains("cargo test"), "{read_back}");
    }

    #[tokio::test]
    async fn a_stale_if_version_is_reported_not_swallowed() {
        let (tools, _dir) = family();
        let write = tool(&tools, MEMORY_WRITE_TOOL);

        write
            .call(&json!({"store": "personal", "key": "k", "content": "one"}))
            .await
            .unwrap();
        write
            .call(&json!({"store": "personal", "key": "k", "content": "two", "if_version": 1}))
            .await
            .unwrap();

        let refused = write
            .call(&json!({"store": "personal", "key": "k", "content": "three", "if_version": 1}))
            .await
            .unwrap_err();
        // The version it is actually at is in the message, so the caller can
        // re-read rather than guess.
        assert!(refused.to_string().contains("found 2"), "{refused}");
    }

    #[tokio::test]
    async fn a_shared_write_is_refused_when_nothing_classified_it() {
        let (tools, _dir) = family();
        let write = tool(&tools, MEMORY_WRITE_TOOL);

        let refused = write
            .call(&json!({"store": "shared", "key": "t", "content": "c"}))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("refused"), "{refused}");
    }

    #[tokio::test]
    async fn listing_names_the_stores_and_their_keys() {
        let (tools, _dir) = family();
        tool(&tools, MEMORY_WRITE_TOOL)
            .call(&json!({"store": "project", "key": "a", "content": "1"}))
            .await
            .unwrap();
        tool(&tools, MEMORY_WRITE_TOOL)
            .call(&json!({"store": "personal", "key": "b", "content": "2"}))
            .await
            .unwrap();

        let listed = tool(&tools, MEMORY_LIST_TOOL)
            .call(&json!({}))
            .await
            .unwrap();
        assert!(listed.contains("project (1 memories)"), "{listed}");
        assert!(listed.contains("personal (1 memories)"), "{listed}");
        // `org` has no store configured; it is named, not fatal.
        assert!(listed.contains("org (not configured)"), "{listed}");

        let only_project = tool(&tools, MEMORY_LIST_TOOL)
            .call(&json!({ "store": "project" }))
            .await
            .unwrap();
        assert!(only_project.contains('a'), "{only_project}");
        assert!(!only_project.contains("personal"), "{only_project}");
    }

    #[tokio::test]
    async fn missing_and_unknown_arguments_are_invalid_not_failures() {
        let (tools, _dir) = family();

        let no_key = tool(&tools, MEMORY_READ_TOOL)
            .call(&json!({ "store": "project" }))
            .await
            .unwrap_err();
        assert!(matches!(no_key, MemoryToolError::Invalid(_)), "{no_key:?}");

        let unknown_store = tool(&tools, MEMORY_READ_TOOL)
            .call(&json!({ "store": "elsewhere", "key": "k" }))
            .await
            .unwrap_err();
        assert!(
            unknown_store.to_string().contains("not a memory store"),
            "{unknown_store}"
        );

        let missing = tool(&tools, MEMORY_READ_TOOL)
            .call(&json!({ "store": "project", "key": "nope" }))
            .await
            .unwrap_err();
        assert!(
            missing.to_string().contains("no project memory"),
            "{missing}"
        );
    }

    #[test]
    fn every_tool_declares_its_arguments() {
        let (tools, _dir) = family();
        for tool in tools.all() {
            let schema = tool.input_schema();
            assert_eq!(schema["type"], "object", "{}", tool.name());
            assert!(!tool.description().is_empty(), "{}", tool.name());
            assert_eq!(tool.output_schema()["type"], "object");
        }
    }

    #[test]
    fn a_long_memory_is_truncated_on_a_character_boundary() {
        let long = "é".repeat(MAX_CONTENT_CHARS + 10);
        let cut = truncate(&long, MAX_CONTENT_CHARS);
        assert!(cut.ends_with("truncated at 100000 characters"), "{cut:?}");
        assert_eq!(cut.chars().filter(|c| *c == 'é').count(), MAX_CONTENT_CHARS);
        assert_eq!(truncate("short", MAX_CONTENT_CHARS), "short");
    }
}
