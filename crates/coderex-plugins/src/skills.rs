//! Skill loading — bundled, user dir, repo dir, plugin, and MCP skills.

use std::path::PathBuf;

/// Parsed skill frontmatter metadata.
#[derive(Debug, Clone)]
pub struct SkillMetadata {
    /// The skill name.
    pub name: String,
    /// Hooks that trigger this skill.
    pub hooks: Vec<String>,
    /// Optional agent this skill is associated with.
    pub agent: Option<String>,
    /// Tools this skill is allowed to use.
    pub allowed_tools: Vec<String>,
}

/// A loaded skill.
#[derive(Debug)]
pub struct Skill {
    /// The skill's parsed metadata.
    pub metadata: SkillMetadata,
    /// Where this skill was loaded from.
    pub source: SkillSource,
}

/// Where a skill was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// Bundled with CodeRex.
    Bundled,
    /// Loaded from the user's skill directory.
    UserDir,
    /// Loaded from the repository's skill directory.
    RepoDir,
    /// Loaded from a plugin.
    Plugin,
    /// Discovered via MCP.
    Mcp,
}

/// Loads skills from all configured sources.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SkillLoader {
    bundled_path: PathBuf,
    user_path: PathBuf,
}

impl SkillLoader {
    /// Create a new skill loader.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(bundled_path: PathBuf, user_path: PathBuf) -> Self {
        Self {
            bundled_path,
            user_path,
        }
    }
}
