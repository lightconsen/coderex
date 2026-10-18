//! The model-facing memory service: named stores, versioned entries, and a
//! classifier gate in front of shared writes.
//!
//! Claude exposes `memory_list` / `memory_read` / `memory_write` over several
//! stores — personal, project, shared, org — with `if_version` optimistic
//! concurrency, and requires a classifier to pass before a shared store is
//! written. CodeRex had no model-facing memory surface at all: the stores
//! existed but only the engine used them. This module is the missing seam. It
//! owns no storage of its own — it routes to the stores that already exist and
//! adds the three things they lack: a store name a model can say, a version to
//! pin, and a gate for writes that leave the machine.
//!
//! # Where the stores live
//!
//! | Model store | Backing store | Key form |
//! |---|---|---|
//! | `personal`  | [`LongTermStore`] under the `personal/` namespace | hierarchical |
//! | `project`   | [`LongTermStore`] under the `project/` namespace | hierarchical |
//! | `shared`    | [`TeamMemorySyncService`] | flat topic |
//! | `org`       | a second, org-scoped [`TeamMemorySyncService`] | flat topic |
//!
//! The `org` row is reachable after [`MemoryService::with_org`] attaches a
//! second, org-scoped [`TeamMemorySyncService`]. The CLI does this at startup
//! (`crates/cli/src/repl.rs`), using `CODEREX_ORG_ID` and falling back to the
//! org id in the remote-settings cache; when neither is configured, `org`
//! answers [`MemoryServiceError::StoreUnavailable`] rather than falling back
//! to another store.
//!
//! `personal` and `project` share one [`LongTermStore`] and are told apart by a
//! key namespace. That is deliberate for a local-first tool: there is no
//! hidden `~/.coderex` memory directory, and a "personal" memory is still
//! written next to the workspace it belongs to. The two namespaces keep the
//! two scopes from colliding; they do not promise a global one.
//!
//! [`SessionMemoryStore`](crate::session_memory::SessionMemoryStore) is not a
//! model store. Its facts are extracted from a session by the engine, are
//! keyed by session id, and are not addressed by a model-chosen key — exposing
//! it as `memory_write` would mean inventing keys the store does not have.
//!
//! # Versioning
//!
//! Every entry carries a `version`, bumped on each write. A write may pin the
//! version it expects (`if_version`); if the entry has moved on, the write is
//! refused and reports what the version is now, so a caller can re-read rather
//! than clobber a change it did not see. This is the same convention the
//! artifact database uses (`coderex-artifacts`'s `db::Write`), reused rather
//! than reinvented.
//!
//! # The classifier gate
//!
//! A write to `shared` or `org` leaves the machine, so it must pass a
//! [`MemoryWriteClassifier`] first. When no classifier is configured the write
//! is **refused**, not waved through: a shared write that nobody classified is
//! exactly the write the gate exists to stop. A caller that really wants the
//! gate open says so with [`AllowAllClassifier`] — an explicit, greppable
//! choice rather than a silent skip.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::long_term::{LongTermStore, MemoryKey};
use crate::team_memory::{TeamMemoryEntry, TeamMemorySyncService};

/// The namespace `personal` entries live under inside the long-term store.
pub const PERSONAL_PREFIX: &str = "personal";

/// The namespace `project` entries live under inside the long-term store.
pub const PROJECT_PREFIX: &str = "project";

/// Longest a memory key may be.
pub const MAX_KEY_LEN: usize = 512;

/// Which store a memory call is addressed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryStore {
    /// This user's memories for this workspace.
    Personal,
    /// Memories for this workspace, shared by every session in it.
    Project,
    /// Memories shared with the repo's team; synced, and gated by a classifier.
    Shared,
    /// Org-scoped shared memories; synced, and gated by a classifier.
    Org,
}

impl MemoryStore {
    /// The name a model addresses the store by.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Project => "project",
            Self::Shared => "shared",
            Self::Org => "org",
        }
    }

    /// Parse a store name, case-insensitively.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "personal" => Some(Self::Personal),
            "project" => Some(Self::Project),
            "shared" => Some(Self::Shared),
            "org" => Some(Self::Org),
            _ => None,
        }
    }

    /// Every store name, for a listing that has to say what exists.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::Personal, Self::Project, Self::Shared, Self::Org]
    }

    /// Whether a write to this store leaves the machine and must be classified.
    #[must_use]
    pub const fn requires_classification(self) -> bool {
        matches!(self, Self::Shared | Self::Org)
    }

    const fn long_term_prefix(self) -> Option<&'static str> {
        match self {
            Self::Personal => Some(PERSONAL_PREFIX),
            Self::Project => Some(PROJECT_PREFIX),
            Self::Shared | Self::Org => None,
        }
    }
}

impl std::fmt::Display for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One memory, as the service sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRecord {
    /// The store it lives in.
    pub store: MemoryStore,
    /// The key it is addressed by, without the store's internal namespace.
    pub key: String,
    /// The memory's text.
    pub content: String,
    /// Monotonic version, bumped on every write. Pin it with `if_version`.
    pub version: u64,
    /// When the memory was first written.
    pub created_at: DateTime<Utc>,
    /// When the memory was last written.
    pub updated_at: DateTime<Utc>,
}

/// Something wrong with a memory call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryServiceError {
    /// No entry at that key.
    #[error("no {store} memory at '{key}'")]
    NotFound {
        /// The store.
        store: MemoryStore,
        /// The key.
        key: String,
    },
    /// The pinned version is not the current one.
    #[error("version mismatch on {store} '{key}': expected {expected}, found {found}")]
    VersionMismatch {
        /// The store.
        store: MemoryStore,
        /// The key.
        key: String,
        /// The version the caller pinned.
        expected: u64,
        /// The version the entry is at now.
        found: u64,
    },
    /// The classifier refused the write, or there was none to ask.
    #[error("the {store} write to '{key}' was refused: {reason}")]
    Refused {
        /// The store.
        store: MemoryStore,
        /// The key.
        key: String,
        /// Why it was refused.
        reason: String,
    },
    /// The store is not configured.
    #[error("the {0} store is not configured")]
    StoreUnavailable(MemoryStore),
    /// The key or argument is not usable.
    #[error("{0}")]
    Invalid(String),
    /// The backing store failed.
    #[error("memory store failed: {0}")]
    Io(String),
}

/// A gate a shared write must pass before it leaves the machine.
///
/// The permission engine already has this shape — a hook that looks at a
/// request and returns allow or refuse (`AutoClassifierHook` in
/// `coderex-core`). This trait is the memory-shaped face of it: `Ok(())` lets
/// the write through, `Err(reason)` refuses it and the reason is reported back
/// to the caller.
#[async_trait::async_trait]
pub trait MemoryWriteClassifier: Send + Sync {
    /// Decide whether this write may proceed.
    ///
    /// # Errors
    ///
    /// Returns the reason the write must not proceed.
    async fn classify(&self, store: MemoryStore, key: &str, content: &str) -> Result<(), String>;
}

/// A classifier that lets everything through.
///
/// This is the explicit opt-out from the shared-store gate. It exists so that
/// "no classification" is a named, greppable decision rather than the silent
/// default of a missing hook.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllClassifier;

#[async_trait::async_trait]
impl MemoryWriteClassifier for AllowAllClassifier {
    async fn classify(
        &self,
        _store: MemoryStore,
        _key: &str,
        _content: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// The model-facing memory service over the stores that already exist.
#[derive(Clone)]
pub struct MemoryService {
    long_term: Arc<LongTermStore>,
    team: Arc<TeamMemorySyncService>,
    org: Option<Arc<TeamMemorySyncService>>,
    classifier: Option<Arc<dyn MemoryWriteClassifier>>,
}

impl std::fmt::Debug for MemoryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryService")
            .field("has_org", &self.org.is_some())
            .field("has_classifier", &self.classifier.is_some())
            .finish_non_exhaustive()
    }
}

impl MemoryService {
    /// Build the service over the long-term and shared stores.
    ///
    /// The stores are the ones the REPL already constructs: `LongTermStore`
    /// for `personal`/`project`, `TeamMemorySyncService` for `shared`. Add an
    /// org store with [`Self::with_org`] and a gate with
    /// [`Self::with_classifier`].
    #[must_use]
    pub fn new(long_term: Arc<LongTermStore>, team: Arc<TeamMemorySyncService>) -> Self {
        Self {
            long_term,
            team,
            org: None,
            classifier: None,
        }
    }

    /// Attach the org-scoped shared store, enabling the `org` store.
    ///
    /// # Status
    ///
    /// The CLI calls this at startup (`crates/cli/src/repl.rs`) when an org id
    /// is configured, so `org` is a live store in production. Without an id
    /// the store stays unattached and every `org` call answers
    /// [`MemoryServiceError::StoreUnavailable`] rather than silently using a
    /// different store.
    ///
    /// The classifier is not optional for `org`: like `shared`, an org write
    /// leaves the machine and is refused unless something classified it.
    #[must_use]
    pub fn with_org(mut self, org: Arc<TeamMemorySyncService>) -> Self {
        self.org = Some(org);
        self
    }

    /// Attach the classifier that gates shared and org writes.
    #[must_use]
    pub fn with_classifier(mut self, classifier: Arc<dyn MemoryWriteClassifier>) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// The long-term store backing `personal` and `project`.
    #[must_use]
    pub const fn long_term(&self) -> &Arc<LongTermStore> {
        &self.long_term
    }

    /// List every memory in a store, newest keys in a stable order.
    ///
    /// # Errors
    ///
    /// Fails when the store is not configured or the backing store fails.
    pub fn list(&self, store: MemoryStore) -> Result<Vec<MemoryRecord>, MemoryServiceError> {
        match store {
            MemoryStore::Personal | MemoryStore::Project => {
                let prefix = store.long_term_prefix().unwrap_or(PERSONAL_PREFIX);
                let entries = self
                    .long_term
                    .read_by_prefix(&MemoryKey::from_str(prefix))
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?;
                let mut records: Vec<MemoryRecord> = entries
                    .into_iter()
                    .filter_map(|entry| {
                        let key = entry.key.as_deref()?.strip_prefix(&format!("{prefix}/"))?;
                        Some(MemoryRecord {
                            store,
                            key: key.to_string(),
                            content: entry.content,
                            version: entry.version,
                            created_at: entry.created_at,
                            updated_at: entry.updated_at,
                        })
                    })
                    .collect();
                records.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(records)
            }
            MemoryStore::Shared => {
                let entries = self
                    .team
                    .list()
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?;
                Ok(entries.into_iter().map(record_from_team(store)).collect())
            }
            MemoryStore::Org => {
                let org = self
                    .org
                    .as_ref()
                    .ok_or(MemoryServiceError::StoreUnavailable(store))?;
                let entries = org
                    .list()
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?;
                Ok(entries.into_iter().map(record_from_team(store)).collect())
            }
        }
    }

    /// Read one memory.
    ///
    /// # Errors
    ///
    /// [`MemoryServiceError::NotFound`] when there is no entry at the key.
    pub fn read(&self, store: MemoryStore, key: &str) -> Result<MemoryRecord, MemoryServiceError> {
        let key = normalize_key(key)?;
        check_store_key(store, &key)?;
        match store {
            MemoryStore::Personal | MemoryStore::Project => {
                let namespaced = namespaced_key(store, &key);
                let entry = self
                    .long_term
                    .read_by_key(&MemoryKey::from_str(&namespaced))
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?
                    .ok_or_else(|| MemoryServiceError::NotFound {
                        store,
                        key: key.clone(),
                    })?;
                Ok(MemoryRecord {
                    store,
                    key,
                    content: entry.content,
                    version: entry.version,
                    created_at: entry.created_at,
                    updated_at: entry.updated_at,
                })
            }
            MemoryStore::Shared => {
                let entry = self
                    .team
                    .read(&key)
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?
                    .ok_or_else(|| MemoryServiceError::NotFound {
                        store,
                        key: key.clone(),
                    })?;
                Ok(record_from_team(store)(entry))
            }
            MemoryStore::Org => {
                let org = self
                    .org
                    .as_ref()
                    .ok_or(MemoryServiceError::StoreUnavailable(store))?;
                let entry = org
                    .read(&key)
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?
                    .ok_or_else(|| MemoryServiceError::NotFound {
                        store,
                        key: key.clone(),
                    })?;
                Ok(record_from_team(store)(entry))
            }
        }
    }

    /// Write a memory, optionally pinning the version it replaces.
    ///
    /// A write to `shared` or `org` is classified first; see the module docs
    /// for why a missing classifier refuses rather than allows.
    ///
    /// # Errors
    ///
    /// [`MemoryServiceError::VersionMismatch`] when `if_version` is stale,
    /// [`MemoryServiceError::Refused`] when the gate declines.
    pub async fn write(
        &self,
        store: MemoryStore,
        key: &str,
        content: &str,
        if_version: Option<u64>,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let key = normalize_key(key)?;
        check_store_key(store, &key)?;

        if store.requires_classification() {
            let classifier =
                self.classifier
                    .as_ref()
                    .ok_or_else(|| MemoryServiceError::Refused {
                        store,
                        key: key.clone(),
                        reason: "no classifier is configured for this store".into(),
                    })?;
            classifier
                .classify(store, &key, content)
                .await
                .map_err(|reason| MemoryServiceError::Refused {
                    store,
                    key: key.clone(),
                    reason,
                })?;
        }

        let found = self.current_version(store, &key)?;
        if let Some(expected) = if_version {
            if expected != found {
                return Err(MemoryServiceError::VersionMismatch {
                    store,
                    key,
                    expected,
                    found,
                });
            }
        }

        match store {
            MemoryStore::Personal | MemoryStore::Project => {
                let namespaced = namespaced_key(store, &key);
                self.long_term
                    .write_with_key(&key, &MemoryKey::from_str(&namespaced), content)
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?;
                self.read(store, &key)
            }
            MemoryStore::Shared => {
                self.team
                    .upsert(&key, content)
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?;
                self.read(store, &key)
            }
            MemoryStore::Org => {
                let org = self
                    .org
                    .as_ref()
                    .ok_or(MemoryServiceError::StoreUnavailable(store))?;
                org.upsert(&key, content)
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))?;
                self.read(store, &key)
            }
        }
    }

    /// Delete a memory, optionally pinning the version it must be at.
    ///
    /// # Errors
    ///
    /// [`MemoryServiceError::NotFound`] when there is nothing to delete, and
    /// [`MemoryServiceError::VersionMismatch`] on a stale pin.
    pub fn delete(
        &self,
        store: MemoryStore,
        key: &str,
        if_version: Option<u64>,
    ) -> Result<(), MemoryServiceError> {
        let key = normalize_key(key)?;
        check_store_key(store, &key)?;
        let found = self.current_version(store, &key)?;
        if found == 0 {
            return Err(MemoryServiceError::NotFound { store, key });
        }
        if let Some(expected) = if_version {
            if expected != found {
                return Err(MemoryServiceError::VersionMismatch {
                    store,
                    key,
                    expected,
                    found,
                });
            }
        }

        match store {
            MemoryStore::Personal | MemoryStore::Project => self
                .long_term
                .delete_by_key(&MemoryKey::from_str(&namespaced_key(store, &key)))
                .map_err(|error| MemoryServiceError::Io(error.to_string())),
            MemoryStore::Shared => self
                .team
                .delete(&key)
                .map_err(|error| MemoryServiceError::Io(error.to_string())),
            MemoryStore::Org => {
                let org = self
                    .org
                    .as_ref()
                    .ok_or(MemoryServiceError::StoreUnavailable(store))?;
                org.delete(&key)
                    .map_err(|error| MemoryServiceError::Io(error.to_string()))
            }
        }
    }

    /// The current version of an entry, or `0` when it is not there.
    fn current_version(&self, store: MemoryStore, key: &str) -> Result<u64, MemoryServiceError> {
        match self.read(store, key) {
            Ok(record) => Ok(record.version),
            Err(MemoryServiceError::NotFound { .. }) => Ok(0),
            Err(other) => Err(other),
        }
    }
}

fn record_from_team(store: MemoryStore) -> impl Fn(TeamMemoryEntry) -> MemoryRecord {
    move |entry| MemoryRecord {
        store,
        key: entry.topic,
        content: entry.content,
        version: entry.version,
        created_at: entry.created_at,
        updated_at: entry.updated_at,
    }
}

/// The key an entry is stored under inside the long-term store.
fn namespaced_key(store: MemoryStore, key: &str) -> String {
    store
        .long_term_prefix()
        .map_or_else(|| key.to_string(), |prefix| format!("{prefix}/{key}"))
}

/// Check and trim a key.
///
/// A key may be hierarchical (`backend/auth`), so `/` is allowed as a
/// separator, but it may not be absolute, may not contain `.`/`..` segments,
/// and may not be empty or carry control characters — the key becomes a path
/// in the long-term store, and a key that can walk out of it is not a key.
fn normalize_key(key: &str) -> Result<String, MemoryServiceError> {
    let trimmed = key.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err(MemoryServiceError::Invalid(
            "a memory key cannot be empty".into(),
        ));
    }
    if trimmed.len() > MAX_KEY_LEN {
        return Err(MemoryServiceError::Invalid(format!(
            "a memory key is at most {MAX_KEY_LEN} characters, got {}",
            trimmed.len()
        )));
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() {
            return Err(MemoryServiceError::Invalid(format!(
                "'{key}' has an empty path segment"
            )));
        }
        if segment == "." || segment == ".." {
            return Err(MemoryServiceError::Invalid(format!(
                "'{key}' has a path segment that is not a name"
            )));
        }
        if segment.chars().any(char::is_control) {
            return Err(MemoryServiceError::Invalid(format!(
                "'{key}' carries a control character"
            )));
        }
    }
    Ok(trimmed.to_string())
}

/// Check that a key is something the addressed store can hold.
///
/// The long-term store keys by path, so `personal` and `project` take
/// hierarchical keys. The synced stores key by topic and write `<topic>.json`,
/// replacing `/`, `\`, `:` and space with `_` — so `backend/auth` and
/// `backend_auth` would be the same file. Refusing the first is better than
/// letting two keys silently merge into one.
fn check_store_key(store: MemoryStore, key: &str) -> Result<(), MemoryServiceError> {
    if store.long_term_prefix().is_some() {
        return Ok(());
    }
    if key.contains(['/', '\\', ':', ' ']) {
        return Err(MemoryServiceError::Invalid(format!(
            "'{key}' is not a flat topic; the {store} store is keyed by topic, not by path"
        )));
    }
    Ok(())
}

/// Convenience for building a service over store roots.
///
/// The REPL constructs the stores itself; this is for tests and for callers
/// that only have directories.
#[must_use]
pub fn service_over(long_term_root: &Path, team_root: &Path) -> MemoryService {
    MemoryService::new(
        Arc::new(LongTermStore::new(long_term_root.to_path_buf())),
        Arc::new(TeamMemorySyncService::new(
            "default".to_string(),
            team_root.to_path_buf(),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (MemoryService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let service = service_over(&dir.path().join("long_term"), &dir.path().join("team"));
        (service, dir)
    }

    #[tokio::test]
    async fn personal_and_project_are_separate_namespaces() {
        let (service, _dir) = service();

        service
            .write(MemoryStore::Personal, "greeting", "hello", None)
            .await
            .unwrap();
        service
            .write(MemoryStore::Project, "greeting", "hi", None)
            .await
            .unwrap();

        assert_eq!(
            service
                .read(MemoryStore::Personal, "greeting")
                .unwrap()
                .content,
            "hello"
        );
        assert_eq!(
            service
                .read(MemoryStore::Project, "greeting")
                .unwrap()
                .content,
            "hi"
        );
        assert_eq!(service.list(MemoryStore::Personal).unwrap().len(), 1);
        assert_eq!(service.list(MemoryStore::Project).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_write_bumps_the_version_and_a_stale_pin_is_refused() {
        let (service, _dir) = service();

        let first = service
            .write(MemoryStore::Project, "note", "v1", None)
            .await
            .unwrap();
        assert_eq!(first.version, 1);

        let second = service
            .write(MemoryStore::Project, "note", "v2", Some(1))
            .await
            .unwrap();
        assert_eq!(second.version, 2);

        let stale = service
            .write(MemoryStore::Project, "note", "clobber", Some(1))
            .await
            .unwrap_err();
        assert_eq!(
            stale,
            MemoryServiceError::VersionMismatch {
                store: MemoryStore::Project,
                key: "note".into(),
                expected: 1,
                found: 2,
            }
        );
        // The refused write did not land.
        assert_eq!(
            service.read(MemoryStore::Project, "note").unwrap().content,
            "v2"
        );
    }

    #[tokio::test]
    async fn a_shared_write_without_a_classifier_is_refused() {
        let (service, _dir) = service();

        let refused = service
            .write(MemoryStore::Shared, "topic", "content", None)
            .await
            .unwrap_err();
        assert!(
            matches!(refused, MemoryServiceError::Refused { .. }),
            "{refused:?}"
        );
        assert!(
            service.read(MemoryStore::Shared, "topic").is_err(),
            "the refused write landed anyway"
        );
    }

    #[tokio::test]
    async fn a_classifier_can_refuse_a_shared_write() {
        struct NeverWritesSecrets;
        #[async_trait::async_trait]
        impl MemoryWriteClassifier for NeverWritesSecrets {
            async fn classify(
                &self,
                _store: MemoryStore,
                _key: &str,
                content: &str,
            ) -> Result<(), String> {
                if content.contains("secret") {
                    Err("content looks like a secret".into())
                } else {
                    Ok(())
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let service = service_over(&dir.path().join("lt"), &dir.path().join("team"))
            .with_classifier(Arc::new(NeverWritesSecrets));

        let refused = service
            .write(MemoryStore::Shared, "creds", "the secret is hunter2", None)
            .await
            .unwrap_err();
        assert!(
            matches!(refused, MemoryServiceError::Refused { ref reason, .. } if reason.contains("secret")),
            "{refused:?}"
        );

        let allowed = service
            .write(MemoryStore::Shared, "creds", "the build uses cargo", None)
            .await
            .unwrap();
        assert_eq!(allowed.store, MemoryStore::Shared);
        assert_eq!(allowed.version, 1);
    }

    #[tokio::test]
    async fn an_explicit_allow_all_classifier_opens_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_over(&dir.path().join("lt"), &dir.path().join("team"))
            .with_classifier(Arc::new(AllowAllClassifier));

        let written = service
            .write(MemoryStore::Shared, "topic", "content", None)
            .await
            .unwrap();
        assert_eq!(written.key, "topic");
    }

    #[tokio::test]
    async fn the_org_store_is_unavailable_until_configured() {
        let (service, _dir) = service();

        assert_eq!(
            service.list(MemoryStore::Org).unwrap_err(),
            MemoryServiceError::StoreUnavailable(MemoryStore::Org)
        );
    }

    #[tokio::test]
    async fn org_writes_are_gated_like_shared_writes() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_over(&dir.path().join("lt"), &dir.path().join("team")).with_org(
            Arc::new(TeamMemorySyncService::new_with_org(
                "default".to_string(),
                dir.path().join("team"),
                "acme".to_string(),
            )),
        );

        let refused = service
            .write(MemoryStore::Org, "topic", "content", None)
            .await
            .unwrap_err();
        assert!(matches!(refused, MemoryServiceError::Refused { .. }));
    }

    #[tokio::test]
    async fn a_configured_org_store_is_usable_and_isolated_from_shared() {
        let dir = tempfile::tempdir().unwrap();
        let team = Arc::new(TeamMemorySyncService::new(
            "default".to_string(),
            dir.path().join("team"),
        ));
        let org = Arc::new(TeamMemorySyncService::new_with_org(
            "default".to_string(),
            dir.path().join("team"),
            "acme".to_string(),
        ));
        let service = MemoryService::new(
            Arc::new(LongTermStore::new(dir.path().join("long_term"))),
            team,
        )
        .with_org(org)
        .with_classifier(Arc::new(AllowAllClassifier));

        let written = service
            .write(MemoryStore::Org, "topic", "org content", None)
            .await
            .unwrap();
        assert_eq!(written.store, MemoryStore::Org);
        assert_eq!(written.key, "topic");
        assert_eq!(written.version, 1);

        assert_eq!(
            service.read(MemoryStore::Org, "topic").unwrap().content,
            "org content"
        );
        assert_eq!(service.list(MemoryStore::Org).unwrap().len(), 1);

        // The shared store is a different directory and does not see it.
        assert_eq!(
            service.read(MemoryStore::Shared, "topic").unwrap_err(),
            MemoryServiceError::NotFound {
                store: MemoryStore::Shared,
                key: "topic".into(),
            }
        );

        // The org store honours `if_version` and delete like every other
        // store, not just read/list.
        let updated = service
            .write(MemoryStore::Org, "topic", "org v2", Some(1))
            .await
            .unwrap();
        assert_eq!(updated.version, 2);
        service.delete(MemoryStore::Org, "topic", Some(2)).unwrap();
        assert!(service.list(MemoryStore::Org).unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_removes_and_respects_the_pin() {
        let (service, _dir) = service();
        service
            .write(MemoryStore::Project, "gone", "x", None)
            .await
            .unwrap();

        let stale = service
            .delete(MemoryStore::Project, "gone", Some(5))
            .unwrap_err();
        assert!(matches!(stale, MemoryServiceError::VersionMismatch { .. }));

        service
            .delete(MemoryStore::Project, "gone", Some(1))
            .unwrap();
        assert_eq!(
            service.read(MemoryStore::Project, "gone").unwrap_err(),
            MemoryServiceError::NotFound {
                store: MemoryStore::Project,
                key: "gone".into(),
            }
        );
    }

    #[tokio::test]
    async fn a_hierarchical_key_is_refused_by_a_synced_store() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_over(&dir.path().join("lt"), &dir.path().join("team"))
            .with_classifier(Arc::new(AllowAllClassifier));

        // `backend/auth` would land on `backend_auth.json`, the same file as
        // the key `backend_auth`.
        let refused = service
            .write(MemoryStore::Shared, "backend/auth", "content", None)
            .await
            .unwrap_err();
        assert!(
            matches!(refused, MemoryServiceError::Invalid(_)),
            "{refused:?}"
        );
        assert!(
            service.read(MemoryStore::Shared, "backend/auth").is_err(),
            "the refused key was readable anyway"
        );

        // A flat topic is what the store is for, and it works.
        service
            .write(MemoryStore::Shared, "backend_auth", "content", None)
            .await
            .unwrap();

        // The long-term-backed stores still take paths.
        service
            .write(MemoryStore::Project, "backend/auth", "content", None)
            .await
            .unwrap();
    }

    #[test]
    fn hostile_keys_are_refused() {
        for hostile in ["", "  ", "../etc", "a/../b", "a//b", "x\u{0}y"] {
            assert!(normalize_key(hostile).is_err(), "{hostile} was accepted");
        }
        assert_eq!(normalize_key("/backend/auth/").unwrap(), "backend/auth");
    }

    #[test]
    fn store_names_round_trip() {
        for store in MemoryStore::all() {
            assert_eq!(MemoryStore::parse(store.as_str()), Some(store));
        }
        assert_eq!(MemoryStore::parse("PERSONAL"), Some(MemoryStore::Personal));
        assert_eq!(MemoryStore::parse("nope"), None);
    }
}
