//! The artifact tool family.
//!
//! Four tools, one implementation. `Artifact` publishes and reads pages;
//! `ArtifactData` is the page's database; `ArtifactComments` is the thread on
//! a page; `ArtifactCheck` verifies one. Each is a distinct name because a
//! permission rule has to be able to allow publishing while still asking
//! about writes to a page's data — but they share the vocabulary, so an action
//! means the same thing whichever door it arrives through.
//!
//! Older prompts call the actions directly on `Artifact` — `read_db`,
//! `write_db`, `verify`, `preview`. Those still work: they are translated to
//! the tool and action they now belong to, which is why the split did not
//! break what was already written against it.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

use coderex_tools::{
    InterruptBehavior, Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolOutput,
    ToolRuleExemptions, ToolTier,
};

use crate::db::{CURRENT_USER, Write as DbWrite};
use crate::limits::{DB_READ_MAX_RESULT_SIZE_CHARS, MAX_RESULT_SIZE_CHARS};
use crate::markers::attached_marker;
use crate::service::ArtifactService;
use crate::slug::Slug;
use crate::store::{ENTRY_FILE, Page};

/// The tool that publishes and reads pages.
pub const ARTIFACT_TOOL: &str = "Artifact";
/// The tool for a page's shared database.
pub const DATA_TOOL: &str = "ArtifactData";
/// The tool for a page's comments.
pub const COMMENTS_TOOL: &str = "ArtifactComments";
/// The tool that verifies a page.
pub const CHECK_TOOL: &str = "ArtifactCheck";

/// Every tool in the family.
pub const FAMILY: [&str; 4] = [ARTIFACT_TOOL, DATA_TOOL, COMMENTS_TOOL, CHECK_TOOL];

/// Which door an action arrived through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// The page itself.
    Artifact,
    /// The page's database.
    Data,
    /// The page's comments.
    Comments,
    /// Verifying a page.
    Check,
}

/// The action a call is asking for, once legacy spellings are resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAction {
    /// The tool it belongs to.
    pub family: Family,
    /// The action itself.
    pub action: String,
    /// Whether the action was named with its old spelling.
    pub legacy: bool,
}

/// Resolve an action plus the tool it was called on.
///
/// A legacy action (`read_db`, `write_db`, `verify`, `preview`) carries the
/// tool it belongs to with it, so calling it on `Artifact` reaches the same
/// code as calling the new action on the right tool.
#[must_use]
pub fn resolve_action(called_on: &str, action: &str) -> ResolvedAction {
    let called_on = if called_on.is_empty() {
        ARTIFACT_TOOL
    } else {
        called_on
    };

    let (family, action, legacy) = match action {
        // The old names, and where they live now. These are forwarded to the
        // tool they belong to however they were addressed, because that is
        // what they were for: prompts written before the split keep working.
        "read_db" => (Family::Data, "get", true),
        "write_db" => (Family::Data, "set", true),
        "verify" | "preview" => (Family::Check, "verify", true),
        "" => (family_of(called_on), default_action(called_on), false),
        // An action that belongs to exactly one tool is routed to it, so
        // addressing it elsewhere is named as the mistake it is. Actions two
        // tools share (`list`, `delete`, `read`) stay with the tool they were
        // addressed to.
        other => owning_family(other).map_or_else(
            || (family_of(called_on), other, false),
            |owner| (owner, other, false),
        ),
    };

    ResolvedAction {
        family,
        action: action.to_string(),
        legacy,
    }
}

/// The actions each tool answers.
const ARTIFACT_ACTIONS: [&str; 11] = [
    "publish",
    "list",
    "read",
    "version",
    "delete",
    "list_files",
    "read_file",
    "upload_asset",
    "list_assets",
    "read_asset",
    "delete_asset",
];

/// The actions the database tool answers.
const DATA_ACTIONS: [&str; 8] = [
    "get",
    "list",
    "query",
    "set",
    "update",
    "delete",
    "str_replace",
    "batch",
];

/// The actions the comments tool answers.
const COMMENTS_ACTIONS: [&str; 6] = ["comments", "read", "list", "reply", "add", "resolve"];

/// The actions the check tool answers.
const CHECK_ACTIONS: [&str; 1] = ["verify"];

const fn actions_of(family: Family) -> &'static [&'static str] {
    match family {
        Family::Artifact => &ARTIFACT_ACTIONS,
        Family::Data => &DATA_ACTIONS,
        Family::Comments => &COMMENTS_ACTIONS,
        Family::Check => &CHECK_ACTIONS,
    }
}

/// Whether an action changes something.
///
/// A whole-tool allow rule says "this tool, anywhere, is fine". For a reader of
/// a published page that is a reasonable thing to write; for the same tool's
/// publishing, deleting, uploading and database writes it is a single approval
/// that covers all of them, so those actions refuse the shortcut — the
/// reference's `ignoresWholeToolAllowRule` — and do not offer to be allowed for
/// the rest of the session either. Reads and `verify` are ordinary.
///
/// `verify` is read-only (`ArtifactCheck` reports on a page, it does not touch
/// it), so it is not in the set.
#[must_use]
fn is_mutating(family: Family, action: &str) -> bool {
    match family {
        Family::Artifact => {
            matches!(
                action,
                "publish" | "delete" | "upload_asset" | "delete_asset"
            )
        }
        Family::Data => matches!(
            action,
            "set" | "update" | "delete" | "str_replace" | "batch"
        ),
        Family::Comments => matches!(action, "add" | "reply" | "resolve"),
        Family::Check => false,
    }
}

/// The one tool an action belongs to, when it belongs to exactly one.
fn owning_family(action: &str) -> Option<Family> {
    let mut owners = [
        Family::Artifact,
        Family::Data,
        Family::Comments,
        Family::Check,
    ]
    .into_iter()
    .filter(|family| actions_of(*family).contains(&action));

    let first = owners.next()?;
    owners.next().is_none().then_some(first)
}

fn family_of(tool: &str) -> Family {
    match tool {
        DATA_TOOL => Family::Data,
        COMMENTS_TOOL => Family::Comments,
        CHECK_TOOL => Family::Check,
        _ => Family::Artifact,
    }
}

fn default_action(tool: &str) -> &'static str {
    match family_of(tool) {
        Family::Artifact => "publish",
        Family::Data => "get",
        Family::Comments => "comments",
        Family::Check => "verify",
    }
}

/// The artifact family, sharing one service.
#[derive(Debug, Clone)]
pub struct ArtifactTools {
    service: Arc<ArtifactService>,
}

impl ArtifactTools {
    /// Build the family over a service.
    #[must_use]
    pub const fn new(service: Arc<ArtifactService>) -> Self {
        Self { service }
    }

    /// The four tools, ready to register.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(ArtifactTool::new(Arc::clone(&self.service), ARTIFACT_TOOL)),
            Arc::new(ArtifactTool::new(Arc::clone(&self.service), DATA_TOOL)),
            Arc::new(ArtifactTool::new(Arc::clone(&self.service), COMMENTS_TOOL)),
            Arc::new(ArtifactTool::new(Arc::clone(&self.service), CHECK_TOOL)),
        ]
    }
}

/// One tool of the family.
#[derive(Debug, Clone)]
pub struct ArtifactTool {
    service: Arc<ArtifactService>,
    name: &'static str,
}

impl ArtifactTool {
    /// Build one member of the family.
    #[must_use]
    pub const fn new(service: Arc<ArtifactService>, name: &'static str) -> Self {
        Self { service, name }
    }

    fn family(&self) -> Family {
        family_of(self.name)
    }

    fn dispatch(&self, arguments: &Value, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
        let action_name = action_of(arguments);
        let resolved = resolve_action(self.name, &action_name);

        // A modern action addressed to the wrong tool is a mistake worth
        // naming. A legacy one is not a mistake — it is how the action was
        // spelled before the family was split, so it goes where it now lives.
        if resolved.family != self.family() && !resolved.legacy {
            return Err(ToolError::InvalidInput {
                message: format!(
                    "action '{action_name}' belongs to {} — call it there",
                    tool_for(resolved.family)
                ),
            });
        }

        match resolved.family {
            Family::Artifact => self.page_action(&resolved.action, arguments, ctx),
            Family::Data => self.data_action(&resolved.action, arguments),
            Family::Comments => self.comment_action(&resolved.action, arguments),
            Family::Check => self.check_action(&resolved.action, arguments),
        }
    }

    fn page_action(
        &self,
        action: &str,
        arguments: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<String, ToolError> {
        match action {
            "publish" => self.publish(arguments, ctx),
            "list" => self.list(),
            "read" => self.read(arguments),
            "version" => self.versions(arguments),
            "delete" => self.delete(arguments),
            "list_files" => self.list_files(arguments),
            "read_file" => self.read_file(arguments),
            "upload_asset" => self.upload_asset(arguments, ctx),
            "list_assets" => self.list_assets(arguments),
            "read_asset" => self.read_asset(arguments),
            "delete_asset" => self.delete_asset(arguments),
            other => Err(unknown_action(other, ARTIFACT_TOOL)),
        }
    }

    fn publish(&self, arguments: &Value, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
        let title =
            string_arg(arguments, "title").ok_or_else(|| invalid("publish needs a title"))?;

        let slug = match string_arg(arguments, "url").or_else(|| string_arg(arguments, "slug")) {
            Some(reference) => Some(
                Slug::parse(&reference)
                    .ok_or_else(|| invalid(&format!("'{reference}' is not an artifact url")))?,
            ),
            None => None,
        };

        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        if let Some(entry) = string_arg(arguments, "file_path") {
            let path = ctx.cwd.join(&entry);
            let contents = std::fs::read(&path).map_err(|error| ToolError::InvalidInput {
                message: format!("cannot read '{entry}': {error}"),
            })?;
            files.insert(ENTRY_FILE.to_string(), contents);
        }
        if let Some(map) = arguments.get("files").and_then(Value::as_object) {
            for (path, contents) in map {
                let Some(text) = contents.as_str() else {
                    return Err(invalid(&format!("'{path}' needs text contents")));
                };
                files.insert(path.clone(), text.as_bytes().to_vec());
            }
        }
        if files.is_empty() {
            return Err(invalid(
                "publish needs either file_path (the page) or files (path → contents)",
            ));
        }

        let manifest = self
            .service
            .store()
            .publish(slug, &Page { title, files })
            .map_err(store_error)?;

        let slug = Slug::parse(&manifest.slug).ok_or_else(|| internal("bad slug"))?;
        let url = self.service.url(&slug);

        Ok(format!(
            "Published '{}' ({url})\nVersion {} holds {} file(s).\n{}",
            manifest.title,
            manifest.current_version,
            manifest.current().map_or(0, |version| version.files.len()),
            attached_marker(&url, Some(&manifest.title), true)
        ))
    }

    fn list(&self) -> Result<String, ToolError> {
        use std::fmt::Write as _;

        let artifacts = self.service.store().list().map_err(store_error)?;
        if artifacts.is_empty() {
            return Ok("No artifacts have been published.".into());
        }

        let mut out = String::new();
        for manifest in artifacts {
            let Some(slug) = Slug::parse(&manifest.slug) else {
                continue;
            };
            let _ = writeln!(
                out,
                "{}\t{}\t{} file(s)\t{}",
                self.service.url(&slug),
                manifest.title,
                manifest.current().map_or(0, |version| version.files.len()),
                manifest.current_version
            );
        }
        Ok(out.trim_end().to_string())
    }

    fn read(&self, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;
        let manifest = self.service.store().manifest(&slug).map_err(store_error)?;
        let url = self.service.url(&slug);

        let contents = self
            .service
            .store()
            .read_file(&slug, ENTRY_FILE, None)
            .map_err(store_error)?;
        let page = String::from_utf8_lossy(&contents);

        Ok(format!(
            "{} {url}\nVersion {} holds {} file(s): {}\n\n{}",
            attached_marker(&url, Some(&manifest.title), false),
            manifest.current_version,
            manifest.current().map_or(0, |version| version.files.len()),
            manifest
                .current()
                .map(|version| version
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default(),
            truncate(&page, MAX_RESULT_SIZE_CHARS)
        ))
    }

    fn versions(&self, arguments: &Value) -> Result<String, ToolError> {
        use std::fmt::Write as _;

        let slug = Self::slug_arg(arguments)?;
        let manifest = self.service.store().manifest(&slug).map_err(store_error)?;

        if let Some(label) = string_arg(arguments, "version") {
            let version = manifest
                .versions
                .iter()
                .find(|version| version.label == label)
                .ok_or_else(|| invalid(&format!("no version '{label}'")))?;
            let contents = self
                .service
                .store()
                .read_file(&slug, ENTRY_FILE, Some(&version.label))
                .map_err(store_error)?;
            return Ok(format!(
                "{}: {}\n\n{}",
                version.label,
                version.title,
                truncate(&String::from_utf8_lossy(&contents), MAX_RESULT_SIZE_CHARS)
            ));
        }

        let mut out = format!(
            "{} (current {})\n",
            manifest.title, manifest.current_version
        );
        for version in &manifest.versions {
            let _ = writeln!(
                out,
                "{}\t{}\t{} file(s)",
                version.label,
                version.created_at,
                version.files.len()
            );
        }
        Ok(out.trim_end().to_string())
    }

    fn delete(&self, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;
        let url = self.service.url(&slug);
        let removed = self.service.store().delete(&slug).map_err(store_error)?;

        if removed {
            Ok(format!(
                "Deleted {url}.\n{}",
                crate::markers::deleted_marker(&url)
            ))
        } else {
            Err(invalid(&format!("no artifact at {url}")))
        }
    }

    fn list_files(&self, arguments: &Value) -> Result<String, ToolError> {
        use std::fmt::Write as _;

        let slug = Self::slug_arg(arguments)?;
        let manifest = self.service.store().manifest(&slug).map_err(store_error)?;
        let version = manifest
            .current()
            .ok_or_else(|| invalid("this artifact has no version"))?;

        let mut out = String::new();
        for file in &version.files {
            let _ = writeln!(out, "{}\t{} bytes", file.path, file.bytes);
        }
        Ok(out.trim_end().to_string())
    }

    fn read_file(&self, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;
        let path = string_arg(arguments, "path").unwrap_or_else(|| ENTRY_FILE.to_string());
        let version = string_arg(arguments, "version");

        let contents = self
            .service
            .store()
            .read_file(&slug, &path, version.as_deref())
            .map_err(store_error)?;

        Ok(truncate(
            &String::from_utf8_lossy(&contents),
            MAX_RESULT_SIZE_CHARS,
        ))
    }

    fn upload_asset(&self, arguments: &Value, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;

        let contents = if let Some(path) = string_arg(arguments, "path") {
            std::fs::read(ctx.cwd.join(&path)).map_err(|error| ToolError::InvalidInput {
                message: format!("cannot read '{path}': {error}"),
            })?
        } else if let Some(text) = string_arg(arguments, "content") {
            text.as_bytes().to_vec()
        } else {
            return Err(invalid("upload_asset needs path or content"));
        };

        let asset_id = self
            .service
            .upload_asset(&slug, &contents)
            .map_err(store_error)?;
        Ok(format!(
            "Stored asset {asset_id} ({} bytes)",
            contents.len()
        ))
    }

    fn list_assets(&self, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;
        let assets = self.service.list_assets(&slug).map_err(store_error)?;
        if assets.is_empty() {
            return Ok("This artifact has no assets.".into());
        }
        Ok(assets.join("\n"))
    }

    fn read_asset(&self, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;
        let asset_id = string_arg(arguments, "asset_id")
            .ok_or_else(|| invalid("read_asset needs an asset_id"))?;
        let contents = self
            .service
            .read_asset(&slug, &asset_id)
            .map_err(store_error)?;

        // An asset is usually an image; hand back what can be read as text and
        // say how big the rest is rather than dumping bytes into the context.
        match String::from_utf8(contents) {
            Ok(text) => Ok(truncate(&text, MAX_RESULT_SIZE_CHARS)),
            Err(error) => Ok(format!(
                "Asset {asset_id} is binary ({} bytes)",
                error.into_bytes().len()
            )),
        }
    }

    fn delete_asset(&self, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;
        let asset_id = string_arg(arguments, "asset_id")
            .ok_or_else(|| invalid("delete_asset needs an asset_id"))?;
        let removed = self
            .service
            .delete_asset(&slug, &asset_id)
            .map_err(store_error)?;
        Ok(if removed {
            format!("Deleted asset {asset_id}")
        } else {
            format!("Asset {asset_id} was not there")
        })
    }

    fn data_action(&self, action: &str, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;
        let db = self.service.db(&slug);

        match action {
            "get" => {
                let (collection, doc_id) = Self::doc_args(arguments)?;
                let document = db
                    .get(&collection, &doc_id)
                    .map_err(|error| db_error(&error))?;
                Ok(truncate(
                    &serde_json::to_string_pretty(&json!({
                        "id": document.id,
                        "version": document.version,
                        "data": document.data,
                    }))
                    .unwrap_or_default(),
                    DB_READ_MAX_RESULT_SIZE_CHARS,
                ))
            }
            "list" => {
                let collection = Self::collection_arg(arguments)?;
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
                let cursor = string_arg(arguments, "cursor");
                let page = db
                    .list(&collection, limit, cursor.as_deref())
                    .map_err(|error| db_error(&error))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "documents": page.documents,
                    "next_cursor": page.next_cursor,
                }))
                .unwrap_or_default())
            }
            "query" => {
                let collection = Self::collection_arg(arguments)?;
                let mut equals = Vec::new();
                if let Some(where_clause) = arguments.get("where").and_then(Value::as_object) {
                    for (field, value) in where_clause {
                        equals.push((field.clone(), value.clone()));
                    }
                } else if let Some(field) = string_arg(arguments, "field") {
                    let value = arguments
                        .get("value")
                        .cloned()
                        .ok_or_else(|| invalid("query with a field needs a value"))?;
                    equals.push((field, value));
                }
                let documents = db
                    .query(&collection, &equals)
                    .map_err(|error| db_error(&error))?;
                Ok(truncate(
                    &serde_json::to_string_pretty(&documents).unwrap_or_default(),
                    DB_READ_MAX_RESULT_SIZE_CHARS,
                ))
            }
            "set" | "update" | "delete" | "str_replace" => {
                let write = Self::write_arg(action, arguments)?;
                let entry = db.apply(&write).map_err(|error| db_error(&error))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "collection": entry.collection,
                    "doc_id": entry.doc_id,
                    "version": entry.version,
                }))
                .unwrap_or_default())
            }
            "batch" => {
                let writes = arguments
                    .get("writes")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("batch needs a writes array"))?;

                let mut parsed = Vec::with_capacity(writes.len());
                for write in writes {
                    let op = string_arg(write, "op").unwrap_or_else(|| "set".to_string());
                    parsed.push(Self::write_arg(&op, write)?);
                }

                let outcome = db.batch(&parsed).map_err(|error| db_error(&error))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "results": outcome.results,
                }))
                .unwrap_or_default())
            }
            other => Err(unknown_action(other, DATA_TOOL)),
        }
    }

    fn comment_action(&self, action: &str, arguments: &Value) -> Result<String, ToolError> {
        let slug = Self::slug_arg(arguments)?;

        match action {
            "comments" | "read" | "list" => {
                let comments = self.service.comments(&slug).map_err(store_error)?;
                if comments.is_empty() {
                    return Ok("No comments on this artifact.".into());
                }
                Ok(serde_json::to_string_pretty(&comments).unwrap_or_default())
            }
            "reply" | "add" => {
                let body = string_arg(arguments, "body")
                    .or_else(|| string_arg(arguments, "prompt"))
                    .ok_or_else(|| invalid("reply needs a body"))?;
                let comment = self
                    .service
                    .add_comment(&slug, "assistant", &body)
                    .map_err(store_error)?;
                Ok(format!("Added comment {}.", comment.id))
            }
            "resolve" => {
                let comment_id = string_arg(arguments, "comment_id")
                    .ok_or_else(|| invalid("resolve needs a comment_id"))?;
                let resolved = self
                    .service
                    .resolve_comment(&slug, &comment_id)
                    .map_err(store_error)?;
                Ok(if resolved {
                    format!("Resolved {comment_id}.")
                } else {
                    format!("No comment {comment_id}.")
                })
            }
            other => Err(unknown_action(other, COMMENTS_TOOL)),
        }
    }

    fn check_action(&self, action: &str, arguments: &Value) -> Result<String, ToolError> {
        if action != "verify" {
            return Err(unknown_action(action, CHECK_TOOL));
        }
        let slug = Self::slug_arg(arguments)?;
        let report = self.service.verify(&slug).map_err(store_error)?;

        if report.is_intact() {
            return Ok(format!(
                "{} is intact: {} file(s) at {}.",
                self.service.url(&slug),
                report.files,
                report.version
            ));
        }
        Ok(format!(
            "{} has problems at {}:\n{}",
            self.service.url(&slug),
            report.version,
            report.problems.join("\n")
        ))
    }

    fn slug_arg(arguments: &Value) -> Result<Slug, ToolError> {
        let reference = string_arg(arguments, "url")
            .or_else(|| string_arg(arguments, "slug"))
            .or_else(|| string_arg(arguments, "artifact"))
            .ok_or_else(|| invalid("this action needs the artifact's url"))?;
        Slug::parse(&reference)
            .ok_or_else(|| invalid(&format!("'{reference}' is not an artifact url")))
    }

    fn collection_arg(arguments: &Value) -> Result<String, ToolError> {
        string_arg(arguments, "collection")
            .or_else(|| string_arg(arguments, "path"))
            .ok_or_else(|| invalid("this action needs a collection"))
    }

    fn doc_args(arguments: &Value) -> Result<(String, String), ToolError> {
        let collection = Self::collection_arg(arguments)?;
        let doc_id = string_arg(arguments, "doc_id")
            .or_else(|| string_arg(arguments, "document"))
            .ok_or_else(|| invalid("this action needs a doc_id"))?;
        Ok((collection, doc_id))
    }

    fn write_arg(op: &str, arguments: &Value) -> Result<DbWrite, ToolError> {
        let collection = Self::collection_arg(arguments)?;
        let doc_id = string_arg(arguments, "doc_id")
            .or_else(|| string_arg(arguments, "document"))
            .ok_or_else(|| invalid("a write needs a doc_id"))?;
        let if_version = arguments.get("if_version").and_then(Value::as_u64);

        Ok(match op {
            "set" => DbWrite::Set {
                collection,
                doc_id,
                data: arguments.get("data").cloned().unwrap_or(Value::Null),
                if_version,
            },
            "update" => DbWrite::Update {
                collection,
                doc_id,
                data: arguments.get("data").cloned().unwrap_or(Value::Null),
                if_version,
            },
            "delete" => DbWrite::Delete {
                collection,
                doc_id,
                if_version,
            },
            "str_replace" => DbWrite::StrReplace {
                collection,
                doc_id,
                field: string_arg(arguments, "field")
                    .ok_or_else(|| invalid("str_replace needs a field"))?,
                old: string_arg(arguments, "old_str")
                    .or_else(|| string_arg(arguments, "old"))
                    .ok_or_else(|| invalid("str_replace needs old_str"))?,
                new: string_arg(arguments, "new_str")
                    .or_else(|| string_arg(arguments, "new"))
                    .unwrap_or_default(),
                replace_all: arguments
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                if_version,
            },
            other => return Err(unknown_action(other, DATA_TOOL)),
        })
    }
}

#[async_trait::async_trait]
impl Tool for ArtifactTool {
    fn descriptor(&self) -> ToolDescriptor {
        let (description, read_only) = match self.family() {
            Family::Artifact => ("render an HTML file to a web page and read it back", false),
            Family::Data => ("read and write a published page's shared database", false),
            Family::Comments => ("read and reply to comments on a published page", false),
            Family::Check => ("verify that a published page is intact", true),
        };

        ToolDescriptor {
            name: self.name.to_string(),
            tier: ToolTier::Baseline,
            input_schema: json!({
                "type": "object",
                "description": description,
                "properties": {
                    "action": { "type": "string", "description": "what to do; defaults to publish (Artifact), get (ArtifactData), comments (ArtifactComments), verify (ArtifactCheck)" },
                    "title": { "type": "string" },
                    "url": { "type": "string", "description": "the artifact's URL" },
                    "file_path": { "type": "string" },
                    "files": { "type": "object" },
                    "path": { "type": "string" },
                    "version": { "type": "string" },
                    "collection": { "type": "string" },
                    "doc_id": { "type": "string" },
                    "data": {},
                    "if_version": { "type": "integer" },
                    "where": { "type": "object" },
                    "writes": { "type": "array" },
                    "asset_id": { "type": "string" },
                    "content": { "type": "string" },
                    "body": { "type": "string" },
                    "comment_id": { "type": "string" }
                }
            }),
            output_schema: json!({ "type": "object", "properties": { "output": { "type": "string" } } }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: read_only,
            // A publish renumbers versions, and a database write merges into a
            // document someone may be reading; neither may overlap itself.
            is_concurrency_safe: read_only,
            interrupt_behavior: InterruptBehavior::RunToCompletion,
        }
    }

    /// The flags are per action, not per tool — see [`is_mutating`]. The action
    /// is resolved first, so a legacy spelling (`write_db`, `preview`) answers
    /// for the action it now names.
    fn rule_exemptions_for(&self, input: &Value) -> ToolRuleExemptions {
        let resolved = resolve_action(self.name, &action_of(input));
        if is_mutating(resolved.family, &resolved.action) {
            ToolRuleExemptions {
                ignores_whole_tool_allow_rule: true,
                suppresses_always_allow_rule: true,
            }
        } else {
            ToolRuleExemptions::NONE
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let output = self.dispatch(&input.arguments, &ctx)?;
        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

fn action_of(arguments: &Value) -> String {
    string_arg(arguments, "action").unwrap_or_default()
}

const fn tool_for(family: Family) -> &'static str {
    match family {
        Family::Artifact => ARTIFACT_TOOL,
        Family::Data => DATA_TOOL,
        Family::Comments => COMMENTS_TOOL,
        Family::Check => CHECK_TOOL,
    }
}

fn string_arg(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidInput {
        message: message.to_string(),
    }
}

fn internal(message: &str) -> ToolError {
    ToolError::Execution {
        name: ARTIFACT_TOOL.to_string(),
        message: message.to_string(),
    }
}

fn unknown_action(action: &str, tool: &str) -> ToolError {
    invalid(&format!("'{action}' is not an action {tool} has"))
}

fn db_error(error: &crate::db::DbError) -> ToolError {
    invalid(&error.to_string())
}

fn store_error(error: crate::store::StoreError) -> ToolError {
    match error {
        crate::store::StoreError::Invalid(message) => invalid(&message),
        crate::store::StoreError::NotFound(message) => invalid(&format!("no {message}")),
        crate::store::StoreError::Io(message) => ToolError::Execution {
            name: ARTIFACT_TOOL.to_string(),
            message,
        },
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }

    // Cut on a character boundary, not a byte one.
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated at {limit} characters)", &text[..end])
}

/// The document id standing for the current viewer, re-exported for callers
/// that build a path by hand.
pub const CURRENT_USER_SEGMENT: &str = CURRENT_USER;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tools() -> (Arc<ArtifactService>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::ArtifactStore::open(dir.path());
        (
            Arc::new(ArtifactService::new(store, "http://127.0.0.1:8722", "u_1")),
            dir,
        )
    }

    fn ctx<'a>(cwd: &'a Path) -> ToolContext<'a> {
        ToolContext::new(coderex_tools::ExecutionContext::Normal, cwd)
    }

    async fn call(tool: &ArtifactTool, arguments: Value, cwd: &Path) -> Result<String, ToolError> {
        tool.execute(
            ToolInput {
                call_id: "call-1".into(),
                name: tool.name.to_string(),
                arguments,
            },
            ctx(cwd),
        )
        .await
        .map(|output| output.output)
    }

    fn page_tool(service: &Arc<ArtifactService>, name: &'static str) -> ArtifactTool {
        ArtifactTool::new(Arc::clone(service), name)
    }

    #[tokio::test]
    async fn publishing_from_files_returns_a_url_and_a_marker() {
        let (service, dir) = tools();
        let tool = page_tool(&service, ARTIFACT_TOOL);

        let output = call(
            &tool,
            json!({
                "title": "Report",
                "files": { "index.html": "<html><body>published</body></html>" }
            }),
            dir.path(),
        )
        .await
        .unwrap();

        assert!(
            output.contains("http://127.0.0.1:8722/artifact/"),
            "{output}"
        );
        assert!(output.contains("<artifact-attached"), "{output}");
        assert!(output.contains("own"), "{output}");
        assert!(output.contains("Version v1"), "{output}");

        let url = output
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();
        assert!(Slug::parse(&url).is_some());
    }

    #[tokio::test]
    async fn publishing_from_a_file_on_disk_uses_it_as_the_page() {
        let (service, dir) = tools();
        let tool = page_tool(&service, ARTIFACT_TOOL);
        std::fs::write(dir.path().join("page.html"), "<html>from disk</html>").unwrap();

        let output = call(
            &tool,
            json!({"title": "From disk", "file_path": "page.html"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(output.contains("Version v1"), "{output}");

        let url = output
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')');
        let slug = Slug::parse(url).unwrap();
        let served = service.store().read_file(&slug, ENTRY_FILE, None).unwrap();
        assert_eq!(String::from_utf8_lossy(&served), "<html>from disk</html>");
    }

    #[tokio::test]
    async fn a_publish_without_a_page_is_refused() {
        let (service, dir) = tools();
        let tool = page_tool(&service, ARTIFACT_TOOL);

        assert!(
            call(&tool, json!({"title": "Nothing"}), dir.path())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn republishing_adds_a_version_and_reading_names_it() {
        let (service, dir) = tools();
        let tool = page_tool(&service, ARTIFACT_TOOL);

        let first = call(
            &tool,
            json!({"title": "T", "files": {"index.html": "<html>one</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = first
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        call(
            &tool,
            json!({"url": url, "title": "T", "files": {"index.html": "<html>two</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();

        let read = call(&tool, json!({"action": "read", "url": url}), dir.path())
            .await
            .unwrap();
        assert!(read.contains("<html>two</html>"), "{read}");
        assert!(read.contains("Version v2"), "{read}");

        let versions = call(&tool, json!({"action": "version", "url": url}), dir.path())
            .await
            .unwrap();
        assert!(versions.contains("v1"), "{versions}");
        assert!(versions.contains("v2"), "{versions}");

        let old = call(
            &tool,
            json!({"action": "version", "url": url, "version": "v1"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(old.contains("<html>one</html>"), "{old}");
    }

    #[tokio::test]
    async fn list_and_delete_work_on_published_pages() {
        let (service, dir) = tools();
        let tool = page_tool(&service, ARTIFACT_TOOL);

        let empty = call(&tool, json!({"action": "list"}), dir.path())
            .await
            .unwrap();
        assert!(empty.contains("No artifacts"), "{empty}");

        let published = call(
            &tool,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        let listed = call(&tool, json!({"action": "list"}), dir.path())
            .await
            .unwrap();
        assert!(listed.contains(&url), "{listed}");

        let deleted = call(&tool, json!({"action": "delete", "url": url}), dir.path())
            .await
            .unwrap();
        assert!(deleted.contains("<artifact-deleted"), "{deleted}");
        assert!(
            call(&tool, json!({"action": "delete", "url": url}), dir.path())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused() {
        let (service, dir) = tools();
        let tool = page_tool(&service, ARTIFACT_TOOL);
        assert!(
            call(
                &tool,
                json!({"action": "read", "url": "nonsense"}),
                dir.path()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn assets_are_uploaded_listed_read_and_deleted() {
        let (service, dir) = tools();
        let tool = page_tool(&service, ARTIFACT_TOOL);

        let published = call(
            &tool,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        let uploaded = call(
            &tool,
            json!({"action": "upload_asset", "url": url, "content": "hello asset"}),
            dir.path(),
        )
        .await
        .unwrap();
        let asset_id = uploaded.split_whitespace().nth(2).unwrap().to_string();

        let listed = call(
            &tool,
            json!({"action": "list_assets", "url": url}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(listed.contains(&asset_id), "{listed}");

        let read = call(
            &tool,
            json!({"action": "read_asset", "url": url, "asset_id": asset_id}),
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(read, "hello asset");

        call(
            &tool,
            json!({"action": "delete_asset", "url": url, "asset_id": asset_id}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(
            call(
                &tool,
                json!({"action": "read_asset", "url": url, "asset_id": asset_id}),
                dir.path()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn the_database_round_trips_through_the_tool() {
        let (service, dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);
        let data = page_tool(&service, DATA_TOOL);

        let published = call(
            &page,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        let written = call(
            &data,
            json!({
                "url": url,
                "action": "set",
                "collection": "boards",
                "doc_id": "b1",
                "data": {"title": "first"}
            }),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(written.contains("\"version\": 1"), "{written}");

        let read = call(
            &data,
            json!({"url": url, "collection": "boards", "doc_id": "b1"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(read.contains("first"), "{read}");
        assert!(read.contains("\"version\": 1"), "{read}");

        // A stale pin is refused and says what the version is now.
        let stale = call(
            &data,
            json!({
                "url": url,
                "action": "set",
                "collection": "boards",
                "doc_id": "b1",
                "data": {"title": "clobber"},
                "if_version": 5
            }),
            dir.path(),
        )
        .await;
        let message = stale.unwrap_err().to_string();
        assert!(message.contains("version mismatch"), "{message}");
        assert!(message.contains("found 1"), "{message}");

        let listed = call(
            &data,
            json!({"url": url, "action": "list", "collection": "boards"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(listed.contains("b1"), "{listed}");

        let batched = call(
            &data,
            json!({
                "url": url,
                "action": "batch",
                "writes": [
                    {"op": "set", "collection": "boards", "doc_id": "b2", "data": {"n": 2}},
                    {"op": "delete", "collection": "boards", "doc_id": "b1"}
                ]
            }),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(batched.contains("b2"), "{batched}");
    }

    #[tokio::test]
    async fn the_even_segment_refusal_reaches_the_caller() {
        let (service, dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);
        let data = page_tool(&service, DATA_TOOL);

        let published = call(
            &page,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        let error = call(
            &data,
            json!({"url": url, "action": "list", "collection": "boards/b1"}),
            dir.path(),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("document path, not a collection"), "{error}");
        assert!(error.contains("doc_id"), "{error}");
    }

    #[tokio::test]
    async fn the_data_tool_resolves_the_viewer_subtree() {
        let (service, dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);
        let data = page_tool(&service, DATA_TOOL);

        let published = call(
            &page,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        call(
            &data,
            json!({
                "url": url,
                "action": "set",
                "collection": "data/users/me/decks/cards",
                "doc_id": "c1",
                "data": {"front": "q"}
            }),
            dir.path(),
        )
        .await
        .unwrap();

        let path = service
            .store()
            .db_dir(&Slug::parse(&url).unwrap())
            .join("data/users/u_1/decks/cards/c1.json");
        assert!(path.is_file(), "expected {path:?}");
    }

    #[tokio::test]
    async fn comments_are_added_listed_and_resolved() {
        let (service, dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);
        let comments = page_tool(&service, COMMENTS_TOOL);

        let published = call(
            &page,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        assert!(
            call(&comments, json!({"url": url}), dir.path())
                .await
                .unwrap()
                .contains("No comments")
        );

        let added = call(
            &comments,
            json!({"url": url, "action": "reply", "body": "nice page"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(added.contains("c1"), "{added}");

        let listed = call(&comments, json!({"url": url}), dir.path())
            .await
            .unwrap();
        assert!(listed.contains("nice page"), "{listed}");

        let resolved = call(
            &comments,
            json!({"url": url, "action": "resolve", "comment_id": "c1"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(resolved.contains("Resolved"), "{resolved}");
    }

    #[tokio::test]
    async fn check_reports_a_page_that_is_intact() {
        let (service, dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);
        let check = page_tool(&service, CHECK_TOOL);

        let published = call(
            &page,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        let report = call(&check, json!({"url": url}), dir.path()).await.unwrap();
        assert!(report.contains("is intact"), "{report}");
    }

    #[test]
    fn legacy_actions_resolve_to_the_tool_they_now_belong_to() {
        assert_eq!(
            resolve_action(ARTIFACT_TOOL, "read_db"),
            ResolvedAction {
                family: Family::Data,
                action: "get".into(),
                legacy: true,
            }
        );
        assert_eq!(
            resolve_action(ARTIFACT_TOOL, "write_db").family,
            Family::Data
        );
        assert_eq!(
            resolve_action(ARTIFACT_TOOL, "verify").family,
            Family::Check
        );
        assert_eq!(
            resolve_action(ARTIFACT_TOOL, "preview").family,
            Family::Check
        );
    }

    #[test]
    fn an_empty_action_is_the_tools_default() {
        assert_eq!(resolve_action(ARTIFACT_TOOL, "").action, "publish");
        assert_eq!(resolve_action(DATA_TOOL, "").action, "get");
        assert_eq!(resolve_action(COMMENTS_TOOL, "").action, "comments");
        assert_eq!(resolve_action(CHECK_TOOL, "").action, "verify");
        assert_eq!(resolve_action("", "").action, "publish");
    }

    #[tokio::test]
    async fn a_legacy_action_on_the_page_tool_reaches_the_database() {
        let (service, dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);

        let published = call(
            &page,
            json!({"title": "T", "files": {"index.html": "<html>x</html>"}}),
            dir.path(),
        )
        .await
        .unwrap();
        let url = published
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();

        // `read_db` on Artifact is what older prompts call; it reaches the
        // database rather than being refused.
        call(
            &page,
            json!({
                "url": url,
                "action": "write_db",
                "collection": "boards",
                "doc_id": "b1",
                "data": {"title": "written through the old name"}
            }),
            dir.path(),
        )
        .await
        .unwrap();

        let read = call(
            &page,
            json!({"url": url, "action": "read_db", "collection": "boards", "doc_id": "b1"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(read.contains("written through the old name"), "{read}");

        // `verify` likewise.
        let checked = call(&page, json!({"url": url, "action": "verify"}), dir.path())
            .await
            .unwrap();
        assert!(checked.contains("is intact"), "{checked}");
    }

    #[tokio::test]
    async fn an_action_from_another_tool_is_refused_with_the_tool_to_use() {
        let (service, dir) = tools();
        let check = page_tool(&service, CHECK_TOOL);
        let page = page_tool(&service, ARTIFACT_TOOL);

        let error = call(&check, json!({"action": "publish"}), dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(ARTIFACT_TOOL), "{error}");

        // And a database action addressed to the page tool says the same.
        let error = call(&page, json!({"action": "set"}), dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(DATA_TOOL), "{error}");

        // The legacy names are not refused: they are forwarded.
        assert_eq!(resolve_action(ARTIFACT_TOOL, "read_db").legacy, true);
        assert_eq!(resolve_action(ARTIFACT_TOOL, "publish").legacy, false);
    }

    #[tokio::test]
    async fn an_unknown_action_is_refused() {
        let (service, dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);
        let error = call(&page, json!({"action": "teleport"}), dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("teleport"), "{error}");
    }

    /// The mutating actions refuse a whole-tool allow rule; the reads honour
    /// it. This split is why the flags are answered per call rather than
    /// declared once for the tool.
    #[test]
    fn mutating_actions_carry_the_rule_exemptions_and_reads_do_not() {
        let (service, _dir) = tools();
        let exemptions = |name: &'static str, action: &str| {
            page_tool(&service, name).rule_exemptions_for(&json!({ "action": action }))
        };

        let cases: [(&'static str, &[&str], &[&str]); 4] = [
            (
                ARTIFACT_TOOL,
                &["publish", "delete", "upload_asset", "delete_asset"],
                &[
                    "list",
                    "read",
                    "version",
                    "list_files",
                    "read_file",
                    "list_assets",
                    "read_asset",
                ],
            ),
            (
                DATA_TOOL,
                &["set", "update", "delete", "str_replace", "batch"],
                &["get", "list", "query"],
            ),
            (
                COMMENTS_TOOL,
                &["add", "reply", "resolve"],
                &["comments", "read", "list"],
            ),
            (CHECK_TOOL, &[], &["verify"]),
        ];

        for (tool_name, mutating, reads) in cases {
            for action in mutating {
                let declared = exemptions(tool_name, action);
                assert!(
                    declared.ignores_whole_tool_allow_rule,
                    "{tool_name}/{action} must refuse a whole-tool allow rule"
                );
                assert!(
                    declared.suppresses_always_allow_rule,
                    "{tool_name}/{action} must not offer a session-wide allow"
                );
            }
            for action in reads {
                assert_eq!(
                    exemptions(tool_name, action),
                    ToolRuleExemptions::NONE,
                    "{tool_name}/{action} is an ordinary action"
                );
            }
        }

        // The page tool's default action is `publish`, and publishing mutates.
        assert!(
            page_tool(&service, ARTIFACT_TOOL)
                .rule_exemptions_for(&json!({}))
                .ignores_whole_tool_allow_rule,
            "a publish with no action named must refuse the shortcut"
        );
    }

    /// A legacy action answers for the action it now names, wherever it was
    /// addressed: `write_db` writes, `read_db` and `verify` do not.
    #[test]
    fn legacy_spellings_answer_for_the_action_they_now_name() {
        let (service, _dir) = tools();
        let page = page_tool(&service, ARTIFACT_TOOL);
        let data = page_tool(&service, DATA_TOOL);

        for tool in [&page, &data] {
            assert!(
                tool.rule_exemptions_for(&json!({ "action": "write_db" }))
                    .ignores_whole_tool_allow_rule
            );
            assert_eq!(
                tool.rule_exemptions_for(&json!({ "action": "read_db" })),
                ToolRuleExemptions::NONE
            );
        }
        assert_eq!(
            page.rule_exemptions_for(&json!({ "action": "verify" })),
            ToolRuleExemptions::NONE
        );
        assert_eq!(
            page.rule_exemptions_for(&json!({ "action": "preview" })),
            ToolRuleExemptions::NONE
        );
    }

    /// The registry is what the engine asks, so the family's answer has to
    /// survive the trip through it — and a name nothing declares stays nothing.
    #[test]
    fn the_registry_answers_the_familys_exemptions_per_action() {
        let (service, _dir) = tools();
        let mut registry = coderex_tools::ToolRegistry::new();
        for tool in ArtifactTools::new(Arc::clone(&service)).all() {
            registry.register(tool);
        }

        assert!(
            registry
                .rule_exemptions_for(ARTIFACT_TOOL, &json!({ "action": "publish" }))
                .ignores_whole_tool_allow_rule
        );
        assert_eq!(
            registry.rule_exemptions_for(ARTIFACT_TOOL, &json!({ "action": "read" })),
            ToolRuleExemptions::NONE
        );
        assert!(
            registry
                .rule_exemptions_for(DATA_TOOL, &json!({ "action": "set" }))
                .ignores_whole_tool_allow_rule
        );
        assert_eq!(
            registry.rule_exemptions_for(COMMENTS_TOOL, &json!({ "action": "comments" })),
            ToolRuleExemptions::NONE
        );
        assert_eq!(
            registry.rule_exemptions_for(CHECK_TOOL, &json!({ "action": "verify" })),
            ToolRuleExemptions::NONE
        );
        assert_eq!(
            registry.rule_exemptions_for("NotATool", &json!({ "action": "publish" })),
            ToolRuleExemptions::NONE
        );
    }

    #[test]
    fn truncation_cuts_on_a_character_boundary() {
        let text = "é".repeat(20);
        let cut = truncate(&text, 5);
        assert!(cut.starts_with("é"), "{cut}");
        assert!(cut.contains("truncated"), "{cut}");
    }

    #[test]
    fn the_family_has_four_distinct_names() {
        assert_eq!(FAMILY.len(), 4);
        let unique: std::collections::HashSet<_> = FAMILY.iter().collect();
        assert_eq!(unique.len(), 4);
    }
}
