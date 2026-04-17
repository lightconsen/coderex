//! Session-level memory store — in-memory facts with disk persistence.

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// A single fact extracted from a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFact {
    /// Unique fact identifier.
    pub id: Uuid,
    /// The session this fact belongs to.
    pub session_id: Uuid,
    /// The fact content.
    pub fact: String,
    /// Category of the fact.
    pub category: FactCategory,
    /// When the fact was created.
    pub created_at: DateTime<Utc>,
    /// Confidence in the fact (0.0 to 1.0).
    pub confidence: f64,
}

/// Category of a session fact.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum FactCategory {
    /// Project structure information.
    ProjectStructure,
    /// Code patterns observed.
    CodePattern,
    /// Dependency information.
    Dependency,
    /// Architecture decisions.
    Architecture,
    /// User preferences.
    UserPreference,
    /// Project conventions.
    Convention,
    /// Other/unclassified.
    Other,
}

/// Session-level facts store.
///
/// Facts are stored in memory with optional disk persistence.
#[derive(Debug)]
pub struct SessionMemoryStore {
    base_path: PathBuf,
    facts: RwLock<HashMap<Uuid, Vec<SessionFact>>>,
}

impl SessionMemoryStore {
    /// Create a new session memory store.
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            facts: RwLock::new(HashMap::new()),
        }
    }

    /// Store a fact for a session.
    pub fn extract_fact(&self, session_id: Uuid, fact: SessionFact) {
        self.facts.write().entry(session_id).or_default().push(fact);
    }

    /// Get all facts for a session.
    pub fn get_facts(&self, session_id: Uuid) -> Vec<SessionFact> {
        self.facts
            .read()
            .get(&session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Get facts filtered by category.
    pub fn get_facts_by_category(
        &self,
        session_id: Uuid,
        category: FactCategory,
    ) -> Vec<SessionFact> {
        self.facts
            .read()
            .get(&session_id)
            .map(|facts| facts.iter().filter(|f| f.category == category).cloned().collect())
            .unwrap_or_default()
    }

    /// Persist all facts to disk for a session.
    pub fn persist(&self, session_id: Uuid) -> std::io::Result<()> {
        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir)?;
        let facts = {
            let guard = self.facts.read();
            guard.get(&session_id).cloned()
        };
        if let Some(session_facts) = facts {
            let serialized = serde_json::to_string_pretty(&session_facts)
                .map_err(std::io::Error::other)?;
            std::fs::write(dir.join("facts.json"), serialized)?;
        }
        Ok(())
    }

    /// Load facts from disk for a session.
    pub fn load(&self, session_id: Uuid) -> std::io::Result<Vec<SessionFact>> {
        let path = self.session_dir(session_id).join("facts.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let facts: Vec<SessionFact> = serde_json::from_str(&content).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        self.facts.write().insert(session_id, facts.clone());
        Ok(facts)
    }

    fn session_dir(&self, session_id: Uuid) -> PathBuf {
        self.base_path.join(session_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (SessionMemoryStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionMemoryStore::new(tmp.path().to_path_buf());
        (store, tmp)
    }

    fn test_fact(session_id: Uuid, fact: &str, category: FactCategory) -> SessionFact {
        SessionFact {
            id: Uuid::new_v4(),
            session_id,
            fact: fact.into(),
            category,
            created_at: Utc::now(),
            confidence: 0.9,
        }
    }

    #[test]
    fn extract_fact_and_get_facts_returns_it() {
        let (store, _tmp) = temp_store();
        let sid = Uuid::new_v4();

        store.extract_fact(sid, test_fact(sid, "uses Cargo workspace", FactCategory::ProjectStructure));

        let facts = store.get_facts(sid);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].fact, "uses Cargo workspace");
        assert_eq!(facts[0].category, FactCategory::ProjectStructure);
    }

    #[test]
    fn get_facts_by_category_filters_correctly() {
        let (store, _tmp) = temp_store();
        let sid = Uuid::new_v4();

        store.extract_fact(sid, test_fact(sid, "uses Cargo", FactCategory::ProjectStructure));
        store.extract_fact(sid, test_fact(sid, "uses tokio", FactCategory::Dependency));
        store.extract_fact(sid, test_fact(sid, "prefers sonnet", FactCategory::UserPreference));

        let deps = store.get_facts_by_category(sid, FactCategory::Dependency);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].fact, "uses tokio");

        let all = store.get_facts(sid);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn persist_and_load_roundtrips_data() {
        let (store, _tmp) = temp_store();
        let sid = Uuid::new_v4();

        store.extract_fact(sid, test_fact(sid, "uses Cargo workspace", FactCategory::ProjectStructure));
        store.persist(sid).unwrap();

        let loaded = store.load(sid).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].fact, "uses Cargo workspace");
    }

    #[test]
    fn load_nonexistent_session_returns_empty() {
        let (store, _tmp) = temp_store();
        let sid = Uuid::new_v4();

        let facts = store.load(sid).unwrap();
        assert!(facts.is_empty());
    }
}
