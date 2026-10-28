//! Agent integration tests — covering all 7 agent types and lifecycle behavior.
//!
//! These tests correspond to the equivalence test plan Module 6:
//! 6.1 general-purpose, 6.2 Explore, 6.3 Plan, 6.4 run_in_background,
//! 6.5 SendMessage, 6.6 worktree isolation, 6.7 tool whitelist

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use coderex_agents::{
    AgentError, AgentManager, AgentResult, AgentStatus, AgentType, SpawnOptions, TeammateEvent,
    Thoroughness,
};
use coderex_core::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use coderex_core::job_record::{JobKind, JobRecord, JobState, JobStore};
use coderex_core::task::now_ms;
use coderex_core::tool_executor::ToolInfo;
use coderex_core::types::{ModelId, UsageSummary};
use futures::Stream;
use tokio::sync::Notify;

fn test_manager() -> AgentManager {
    let mut mgr = AgentManager::standalone();
    mgr.enable_test_mode();
    mgr
}

// ═══════════════════════════════════════════════════════════════════════════
//  6.1 General-purpose agent — spawns and completes a task
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_general_purpose_spawns_and_completes() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "explain what this project does",
            SpawnOptions::default(),
            "sess_001",
            tmp.path().to_str().unwrap(),
            3,
        )
        .await
        .unwrap();

    assert_eq!(instance.status(), AgentStatus::Completed);
    assert!(instance.context.is_some());
    let ctx = instance.context.as_ref().unwrap();
    assert_eq!(ctx.parent_session_id, "sess_001");
    assert_eq!(ctx.recent_message_count, 3);
    assert!(instance.output_file.is_some());
}

// ═══════════════════════════════════════════════════════════════════════════
//  6.2 Explore agent — different thoroughness levels produce different prompts
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_explore_quick_thoroughness() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::Explore {
                thoroughness: Thoroughness::Quick,
            },
            "find all Rust files",
            SpawnOptions::default(),
            "sess_002",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("quick scan"));
    assert_eq!(instance.status(), AgentStatus::Completed);
}

#[tokio::test]
async fn agent_explore_medium_thoroughness() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::Explore {
                thoroughness: Thoroughness::Medium,
            },
            "analyze the codebase structure",
            SpawnOptions::default(),
            "sess_003",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("moderate depth"));
}

#[tokio::test]
async fn agent_explore_very_thorough() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::Explore {
                thoroughness: Thoroughness::VeryThorough,
            },
            "deep audit of dependencies",
            SpawnOptions::default(),
            "sess_004",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("full recursive analysis"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  6.3 Plan agent — returns implementation plan in system prompt
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_plan_produces_implementation_plan() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::Plan,
            "add OAuth2 authentication",
            SpawnOptions::default(),
            "sess_005",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("planning agent"));
    assert!(instance.prompt.contains("implementation plans"));
    assert_eq!(instance.status(), AgentStatus::Completed);
}

// ═══════════════════════════════════════════════════════════════════════════
//  6.4 run_in_background — spawn returns immediately, agent runs async
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_background_spawn_returns_immediately() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let start = std::time::Instant::now();
    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "long running analysis",
            SpawnOptions {
                run_in_background: true,
                ..Default::default()
            },
            "sess_006",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    // spawn_agent should return quickly (< 1s) even for background tasks
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    // In test mode without API client, the agent completes immediately
    assert_eq!(instance.status(), AgentStatus::Completed);
    assert!(instance.options.run_in_background);
}

// ═══════════════════════════════════════════════════════════════════════════
//  6.5 SendMessage — an unknown agent cannot receive messages
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_send_message_to_unknown_agent_fails() {
    let mgr = test_manager();

    // There is no external-agent transport in this product: an id that was
    // not spawned by this manager has no receiver, so the message is refused
    // rather than silently dropped.
    let result = mgr
        .send_message("external_agent_1", "please also check tests")
        .await;
    assert!(matches!(result, Err(AgentError::NotFound(_))));
}

#[tokio::test]
async fn agent_send_message_to_completed_agent_fails() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    mgr.spawn_agent(
        AgentType::GeneralPurpose,
        "hello",
        SpawnOptions::default(),
        "sess_008",
        tmp.path().to_str().unwrap(),
        0,
    )
    .await
    .unwrap();

    let agent_id = mgr.list_agents()[0].task_handle.id.clone();
    // In test mode agent completes immediately
    let result = mgr.send_message(&agent_id, "follow up").await;
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
//  6.6 Worktree isolation — agent runs in separate working directory
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_worktree_isolation_separate_cwd() {
    let mut mgr = test_manager();
    let parent_dir = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "analyze this directory",
            SpawnOptions {
                cwd: Some(agent_dir.path().to_path_buf()),
                ..Default::default()
            },
            "sess_009",
            parent_dir.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    // The agent's output file should be in the agent's cwd, not the parent's
    if let Some(ref path) = instance.output_file {
        let parent = path.parent().unwrap();
        assert_eq!(parent, agent_dir.path());
    } else {
        panic!("expected output_file to be set");
    }

    // Context cwd should be the parent's cwd
    let ctx = instance.context.as_ref().unwrap();
    assert_eq!(ctx.cwd, parent_dir.path().to_str().unwrap());
}

// ═══════════════════════════════════════════════════════════════════════════
//  6.7 Tool whitelist — only specified tools are available to the agent
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_tool_whitelist_restricts_tools() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "read and search only",
            SpawnOptions {
                allowed_tools: Some(vec!["Read".into(), "Grep".into()]),
                ..Default::default()
            },
            "sess_010",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    let ctx = instance.context.as_ref().unwrap();
    assert_eq!(ctx.allowed_tools, vec!["Read", "Grep"]);
}

#[tokio::test]
async fn agent_tool_whitelist_empty_allows_none() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "no tools",
            SpawnOptions {
                allowed_tools: Some(vec![]),
                ..Default::default()
            },
            "sess_011",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    let ctx = instance.context.as_ref().unwrap();
    assert!(ctx.allowed_tools.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: lifecycle, callbacks, and edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_wait_for_completion_returns_result() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "do something",
            SpawnOptions::default(),
            "sess_wait",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    let agent_id = instance.task_handle.id.clone();
    let result: AgentResult = mgr.wait_for_completion(&agent_id).await.unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
}

#[tokio::test]
async fn agent_stop_agent_changes_status() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    mgr.spawn_agent(
        AgentType::GeneralPurpose,
        "long task",
        SpawnOptions::default(),
        "sess_stop",
        tmp.path().to_str().unwrap(),
        0,
    )
    .await
    .unwrap();

    let agent_id = mgr.list_agents()[0].task_handle.id.clone();
    mgr.stop_agent(&agent_id).await.unwrap();

    assert_eq!(mgr.list_agents()[0].status(), AgentStatus::Stopped);
}

#[tokio::test]
async fn agent_teammate_event_callback_receives_events() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let event_received = Arc::new(AtomicBool::new(false));
    let flag = event_received.clone();
    mgr.set_event_callback(move |event| {
        if let TeammateEvent::TaskCompleted { .. } = event {
            flag.store(true, Ordering::SeqCst);
        }
    });

    mgr.spawn_agent(
        AgentType::GeneralPurpose,
        "test",
        SpawnOptions::default(),
        "sess_event",
        tmp.path().to_str().unwrap(),
        0,
    )
    .await
    .unwrap();

    let agent_id = mgr.list_agents()[0].task_handle.id.clone();
    mgr.complete_agent(&agent_id, "subject", "summary")
        .await
        .unwrap();

    assert!(event_received.load(Ordering::SeqCst));
}

#[tokio::test]
async fn agent_verification_type_spawns() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::Verification,
            "verify this code is correct",
            SpawnOptions::default(),
            "sess_verify",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("verification agent"));
    assert_eq!(instance.status(), AgentStatus::Completed);
}

#[tokio::test]
async fn agent_custom_agent_with_system_prompt() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::Custom {
                prompt: "You are a Rust expert focused on unsafe code.".into(),
            },
            "review this unsafe block",
            SpawnOptions::default(),
            "sess_custom",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("Rust expert"));
    assert!(instance.prompt.contains("unsafe code"));
}

#[tokio::test]
async fn agent_claude_code_guide_type_spawns() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::ClaudeCodeGuide,
            "how do I use worktrees?",
            SpawnOptions::default(),
            "sess_guide",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("Claude Code"));
    assert_eq!(instance.status(), AgentStatus::Completed);
}

#[tokio::test]
async fn agent_statusline_setup_type_spawns() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::StatuslineSetup,
            "configure my status line",
            SpawnOptions::default(),
            "sess_status",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    assert!(instance.prompt.contains("status line"));
    assert_eq!(instance.status(), AgentStatus::Completed);
}

#[tokio::test]
async fn agent_multiple_agents_tracked_independently() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    for i in 0..5 {
        mgr.spawn_agent(
            AgentType::GeneralPurpose,
            &format!("task {}", i),
            SpawnOptions::default(),
            "sess_multi",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();
    }

    assert_eq!(mgr.list_agents().len(), 5);
}

#[tokio::test]
async fn agent_get_agent_by_id() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "test",
            SpawnOptions::default(),
            "sess_get",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

    let agent_id = instance.task_handle.id.clone();
    let fetched = mgr.get_agent(&agent_id).unwrap();
    assert_eq!(fetched.task_handle.id, agent_id);
}

#[tokio::test]
async fn agent_extra_env_is_refused() {
    let mut mgr = test_manager();
    let tmp = tempfile::tempdir().unwrap();

    let mut env = HashMap::new();
    env.insert("AGENT_VAR".into(), "agent_value".into());

    let result = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "test",
            SpawnOptions {
                extra_env: env,
                ..Default::default()
            },
            "sess_env",
            tmp.path().to_str().unwrap(),
            0,
        )
        .await;

    // Sub-agents run in the parent process, so there is no child environment
    // to inject into; the spawn is refused instead of silently dropping the
    // variables the caller asked for.
    assert!(matches!(result, Err(AgentError::Configuration(_))));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Durable job records for background agents
// ═══════════════════════════════════════════════════════════════════════════

/// An API client whose first non-streaming call blocks until released, so a
/// background agent is observably `Running` rather than completing instantly.
struct BlockingApiClient {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl BlockingApiClient {
    fn new() -> Self {
        Self {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }
    }
}

#[async_trait::async_trait]
impl ApiClient for BlockingApiClient {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(Self {
            started: Arc::clone(&self.started),
            release: Arc::clone(&self.release),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(ApiResponse {
            content: "finished".into(),
            tool_calls: Vec::new(),
            thinking: None,
            usage: UsageSummary {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            stop_reason: "end_turn".into(),
        })
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // The engine's `submit` path is non-streaming; an empty stream is
        // enough for this test client.
        Box::pin(futures::stream::empty())
    }
}

fn blocking_manager() -> (AgentManager, Arc<Notify>, Arc<Notify>) {
    let client = BlockingApiClient::new();
    let started = Arc::clone(&client.started);
    let release = Arc::clone(&client.release);
    let mut mgr = AgentManager::standalone();
    mgr.set_api_client(Box::new(client));
    (mgr, started, release)
}

async fn spawn_background_agent(
    mgr: &mut AgentManager,
    cwd: &std::path::Path,
) -> (JobStore, String, String) {
    let instance = mgr
        .spawn_agent(
            AgentType::GeneralPurpose,
            "background analysis",
            SpawnOptions {
                run_in_background: true,
                ..Default::default()
            },
            "sess_job",
            cwd.to_str().unwrap(),
            0,
        )
        .await
        .unwrap();
    let (store, id) = instance
        .job_record
        .clone()
        .expect("a background agent must be recorded");
    let agent_id = instance.task_handle.id.clone();
    (store, id, agent_id)
}

async fn wait_for_terminal(store: &JobStore, id: &str) -> JobRecord {
    for _ in 0..400 {
        if let Ok(Some(record)) = store.load(id)
            && record.state != JobState::Running
        {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("background agent record {id} never reached a terminal state");
}

#[tokio::test]
async fn background_agent_record_running_then_done() {
    let (mut mgr, started, release) = blocking_manager();
    let tmp = tempfile::tempdir().unwrap();
    let (store, id, _agent_id) = spawn_background_agent(&mut mgr, tmp.path()).await;

    let running = store.load(&id).unwrap().expect("record written at spawn");
    assert_eq!(running.state, JobState::Running);
    assert_eq!(running.kind, JobKind::Agent);
    assert_eq!(running.session_id.as_deref(), Some("sess_job"));
    assert!(running.pid.is_some());
    assert!(running.ended_at_ms.is_none());
    assert!(running.description.contains("background analysis"));

    started.notified().await;
    release.notify_one();

    let done = wait_for_terminal(&store, &id).await;
    assert_eq!(done.state, JobState::Done);
    assert!(done.ended_at_ms.is_some());
    assert!(done.exit_code.is_none());
    assert!(done.error.is_none());
}

#[tokio::test]
async fn background_agent_stop_records_interrupted() {
    let (mut mgr, started, release) = blocking_manager();
    let tmp = tempfile::tempdir().unwrap();
    let (store, id, agent_id) = spawn_background_agent(&mut mgr, tmp.path()).await;

    started.notified().await;
    mgr.stop_agent(&agent_id).await.unwrap();

    let stopped = store.load(&id).unwrap().expect("record survives stop");
    assert_eq!(stopped.state, JobState::Interrupted);
    assert!(stopped.ended_at_ms.is_some());
    assert!(
        stopped.error.as_deref().unwrap().contains("stopped"),
        "unexpected error: {:?}",
        stopped.error
    );

    // The aborted task must not resurrect the record as Done.
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store.load(&id).unwrap().unwrap().state,
        JobState::Interrupted
    );
}

#[tokio::test]
async fn background_agent_drop_records_interrupted() {
    let (mut mgr, started, release) = blocking_manager();
    let tmp = tempfile::tempdir().unwrap();
    let (store, id, _agent_id) = spawn_background_agent(&mut mgr, tmp.path()).await;
    started.notified().await;

    drop(mgr);

    let dropped = store
        .load(&id)
        .unwrap()
        .expect("record survives manager drop");
    assert_eq!(dropped.state, JobState::Interrupted);
    assert!(dropped.ended_at_ms.is_some());
    assert!(
        dropped
            .error
            .as_deref()
            .unwrap()
            .contains("manager dropped"),
        "unexpected error: {:?}",
        dropped.error
    );

    release.notify_one();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store.load(&id).unwrap().unwrap().state,
        JobState::Interrupted
    );
}

#[tokio::test]
async fn fresh_session_adopts_background_agent_as_interrupted() {
    let (mut mgr, started, release) = blocking_manager();
    let tmp = tempfile::tempdir().unwrap();
    let (_store, id, _agent_id) = spawn_background_agent(&mut mgr, tmp.path()).await;
    started.notified().await;

    // A fresh store, exactly as the CLI builds one at startup.
    let fresh = JobStore::new(tmp.path());
    let adopted = coderex_core::job_record::adopt_for_session(&fresh, now_ms(), "sess_job")
        .expect("adoption succeeds");

    assert!(
        adopted
            .outcome
            .interrupted
            .iter()
            .any(|record| record.id == id),
        "agent record was not classified interrupted: {:?}",
        adopted.outcome
    );
    assert!(
        adopted.notifications.iter().any(|note| note.job_id == id),
        "no notification for the adopted agent record"
    );

    let reconciled = fresh.load(&id).unwrap().expect("record still on disk");
    assert_eq!(reconciled.state, JobState::Interrupted);
    assert!(reconciled.ended_at_ms.is_some());

    release.notify_one();
    drop(mgr);
}
