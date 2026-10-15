//! The size and count limits a publish is held to.
//!
//! Every one of these is a documented number rather than a round guess: they
//! are what the artifact service enforces, and a local store that accepts more
//! than the service would produces artifacts that cannot be published later.

/// Largest single publish, in bytes (16 MiB).
pub const MAX_ARTIFACT_BYTES: usize = 16_777_216;

/// Largest single binary file, in bytes (15 MiB).
pub const BINARY_FILE_MAX_BYTES: usize = 15_728_640;

/// Largest total for one version, in bytes (256 MiB).
pub const VERSION_TOTAL_BYTES: usize = 268_435_456;

/// Most files one publish may carry.
pub const PUBLISH_MAX_FILES: usize = 256;

/// Most files one version may hold.
pub const VERSION_MAX_ENTRIES: usize = 512;

/// Longest path a manifest entry may carry.
pub const MAX_MANIFEST_PATH: usize = 4096;

/// Most characters a tool result is truncated to.
pub const MAX_RESULT_SIZE_CHARS: usize = 16_000;

/// Most characters a file read is inlined to.
pub const FILE_INLINE_RESULT_CAP: usize = 20_000;

/// Most characters a database read is truncated to.
pub const DB_READ_MAX_RESULT_SIZE_CHARS: usize = 300_000;

/// Most files a `list_files` result names.
pub const MAX_LISTED_FILES_SHOWN: usize = 1024;

/// Default page size for a listing.
pub const DEFAULT_LIST_LIMIT: usize = 25;

/// Largest page a listing may ask for.
pub const LIST_LIMIT_MAX: usize = 200;

/// Largest page a type listing may ask for.
pub const TYPE_LIST_LIMIT_MAX: usize = 50;

/// Most documents one database batch may write.
pub const DB_BATCH_MAX_WRITES: usize = 50;

/// Largest database batch body, in bytes (1 MiB).
pub const DB_BATCH_MAX_BYTES: usize = 1_048_576;

/// Most artifacts one `copy_from` may read from.
pub const MAX_COPY_SOURCES: usize = 4;
