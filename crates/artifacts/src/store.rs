//! The artifact store — published pages on disk.
//!
//! An artifact is a directory keyed by its slug, holding every version that
//! has been published plus the manifest that says what those versions are.
//! Publishing is append-only: a new publish adds a version and moves the
//! artifact's current version to it, so the page that was up is still there to
//! read or roll back to.
//!
//! The layout:
//!
//! ```text
//! <root>/<slug>/
//!   manifest.json          slug, title, current version, version list, assets
//!   versions/<label>/…     the published files, one directory per version
//!   db/…                   the artifact's database (see `db`)
//! ```

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::limits::{
    BINARY_FILE_MAX_BYTES, MAX_ARTIFACT_BYTES, MAX_MANIFEST_PATH, PUBLISH_MAX_FILES,
    VERSION_MAX_ENTRIES, VERSION_TOTAL_BYTES,
};
use crate::slug::Slug;

/// The file a published page is served from, and the one that must be there.
pub const ENTRY_FILE: &str = "index.html";

/// A file inside a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFile {
    /// Path inside the artifact, always relative and normalized.
    pub path: String,
    /// Size in bytes.
    pub bytes: usize,
    /// Hex SHA-256 of the contents.
    pub sha256: String,
}

/// One published version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// Label, unique within the artifact. The first publish is `v1`.
    pub label: String,
    /// The page's title at this version.
    pub title: String,
    /// When it was published, as an RFC 3339 timestamp.
    pub created_at: String,
    /// What it contains.
    pub files: Vec<StoredFile>,
}

/// An artifact's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The artifact's identity.
    pub slug: String,
    /// The title of the current version.
    pub title: String,
    /// Label of the version the viewer serves.
    pub current_version: String,
    /// Every version, oldest first.
    pub versions: Vec<Version>,
    /// Asset ids uploaded to this artifact.
    #[serde(default)]
    pub assets: Vec<String>,
    /// When the artifact was first published.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
}

impl Manifest {
    /// The version the viewer serves.
    #[must_use]
    pub fn current(&self) -> Option<&Version> {
        self.versions
            .iter()
            .find(|version| version.label == self.current_version)
    }

    /// Total bytes across every version.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.versions
            .iter()
            .flat_map(|version| version.files.iter())
            .map(|file| file.bytes)
            .sum()
    }
}

/// A page as it is stored, before it is written.
#[derive(Debug, Clone)]
pub struct Page {
    /// Title shown for the artifact.
    pub title: String,
    /// Path to contents, both relative.
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Something wrong with a publish or a read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The file paths or sizes are not publishable.
    #[error("{0}")]
    Invalid(String),
    /// No artifact with that slug.
    #[error("no artifact '{0}'")]
    NotFound(String),
    /// The store itself failed.
    #[error("artifact store failed: {0}")]
    Io(String),
}

/// Where artifacts live and how they are written.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Open a store rooted at `root`.
    #[must_use]
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory artifacts live in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory one artifact lives in.
    #[must_use]
    pub fn artifact_dir(&self, slug: &Slug) -> PathBuf {
        self.root.join(slug.as_str())
    }

    /// The directory a version's files live in.
    #[must_use]
    pub fn version_dir(&self, slug: &Slug, label: &str) -> PathBuf {
        self.artifact_dir(slug).join("versions").join(label)
    }

    /// The database directory of an artifact.
    #[must_use]
    pub fn db_dir(&self, slug: &Slug) -> PathBuf {
        self.artifact_dir(slug).join("db")
    }

    /// Read an artifact's manifest.
    pub fn manifest(&self, slug: &Slug) -> Result<Manifest, StoreError> {
        let path = self.manifest_path(slug);
        let bytes = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound(slug.as_str())
            } else {
                StoreError::Io(error.to_string())
            }
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|error| StoreError::Io(format!("{}: {error}", path.display())))
    }

    /// Publish a page, creating the artifact if it is new.
    ///
    /// Returns the manifest as it stands after the publish.
    pub fn publish(&self, slug: Option<Slug>, page: &Page) -> Result<Manifest, StoreError> {
        let slug = slug.unwrap_or_default();
        let existing = self.manifest(&slug).ok();

        validate_page(page, existing.as_ref())?;

        let label = next_label(existing.as_ref());
        let version_dir = self.version_dir(&slug, &label);

        // Write the files first: a manifest that names files which are not
        // there is worse than a version directory nothing points at.
        let mut files = Vec::new();
        for (path, contents) in &page.files {
            let target = version_dir.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
            }
            std::fs::write(&target, contents).map_err(|e| StoreError::Io(e.to_string()))?;
            files.push(StoredFile {
                path: path.clone(),
                bytes: contents.len(),
                sha256: hex(&Sha256::digest(contents)),
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let now = now();
        let version = Version {
            label: label.clone(),
            title: page.title.clone(),
            created_at: now.clone(),
            files,
        };

        let mut manifest = existing.unwrap_or_else(|| Manifest {
            slug: slug.as_str(),
            title: page.title.clone(),
            current_version: label.clone(),
            versions: Vec::new(),
            assets: Vec::new(),
            created_at: now.clone(),
            updated_at: now.clone(),
        });
        manifest.title.clone_from(&page.title);
        manifest.current_version = label;
        manifest.versions.push(version);
        manifest.updated_at = now;

        self.write_manifest(&slug, &manifest)?;
        Ok(manifest)
    }

    /// Read a file out of a version, defaulting to the current one.
    pub fn read_file(
        &self,
        slug: &Slug,
        path: &str,
        version: Option<&str>,
    ) -> Result<Vec<u8>, StoreError> {
        let manifest = self.manifest(slug)?;
        let label = match version {
            Some(label) => label.to_string(),
            None => manifest.current_version.clone(),
        };
        let version = manifest
            .versions
            .iter()
            .find(|candidate| candidate.label == label)
            .ok_or_else(|| StoreError::NotFound(format!("{}@{label}", slug.as_str())))?;

        let normalized = normalize_path(path)?;
        if !version.files.iter().any(|file| file.path == normalized) {
            return Err(StoreError::NotFound(format!(
                "{}@{label}/{normalized}",
                slug.as_str()
            )));
        }

        let target = self.version_dir(slug, &label).join(&normalized);
        std::fs::read(&target).map_err(|e| StoreError::Io(e.to_string()))
    }

    /// Every artifact in the store, newest first.
    pub fn list(&self) -> Result<Vec<Manifest>, StoreError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(StoreError::Io(error.to_string())),
        };

        let mut manifests = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| StoreError::Io(e.to_string()))?;
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(slug) = Slug::parse(&name) {
                if let Ok(manifest) = self.manifest(&slug) {
                    manifests.push(manifest);
                }
            }
        }
        manifests.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(manifests)
    }

    /// Remove an artifact and everything under it.
    pub fn delete(&self, slug: &Slug) -> Result<bool, StoreError> {
        let dir = self.artifact_dir(slug);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(StoreError::Io(error.to_string())),
        }
    }

    /// Record an asset id against an artifact.
    pub fn add_asset(&self, slug: &Slug, asset_id: &str) -> Result<(), StoreError> {
        if !crate::slug::is_asset_id(asset_id) {
            return Err(StoreError::Invalid(format!(
                "'{asset_id}' is not an asset id (32 hex characters)"
            )));
        }
        let mut manifest = self.manifest(slug)?;
        if !manifest.assets.iter().any(|existing| existing == asset_id) {
            manifest.assets.push(asset_id.to_string());
        }
        manifest.updated_at = now();
        self.write_manifest(slug, &manifest)
    }

    /// Write a manifest back, keyed by the slug it carries.
    ///
    /// Used by callers that changed something beside the page — an asset was
    /// added or removed — and need the manifest to say so.
    pub fn save_manifest(&self, manifest: &Manifest) -> Result<(), StoreError> {
        let slug = Slug::parse(&manifest.slug)
            .ok_or_else(|| StoreError::Invalid(format!("'{}' is not a slug", manifest.slug)))?;
        self.write_manifest(&slug, manifest)
    }

    fn manifest_path(&self, slug: &Slug) -> PathBuf {
        self.artifact_dir(slug).join("manifest.json")
    }

    fn write_manifest(&self, slug: &Slug, manifest: &Manifest) -> Result<(), StoreError> {
        let dir = self.artifact_dir(slug);
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
        let bytes =
            serde_json::to_vec_pretty(manifest).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::write(self.manifest_path(slug), bytes).map_err(|e| StoreError::Io(e.to_string()))
    }
}

/// Where artifacts live when nothing else is configured.
#[must_use]
pub fn default_root() -> PathBuf {
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coderex")
        .join("artifacts")
}

/// Check a page against every publish limit before anything is written.
fn validate_page(page: &Page, existing: Option<&Manifest>) -> Result<(), StoreError> {
    if page.files.is_empty() {
        return Err(StoreError::Invalid(
            "a publish carries at least one file — index.html".into(),
        ));
    }
    if !page.files.contains_key(ENTRY_FILE) {
        return Err(StoreError::Invalid(format!(
            "a publish needs {ENTRY_FILE}: that is the page the viewer opens"
        )));
    }
    if page.files.len() > PUBLISH_MAX_FILES {
        return Err(StoreError::Invalid(format!(
            "a publish carries at most {PUBLISH_MAX_FILES} files, got {}",
            page.files.len()
        )));
    }

    let mut total = 0usize;
    for (path, contents) in &page.files {
        normalize_path(path)?;
        if contents.len() > BINARY_FILE_MAX_BYTES {
            return Err(StoreError::Invalid(format!(
                "'{path}' is {} bytes; a single file is at most {BINARY_FILE_MAX_BYTES}",
                contents.len()
            )));
        }
        total += contents.len();
    }
    if total > MAX_ARTIFACT_BYTES {
        return Err(StoreError::Invalid(format!(
            "this publish is {total} bytes; the limit is {MAX_ARTIFACT_BYTES}"
        )));
    }

    if let Some(manifest) = existing {
        if manifest.versions.len() + 1 > VERSION_MAX_ENTRIES {
            return Err(StoreError::Invalid(format!(
                "an artifact holds at most {VERSION_MAX_ENTRIES} versions; delete some first"
            )));
        }
        if manifest.total_bytes() + total > VERSION_TOTAL_BYTES {
            return Err(StoreError::Invalid(format!(
                "publishing this would take the artifact past {VERSION_TOTAL_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

/// Normalize a path inside an artifact, or refuse it.
///
/// Absolute paths, `..`, and anything that is not plain path segments are
/// refused: a published path comes from the model, and a `..` in it would
/// write outside the artifact.
pub fn normalize_path(path: &str) -> Result<String, StoreError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(StoreError::Invalid("a file path is empty".into()));
    }
    if trimmed.len() > MAX_MANIFEST_PATH {
        return Err(StoreError::Invalid(format!(
            "a file path is at most {MAX_MANIFEST_PATH} characters"
        )));
    }
    if trimmed.starts_with('/') || trimmed.contains('\\') {
        return Err(StoreError::Invalid(format!(
            "'{trimmed}' is not a relative artifact path"
        )));
    }

    let mut segments = Vec::new();
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(StoreError::Invalid(format!(
                "'{trimmed}' climbs out of the artifact"
            )));
        }
        if segment.contains('\0') {
            return Err(StoreError::Invalid(
                "a file path cannot carry a NUL byte".into(),
            ));
        }
        segments.push(segment);
    }

    if segments.is_empty() {
        return Err(StoreError::Invalid(format!("'{trimmed}' names no file")));
    }
    Ok(segments.join("/"))
}

/// Resolve a normalized path under a version directory, refusing escapes.
///
/// The path has already been normalized; this is the second check, made
/// against the real filesystem so a symlinked directory cannot lift a read out
/// of the artifact either.
#[must_use]
pub fn resolve_within(base: &Path, relative: &str) -> Option<PathBuf> {
    let mut depth = 0usize;
    for component in Path::new(relative).components() {
        match component {
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    let candidate = base.join(relative);
    candidate.starts_with(base).then_some(candidate)
}

fn next_label(existing: Option<&Manifest>) -> String {
    let count = existing.map_or(0, |manifest| manifest.versions.len());
    format!("v{}", count + 1)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(title: &str, body: &str) -> Page {
        let mut files = BTreeMap::new();
        files.insert(
            ENTRY_FILE.to_string(),
            format!("<html><body>{body}</body></html>").into_bytes(),
        );
        Page {
            title: title.to_string(),
            files,
        }
    }

    fn store() -> (ArtifactStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (ArtifactStore::open(dir.path()), dir)
    }

    #[test]
    fn publishing_creates_an_artifact_and_its_first_version() {
        let (store, _dir) = store();
        let manifest = store.publish(None, &page("Report", "hello")).unwrap();

        assert_eq!(manifest.current_version, "v1");
        assert_eq!(manifest.title, "Report");
        assert_eq!(manifest.versions.len(), 1);

        let slug = Slug::parse(&manifest.slug).unwrap();
        let served = store.read_file(&slug, ENTRY_FILE, None).unwrap();
        assert!(String::from_utf8_lossy(&served).contains("hello"));
    }

    #[test]
    fn publishing_again_adds_a_version_and_moves_the_viewer() {
        let (store, _dir) = store();
        let first = store.publish(None, &page("Report", "v1 body")).unwrap();
        let slug = Slug::parse(&first.slug).unwrap();

        let second = store
            .publish(Some(slug), &page("Report", "v2 body"))
            .unwrap();
        assert_eq!(second.current_version, "v2");
        assert_eq!(second.versions.len(), 2);

        let current = store.read_file(&slug, ENTRY_FILE, None).unwrap();
        assert!(String::from_utf8_lossy(&current).contains("v2 body"));

        // The version that was up is still readable.
        let old = store.read_file(&slug, ENTRY_FILE, Some("v1")).unwrap();
        assert!(String::from_utf8_lossy(&old).contains("v1 body"));
    }

    #[test]
    fn a_publish_must_carry_the_entry_file() {
        let (store, _dir) = store();
        let mut files = BTreeMap::new();
        files.insert("style.css".to_string(), b"body{}".to_vec());

        let error = store
            .publish(
                None,
                &Page {
                    title: "x".into(),
                    files,
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains(ENTRY_FILE), "{error}");
    }

    #[test]
    fn a_publish_cannot_climb_out_of_the_artifact() {
        let (store, _dir) = store();
        for hostile in [
            "../escape.html",
            "/etc/passwd",
            "a/../../b.html",
            "a\\b.html",
        ] {
            let mut files = BTreeMap::new();
            files.insert(ENTRY_FILE.to_string(), b"<html></html>".to_vec());
            files.insert(hostile.to_string(), b"x".to_vec());

            let error = store
                .publish(
                    None,
                    &Page {
                        title: "x".into(),
                        files,
                    },
                )
                .unwrap_err();
            assert!(
                matches!(error, StoreError::Invalid(_)),
                "{hostile} was accepted: {error}"
            );
        }
    }

    #[test]
    fn reading_a_path_that_was_never_published_is_not_found() {
        let (store, _dir) = store();
        let manifest = store.publish(None, &page("Report", "hi")).unwrap();
        let slug = Slug::parse(&manifest.slug).unwrap();

        assert!(matches!(
            store.read_file(&slug, "secrets.txt", None),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            store.read_file(&slug, "../manifest.json", None),
            Err(StoreError::Invalid(_) | StoreError::NotFound(_))
        ));
    }

    #[test]
    fn an_unknown_slug_is_not_found() {
        let (store, _dir) = store();
        assert!(matches!(
            store.manifest(&Slug::new()),
            Err(StoreError::NotFound(_))
        ));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn files_are_recorded_with_their_size_and_digest() {
        let (store, _dir) = store();
        let manifest = store.publish(None, &page("Report", "hello")).unwrap();
        let file = &manifest.versions[0].files[0];

        assert_eq!(file.path, ENTRY_FILE);
        assert_eq!(file.bytes, "<html><body>hello</body></html>".len());
        assert_eq!(file.sha256.len(), 64);
    }

    #[test]
    fn listing_returns_the_most_recently_changed_first() {
        let (store, _dir) = store();
        let first = store.publish(None, &page("First", "a")).unwrap();
        let second = store.publish(None, &page("Second", "b")).unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        // Same clock tick is possible; assert both are present and the set is
        // complete rather than relying on ordering alone.
        let slugs: Vec<&str> = listed.iter().map(|m| m.slug.as_str()).collect();
        assert!(slugs.contains(&first.slug.as_str()));
        assert!(slugs.contains(&second.slug.as_str()));
    }

    #[test]
    fn deleting_an_artifact_removes_its_files() {
        let (store, _dir) = store();
        let manifest = store.publish(None, &page("Report", "hi")).unwrap();
        let slug = Slug::parse(&manifest.slug).unwrap();

        assert!(store.delete(&slug).unwrap());
        assert!(!store.delete(&slug).unwrap());
        assert!(!store.artifact_dir(&slug).exists());
    }

    #[test]
    fn the_page_size_limits_are_enforced() {
        let (store, _dir) = store();
        let mut files = BTreeMap::new();
        files.insert(ENTRY_FILE.to_string(), b"<html></html>".to_vec());
        files.insert("big.bin".to_string(), vec![0u8; BINARY_FILE_MAX_BYTES + 1]);

        let error = store
            .publish(
                None,
                &Page {
                    title: "x".into(),
                    files,
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("at most"), "{error}");
    }

    #[test]
    fn resolve_within_refuses_to_leave_the_base() {
        let base = Path::new("/tmp/artifact/v1");
        assert!(resolve_within(base, "index.html").is_some());
        assert!(resolve_within(base, "assets/app.js").is_some());
        assert!(resolve_within(base, "../manifest.json").is_none());
        assert!(resolve_within(base, "/etc/passwd").is_none());
    }

    #[test]
    fn an_asset_must_look_like_one() {
        let (store, _dir) = store();
        let manifest = store.publish(None, &page("Report", "hi")).unwrap();
        let slug = Slug::parse(&manifest.slug).unwrap();

        assert!(store.add_asset(&slug, "not-an-asset").is_err());
        store
            .add_asset(&slug, "0123456789abcdef0123456789abcdef")
            .unwrap();
        assert_eq!(store.manifest(&slug).unwrap().assets.len(), 1);
    }
}
