//! Job record store + adoption integration tests.
//!
//! These cover the durable half of background-task adoption: the on-disk
//! record store and what a starting session does with what it finds. The pure
//! classification matrix lives next to the code in
//! `crates/core/src/job_record.rs`; here we exercise the store itself.

use coderex_core::job_record::{
    JobKind, JobRecord, JobState, JobStore, STALE_THRESHOLD_MS, adopt_and_reconcile,
};
use std::sync::{Arc, Barrier};

const NOW: i64 = 1_700_000_000_000;

fn record(id: &str, kind: JobKind, state: JobState, at: i64) -> JobRecord {
    let mut r = JobRecord::new(id, kind, "a background job", "/work", Some("sess_a"), at);
    r.state = state;
    r.updated_at_ms = at;
    r
}

fn json_files(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(std::result::Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

// ─── Layout ─────────────────────────────────────────────────────────────────

#[test]
fn store_root_is_dot_coderex_jobs_under_cwd() {
    let store = JobStore::new("/some/project");
    assert_eq!(
        store.root(),
        std::path::Path::new("/some/project/.coderex/jobs")
    );
}

// ─── Round trip ─────────────────────────────────────────────────────────────

#[test]
fn write_load_list_remove_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());

    let a = record("task_1", JobKind::Shell, JobState::Running, NOW - 1_000);
    let b = record("cron-2", JobKind::Cron, JobState::Running, NOW - 2_000);
    store.write(&a).unwrap();
    store.write(&b).unwrap();

    // One file per job, named after the id.
    let mut files = json_files(store.root());
    files.sort();
    assert_eq!(
        files,
        vec!["cron-2.json".to_string(), "task_1.json".to_string()]
    );

    let loaded = store.load("task_1").unwrap().unwrap();
    assert_eq!(loaded, a);
    assert!(store.load("nope").unwrap().is_none());

    // list() is ordered by id and sees both.
    let listed = store.list().unwrap();
    assert_eq!(
        listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["cron-2", "task_1"]
    );
    assert_eq!(listed[1], a);

    assert!(store.remove("task_1").unwrap());
    assert!(
        !store.remove("task_1").unwrap(),
        "removing twice is a no-op"
    );
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn list_of_missing_store_is_empty_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path().join("does-not-exist"));
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn prune_done_removes_only_terminal_records() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());

    store
        .write(&record("done", JobKind::Shell, JobState::Done, NOW))
        .unwrap();
    store
        .write(&record("failed", JobKind::Cron, JobState::Failed, NOW))
        .unwrap();
    store
        .write(&record("running", JobKind::Shell, JobState::Running, NOW))
        .unwrap();
    store
        .write(&record(
            "interrupted",
            JobKind::Agent,
            JobState::Interrupted,
            NOW,
        ))
        .unwrap();

    assert_eq!(store.prune_done().unwrap(), 2);
    let left: Vec<String> = store.list().unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(left, vec!["interrupted", "running"]);
}

// ─── Robustness ─────────────────────────────────────────────────────────────

#[test]
fn corrupt_and_unrelated_files_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    store
        .write(&record("task_1", JobKind::Shell, JobState::Running, NOW))
        .unwrap();

    // A truncated record must not make adoption impossible.
    std::fs::write(store.root().join("task_2.json"), b"{ not json").unwrap();
    // Neither must a stray file that is not a record at all.
    std::fs::write(store.root().join("README.md"), b"hello").unwrap();

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "task_1");
}

#[test]
fn ids_that_could_escape_the_store_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());

    assert!(store.load("../escape").is_err());
    assert!(store.remove("a/b").is_err());
    let mut bad = record("ok", JobKind::Shell, JobState::Running, NOW);
    bad.id = "..".into();
    assert!(store.write(&bad).is_err());

    // Nothing was created outside or inside the store.
    assert!(json_files(store.root()).is_empty());
}

// ─── Atomicity ──────────────────────────────────────────────────────────────

#[test]
fn concurrent_writers_and_readers_never_see_a_partial_record() {
    const WRITERS: usize = 4;
    const ROUNDS: usize = 40;

    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    // Every thread waits for the barrier so the readers really do overlap the
    // writers.
    let barrier = Arc::new(Barrier::new(WRITERS + 1));

    let writers: Vec<_> = (0..WRITERS)
        .map(|i| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for n in 0..ROUNDS {
                    let id = format!("w{i}");
                    let rec = JobRecord::new(
                        id,
                        JobKind::Shell,
                        "x".repeat(512),
                        "/work",
                        Some("sess_a"),
                        NOW + i64::try_from(n).unwrap(),
                    );
                    store.write(&rec).unwrap();
                }
            })
        })
        .collect();

    barrier.wait();

    // Reading while writes are in flight: atomic rename means a reader sees a
    // complete record or the previous one, never a truncated file.
    let mut reads = 0usize;
    for _ in 0..400 {
        let listed = store.list().expect("list must not fail mid-write");
        for r in &listed {
            assert!(r.id.starts_with('w'));
            assert!(!r.description.is_empty());
        }
        reads += 1;
    }
    assert_eq!(reads, 400);

    for w in writers {
        w.join().unwrap();
    }

    assert_eq!(store.list().unwrap().len(), WRITERS);

    // No temp files were left behind.
    for name in json_files(store.root()) {
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(std::ffi::OsStr::to_str);
        assert_eq!(ext, Some("json"), "unexpected leftover file {name}");
    }
}

// ─── Adoption: interrupted work is surfaced, never silently dropped ─────────

#[test]
fn interrupted_record_is_reported_and_stops_claiming_running() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    store
        .write(&record(
            "task_7",
            JobKind::Agent,
            JobState::Running,
            NOW - 5_000,
        ))
        .unwrap();

    let result = adopt_and_reconcile(&store, NOW, "sess_a").unwrap();

    assert_eq!(result.outcome.interrupted.len(), 1);
    assert_eq!(result.notifications.len(), 1);
    assert_eq!(result.notifications[0].job_id, "task_7");
    assert!(
        result.notifications[0]
            .text
            .contains("could not be resumed")
    );

    // Never leave a phantom `running` behind.
    let on_disk = store.load("task_7").unwrap().unwrap();
    assert_eq!(on_disk.state, JobState::Interrupted);
    assert_eq!(on_disk.updated_at_ms, NOW);
}

#[test]
fn an_already_interrupted_record_is_not_reported_again() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    store
        .write(&record(
            "task_7",
            JobKind::Shell,
            JobState::Running,
            NOW - 5_000,
        ))
        .unwrap();

    // First session after the crash reports it.
    let first = adopt_and_reconcile(&store, NOW, "sess_a").unwrap();
    assert_eq!(first.notifications.len(), 1);

    // The next session sees a record that was already accounted for.
    let second = adopt_and_reconcile(&store, NOW + 1_000, "sess_a").unwrap();
    assert_eq!(second.outcome.interrupted.len(), 1);
    assert!(
        second.notifications.is_empty(),
        "a crash is reported once, not on every start"
    );
}

#[test]
fn foreign_and_terminal_records_are_left_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());

    let mut foreign = record("other", JobKind::Shell, JobState::Running, NOW - 1_000);
    foreign.session_id = Some("sess_other".into());
    store.write(&foreign).unwrap();

    store
        .write(&record("done", JobKind::Shell, JobState::Done, NOW - 1_000))
        .unwrap();

    // A record that is ours but very old is stale: reported to nobody, marked
    // nothing.
    store
        .write(&record(
            "ancient",
            JobKind::Shell,
            JobState::Running,
            NOW - STALE_THRESHOLD_MS - 1,
        ))
        .unwrap();

    let result = adopt_and_reconcile(&store, NOW, "sess_a").unwrap();
    assert_eq!(result.outcome.foreign.len(), 1);
    assert_eq!(result.outcome.terminal.len(), 1);
    assert_eq!(result.outcome.stale.len(), 1);
    assert!(result.notifications.is_empty());

    assert_eq!(
        store.load("other").unwrap().unwrap().state,
        JobState::Running,
        "another session's record is not ours to rewrite"
    );
    assert_eq!(
        store.load("ancient").unwrap().unwrap().state,
        JobState::Running
    );
}

#[test]
fn resumable_cron_definitions_are_classified_but_not_duplicated() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    store
        .write(&record(
            "cron-1",
            JobKind::Cron,
            JobState::Running,
            NOW - 100,
        ))
        .unwrap();

    let result = adopt_and_reconcile(&store, NOW, "sess_a").unwrap();

    assert_eq!(result.outcome.resumable.len(), 1);
    assert!(result.notifications.is_empty());
    // Re-scheduling is the `CronScheduler`'s job (it reloads durable
    // definitions at session start); adoption only classifies, so the record
    // itself is not rewritten and no second schedule is created.
    assert_eq!(
        store.load("cron-1").unwrap().unwrap().state,
        JobState::Running
    );
}

#[test]
fn adopt_outcome_accounts_for_every_record_found() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());

    let mut foreign = record("f", JobKind::Cron, JobState::Running, NOW);
    foreign.session_id = Some("sess_other".into());
    store.write(&foreign).unwrap();
    store
        .write(&record("own", JobKind::Shell, JobState::Running, NOW))
        .unwrap();
    store
        .write(&record("cron", JobKind::Cron, JobState::Running, NOW))
        .unwrap();
    store
        .write(&record("done", JobKind::Workflow, JobState::Done, NOW))
        .unwrap();
    store
        .write(&record(
            "old",
            JobKind::Shell,
            JobState::Running,
            NOW - STALE_THRESHOLD_MS - 1,
        ))
        .unwrap();

    let result = adopt_and_reconcile(&store, NOW, "sess_a").unwrap();
    assert_eq!(result.outcome.total(), store.list().unwrap().len());
    assert_eq!(result.outcome.total(), 5);
    assert_eq!(result.notifications.len(), 1);
}

// ─── Lifecycle settlement ───────────────────────────────────────────────────

#[test]
fn settle_stamps_end_fields_and_is_idempotent_for_terminal_states() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    let running = record("shell-1", JobKind::Shell, JobState::Running, NOW);
    store.write(&running).unwrap();

    assert!(
        store
            .settle(
                "shell-1",
                JobState::Failed,
                NOW + 500,
                Some(7),
                Some("command exited with status 7".into()),
            )
            .unwrap()
    );

    let failed = store.load("shell-1").unwrap().unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.ended_at_ms, Some(NOW + 500));
    assert_eq!(failed.updated_at_ms, NOW + 500);
    assert_eq!(failed.exit_code, Some(7));
    assert_eq!(
        failed.error.as_deref(),
        Some("command exited with status 7")
    );

    // Cancellation/adoption may have won the race; a late completion must not
    // rewrite the record.
    assert!(
        !store
            .settle("shell-1", JobState::Done, NOW + 900, Some(0), None)
            .unwrap()
    );
    assert_eq!(store.load("shell-1").unwrap().unwrap(), failed);
}

#[test]
fn settle_missing_job_is_a_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    assert!(
        !store
            .settle("absent", JobState::Done, NOW, None, None)
            .unwrap()
    );
}

#[test]
fn adoption_of_a_written_running_record_marks_it_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let store = JobStore::new(tmp.path());
    let running = record("shell-2", JobKind::Shell, JobState::Running, NOW - 10);
    store.write(&running).unwrap();

    let adopted = coderex_core::job_record::adopt_for_session(&store, NOW, "sess_a")
        .expect("adoption succeeds");
    assert_eq!(adopted.outcome.interrupted, vec![running.clone()]);
    assert_eq!(adopted.notifications.len(), 1);

    let reconciled = store.load("shell-2").unwrap().unwrap();
    assert_eq!(reconciled.state, JobState::Interrupted);
    assert_eq!(reconciled.ended_at_ms, Some(NOW));
}
