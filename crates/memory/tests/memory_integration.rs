//! Memory integration tests — covering session memory, long-term memory,
//! session index, and persistence across store recreation.
//!
//! These tests correspond to the equivalence test plan Module 4:
//! 4.2 session memory (facts across turns)
//! 4.3 long-term memory (persist across restart)
//! 4.4 session index (auto-update)
//! 4.5 memory file format (complete fields)
//! 4.6 memory load (new session reads previous state)

use chrono::Utc;
use coderex_memory::long_term::{LongTermStore, MemoryKey};
use coderex_memory::session_index::{SessionIndex, SessionMeta};
use coderex_memory::session_memory::{FactCategory, SessionFact, SessionMemoryStore};
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════════════
//  4.2 会话记忆 — facts extracted across simulated turns are retained
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn session_memory_facts_accumulate_across_turns() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionMemoryStore::new(tmp.path().to_path_buf());
    let sid = Uuid::new_v4();

    // Simulate turn 1 — extract a project-structure fact
    store.extract_fact(
        sid,
        SessionFact {
            id: Uuid::new_v4(),
            session_id: sid,
            fact: "uses Cargo workspace with 3 crates".into(),
            category: FactCategory::ProjectStructure,
            created_at: Utc::now(),
            confidence: 0.95,
        },
    );

    // Simulate turn 2 — extract a code-pattern fact
    store.extract_fact(
        sid,
        SessionFact {
            id: Uuid::new_v4(),
            session_id: sid,
            fact: "prefers anyhow::Result for error handling".into(),
            category: FactCategory::CodePattern,
            created_at: Utc::now(),
            confidence: 0.88,
        },
    );

    // Simulate turn 3 — extract a user-preference fact
    store.extract_fact(
        sid,
        SessionFact {
            id: Uuid::new_v4(),
            session_id: sid,
            fact: "user prefers sonnet over opus for daily tasks".into(),
            category: FactCategory::UserPreference,
            created_at: Utc::now(),
            confidence: 0.92,
        },
    );

    let all = store.get_facts(sid);
    assert_eq!(all.len(), 3, "facts from all turns should be retained");

    let by_cat = store.get_facts_by_category(sid, FactCategory::CodePattern);
    assert_eq!(by_cat.len(), 1);
    assert!(by_cat[0].fact.contains("anyhow"));

    // Persist and reload to verify cross-turn retention survives disk roundtrip
    store.persist(sid).unwrap();

    let new_store = SessionMemoryStore::new(tmp.path().to_path_buf());
    let loaded = new_store.load(sid).unwrap();
    assert_eq!(loaded.len(), 3, "loaded facts should match original count");
}

// ═══════════════════════════════════════════════════════════════════════════
//  4.3 长期记忆 — written entries survive store drop + recreation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn long_term_memory_survives_store_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LongTermStore::new(tmp.path().to_path_buf());

    // Write flat-topic memory
    store
        .write("project_conventions", "Use kebab-case for CLI flags.")
        .unwrap();

    // Write hierarchical-key memory
    let key = MemoryKey::from_str("project/backend/auth");
    store
        .write_with_key(
            "auth_flow",
            &key,
            "OAuth2 with PKCE, refresh token rotation.",
        )
        .unwrap();

    // "Restart" — drop and recreate store pointing at same path
    let restarted = LongTermStore::new(tmp.path().to_path_buf());

    let flat = restarted.read("project_conventions").unwrap();
    assert!(flat.is_some(), "flat-topic memory should survive restart");
    let flat = flat.unwrap();
    assert!(flat.content.contains("kebab-case"));
    assert_eq!(flat.access_count, 1, "first read increments access count");

    let keyed = restarted.read_by_key(&key).unwrap();
    assert!(keyed.is_some(), "keyed memory should survive restart");
    let keyed = keyed.unwrap();
    assert!(keyed.content.contains("PKCE"));
    assert_eq!(keyed.access_count, 1);

    // Second read should increment access count again
    let _ = restarted.read("project_conventions").unwrap();
    let flat2 = restarted.read("project_conventions").unwrap().unwrap();
    assert_eq!(flat2.access_count, 3);
}

#[test]
fn long_term_memory_search_finds_entries_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LongTermStore::new(tmp.path().to_path_buf());

    store
        .write("rust_conventions", "Prefer Result over panic.")
        .unwrap();
    store
        .write("ci_setup", "GitHub Actions with cargo nextest.")
        .unwrap();

    let restarted = LongTermStore::new(tmp.path().to_path_buf());

    let results = restarted.search("Result").unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("panic"));

    let results = restarted.search("cargo").unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("nextest"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  4.4 会话索引 — auto-updates, list/search work correctly
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn session_index_auto_updates_and_lists_recent() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let index = SessionIndex::open(&db_path).unwrap();

    // Create multiple sessions
    let meta1 = SessionMeta {
        session_id: "sess-001".into(),
        project_path: "/projects/alpha".into(),
        ai_title: Some("Add auth middleware".into()),
        tag: None,
        pr_url: None,
        created_at: Utc::now(),
        last_active_at: Utc::now(),
        total_cost_usd: 0.12,
        total_tokens: 1024,
    };
    let mut meta2 = meta1.clone();
    meta2.session_id = "sess-002".into();
    meta2.ai_title = Some("Fix race condition".into());
    meta2.last_active_at = Utc::now();

    index.create(&meta1).unwrap();
    index.create(&meta2).unwrap();

    // Auto-update: update_last_active and cost
    index.update_last_active("sess-001", &Utc::now()).unwrap();
    index
        .update_cost_and_tokens("sess-001", 0.25, 2048)
        .unwrap();

    // List recent
    let recent = index.list_recent(10).unwrap();
    assert_eq!(recent.len(), 2);

    // List by project
    let by_project = index.list_by_project("/projects/alpha").unwrap();
    assert_eq!(by_project.len(), 2);

    // Get single session
    let got = index.get("sess-001").unwrap();
    assert!(got.is_some());
    assert_eq!(got.unwrap().total_cost_usd, 0.25);
}

#[test]
fn session_index_search_finds_by_title_and_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let index = SessionIndex::open(&db_path).unwrap();

    let meta1 = SessionMeta {
        session_id: "sess-101".into(),
        project_path: "/projects/beta".into(),
        ai_title: Some("Refactor database layer".into()),
        tag: Some("refactor".into()),
        pr_url: None,
        created_at: Utc::now(),
        last_active_at: Utc::now(),
        total_cost_usd: 0.05,
        total_tokens: 512,
    };
    let mut meta2 = meta1.clone();
    meta2.session_id = "sess-102".into();
    meta2.ai_title = Some("Add database migration tool".into());
    meta2.tag = Some("feature".into());

    index.create(&meta1).unwrap();
    index.create(&meta2).unwrap();

    // FTS search by title keyword
    let results = index.search("database", 10).unwrap();
    assert_eq!(results.len(), 2, "both sessions mention database");

    // Search by tag
    let results = index.search("refactor", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, "sess-101");
}

// ═══════════════════════════════════════════════════════════════════════════
//  4.5 记忆文件格式 — JSON serialization contains all required fields
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn long_term_memory_json_has_all_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LongTermStore::new(tmp.path().to_path_buf());

    store
        .write("test_topic", "test content for field verification")
        .unwrap();

    // Read raw file to inspect JSON structure
    let entry_file = tmp.path().join("test_topic.json");
    assert!(entry_file.exists());

    let raw = std::fs::read_to_string(&entry_file).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();

    assert!(
        value.get("topic").is_some(),
        "JSON must contain 'topic' field"
    );
    assert!(
        value.get("content").is_some(),
        "JSON must contain 'content' field"
    );
    assert!(
        value.get("created_at").is_some(),
        "JSON must contain 'created_at' field"
    );
    assert!(
        value.get("updated_at").is_some(),
        "JSON must contain 'updated_at' field"
    );
    assert!(
        value.get("access_count").is_some(),
        "JSON must contain 'access_count' field"
    );
    assert_eq!(
        value["topic"].as_str().unwrap(),
        "test_topic",
        "topic value should match"
    );
    assert_eq!(
        value["access_count"].as_u64().unwrap(),
        0,
        "fresh entry should have access_count 0"
    );
}

#[test]
fn long_term_memory_keyed_storage_path_is_correct() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LongTermStore::new(tmp.path().to_path_buf());

    let key = MemoryKey::from_str("project/frontend/components");
    store
        .write_with_key("component_lib", &key, "Button, Modal, Input components.")
        .unwrap();

    // Verify the file was written at the nested path
    let expected_path = tmp.path().join("project/frontend/components.json");
    assert!(
        expected_path.exists(),
        "keyed entry should be stored at nested path"
    );

    // Verify we can read it back via the key
    let entry = store.read_by_key(&key).unwrap();
    assert!(entry.is_some());
    assert!(entry.unwrap().content.contains("Button"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  4.6 记忆读取 — new session store loads previous facts into context
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn new_session_memory_store_loads_previous_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = Uuid::new_v4();

    // Old session writes facts to disk
    {
        let old_store = SessionMemoryStore::new(tmp.path().to_path_buf());
        old_store.extract_fact(
            sid,
            SessionFact {
                id: Uuid::new_v4(),
                session_id: sid,
                fact: "uses Axum for HTTP server".into(),
                category: FactCategory::Dependency,
                created_at: Utc::now(),
                confidence: 0.9,
            },
        );
        old_store.extract_fact(
            sid,
            SessionFact {
                id: Uuid::new_v4(),
                session_id: sid,
                fact: "user prefers json over toml for config".into(),
                category: FactCategory::UserPreference,
                created_at: Utc::now(),
                confidence: 0.85,
            },
        );
        old_store.persist(sid).unwrap();
    } // old_store dropped

    // New session starts — create fresh store and load
    let new_store = SessionMemoryStore::new(tmp.path().to_path_buf());
    let loaded = new_store.load(sid).unwrap();

    assert_eq!(loaded.len(), 2, "new store should load 2 previous facts");

    // Verify they are accessible via get_facts too
    let facts = new_store.get_facts(sid);
    assert_eq!(facts.len(), 2);

    let deps = new_store.get_facts_by_category(sid, FactCategory::Dependency);
    assert_eq!(deps.len(), 1);
    assert!(deps[0].fact.contains("Axum"));

    let prefs = new_store.get_facts_by_category(sid, FactCategory::UserPreference);
    assert_eq!(prefs.len(), 1);
    assert!(prefs[0].fact.contains("json"));
}

#[test]
fn new_long_term_store_loads_previous_entries() {
    let tmp = tempfile::tempdir().unwrap();

    // Old store writes entries
    {
        let old_store = LongTermStore::new(tmp.path().to_path_buf());
        old_store
            .write("api_design", "RESTful with HATEOAS links.")
            .unwrap();
        old_store
            .write_with_key(
                "db_pool",
                &MemoryKey::from_str("infra/database"),
                "deadpool-postgres, max 20 connections.",
            )
            .unwrap();
    }

    // New store reads them
    let new_store = LongTermStore::new(tmp.path().to_path_buf());

    let api = new_store.read("api_design").unwrap();
    assert!(api.is_some());
    assert!(api.unwrap().content.contains("HATEOAS"));

    let db = new_store
        .read_by_key(&MemoryKey::from_str("infra/database"))
        .unwrap();
    assert!(db.is_some());
    assert!(db.unwrap().content.contains("deadpool"));

    // list_all should include both
    let all = new_store.list_all().unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn session_memory_load_nonexistent_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionMemoryStore::new(tmp.path().to_path_buf());
    let sid = Uuid::new_v4();

    let loaded = store.load(sid).unwrap();
    assert!(
        loaded.is_empty(),
        "loading nonexistent session should return empty vec"
    );

    let facts = store.get_facts(sid);
    assert!(facts.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  4.7 SessionMemoryBackend — trait-object roundtrip through the store
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_memory_backend_trait_object_roundtrip() {
    use coderex_core::session_memory::SessionMemoryBackend;
    use coderex_memory::session_memory_backend::StoreSessionMemoryBackend;

    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(SessionMemoryStore::new(dir.path().join("session")));
    let backend: std::sync::Arc<dyn SessionMemoryBackend> =
        std::sync::Arc::new(StoreSessionMemoryBackend::new(store));
    let sid = Uuid::new_v4();

    assert_eq!(backend.load_memory(&sid.to_string()).await, None);
    backend
        .save_memory(&sid.to_string(), "Current State: compaction wired.")
        .await;
    assert_eq!(
        backend.load_memory(&sid.to_string()).await.as_deref(),
        Some("Current State: compaction wired.")
    );

    // Persistence survives a fresh store over the same directory.
    let reloaded = SessionMemoryStore::new(dir.path().join("session"));
    reloaded.load(sid).unwrap();
    assert_eq!(
        reloaded
            .get_facts_by_category(sid, FactCategory::Architecture)
            .pop()
            .map(|f| f.fact),
        Some("Current State: compaction wired.".to_string())
    );
}
