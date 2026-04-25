//! File traversal with gitignore support.
//!
//! Wraps `ignore` and `walkdir` for gitignore-compatible file traversal.

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use std::path::PathBuf;
use thiserror::Error;

/// Errors from file traversal.
#[derive(Debug, Error)]
pub enum TraversalError {
    /// Path does not exist.
    #[error("path does not exist: {0}")]
    NotFound(PathBuf),
    /// I/O error during traversal.
    #[error("I/O error: {0}")]
    Io(std::io::Error),
}

/// Configuration for file traversal.
#[derive(Debug, Clone)]
pub struct TraversalConfig {
    /// Root directory to traverse from.
    pub root: PathBuf,
    /// Maximum directory depth (None for unlimited).
    pub max_depth: Option<usize>,
    /// Follow symbolic links.
    pub follow_links: bool,
    /// Skip hidden files and directories.
    pub skip_hidden: bool,
    /// Whether to read and respect .gitignore files.
    pub git_ignore: bool,
    /// Additional global ignore patterns.
    pub ignore_patterns: Vec<String>,
}

impl TraversalConfig {
    /// Create a new traversal config for the given root directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_depth: None,
            follow_links: false,
            skip_hidden: true,
            git_ignore: true,
            ignore_patterns: Vec::new(),
        }
    }

    /// Set maximum depth.
    #[must_use]
    pub const fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Follow symbolic links.
    #[must_use]
    pub const fn follow_links(mut self) -> Self {
        self.follow_links = true;
        self
    }

    /// Include hidden files.
    #[must_use]
    pub const fn include_hidden(mut self) -> Self {
        self.skip_hidden = false;
        self
    }

    /// Disable gitignore respect.
    #[must_use]
    pub const fn ignore_gitignore(mut self) -> Self {
        self.git_ignore = false;
        self
    }

    /// Add a glob pattern to ignore.
    #[must_use]
    pub fn ignore_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.ignore_patterns.push(pattern.into());
        self
    }
}

/// Iterator over files matching a traversal configuration.
pub struct FileIterator {
    inner: Box<dyn Iterator<Item = Result<PathBuf, TraversalError>>>,
}

impl std::fmt::Debug for FileIterator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileIterator").finish_non_exhaustive()
    }
}

impl Iterator for FileIterator {
    type Item = Result<PathBuf, TraversalError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// Build a file iterator from the given configuration.
pub fn traverse(config: &TraversalConfig) -> Result<FileIterator, TraversalError> {
    if !config.root.exists() {
        return Err(TraversalError::NotFound(config.root.clone()));
    }

    let mut builder = WalkBuilder::new(&config.root);
    builder
        .max_depth(config.max_depth)
        .follow_links(config.follow_links)
        .hidden(config.skip_hidden)
        .git_ignore(config.git_ignore)
        .git_exclude(config.git_ignore)
        .git_global(config.git_ignore);

    if !config.ignore_patterns.is_empty() {
        let mut override_builder = OverrideBuilder::new(&config.root);
        for pattern in &config.ignore_patterns {
            override_builder
                .add(&format!("!{pattern}"))
                .map_err(|e| TraversalError::Io(std::io::Error::other(e)))?;
        }
        builder.overrides(
            override_builder
                .build()
                .map_err(|e| TraversalError::Io(std::io::Error::other(e)))?,
        );
    }

    let iter = builder.build().filter_map(|entry| match entry {
        Ok(entry) => {
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                Some(Ok(entry.into_path()))
            } else {
                None
            }
        }
        Err(e) => Some(Err(TraversalError::Io(std::io::Error::other(e)))),
    });

    Ok(FileIterator {
        inner: Box::new(iter),
    })
}

/// Collect all files under a directory into a vector.
pub fn collect_files(root: impl Into<PathBuf>) -> Result<Vec<PathBuf>, TraversalError> {
    let config = TraversalConfig::new(root);
    traverse(&config)?.collect()
}

/// Count files under a directory without collecting paths.
pub fn count_files(root: impl Into<PathBuf>) -> Result<usize, TraversalError> {
    let config = TraversalConfig::new(root);
    let count = traverse(&config)?.count();
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn temp_dir() -> (PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        (path, tmp)
    }

    fn setup_tree(base: &Path) {
        fs::create_dir_all(base.join("src")).unwrap();
        fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(base.join("src/lib.rs"), "pub fn lib() {}").unwrap();
        fs::write(base.join("Cargo.toml"), "[package]").unwrap();
        fs::write(base.join("README.md"), "# project").unwrap();
        // Hidden file
        fs::write(base.join(".hidden"), "secret").unwrap();
        // Gitignore (requires .git directory for ignore crate to respect it)
        fs::create_dir_all(base.join(".git")).unwrap();
        fs::write(base.join(".gitignore"), "*.log\n").unwrap();
        fs::write(base.join("debug.log"), "log data").unwrap();
        // Subdirectory
        fs::create_dir_all(base.join("target")).unwrap();
        fs::write(base.join("target/output.bin"), "binary").unwrap();
    }

    #[test]
    fn traverse_respects_gitignore() {
        let (base, _tmp) = temp_dir();
        setup_tree(&base);

        let config = TraversalConfig::new(&base);
        let files: Vec<_> = traverse(&config).unwrap().map(|r| r.unwrap()).collect();

        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"lib.rs"));
        assert!(names.contains(&"Cargo.toml"));
        assert!(names.contains(&"README.md"));
        // Hidden files should be skipped
        assert!(!names.contains(&".hidden"));
        // .log files should be ignored by .gitignore
        assert!(!names.contains(&"debug.log"));
    }

    #[test]
    fn traverse_with_max_depth() {
        let (base, _tmp) = temp_dir();
        setup_tree(&base);

        let config = TraversalConfig::new(&base).max_depth(1);
        let files: Vec<_> = traverse(&config).unwrap().map(|r| r.unwrap()).collect();

        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        // Should have top-level files but not src/ or target/ contents
        assert!(names.contains(&"Cargo.toml"));
        assert!(names.contains(&"README.md"));
        assert!(!names.contains(&"main.rs"));
    }

    #[test]
    fn traverse_include_hidden() {
        let (base, _tmp) = temp_dir();
        setup_tree(&base);

        let config = TraversalConfig::new(&base).include_hidden();
        let files: Vec<_> = traverse(&config).unwrap().map(|r| r.unwrap()).collect();

        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        assert!(names.contains(&".hidden"));
    }

    #[test]
    fn traverse_ignore_gitignore_disabled() {
        let (base, _tmp) = temp_dir();
        setup_tree(&base);

        let config = TraversalConfig::new(&base)
            .ignore_gitignore()
            .include_hidden();
        let files: Vec<_> = traverse(&config).unwrap().map(|r| r.unwrap()).collect();

        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        // .log file should appear when gitignore is disabled
        assert!(names.contains(&"debug.log"));
    }

    #[test]
    fn collect_files_returns_all_files() {
        let (base, _tmp) = temp_dir();
        setup_tree(&base);

        // Use include_hidden + ignore_gitignore to get all files for counting
        let config = TraversalConfig::new(&base)
            .include_hidden()
            .ignore_gitignore();
        let files = traverse(&config)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        // main.rs, lib.rs, Cargo.toml, README.md, .hidden, .gitignore, debug.log, output.bin
        assert_eq!(files.len(), 8);
    }

    #[test]
    fn traverse_nonexistent_path_returns_error() {
        let config = TraversalConfig::new("/nonexistent/path/that/does/not/exist");
        let result = traverse(&config);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TraversalError::NotFound(_)));
    }
}
