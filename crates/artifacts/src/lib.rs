//! CodeRex artifacts — publishing pages, and the data behind them.
//!
//! An artifact is a page the model publishes and a person can open: an HTML
//! file plus whatever it needs, addressable by URL, kept as versions so what
//! was up is still readable after a republish. Pages can carry a database, and
//! the database is where the shape matters most — see [`db`] for the
//! collection/document path rule.
//!
//! The reference publishes to a hosted service. CodeRex has no such service,
//! so publishing writes to a local store and serves from a local origin: the
//! contract a caller sees (slugs, versions, the action vocabulary, the limits,
//! the transcript markers) is the same, the host is not.

#![warn(missing_docs)]

pub mod db;
pub mod limits;
pub mod markers;
pub mod server;
pub mod slug;
pub mod store;

pub use db::{ArtifactDb, BatchEntry, BatchOutcome, DbError, Document, Page as DbPage, Write};
pub use limits::{
    DB_BATCH_MAX_BYTES, DB_BATCH_MAX_WRITES, DB_READ_MAX_RESULT_SIZE_CHARS, MAX_ARTIFACT_BYTES,
    MAX_RESULT_SIZE_CHARS, PUBLISH_MAX_FILES,
};
pub use markers::{Marker, attached_marker, deleted_marker, parse_markers};
pub use server::ArtifactServer;
pub use slug::{ArtifactUrl, Slug, parse_artifact_url};
pub use store::{ArtifactStore, ENTRY_FILE, Manifest, Page, StoreError, Version};

/// The viewer origin used when none is configured.
pub const DEFAULT_ORIGIN: &str = "http://127.0.0.1:8722";
