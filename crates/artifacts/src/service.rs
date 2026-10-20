//! The artifact service — what the tools talk to.
//!
//! One place that knows where artifacts live, what origin serves them, who is
//! looking, and how the pieces beside a page (assets, comments) are kept. The
//! tools are thin over this; the store and the database stay unaware of the
//! tool vocabulary.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::db::ArtifactDb;
use crate::slug::{Slug, viewer_url};
use crate::store::{ArtifactStore, StoreError};

/// A comment on a published page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// Comment id.
    pub id: String,
    /// Who wrote it: `"user"` or the session that replied.
    pub author: String,
    /// The comment body.
    pub body: String,
    /// When it was written.
    pub created_at: String,
    /// Whether the thread it belongs to is resolved.
    pub resolved: bool,
}

/// The artifacts of one session: pages, their assets, their comments, their
/// databases.
#[derive(Debug, Clone)]
pub struct ArtifactService {
    store: ArtifactStore,
    origin: String,
    user_id: String,
}

impl ArtifactService {
    /// Build a service over `store`, served from `origin`.
    ///
    /// `user_id` is what `data/users/me` resolves to in a page's database.
    #[must_use]
    pub fn new(
        store: ArtifactStore,
        origin: impl Into<String>,
        user_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            origin: origin.into().trim_end_matches('/').to_string(),
            user_id: user_id.into(),
        }
    }

    /// The store pages live in.
    #[must_use]
    pub const fn store(&self) -> &ArtifactStore {
        &self.store
    }

    /// The origin pages are served from.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The URL a person opens to see this page.
    #[must_use]
    pub fn url(&self, slug: &Slug) -> String {
        viewer_url(&self.origin, slug)
    }

    /// The database belonging to a page.
    #[must_use]
    pub fn db(&self, slug: &Slug) -> ArtifactDb {
        ArtifactDb::open(self.store.db_dir(slug), self.user_id.clone())
    }

    /// Store an asset, content-addressed.
    ///
    /// The id is the first 32 hex characters of the content's SHA-256, so the
    /// same bytes store once and a different file cannot take the same id.
    pub fn upload_asset(&self, slug: &Slug, contents: &[u8]) -> Result<String, StoreError> {
        let asset_id = asset_id(contents);
        let dir = self.assets_dir(slug);
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::write(dir.join(&asset_id), contents).map_err(|e| StoreError::Io(e.to_string()))?;
        self.store.add_asset(slug, &asset_id)?;
        Ok(asset_id)
    }

    /// Read an asset back.
    pub fn read_asset(&self, slug: &Slug, asset_id: &str) -> Result<Vec<u8>, StoreError> {
        if !crate::slug::is_asset_id(asset_id) {
            return Err(StoreError::Invalid(format!(
                "'{asset_id}' is not an asset id (32 hex characters)"
            )));
        }
        let path = self.assets_dir(slug).join(asset_id);
        std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound(format!("asset {asset_id}"))
            } else {
                StoreError::Io(error.to_string())
            }
        })
    }

    /// The assets a page has stored.
    pub fn list_assets(&self, slug: &Slug) -> Result<Vec<String>, StoreError> {
        Ok(self.store.manifest(slug)?.assets)
    }

    /// Remove an asset.
    pub fn delete_asset(&self, slug: &Slug, asset_id: &str) -> Result<bool, StoreError> {
        let path = self.assets_dir(slug).join(asset_id);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(StoreError::Io(error.to_string())),
        };

        let mut manifest = self.store.manifest(slug)?;
        manifest.assets.retain(|existing| existing != asset_id);
        self.store.save_manifest(&manifest)?;
        Ok(removed)
    }

    /// The comments on a page.
    pub fn comments(&self, slug: &Slug) -> Result<Vec<Comment>, StoreError> {
        let path = self.comments_path(slug);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| StoreError::Io(format!("{}: {error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(StoreError::Io(error.to_string())),
        }
    }

    /// Add a comment to a page.
    pub fn add_comment(
        &self,
        slug: &Slug,
        author: &str,
        body: &str,
    ) -> Result<Comment, StoreError> {
        if body.trim().is_empty() {
            return Err(StoreError::Invalid("a comment needs a body".into()));
        }
        if !self.store.artifact_dir(slug).exists() {
            return Err(StoreError::NotFound(slug.as_str()));
        }

        let mut comments = self.comments(slug)?;
        let comment = Comment {
            id: format!("c{}", comments.len() + 1),
            author: author.to_string(),
            body: body.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            resolved: false,
        };
        comments.push(comment.clone());
        self.write_comments(slug, &comments)?;
        Ok(comment)
    }

    /// Mark a comment resolved.
    pub fn resolve_comment(&self, slug: &Slug, comment_id: &str) -> Result<bool, StoreError> {
        let mut comments = self.comments(slug)?;
        let mut found = false;
        for comment in &mut comments {
            if comment.id == comment_id {
                comment.resolved = true;
                found = true;
            }
        }
        if found {
            self.write_comments(slug, &comments)?;
        }
        Ok(found)
    }

    /// Check a page's stored files against its manifest.
    ///
    /// This is what `verify` answers for a local artifact: the viewer's runtime
    /// diagnostics are a hosted-service idea, but "are the bytes that were
    /// published still the bytes on disk" is not.
    pub fn verify(&self, slug: &Slug) -> Result<VerifyReport, StoreError> {
        let manifest = self.store.manifest(slug)?;
        let version = manifest
            .current()
            .ok_or_else(|| StoreError::NotFound(slug.as_str()))?;

        let mut problems = Vec::new();
        for file in &version.files {
            let path = self
                .store
                .version_dir(slug, &version.label)
                .join(&file.path);
            match std::fs::read(&path) {
                Ok(contents) => {
                    if contents.len() != file.bytes {
                        problems.push(format!(
                            "{}: {} bytes on disk, {} in the manifest",
                            file.path,
                            contents.len(),
                            file.bytes
                        ));
                    } else if asset_id(&contents) != file.sha256[..32] {
                        problems.push(format!("{}: contents changed since publishing", file.path));
                    }
                }
                Err(_) => problems.push(format!("{}: missing", file.path)),
            }
        }

        Ok(VerifyReport {
            slug: slug.as_str(),
            version: version.label.clone(),
            files: version.files.len(),
            problems,
        })
    }

    fn assets_dir(&self, slug: &Slug) -> PathBuf {
        self.store.artifact_dir(slug).join("assets")
    }

    fn comments_path(&self, slug: &Slug) -> PathBuf {
        self.store.artifact_dir(slug).join("comments.json")
    }

    fn write_comments(&self, slug: &Slug, comments: &[Comment]) -> Result<(), StoreError> {
        let path = self.comments_path(slug);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let bytes =
            serde_json::to_vec_pretty(comments).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::write(path, bytes).map_err(|e| StoreError::Io(e.to_string()))
    }
}

/// What `verify` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// The artifact checked.
    pub slug: String,
    /// The version checked.
    pub version: String,
    /// How many files it holds.
    pub files: usize,
    /// Everything wrong with it; empty means the page is intact.
    pub problems: Vec<String>,
}

impl VerifyReport {
    /// Whether the page is intact.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        self.problems.is_empty()
    }
}

/// The content address of a blob: 32 hex characters of its SHA-256.
#[must_use]
pub fn asset_id(contents: &[u8]) -> String {
    let digest = Sha256::digest(contents);
    digest[..16].iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Page;
    use std::collections::BTreeMap;

    fn service() -> (ArtifactService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path());
        (
            ArtifactService::new(store, "http://127.0.0.1:8722", "u_1"),
            dir,
        )
    }

    fn publish(service: &ArtifactService) -> Slug {
        let mut files = BTreeMap::new();
        files.insert(
            crate::store::ENTRY_FILE.to_string(),
            b"<html><body>hi</body></html>".to_vec(),
        );
        let manifest = service
            .store()
            .publish(
                None,
                &Page {
                    title: "Report".into(),
                    files,
                },
            )
            .unwrap();
        Slug::parse(&manifest.slug).unwrap()
    }

    #[test]
    fn the_viewer_url_is_the_origin_and_the_path() {
        let (service, _dir) = service();
        let slug = publish(&service);
        assert_eq!(
            service.url(&slug),
            format!("http://127.0.0.1:8722/artifact/{}", slug.as_str())
        );
    }

    #[test]
    fn an_asset_is_addressed_by_its_contents() {
        let (service, _dir) = service();
        let slug = publish(&service);

        let first = service.upload_asset(&slug, b"image bytes").unwrap();
        assert_eq!(first.len(), 32);
        assert!(crate::slug::is_asset_id(&first));
        assert_eq!(service.read_asset(&slug, &first).unwrap(), b"image bytes");

        // The same bytes are the same asset, stored once.
        let again = service.upload_asset(&slug, b"image bytes").unwrap();
        assert_eq!(first, again);
        assert_eq!(service.list_assets(&slug).unwrap(), vec![first.clone()]);

        // Different bytes cannot take the same id.
        let other = service.upload_asset(&slug, b"other bytes").unwrap();
        assert_ne!(first, other);
        assert_eq!(service.list_assets(&slug).unwrap().len(), 2);
    }

    #[test]
    fn deleting_an_asset_removes_it_from_the_page() {
        let (service, _dir) = service();
        let slug = publish(&service);
        let id = service.upload_asset(&slug, b"bytes").unwrap();

        assert!(service.delete_asset(&slug, &id).unwrap());
        assert!(!service.delete_asset(&slug, &id).unwrap());
        assert!(service.list_assets(&slug).unwrap().is_empty());
        assert!(service.read_asset(&slug, &id).is_err());
    }

    #[test]
    fn an_asset_id_that_is_not_one_is_refused() {
        let (service, _dir) = service();
        let slug = publish(&service);
        assert!(service.read_asset(&slug, "../manifest.json").is_err());
        assert!(service.read_asset(&slug, "short").is_err());
    }

    #[test]
    fn comments_are_added_and_resolved() {
        let (service, _dir) = service();
        let slug = publish(&service);

        let first = service.add_comment(&slug, "user", "looks good").unwrap();
        let second = service.add_comment(&slug, "assistant", "thanks").unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(service.comments(&slug).unwrap().len(), 2);

        assert!(service.resolve_comment(&slug, &first.id).unwrap());
        assert!(!service.resolve_comment(&slug, "nope").unwrap());
        let comments = service.comments(&slug).unwrap();
        assert!(comments[0].resolved);
        assert!(!comments[1].resolved);
    }

    #[test]
    fn a_comment_needs_a_body_and_a_page() {
        let (service, _dir) = service();
        let slug = publish(&service);

        assert!(service.add_comment(&slug, "user", "   ").is_err());
        assert!(service.add_comment(&Slug::new(), "user", "hi").is_err());
    }

    #[test]
    fn verify_passes_on_an_intact_page() {
        let (service, _dir) = service();
        let slug = publish(&service);

        let report = service.verify(&slug).unwrap();
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.files, 1);
    }

    #[test]
    fn verify_notices_a_page_that_changed_on_disk() {
        let (service, _dir) = service();
        let slug = publish(&service);

        let path = service
            .store()
            .version_dir(&slug, "v1")
            .join(crate::store::ENTRY_FILE);

        // Same length, different bytes: only the digest catches this one.
        let original = std::fs::read(&path).unwrap();
        let mut tampered = original.clone();
        tampered[0] = b'X';
        std::fs::write(&path, &tampered).unwrap();

        let report = service.verify(&slug).unwrap();
        assert!(!report.is_intact());
        assert!(report.problems[0].contains("changed"), "{report:?}");

        // A different length is caught as a size change.
        std::fs::write(&path, b"<html>tampered, and longer</html>").unwrap();
        let report = service.verify(&slug).unwrap();
        assert!(report.problems[0].contains("bytes on disk"), "{report:?}");
    }

    #[test]
    fn verify_notices_a_missing_file() {
        let (service, _dir) = service();
        let slug = publish(&service);

        std::fs::remove_file(
            service
                .store()
                .version_dir(&slug, "v1")
                .join(crate::store::ENTRY_FILE),
        )
        .unwrap();

        let report = service.verify(&slug).unwrap();
        assert!(!report.is_intact());
        assert!(report.problems[0].contains("missing"), "{report:?}");
    }
}
