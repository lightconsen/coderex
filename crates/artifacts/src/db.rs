//! The artifact database — collections and documents behind a published page.
//!
//! The shape is unusual and worth stating plainly, because getting it wrong
//! means writing to the wrong place:
//!
//! - A path is made of segments, at most 15.
//! - **An odd number of segments is a collection; an even number is a
//!   document.** `boards` is a collection, `boards/b1` is a document in it,
//!   `boards/b1/columns` is a collection inside that document.
//! - A document is addressed by its collection plus a document id, never by a
//!   full even-segment path — that path is how you *find* the document, not how
//!   you name it.
//! - `data/users/<id>` holds one viewer's private documents, and `me` stands
//!   for whoever is looking.
//!
//! Every document carries a version. A write may pin the version it expects
//! (`if_version`); if the document has moved on, the write fails and reports
//! what the version is now, so a caller can re-read rather than clobber
//! someone else's change.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::limits::{DB_BATCH_MAX_BYTES, DB_BATCH_MAX_WRITES};

/// Most segments a path may have.
pub const MAX_PATH_SEGMENTS: usize = 15;

/// Longest a single segment may be.
pub const MAX_SEGMENT_LEN: usize = 200;

/// The segment standing in for "whoever is looking".
pub const CURRENT_USER: &str = "me";

/// The prefix of the per-viewer private subtree.
pub const USER_PREFIX: &str = "data/users";

/// Default page size for a listing.
pub const DEFAULT_PAGE_LIMIT: usize = 100;

/// Largest page a listing may ask for.
pub const MAX_PAGE_LIMIT: usize = 1000;

/// Something wrong with a database call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DbError {
    /// The path or argument is not usable.
    #[error("{0}")]
    InvalidArgument(String),
    /// The document is not there.
    #[error("no document at '{collection}/{doc_id}'")]
    NotFound {
        /// Collection path.
        collection: String,
        /// Document id.
        doc_id: String,
    },
    /// The pinned version is not the current one.
    #[error("version mismatch on '{collection}/{doc_id}': expected {expected}, found {found}")]
    VersionMismatch {
        /// Collection path.
        collection: String,
        /// Document id.
        doc_id: String,
        /// The version the caller pinned.
        expected: u64,
        /// The version the document is at now.
        found: u64,
    },
    /// The request was larger than a batch may be.
    #[error("{0}")]
    TooLarge(String),
    /// The store itself failed.
    #[error("artifact database store failed: {0}")]
    Store(String),
}

/// A stored document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Document id within its collection.
    pub id: String,
    /// The collection it lives in.
    pub collection: String,
    /// Monotonic version, incremented on every write.
    pub version: u64,
    /// The document body.
    pub data: Value,
}

/// One document from a listing, without its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSummary {
    /// Document id.
    pub id: String,
    /// Its current version.
    pub version: u64,
}

/// A page of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    /// The documents on this page.
    pub documents: Vec<DocumentSummary>,
    /// Cursor to pass back for the next page, when there is one.
    pub next_cursor: Option<String>,
}

/// A write to apply in a batch.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// Replace a document's body.
    Set {
        /// Collection path.
        collection: String,
        /// Document id.
        doc_id: String,
        /// New body.
        data: Value,
        /// Version the caller expects, if any.
        if_version: Option<u64>,
    },
    /// Merge fields into a document's body.
    Update {
        /// Collection path.
        collection: String,
        /// Document id.
        doc_id: String,
        /// Fields to merge.
        data: Value,
        /// Version the caller expects, if any.
        if_version: Option<u64>,
    },
    /// Remove a document.
    Delete {
        /// Collection path.
        collection: String,
        /// Document id.
        doc_id: String,
        /// Version the caller expects, if any.
        if_version: Option<u64>,
    },
    /// Replace a piece of text inside one string field, in place.
    StrReplace {
        /// Collection path.
        collection: String,
        /// Document id.
        doc_id: String,
        /// The field to edit.
        field: String,
        /// Text to find; must occur exactly once unless `replace_all`.
        old: String,
        /// Replacement text.
        new: String,
        /// Replace every occurrence instead of requiring one.
        replace_all: bool,
        /// Version the caller expects, if any.
        if_version: Option<u64>,
    },
}

impl Write {
    /// The document this write targets.
    #[must_use]
    pub fn target(&self) -> (&str, &str) {
        match self {
            Self::Set {
                collection, doc_id, ..
            }
            | Self::Update {
                collection, doc_id, ..
            }
            | Self::Delete {
                collection, doc_id, ..
            }
            | Self::StrReplace {
                collection, doc_id, ..
            } => (collection, doc_id),
        }
    }
}

/// What a batch did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOutcome {
    /// One entry per write, in order.
    pub results: Vec<BatchEntry>,
}

/// The result of one write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchEntry {
    /// Collection written to.
    pub collection: String,
    /// Document written.
    pub doc_id: String,
    /// The version after the write; `None` for a delete.
    pub version: Option<u64>,
}

/// A database belonging to one artifact.
#[derive(Debug, Clone)]
pub struct ArtifactDb {
    root: PathBuf,
    user_id: String,
}

impl ArtifactDb {
    /// Open the database rooted at `root`.
    ///
    /// `user_id` is what [`CURRENT_USER`] resolves to.
    #[must_use]
    pub fn open(root: impl Into<PathBuf>, user_id: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            user_id: user_id.into(),
        }
    }

    /// The directory this database writes to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read a document.
    pub fn get(&self, collection: &str, doc_id: &str) -> Result<Document, DbError> {
        let path = self.document_path(collection, doc_id)?;
        self.read_document(&path, collection, doc_id)
    }

    /// List a collection's documents, one page at a time.
    pub fn list(
        &self,
        collection: &str,
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<Page, DbError> {
        let dir = self.collection_dir(collection)?;
        let mut ids = self.document_ids(&dir)?;
        ids.sort();

        // The cursor names the first document of the next page, so a page
        // starts at it rather than after it.
        let skip = match cursor {
            Some(cursor) => ids.iter().position(|id| id == cursor).unwrap_or(ids.len()),
            None => 0,
        };

        let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT).min(MAX_PAGE_LIMIT);
        let mut documents = Vec::new();
        for id in ids.iter().skip(skip).take(limit) {
            let path = dir.join(format!("{id}.json"));
            let document = self.read_document(&path, collection, id)?;
            documents.push(DocumentSummary {
                id: id.clone(),
                version: document.version,
            });
        }

        let next_cursor = ids.get(skip + documents.len()).cloned();
        Ok(Page {
            documents,
            next_cursor,
        })
    }

    /// List documents whose body matches every field in `equals`.
    pub fn query(
        &self,
        collection: &str,
        equals: &[(String, Value)],
    ) -> Result<Vec<Document>, DbError> {
        let dir = self.collection_dir(collection)?;
        let mut ids = self.document_ids(&dir)?;
        ids.sort();

        let mut matches = Vec::new();
        for id in ids {
            let path = dir.join(format!("{id}.json"));
            let document = self.read_document(&path, collection, &id)?;
            if equals
                .iter()
                .all(|(field, expected)| document.data.get(field) == Some(expected))
            {
                matches.push(document);
            }
        }
        Ok(matches)
    }

    /// Apply writes as one unit.
    ///
    /// Every write is checked against the version it pinned first; if any of
    /// them has moved on, nothing is written and the mismatch is reported, so
    /// a caller cannot half-apply a batch over a document someone else edited.
    pub fn batch(&self, writes: &[Write]) -> Result<BatchOutcome, DbError> {
        if writes.len() > DB_BATCH_MAX_WRITES {
            return Err(DbError::TooLarge(format!(
                "a batch writes at most {DB_BATCH_MAX_WRITES} documents, got {}",
                writes.len()
            )));
        }
        let body_bytes: usize = writes
            .iter()
            .map(|write| match write {
                Write::Set { data, .. } | Write::Update { data, .. } => {
                    serde_json::to_vec(data).map_or(0, |bytes| bytes.len())
                }
                Write::StrReplace { old, new, .. } => old.len() + new.len(),
                Write::Delete { .. } => 0,
            })
            .sum();
        if body_bytes > DB_BATCH_MAX_BYTES {
            return Err(DbError::TooLarge(format!(
                "a batch body is at most {DB_BATCH_MAX_BYTES} bytes, got {body_bytes}"
            )));
        }

        // Check every pinned version before writing anything.
        for write in writes {
            let (collection, doc_id) = write.target();
            let Some(expected) = pinned_version(write) else {
                continue;
            };
            let found = match self.get(collection, doc_id) {
                Ok(document) => document.version,
                Err(DbError::NotFound { .. }) => 0,
                Err(other) => return Err(other),
            };
            if found != expected {
                return Err(DbError::VersionMismatch {
                    collection: collection.to_string(),
                    doc_id: doc_id.to_string(),
                    expected,
                    found,
                });
            }
        }

        let mut results = Vec::with_capacity(writes.len());
        for write in writes {
            results.push(self.apply(write)?);
        }
        Ok(BatchOutcome { results })
    }

    /// Apply one write.
    ///
    /// A pinned version is checked here too: [`Self::batch`] checks every pin
    /// before it writes anything, but a single write has to refuse a stale pin
    /// on its own as well.
    pub fn apply(&self, write: &Write) -> Result<BatchEntry, DbError> {
        let (collection, doc_id) = write.target();
        let path = self.document_path(collection, doc_id)?;

        if let Some(expected) = pinned_version(write) {
            let found = match self.read_document(&path, collection, doc_id) {
                Ok(document) => document.version,
                Err(DbError::NotFound { .. }) => 0,
                Err(other) => return Err(other),
            };
            if found != expected {
                return Err(DbError::VersionMismatch {
                    collection: collection.to_string(),
                    doc_id: doc_id.to_string(),
                    expected,
                    found,
                });
            }
        }

        match write {
            Write::Set { data, .. } => {
                let version = self.next_version(&path, collection, doc_id)?;
                self.write_document(
                    &path,
                    &Document {
                        id: doc_id.to_string(),
                        collection: collection.to_string(),
                        version,
                        data: data.clone(),
                    },
                )?;
                Ok(BatchEntry {
                    collection: collection.to_string(),
                    doc_id: doc_id.to_string(),
                    version: Some(version),
                })
            }
            Write::Update { data, .. } => {
                let mut document =
                    self.read_document(&path, collection, doc_id)
                        .or_else(|error| match error {
                            DbError::NotFound { .. } => Ok(Document {
                                id: doc_id.to_string(),
                                collection: collection.to_string(),
                                version: 0,
                                data: Value::Object(serde_json::Map::new()),
                            }),
                            other => Err(other),
                        })?;
                let incoming = data.as_object().ok_or_else(|| {
                    DbError::InvalidArgument("update data must be an object".into())
                })?;
                let target = document.data.as_object_mut().ok_or_else(|| {
                    DbError::InvalidArgument(format!(
                        "'{collection}/{doc_id}' does not hold an object"
                    ))
                })?;
                let mut version = document.version;
                for (key, value) in incoming {
                    if value.get("__delete__").is_some() {
                        target.remove(key);
                    } else {
                        target.insert(key.clone(), value.clone());
                    }
                    version += 1;
                }
                document.version = version;
                self.write_document(&path, &document)?;
                Ok(BatchEntry {
                    collection: collection.to_string(),
                    doc_id: doc_id.to_string(),
                    version: Some(version),
                })
            }
            Write::Delete { .. } => {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(DbError::Store(error.to_string())),
                }
                Ok(BatchEntry {
                    collection: collection.to_string(),
                    doc_id: doc_id.to_string(),
                    version: None,
                })
            }
            Write::StrReplace {
                field,
                old,
                new,
                replace_all,
                ..
            } => {
                let mut document = self.read_document(&path, collection, doc_id)?;
                let target = document.data.get_mut(field).ok_or_else(|| {
                    DbError::InvalidArgument(format!(
                        "field '{field}' is missing on '{collection}/{doc_id}'"
                    ))
                })?;
                let text = target.as_str().ok_or_else(|| {
                    DbError::InvalidArgument(format!("field '{field}' is not a string"))
                })?;

                let occurrences = text.matches(old.as_str()).count();
                if occurrences == 0 {
                    return Err(DbError::InvalidArgument(format!(
                        "'{old}' does not occur in field '{field}'"
                    )));
                }
                if occurrences > 1 && !replace_all {
                    return Err(DbError::InvalidArgument(format!(
                        "'{old}' occurs {occurrences} times in field '{field}' — make it unique \
                         or pass replace_all"
                    )));
                }

                let replaced = if *replace_all {
                    text.replace(old.as_str(), new)
                } else {
                    text.replacen(old.as_str(), new, 1)
                };
                *target = Value::String(replaced);
                document.version += 1;
                let version = document.version;
                self.write_document(&path, &document)?;
                Ok(BatchEntry {
                    collection: collection.to_string(),
                    doc_id: doc_id.to_string(),
                    version: Some(version),
                })
            }
        }
    }

    /// Resolve a collection path to the directory it lives in.
    ///
    /// # Errors
    ///
    /// A path with an even number of segments is a document, not a collection,
    /// and is refused with the correction rather than used as a directory.
    pub fn collection_dir(&self, collection: &str) -> Result<PathBuf, DbError> {
        let segments = self.resolve_segments(collection)?;
        if segments.len() % 2 == 0 {
            let last = segments.last().cloned().unwrap_or_default();
            let parent = segments[..segments.len() - 1].join("/");
            return Err(DbError::InvalidArgument(format!(
                "collection '{collection}' has {} segments: that is a document path, not a \
                 collection. Collections have an odd number of segments — use collection \
                 '{parent}' with doc_id '{last}'.",
                segments.len()
            )));
        }
        Ok(self.root.join(segments.join("/")))
    }

    /// Resolve the file backing a document.
    fn document_path(&self, collection: &str, doc_id: &str) -> Result<PathBuf, DbError> {
        let dir = self.collection_dir(collection)?;
        validate_doc_id(doc_id)?;
        Ok(dir.join(format!("{doc_id}.json")))
    }

    /// Split and validate a collection path, resolving `me`.
    fn resolve_segments(&self, collection: &str) -> Result<Vec<String>, DbError> {
        let trimmed = collection.trim().trim_matches('/');
        if trimmed.is_empty() {
            return Err(DbError::InvalidArgument(
                "collection path is empty; pass a collection such as 'boards'".into(),
            ));
        }

        let mut segments: Vec<String> = Vec::new();
        for raw in trimmed.split('/') {
            validate_segment(raw)?;
            if segments.len() == 2
                && segments[0] == "data"
                && segments[1] == "users"
                && raw == CURRENT_USER
            {
                segments.push(self.user_id.clone());
            } else {
                segments.push(raw.to_string());
            }
        }

        if segments.len() > MAX_PATH_SEGMENTS {
            return Err(DbError::InvalidArgument(format!(
                "a path has at most {MAX_PATH_SEGMENTS} segments, got {}",
                segments.len()
            )));
        }
        Ok(segments)
    }

    fn next_version(&self, path: &Path, collection: &str, doc_id: &str) -> Result<u64, DbError> {
        match self.read_document(path, collection, doc_id) {
            Ok(document) => Ok(document.version + 1),
            Err(DbError::NotFound { .. }) => Ok(1),
            Err(other) => Err(other),
        }
    }

    fn read_document(
        &self,
        path: &Path,
        collection: &str,
        doc_id: &str,
    ) -> Result<Document, DbError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(DbError::NotFound {
                    collection: collection.to_string(),
                    doc_id: doc_id.to_string(),
                });
            }
            Err(error) => return Err(DbError::Store(error.to_string())),
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            DbError::Store(format!(
                "{} is not a readable document: {error}",
                path.display()
            ))
        })
    }

    fn write_document(&self, path: &Path, document: &Document) -> Result<(), DbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DbError::Store(e.to_string()))?;
        }
        let bytes =
            serde_json::to_vec_pretty(document).map_err(|e| DbError::Store(e.to_string()))?;
        std::fs::write(path, bytes).map_err(|e| DbError::Store(e.to_string()))
    }

    fn document_ids(&self, dir: &Path) -> Result<Vec<String>, DbError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(DbError::Store(error.to_string())),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| DbError::Store(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".json") {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }
}

fn pinned_version(write: &Write) -> Option<u64> {
    match write {
        Write::Set { if_version, .. }
        | Write::Update { if_version, .. }
        | Write::Delete { if_version, .. }
        | Write::StrReplace { if_version, .. } => *if_version,
    }
}

/// Validate one path segment.
fn validate_segment(segment: &str) -> Result<(), DbError> {
    if segment.is_empty() {
        return Err(DbError::InvalidArgument(
            "a path segment is empty; './' and '//' are not usable paths".into(),
        ));
    }
    if segment == "." || segment == ".." {
        return Err(DbError::InvalidArgument(format!(
            "'{segment}' is not a usable path segment"
        )));
    }
    if segment.len() > MAX_SEGMENT_LEN {
        return Err(DbError::InvalidArgument(format!(
            "a path segment is at most {MAX_SEGMENT_LEN} characters, got {}",
            segment.len()
        )));
    }
    if !segment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-.:@+~".contains(c))
    {
        return Err(DbError::InvalidArgument(format!(
            "'{segment}' has characters a path segment cannot carry; allowed are letters, \
             digits, and _ - . : @ + ~"
        )));
    }
    Ok(())
}

/// Validate a document id.
fn validate_doc_id(doc_id: &str) -> Result<(), DbError> {
    if doc_id.is_empty() || doc_id.len() > MAX_SEGMENT_LEN {
        return Err(DbError::InvalidArgument(format!(
            "a document id is 1 to {MAX_SEGMENT_LEN} characters, got {}",
            doc_id.len()
        )));
    }
    if doc_id.starts_with("__") && doc_id.ends_with("__") {
        return Err(DbError::InvalidArgument(format!(
            "'{doc_id}' is a reserved name"
        )));
    }
    if doc_id.contains(['"', '\\', '.', '/', '[', ']']) {
        return Err(DbError::InvalidArgument(format!(
            "'{doc_id}' has characters a document id cannot carry: \" \\ . / [ ]"
        )));
    }
    if doc_id.chars().any(|c| c.is_control()) {
        return Err(DbError::InvalidArgument(
            "a document id cannot carry control characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> (ArtifactDb, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (ArtifactDb::open(dir.path(), "u_1"), dir)
    }

    #[test]
    fn an_odd_segment_path_is_a_collection() {
        let (db, _dir) = db();
        assert!(db.collection_dir("boards").is_ok());
        assert!(db.collection_dir("boards/b1/columns").is_ok());
    }

    #[test]
    fn an_even_segment_path_is_refused_with_the_correction() {
        let (db, _dir) = db();
        let error = db.collection_dir("boards/b1").unwrap_err();
        let message = error.to_string();

        assert!(message.contains("2 segments"), "{message}");
        assert!(
            message.contains("document path, not a collection"),
            "{message}"
        );
        assert!(message.contains("collection 'boards'"), "{message}");
        assert!(message.contains("doc_id 'b1'"), "{message}");
    }

    #[test]
    fn a_path_cannot_escape_the_database() {
        let (db, _dir) = db();
        for hostile in ["../etc", "boards/../..", "..", "boards/../../etc/passwd"] {
            assert!(
                db.collection_dir(hostile).is_err(),
                "{hostile} was accepted"
            );
        }
    }

    #[test]
    fn a_document_id_cannot_carry_path_syntax() {
        let (db, _dir) = db();
        for hostile in ["../x", "a/b", "a\\b", "__proto__", "a.b", "a[b]"] {
            assert!(
                db.document_path("boards", hostile).is_err(),
                "{hostile} was accepted"
            );
        }
        assert!(db.document_path("boards", "b1").is_ok());
    }

    #[test]
    fn me_resolves_to_the_viewer() {
        let (db, _dir) = db();

        // `data/users/<id>` is a collection (3 segments) and `…/decks/cards`
        // is a collection inside the document `…/decks` (5 segments).
        let resolved = db.collection_dir("data/users/me/decks/cards").unwrap();
        assert!(
            resolved
                .to_string_lossy()
                .ends_with("data/users/u_1/decks/cards"),
            "{resolved:?}"
        );

        // `data/users/me/decks` is a document path, not a collection.
        assert!(db.collection_dir("data/users/me/decks").is_err());
    }

    #[test]
    fn set_get_and_bump_the_version() {
        let (db, _dir) = db();

        let first = db
            .apply(&Write::Set {
                collection: "boards".into(),
                doc_id: "b1".into(),
                data: json!({"title": "first"}),
                if_version: None,
            })
            .unwrap();
        assert_eq!(first.version, Some(1));

        let second = db
            .apply(&Write::Set {
                collection: "boards".into(),
                doc_id: "b1".into(),
                data: json!({"title": "second"}),
                if_version: None,
            })
            .unwrap();
        assert_eq!(second.version, Some(2));

        let document = db.get("boards", "b1").unwrap();
        assert_eq!(document.data["title"], "second");
        assert_eq!(document.version, 2);
    }

    #[test]
    fn update_merges_and_can_remove_fields() {
        let (db, _dir) = db();
        db.apply(&Write::Set {
            collection: "boards".into(),
            doc_id: "b1".into(),
            data: json!({"title": "t", "draft": true}),
            if_version: None,
        })
        .unwrap();

        db.apply(&Write::Update {
            collection: "boards".into(),
            doc_id: "b1".into(),
            data: json!({"title": "renamed", "draft": {"__delete__": true}}),
            if_version: None,
        })
        .unwrap();

        let document = db.get("boards", "b1").unwrap();
        assert_eq!(document.data["title"], "renamed");
        assert!(document.data.get("draft").is_none());
    }

    #[test]
    fn a_pinned_version_that_moved_on_is_refused() {
        let (db, _dir) = db();
        db.apply(&Write::Set {
            collection: "boards".into(),
            doc_id: "b1".into(),
            data: json!({"title": "v1"}),
            if_version: None,
        })
        .unwrap();

        let stale = db.apply(&Write::Set {
            collection: "boards".into(),
            doc_id: "b1".into(),
            data: json!({"title": "clobber"}),
            if_version: Some(0),
        });
        assert_eq!(
            stale.unwrap_err(),
            DbError::VersionMismatch {
                collection: "boards".into(),
                doc_id: "b1".into(),
                expected: 0,
                found: 1,
            }
        );

        // The current version still works.
        db.apply(&Write::Set {
            collection: "boards".into(),
            doc_id: "b1".into(),
            data: json!({"title": "v2"}),
            if_version: Some(1),
        })
        .unwrap();
    }

    #[test]
    fn a_batch_with_one_stale_write_writes_nothing() {
        let (db, _dir) = db();
        db.apply(&Write::Set {
            collection: "boards".into(),
            doc_id: "b1".into(),
            data: json!({"title": "v1"}),
            if_version: None,
        })
        .unwrap();

        let outcome = db.batch(&[
            Write::Set {
                collection: "boards".into(),
                doc_id: "b2".into(),
                data: json!({"title": "new"}),
                if_version: None,
            },
            Write::Set {
                collection: "boards".into(),
                doc_id: "b1".into(),
                data: json!({"title": "clobber"}),
                if_version: Some(0),
            },
        ]);

        assert!(matches!(outcome, Err(DbError::VersionMismatch { .. })));
        assert!(
            db.get("boards", "b2").is_err(),
            "the first write landed despite the batch failing"
        );
        assert_eq!(db.get("boards", "b1").unwrap().data["title"], "v1");
    }

    #[test]
    fn a_batch_is_bounded() {
        let (db, _dir) = db();
        let writes: Vec<Write> = (0..=DB_BATCH_MAX_WRITES)
            .map(|i| Write::Set {
                collection: "boards".into(),
                doc_id: format!("b{i}"),
                data: json!({}),
                if_version: None,
            })
            .collect();

        assert!(matches!(db.batch(&writes), Err(DbError::TooLarge(_))));
    }

    #[test]
    fn list_pages_through_a_collection() {
        let (db, _dir) = db();
        for i in 0..5 {
            db.apply(&Write::Set {
                collection: "boards".into(),
                doc_id: format!("b{i}"),
                data: json!({"n": i}),
                if_version: None,
            })
            .unwrap();
        }

        let first = db.list("boards", Some(2), None).unwrap();
        assert_eq!(
            first
                .documents
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b0", "b1"]
        );
        let cursor = first.next_cursor.unwrap();

        let second = db.list("boards", Some(2), Some(&cursor)).unwrap();
        assert_eq!(
            second
                .documents
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b2", "b3"]
        );
    }

    #[test]
    fn query_matches_on_document_fields() {
        let (db, _dir) = db();
        for (id, status) in [("b1", "open"), ("b2", "done"), ("b3", "open")] {
            db.apply(&Write::Set {
                collection: "boards".into(),
                doc_id: id.into(),
                data: json!({"status": status}),
                if_version: None,
            })
            .unwrap();
        }

        let open = db
            .query("boards", &[("status".to_string(), json!("open"))])
            .unwrap();
        assert_eq!(
            open.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["b1", "b3"]
        );
    }

    #[test]
    fn str_replace_needs_one_match_unless_told_otherwise() {
        let (db, _dir) = db();
        db.apply(&Write::Set {
            collection: "pages".into(),
            doc_id: "home".into(),
            data: json!({"body": "draft draft final"}),
            if_version: None,
        })
        .unwrap();

        let ambiguous = db.apply(&Write::StrReplace {
            collection: "pages".into(),
            doc_id: "home".into(),
            field: "body".into(),
            old: "draft".into(),
            new: "review".into(),
            replace_all: false,
            if_version: None,
        });
        assert!(matches!(ambiguous, Err(DbError::InvalidArgument(_))));

        db.apply(&Write::StrReplace {
            collection: "pages".into(),
            doc_id: "home".into(),
            field: "body".into(),
            old: "draft".into(),
            new: "review".into(),
            replace_all: true,
            if_version: None,
        })
        .unwrap();
        assert_eq!(
            db.get("pages", "home").unwrap().data["body"],
            "review review final"
        );

        let missing = db.apply(&Write::StrReplace {
            collection: "pages".into(),
            doc_id: "home".into(),
            field: "body".into(),
            old: "nowhere".into(),
            new: "x".into(),
            replace_all: false,
            if_version: None,
        });
        assert!(matches!(missing, Err(DbError::InvalidArgument(_))));
    }

    #[test]
    fn delete_removes_a_document_and_is_idempotent() {
        let (db, _dir) = db();
        db.apply(&Write::Set {
            collection: "boards".into(),
            doc_id: "b1".into(),
            data: json!({}),
            if_version: None,
        })
        .unwrap();

        let deleted = db
            .apply(&Write::Delete {
                collection: "boards".into(),
                doc_id: "b1".into(),
                if_version: None,
            })
            .unwrap();
        assert_eq!(deleted.version, None);
        assert!(matches!(
            db.get("boards", "b1"),
            Err(DbError::NotFound { .. })
        ));

        db.apply(&Write::Delete {
            collection: "boards".into(),
            doc_id: "b1".into(),
            if_version: None,
        })
        .unwrap();
    }

    #[test]
    fn a_collection_that_was_never_written_lists_empty() {
        let (db, _dir) = db();
        let page = db.list("nothing/here/yet", None, None).unwrap();
        assert!(page.documents.is_empty());
        assert!(page.next_cursor.is_none());
    }
}
