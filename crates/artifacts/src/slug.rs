//! Artifact identity — slugs, viewer URLs, and asset ids.
//!
//! A published page is addressed by a slug, which is a UUID: it is the part of
//! the URL that survives vanity renaming, and it is what the store keys on.
//! Everything here is about turning user input (a path, a URL, a bare slug)
//! into that one identity, because a miss means reading or overwriting the
//! wrong page.

use std::fmt;
use uuid::Uuid;

/// Path a viewer opens: the plain form.
pub const VIEWER_PREFIX: &str = "/artifact";

/// Path a hosted-code viewer opens.
pub const CODE_VIEWER_PREFIX: &str = "/code/artifact";

/// Path an embedded frame opens.
pub const FRAME_VIEWER_PREFIX: &str = "/code/frame";

/// Query parameter carrying a session key.
pub const SESSION_KEY_PARAM: &str = "sk";

/// An artifact's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Slug(Uuid);

impl Slug {
    /// A fresh identity for a new artifact.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse a slug from a bare slug, a path, or a URL.
    #[must_use]
    pub fn parse(input: &str) -> Option<Self> {
        parse_artifact_url(input).map(|parsed| parsed.slug)
    }

    /// The hyphenated form that appears in URLs.
    #[must_use]
    pub fn as_str(&self) -> String {
        self.0.hyphenated().to_string()
    }
}

impl Default for Slug {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// The parts of an artifact URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactUrl {
    /// The artifact's identity.
    pub slug: Slug,
    /// A human-readable prefix in front of the slug, if the URL carries one.
    pub vanity: Option<String>,
    /// A session key from `?sk=`, if present.
    pub session_key: Option<String>,
}

/// Parse an artifact reference: a URL, a path, or a bare slug.
///
/// Accepted shapes: `https://host/artifact/<slug>`,
/// `https://host/code/artifact/<slug>`, `https://host/code/frame/<slug>`, an
/// optional `<vanity>-` in front of the slug, and a bare UUID.
#[must_use]
pub fn parse_artifact_url(input: &str) -> Option<ArtifactUrl> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(slug) = Uuid::parse_str(trimmed) {
        return Some(ArtifactUrl {
            slug: Slug(slug),
            vanity: None,
            session_key: None,
        });
    }

    let (path, query) = match trimmed.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (trimmed, None),
    };

    let session_key = query.and_then(|query| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == SESSION_KEY_PARAM && is_session_key(value)).then(|| value.to_string())
        })
    });

    // A slug is a hyphenated UUID, which is exactly 36 characters. A vanity
    // prefix sits in front of it as `<vanity>-<uuid>`, so the boundary is
    // found from the end rather than by splitting on a dash — the UUID has
    // four dashes of its own.
    const UUID_LEN: usize = 36;
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    let (vanity, candidate) =
        if last.len() > UUID_LEN + 1 && last.as_bytes()[last.len() - UUID_LEN - 1] == b'-' {
            let (vanity, candidate) = last.split_at(last.len() - UUID_LEN - 1);
            (
                is_vanity(vanity).then(|| vanity.to_string()),
                &candidate[1..],
            )
        } else {
            (None, last)
        };

    let slug = Slug(Uuid::parse_str(candidate).ok()?);
    Some(ArtifactUrl {
        slug,
        vanity,
        session_key,
    })
}

/// Canonicalize a URL the way a permission rule has to see it.
///
/// `http` reads as `https`, a trailing root dot is dropped, and a default port
/// is removed — so two spellings of the same page match the same rule.
#[must_use]
pub fn canonicalize_url(input: &str) -> String {
    let trimmed = input.trim();

    // `http` and `https` name the same page; anything else is left alone.
    let after_scheme = if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else {
        return trimmed.to_string();
    };

    let (authority, path) = match after_scheme.find('/') {
        Some(index) => (&after_scheme[..index], &after_scheme[index..]),
        None => (after_scheme, ""),
    };

    let host = authority.trim_end_matches('.').to_ascii_lowercase();
    let host = host
        .trim_end_matches(":443")
        .trim_end_matches(":80")
        .to_string();

    format!("https://{host}{path}")
}

/// The viewer path for an artifact.
#[must_use]
pub fn viewer_path(slug: &Slug) -> String {
    format!("{VIEWER_PREFIX}/{}", slug.as_str())
}

/// The viewer URL for an artifact on a given origin.
#[must_use]
pub fn viewer_url(origin: &str, slug: &Slug) -> String {
    format!("{}{}", origin.trim_end_matches('/'), viewer_path(slug))
}

/// Whether an id is a resource (asset) id rather than a slug.
#[must_use]
pub fn is_asset_id(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether a version label is one the store accepts.
#[must_use]
pub fn is_version_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_vanity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 60
        && value.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_session_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLUG: &str = "3f2a1c44-9b7e-4d1a-8f0e-5c6b7a8d9e0f";

    #[test]
    fn a_bare_slug_parses() {
        let parsed = parse_artifact_url(SLUG).unwrap();
        assert_eq!(parsed.slug.as_str(), SLUG);
        assert!(parsed.vanity.is_none());
    }

    #[test]
    fn every_viewer_path_parses() {
        for path in [
            format!("/artifact/{SLUG}"),
            format!("/code/artifact/{SLUG}"),
            format!("/code/frame/{SLUG}"),
        ] {
            let url = format!("https://claude.ai{path}");
            assert_eq!(
                parse_artifact_url(&url).unwrap().slug.as_str(),
                SLUG,
                "{url}"
            );
        }
    }

    #[test]
    fn a_vanity_prefix_is_kept_separate_from_the_slug() {
        let url = format!("https://claude.ai/artifact/my-page-{SLUG}");
        let parsed = parse_artifact_url(&url).unwrap();
        assert_eq!(parsed.vanity.as_deref(), Some("my-page"));
        assert_eq!(parsed.slug.as_str(), SLUG);
    }

    #[test]
    fn a_slug_that_merely_contains_a_dash_is_not_a_vanity_pair() {
        // The tail of this one is a UUID-looking fragment, not a UUID.
        assert!(parse_artifact_url("https://claude.ai/artifact/not-a-slug-3f2a1c44").is_none());
    }

    #[test]
    fn the_session_key_is_read_out_of_the_query() {
        let url = format!("https://claude.ai/artifact/{SLUG}?sk=abc_DEF-123&other=1");
        let parsed = parse_artifact_url(&url).unwrap();
        assert_eq!(parsed.session_key.as_deref(), Some("abc_DEF-123"));
        assert_eq!(parsed.slug.as_str(), SLUG);
    }

    #[test]
    fn a_trailing_slash_or_junk_does_not_hide_the_slug() {
        let url = format!("https://claude.ai/artifact/{SLUG}/");
        assert_eq!(parse_artifact_url(&url).unwrap().slug.as_str(), SLUG);
        assert!(parse_artifact_url("https://claude.ai/artifact/").is_none());
    }

    #[test]
    fn canonicalizing_folds_the_spellings_of_one_page() {
        for (input, expected) in [
            ("http://claude.ai", "https://claude.ai"),
            ("https://claude.ai.", "https://claude.ai"),
            ("https://Claude.AI:443", "https://claude.ai"),
            ("http://claude.ai:80/x", "https://claude.ai/x"),
            ("https://claude.ai/a/b", "https://claude.ai/a/b"),
        ] {
            assert_eq!(canonicalize_url(input), expected, "{input}");
        }
    }

    #[test]
    fn the_viewer_path_matches_the_published_shape() {
        let slug = Slug::parse(SLUG).unwrap();
        assert_eq!(viewer_path(&slug), format!("/artifact/{SLUG}"));
        assert_eq!(
            viewer_url("http://127.0.0.1:8722/", &slug),
            format!("http://127.0.0.1:8722/artifact/{SLUG}")
        );
    }

    #[test]
    fn asset_ids_and_version_labels_are_recognised() {
        assert!(is_asset_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_asset_id("0123456789abcdef"));
        assert!(!is_asset_id("0123456789ABCDEF0123456789abcdefg"));

        assert!(is_version_label("v1"));
        assert!(is_version_label("draft_2-final"));
        assert!(!is_version_label(""));
        assert!(!is_version_label("has space"));
        assert!(!is_version_label(&"x".repeat(65)));
    }
}
