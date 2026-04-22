//! Skill loading — bundled, user dir, repo dir, plugin, and MCP skills.

use std::path::{Path, PathBuf};

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
    /// Skill description for display.
    pub description: String,
    /// Whether the skill is enabled.
    pub enabled: bool,
}

/// A loaded skill.
#[derive(Debug)]
pub struct Skill {
    /// The skill's parsed metadata.
    pub metadata: SkillMetadata,
    /// Where this skill was loaded from.
    pub source: SkillSource,
    /// The path to the skill file.
    pub path: PathBuf,
}

/// Where a skill was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

impl SkillSource {
    /// Precedence for conflict resolution.
    /// Higher value = higher priority.
    #[must_use]
    pub fn precedence(self) -> u8 {
        match self {
            SkillSource::UserDir => 5,
            SkillSource::RepoDir => 4,
            SkillSource::Bundled => 3,
            SkillSource::Plugin => 2,
            SkillSource::Mcp => 1,
        }
    }
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillSource::Bundled => write!(f, "bundled"),
            SkillSource::UserDir => write!(f, "user"),
            SkillSource::RepoDir => write!(f, "repo"),
            SkillSource::Plugin => write!(f, "plugin"),
            SkillSource::Mcp => write!(f, "mcp"),
        }
    }
}

/// A skill script file — the actual content loaded from disk.
#[derive(Debug)]
pub struct SkillFile {
    /// The parsed frontmatter.
    pub metadata: SkillMetadata,
    /// The script content (after the frontmatter).
    pub script: String,
}

/// Deserialize a field that may be a single string or a list of strings.
fn deserialize_string_or_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrList;

    impl<'de> serde::de::Visitor<'de> for StringOrList {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or a list of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![value.to_string()])
        }

        fn visit_seq<S>(self, visitor: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            serde::Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(visitor))
        }
    }

    deserializer.deserialize_any(StringOrList)
}

/// Intermediate representation for YAML frontmatter parsing.
#[derive(Debug, serde::Deserialize)]
struct SkillFrontmatter {
    name: String,
    #[serde(default, deserialize_with = "deserialize_string_or_list")]
    hooks: Vec<String>,
    agent: Option<String>,
    #[serde(default, alias = "allowed_tools", deserialize_with = "deserialize_string_or_list")]
    allowed_tools: Vec<String>,
    #[serde(default)]
    description: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Parse skill frontmatter from markdown file content using serde_yaml.
/// Expected format:
/// ```markdown
/// ---
/// name: my-skill
/// hooks: [pre-query, post-tool]
/// agent: general
/// allowed_tools: [Bash, Read]
/// description: A skill that does X
/// enabled: true
/// ---
/// script content here...
/// ```
#[must_use]
pub fn parse_skill_file(content: &str) -> Option<SkillFile> {
    if !content.starts_with("---") {
        return None;
    }

    // Find the closing ---
    let closing = content[3..].find("\n---\n")?;
    let frontmatter_end = 3 + closing + 5; // "---\n" is 4 chars + newline
    let frontmatter = &content[3..3 + closing];

    let parsed: SkillFrontmatter = serde_yaml::from_str(frontmatter).ok()?;

    if parsed.name.is_empty() {
        return None;
    }

    let script = content[frontmatter_end..].to_string();

    Some(SkillFile {
        metadata: SkillMetadata {
            name: parsed.name,
            hooks: parsed.hooks,
            agent: parsed.agent,
            allowed_tools: parsed.allowed_tools,
            description: parsed.description,
            enabled: parsed.enabled,
        },
        script,
    })
}

/// Loads skills from all configured sources.
#[derive(Debug)]
pub struct SkillLoader {
    /// Bundled skills root path.
    bundled_path: PathBuf,
    /// User skills directory.
    user_path: PathBuf,
    /// Loaded skills indexed by name.
    skills: Vec<Skill>,
}

impl SkillLoader {
    /// Create a new skill loader.
    #[must_use]
    pub fn new(bundled_path: PathBuf, user_path: PathBuf) -> Self {
        Self {
            bundled_path,
            user_path,
            skills: Vec::new(),
        }
    }

    /// Discover and load skills from all sources.
    ///
    /// After loading, conflicts are resolved by precedence:
    /// `UserDir > RepoDir > Bundled > Plugin > Mcp`.
    pub fn load_all(&mut self, repo_path: Option<&Path>) {
        self.skills.clear();

        // 1. Bundled skills
        let bundled = self.bundled_path.clone();
        self.load_from_dir(&bundled, SkillSource::Bundled);

        // 2. User skills
        let user = self.user_path.clone();
        self.load_from_dir(&user, SkillSource::UserDir);

        // 3. Repo skills
        if let Some(repo) = repo_path {
            let repo_skills = repo.join(".coderex").join("skills");
            if repo_skills.exists() {
                self.load_from_dir(&repo_skills, SkillSource::RepoDir);
            }
        }

        // 4. Resolve conflicts by precedence
        self.resolve_conflicts();
    }

    /// Resolve duplicate skill names by source precedence.
    ///
    /// Logs a warning for each overridden skill.
    fn resolve_conflicts(&mut self) {
        use std::collections::HashMap;

        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (idx, skill) in self.skills.iter().enumerate() {
            by_name.entry(&skill.metadata.name).or_default().push(idx);
        }

        let mut keep: Vec<bool> = vec![true; self.skills.len()];
        for (_name, indices) in by_name {
            if indices.len() <= 1 {
                continue;
            }
            // Find the highest-precedence skill
            let best_idx = *indices
                .iter()
                .max_by_key(|&&idx| self.skills[idx].source.precedence())
                .unwrap_or(&indices[0]);

            for &idx in &indices {
                if idx != best_idx {
                    let overridden = &self.skills[idx];
                    let winner = &self.skills[best_idx];
                    tracing::warn!(
                        skill = %overridden.metadata.name,
                        overridden_source = %overridden.source,
                        winner_source = %winner.source,
                        "skill conflict resolved: {} overridden by {}",
                        overridden.source,
                        winner.source
                    );
                    keep[idx] = false;
                }
            }
        }

        let mut retained = Vec::with_capacity(self.skills.len());
        for (idx, skill) in self.skills.drain(..).enumerate() {
            if keep[idx] {
                retained.push(skill);
            }
        }
        self.skills = retained;
    }

    /// Load skills from a directory.
    fn load_from_dir(&mut self, dir: &Path, source: SkillSource) {
        if !dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Some(skill_file) = parse_skill_file(&content) {
                            self.skills.push(Skill {
                                metadata: skill_file.metadata,
                                source,
                                path,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Add a skill from a plugin.
    pub fn add_plugin_skill(&mut self, metadata: SkillMetadata, path: PathBuf) {
        self.skills.push(Skill {
            metadata,
            source: SkillSource::Plugin,
            path,
        });
    }

    /// Add a skill from MCP discovery.
    pub fn add_mcp_skill(&mut self, metadata: SkillMetadata) {
        self.skills.push(Skill {
            metadata,
            source: SkillSource::Mcp,
            path: PathBuf::new(),
        });
    }

    /// Get a skill by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.metadata.name == name)
    }

    /// List all enabled skills.
    #[must_use]
    pub fn enabled(&self) -> Vec<&Skill> {
        self.skills.iter().filter(|s| s.metadata.enabled).collect()
    }

    /// List all skills with their source.
    #[must_use]
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    /// Count skills by source.
    #[must_use]
    pub fn count_by_source(&self) -> Vec<(SkillSource, usize)> {
        let mut counts = std::collections::HashMap::new();
        for skill in &self.skills {
            *counts.entry(skill.source).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_content() -> String {
        r#"---
name: test-skill
hooks: [pre-query]
agent: general
allowed_tools: [Bash, Read]
description: A test skill
enabled: true
---
This is the script content.
"#
        .to_string()
    }

    #[test]
    fn parse_valid_skill() {
        let content = skill_content();
        let parsed = parse_skill_file(&content).unwrap();
        assert_eq!(parsed.metadata.name, "test-skill");
        assert_eq!(parsed.metadata.hooks, vec!["pre-query"]);
        assert_eq!(parsed.metadata.agent, Some("general".into()));
        assert_eq!(parsed.metadata.allowed_tools, vec!["Bash", "Read"]);
        assert!(parsed.metadata.enabled);
        assert!(!parsed.script.is_empty());
    }

    #[test]
    fn parse_no_frontmatter_returns_none() {
        assert!(parse_skill_file("just some text").is_none());
    }

    #[test]
    fn parse_empty_name_returns_none() {
        let content = "---\nname: \n---\nscript";
        assert!(parse_skill_file(content).is_none());
    }

    #[test]
    fn parse_disabled_skill() {
        let content = "---\nname: disabled\nenabled: false\n---\nscript";
        let parsed = parse_skill_file(content).unwrap();
        assert!(!parsed.metadata.enabled);
    }

    #[test]
    fn parse_single_hook() {
        let content = "---\nname: x\nhooks: hook1\n---\ns";
        let parsed = parse_skill_file(content).unwrap();
        assert_eq!(parsed.metadata.hooks, vec!["hook1"]);
    }

    #[test]
    fn loader_load_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("test.md");
        std::fs::write(&skill_path, skill_content()).unwrap();

        let mut loader = SkillLoader::new(dir.path().to_path_buf(), PathBuf::from("/nonexistent"));
        loader.load_all(None);

        assert_eq!(loader.list().len(), 1);
        assert_eq!(loader.enabled().len(), 1);
    }

    #[test]
    fn loader_nonexistent_dir_is_silent() {
        let mut loader = SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/also-nonexistent"));
        loader.load_all(None);
        assert!(loader.list().is_empty());
    }

    #[test]
    fn loader_add_plugin_skill() {
        let mut loader = SkillLoader::new(PathBuf::new(), PathBuf::new());
        loader.add_plugin_skill(SkillMetadata {
            name: "plugin-skill".into(),
            hooks: vec!["pre-query".into()],
            agent: None,
            allowed_tools: vec![],
            description: "From plugin".into(),
            enabled: true,
        }, PathBuf::from("/plugin/test.md"));

        let skill = loader.get("plugin-skill").unwrap();
        assert_eq!(skill.source, SkillSource::Plugin);
    }

    #[test]
    fn loader_add_mcp_skill() {
        let mut loader = SkillLoader::new(PathBuf::new(), PathBuf::new());
        loader.add_mcp_skill(SkillMetadata {
            name: "mcp-skill".into(),
            hooks: vec![],
            agent: None,
            allowed_tools: vec![],
            description: "From MCP".into(),
            enabled: true,
        });

        assert_eq!(loader.get("mcp-skill").unwrap().source, SkillSource::Mcp);
    }

    #[test]
    fn count_by_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), skill_content()).unwrap();

        let mut loader = SkillLoader::new(dir.path().to_path_buf(), PathBuf::new());
        loader.load_all(None);
        loader.add_mcp_skill(SkillMetadata {
            name: "m".into(), hooks: vec![], agent: None,
            allowed_tools: vec![], description: String::new(), enabled: true,
        });

        let counts = loader.count_by_source();
        assert!(counts.iter().any(|(s, c)| matches!(s, SkillSource::Bundled) && *c == 1));
        assert!(counts.iter().any(|(s, c)| matches!(s, SkillSource::Mcp) && *c == 1));
    }

    #[test]
    fn repo_dir_skills_loaded() {
        let repo = tempfile::tempdir().unwrap();
        let skills_dir = repo.path().join(".coderex").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("repo.md"), skill_content()).unwrap();

        let mut loader = SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent"));
        loader.load_all(Some(repo.path()));

        assert_eq!(loader.enabled().len(), 1);
        assert_eq!(loader.enabled()[0].source, SkillSource::RepoDir);
    }

    #[test]
    fn conflict_resolution_prefers_user_over_bundled() {
        let dir = tempfile::tempdir().unwrap();
        // Write a bundled skill
        std::fs::write(dir.path().join("bundled.md"), skill_content()).unwrap();

        // Write a user skill with same name
        let user_dir = tempfile::tempdir().unwrap();
        std::fs::write(user_dir.path().join("user.md"), skill_content()).unwrap();

        let mut loader = SkillLoader::new(dir.path().to_path_buf(), user_dir.path().to_path_buf());
        loader.load_all(None);

        // Should only have 1 skill (duplicate resolved)
        assert_eq!(loader.list().len(), 1);
        // UserDir takes precedence over Bundled
        assert_eq!(loader.list()[0].source, SkillSource::UserDir);
    }

    #[test]
    fn source_precedence_order() {
        assert!(SkillSource::UserDir.precedence() > SkillSource::RepoDir.precedence());
        assert!(SkillSource::RepoDir.precedence() > SkillSource::Bundled.precedence());
        assert!(SkillSource::Bundled.precedence() > SkillSource::Plugin.precedence());
        assert!(SkillSource::Plugin.precedence() > SkillSource::Mcp.precedence());
    }

    #[test]
    fn frontmatter_parses_quoted_colons() {
        let content = r#"---
name: quoted-skill
description: "This: has a colon"
hooks: [pre-query]
enabled: true
---
script"#;
        let parsed = parse_skill_file(content).unwrap();
        assert_eq!(parsed.metadata.description, "This: has a colon");
    }
}
