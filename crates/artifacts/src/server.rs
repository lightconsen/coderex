//! The local origin artifacts are served from.
//!
//! A published page is only useful if something can open it, and the reference
//! gets that from a hosted service. CodeRex serves its own store instead: a
//! loopback HTTP server that answers `/artifact/<slug>/…` out of the version
//! the artifact currently points at.
//!
//! What it will not do is serve anything that is not a published file. The
//! request path is checked against the version's manifest and resolved inside
//! the version directory, so a `..` or a symlink cannot reach the manifest,
//! another artifact, or the filesystem behind them.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::slug::Slug;
use crate::store::ArtifactStore;

/// Longest request head the server will read.
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// A running artifact server.
#[derive(Debug)]
pub struct ArtifactServer {
    origin: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl ArtifactServer {
    /// Serve `store` on `127.0.0.1:port` (port 0 picks a free one).
    ///
    /// # Errors
    ///
    /// Fails if the loopback address cannot be bound.
    pub async fn start(store: ArtifactStore, port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let addr = listener.local_addr()?;
        let origin = format!("http://{addr}");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let store = Arc::new(store);
        let bound = origin.clone();

        let handle = tokio::spawn(async move {
            tracing::info!(%bound, "artifact server listening");
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else { continue };
                        let store = Arc::clone(&store);
                        tokio::spawn(async move {
                            if let Err(error) = serve_connection(stream, &store).await {
                                tracing::debug!("artifact request failed: {error}");
                            }
                        });
                    }
                }
            }
        });

        Ok(Self {
            origin,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        })
    }

    /// The origin pages are served from, such as `http://127.0.0.1:8722`.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The address it is bound to.
    ///
    /// # Panics
    ///
    /// Panics if the origin is not a parseable address, which cannot happen
    /// for an origin this type built.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.origin
            .trim_start_matches("http://")
            .parse()
            .expect("origin is a bound loopback address")
    }

    /// Stop serving.
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

/// Answer one request and close.
async fn serve_connection(mut stream: TcpStream, store: &ArtifactStore) -> std::io::Result<()> {
    let request = read_head(&mut stream).await?;
    let response = match request {
        Some(Request { method, path }) => respond(&method, &path, store).await,
        None => Response::status(400, "malformed request"),
    };
    stream.write_all(&response.render()).await?;
    stream.flush().await
}

/// A request head, reduced to what routing needs.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    method: String,
    path: String,
}

async fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];

    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        if buffer.len() > MAX_REQUEST_HEAD {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    let head = String::from_utf8_lossy(&buffer);
    let mut lines = head.lines();
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };

    let path = target.split('?').next().unwrap_or(target).to_string();
    Ok(Some(Request {
        method: method.to_ascii_uppercase(),
        path,
    }))
}

/// A response, ready to write.
#[derive(Debug)]
struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn status(status: u16, message: &str) -> Self {
        Self {
            status,
            reason: reason_for(status),
            content_type: "text/plain; charset=utf-8",
            body: message.as_bytes().to_vec(),
        }
    }

    fn render(&self) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
             X-Content-Type-Options: nosniff\r\n\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

async fn respond(method: &str, path: &str, store: &ArtifactStore) -> Response {
    if method != "GET" && method != "HEAD" {
        return Response::status(405, "artifacts are read with GET");
    }

    let Some(slug) = route(path) else {
        return Response::status(404, "not found");
    };

    let manifest = match store.manifest(&slug) {
        Ok(manifest) => manifest,
        Err(_) => return Response::status(404, "no such artifact"),
    };

    // `/artifact/<slug>` and `/artifact/<slug>/` both open the entry page.
    let relative = entry_path(path);
    let current = manifest.current_version.clone();
    let Some(version) = manifest.current() else {
        return Response::status(404, "this artifact has no version");
    };

    let Ok(normalized) = crate::store::normalize_path(&relative) else {
        return Response::status(404, "not found");
    };
    if !version.files.iter().any(|file| file.path == normalized) {
        return Response::status(404, "not found");
    }

    let base = store.version_dir(&slug, &current);
    let Some(target) = crate::store::resolve_within(&base, &normalized) else {
        return Response::status(404, "not found");
    };

    let body = match std::fs::read(&target) {
        Ok(body) => body,
        Err(_) => return Response::status(404, "not found"),
    };

    Response {
        status: 200,
        reason: "OK",
        content_type: content_type_for(&normalized),
        body,
    }
}

/// The artifact a request path names.
fn route(path: &str) -> Option<Slug> {
    let rest = path
        .strip_prefix(crate::slug::CODE_VIEWER_PREFIX)
        .or_else(|| path.strip_prefix(crate::slug::VIEWER_PREFIX))
        .or_else(|| path.strip_prefix(crate::slug::FRAME_VIEWER_PREFIX))?;
    let rest = rest.trim_start_matches('/');
    let slug = rest.split('/').next()?;
    Slug::parse(slug)
}

/// The file a request path names, defaulting to the entry page.
fn entry_path(path: &str) -> String {
    let rest = path
        .strip_prefix(crate::slug::CODE_VIEWER_PREFIX)
        .or_else(|| path.strip_prefix(crate::slug::VIEWER_PREFIX))
        .or_else(|| path.strip_prefix(crate::slug::FRAME_VIEWER_PREFIX))
        .unwrap_or(path);
    let mut parts = rest.trim_start_matches('/').splitn(2, '/');
    let _slug = parts.next();
    let file = parts.next().unwrap_or("").trim_start_matches('/');

    if file.is_empty() {
        crate::store::ENTRY_FILE.to_string()
    } else {
        file.to_string()
    }
}

fn content_type_for(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn published() -> (ArtifactStore, tempfile::TempDir, Slug) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path());

        let mut files = BTreeMap::new();
        files.insert(
            crate::store::ENTRY_FILE.to_string(),
            b"<html><body>published</body></html>".to_vec(),
        );
        files.insert("app.js".to_string(), b"console.log(1)".to_vec());
        let manifest = store
            .publish(
                None,
                &crate::store::Page {
                    title: "Report".into(),
                    files,
                },
            )
            .unwrap();
        let slug = Slug::parse(&manifest.slug).unwrap();
        (store, dir, slug)
    }

    /// Fetch a path with a real HTTP request.
    ///
    /// The client is async on purpose: the server runs as a task on the test's
    /// own runtime, and a blocking client on a current-thread runtime would
    /// hold the only worker while waiting for a server that cannot run.
    async fn get(origin: &str, path: &str) -> (u16, String, String) {
        let addr = origin.trim_start_matches("http://");
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
            .await
            .unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response).to_string();

        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let content_type = response
            .lines()
            .find_map(|line| line.strip_prefix("Content-Type: "))
            .unwrap_or("")
            .to_string();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, content_type, body)
    }

    #[tokio::test]
    async fn a_published_page_is_served_at_its_url() {
        let (store, _dir, slug) = published();
        let server = ArtifactServer::start(store, 0).await.unwrap();

        let (status, content_type, body) =
            get(server.origin(), &crate::slug::viewer_path(&slug)).await;
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert!(body.contains("published"), "{body}");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_file_beside_the_page_is_served_with_its_type() {
        let (store, _dir, slug) = published();
        let server = ArtifactServer::start(store, 0).await.unwrap();

        let (status, content_type, body) = get(
            server.origin(),
            &format!("{}/app.js", crate::slug::viewer_path(&slug)),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/javascript; charset=utf-8");
        assert!(body.contains("console.log"), "{body}");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn the_manifest_is_not_served() {
        let (store, _dir, slug) = published();
        let server = ArtifactServer::start(store, 0).await.unwrap();

        for path in [
            "manifest.json",
            "../manifest.json",
            "..%2fmanifest.json",
            "db/boards/b1.json",
        ] {
            let (status, _, _) = get(
                server.origin(),
                &format!("{}/{path}", crate::slug::viewer_path(&slug)),
            )
            .await;
            assert_eq!(status, 404, "{path} was served");
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn an_unknown_artifact_or_path_is_not_found() {
        let (store, _dir, slug) = published();
        let server = ArtifactServer::start(store, 0).await.unwrap();

        let (missing_slug, _, _) =
            get(server.origin(), &format!("/artifact/{}", Slug::new())).await;
        assert_eq!(missing_slug, 404);

        let (missing_file, _, _) = get(
            server.origin(),
            &format!("{}/nope.png", crate::slug::viewer_path(&slug)),
        )
        .await;
        assert_eq!(missing_file, 404);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_republish_is_what_the_viewer_serves() {
        let (store, _dir, slug) = published();
        let server = ArtifactServer::start(store.clone(), 0).await.unwrap();

        let mut files = BTreeMap::new();
        files.insert(
            crate::store::ENTRY_FILE.to_string(),
            b"<html><body>second version</body></html>".to_vec(),
        );
        store
            .publish(
                Some(slug),
                &crate::store::Page {
                    title: "Report".into(),
                    files,
                },
            )
            .unwrap();

        let (status, _, body) = get(server.origin(), &crate::slug::viewer_path(&slug)).await;
        assert_eq!(status, 200);
        assert!(body.contains("second version"), "{body}");

        server.shutdown().await;
    }

    #[test]
    fn routing_reads_the_slug_out_of_every_viewer_path() {
        let slug = Slug::new();
        let path = slug.as_str();
        for prefix in [
            crate::slug::VIEWER_PREFIX,
            crate::slug::CODE_VIEWER_PREFIX,
            crate::slug::FRAME_VIEWER_PREFIX,
        ] {
            assert_eq!(route(&format!("{prefix}/{path}")), Some(slug));
            assert_eq!(route(&format!("{prefix}/{path}/app.js")), Some(slug));
        }
        assert!(route("/other/thing").is_none());
    }

    #[test]
    fn content_types_cover_what_a_page_carries() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type_for("a/b/app.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for("style.css"), "text/css; charset=utf-8");
        assert_eq!(content_type_for("data.json"), "application/json");
        assert_eq!(content_type_for("logo.svg"), "image/svg+xml");
        assert_eq!(content_type_for("blob"), "application/octet-stream");
    }

    #[test]
    fn the_entry_path_defaults_to_the_page() {
        let slug = Slug::new();
        let base = crate::slug::viewer_path(&slug);
        assert_eq!(entry_path(&base), crate::store::ENTRY_FILE);
        assert_eq!(entry_path(&format!("{base}/")), crate::store::ENTRY_FILE);
        assert_eq!(entry_path(&format!("{base}/app.js")), "app.js");
    }
}
