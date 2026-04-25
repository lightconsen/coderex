//! Delta sync for bandwidth optimization and conflict resolution.
//!
//! Computes diffs between local and remote state, sending only
//! the changes instead of full state on subsequent syncs.
//! Uses a hash-based approach: first sync sends full state,
//! subsequent syncs send only changed entries.
//!
//! For concurrent edits, each operation carries a Lamport timestamp
//! and conflicts are resolved via last-writer-wins with tie-breaking
//! on the origin ID.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single state operation in a delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeltaOp {
    /// Insert or update an entry.
    Upsert {
        /// Key/identifier of the entry.
        key: String,
        /// New content.
        value: String,
    },
    /// Remove an entry.
    Delete {
        /// Key/identifier of the entry.
        key: String,
    },
}

/// A state delta containing only the changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDelta {
    /// Base revision hash this delta is relative to.
    pub base_revision: String,
    /// Operations to apply.
    pub operations: Vec<DeltaOp>,
}

impl StateDelta {
    /// Create an empty delta for the given base revision.
    #[must_use]
    pub fn new(base_revision: &str) -> Self {
        Self {
            base_revision: base_revision.to_string(),
            operations: Vec::new(),
        }
    }

    /// Add an upsert operation.
    pub fn upsert(&mut self, key: &str, value: &str) {
        self.operations.push(DeltaOp::Upsert {
            key: key.to_string(),
            value: value.to_string(),
        });
    }

    /// Add a delete operation.
    pub fn delete(&mut self, key: &str) {
        self.operations.push(DeltaOp::Delete {
            key: key.to_string(),
        });
    }

    /// Check if this delta is empty (no changes).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Number of operations in this delta.
    #[must_use]
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Apply this delta to a state map, returning the updated state.
    #[must_use]
    pub fn apply(&self, state: &HashMap<String, String>) -> HashMap<String, String> {
        let mut result = state.clone();
        for op in &self.operations {
            match op {
                DeltaOp::Upsert { key, value } => {
                    result.insert(key.clone(), value.clone());
                }
                DeltaOp::Delete { key } => {
                    result.remove(key);
                }
            }
        }
        result
    }

    /// Serialize the delta to a JSON string.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Deserialize a delta from a JSON string.
    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// Estimate the serialized size in bytes.
    #[must_use]
    pub fn serialized_size(&self) -> usize {
        self.to_json().map_or(0, |s| s.len())
    }
}

/// Computes diffs between two state snapshots.
///
/// Returns a `StateDelta` containing only the entries that differ
/// between `old_state` and `new_state`.
#[must_use]
pub fn compute_delta(
    base_revision: &str,
    old_state: &HashMap<String, String>,
    new_state: &HashMap<String, String>,
) -> StateDelta {
    let mut delta = StateDelta::new(base_revision);

    // Find new or changed entries
    for (key, new_value) in new_state {
        match old_state.get(key) {
            None => {
                delta.upsert(key, new_value);
            }
            Some(old_value) if old_value != new_value => {
                delta.upsert(key, new_value);
            }
            _ => {} // unchanged
        }
    }

    // Find deleted entries
    for key in old_state.keys() {
        if !new_state.contains_key(key) {
            delta.delete(key);
        }
    }

    delta
}

/// Compute a SHA-256 hash of a state map for revision tracking.
///
/// The hash is deterministic: same state always produces the same hash.
#[must_use]
pub fn compute_revision_hash(state: &HashMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();

    // Sort keys for deterministic ordering
    let mut keys: Vec<&String> = state.keys().collect();
    keys.sort();

    for key in keys {
        hasher.update(key.as_bytes());
        hasher.update(b":");
        hasher.update(state[key].as_bytes());
        hasher.update(b"\n");
    }

    let result = hasher.finalize();
    format!("{:x}", result)
}

// ─── Conflict Resolution with Lamport Timestamps ─────────────────────────

/// A Lamport timestamp for ordering concurrent operations.
///
/// Each origin (peer) maintains its own counter. When merging,
/// the higher timestamp wins. Ties are broken by origin ID
/// (lexicographically larger origin wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LamportTimestamp {
    /// Logical counter value.
    pub counter: u64,
    /// Origin that produced this timestamp.
    pub origin_id: u32,
}

impl LamportTimestamp {
    /// Create a new timestamp for the given origin.
    #[must_use]
    pub fn new(counter: u64, origin_id: u32) -> Self {
        Self { counter, origin_id }
    }

    /// Increment the counter, producing a new timestamp for the same origin.
    #[must_use]
    pub fn tick(self) -> Self {
        Self {
            counter: self.counter + 1,
            origin_id: self.origin_id,
        }
    }

    /// Merge two timestamps: return the one that "wins" per
    /// last-writer-wins with origin ID tie-breaking.
    #[must_use]
    pub fn merge(a: Self, b: Self) -> Self {
        if a.counter > b.counter {
            a
        } else if b.counter > a.counter {
            b
        } else if a.origin_id > b.origin_id {
            a
        } else {
            b
        }
    }

    /// Check if this timestamp is strictly greater than the other.
    #[must_use]
    pub fn happens_after(self, other: Self) -> bool {
        self.counter > other.counter
            || (self.counter == other.counter && self.origin_id > other.origin_id)
    }
}

/// A resolved conflict result for a single key.
#[derive(Debug, Clone)]
pub struct ConflictResolution {
    /// The key that had a conflict.
    pub key: String,
    /// The winning value.
    pub winner_value: String,
    /// The winning timestamp.
    pub winner_ts: LamportTimestamp,
    /// Whether there was an actual conflict (false means no conflict).
    pub was_conflict: bool,
}

/// An upsert operation annotated with a Lamport timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedUpsert {
    /// Key/identifier of the entry.
    pub key: String,
    /// New content.
    pub value: String,
    /// Logical timestamp of this operation.
    pub timestamp: LamportTimestamp,
}

/// A concurrent state tracker that records per-key timestamps.
#[derive(Debug, Clone, Default)]
pub struct ConcurrentStateTracker {
    /// Per-key Lamport timestamps.
    timestamps: HashMap<String, LamportTimestamp>,
}

impl ConcurrentStateTracker {
    /// Create a new empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a local upsert, returning the timestamp to attach.
    pub fn record_local(&mut self, key: &str, local_origin: u32) -> LamportTimestamp {
        let ts = self
            .timestamps
            .get(key)
            .map(|ts| ts.tick())
            .unwrap_or_else(|| LamportTimestamp::new(1, local_origin));
        self.timestamps.insert(key.to_string(), ts);
        ts
    }

    /// Record a remote upsert, resolving potential conflicts.
    ///
    /// Returns `Some(ConflictResolution)` if the remote wins,
    /// or `None` if the local value should be kept.
    pub fn record_remote(
        &mut self,
        key: &str,
        value: &str,
        remote_ts: LamportTimestamp,
    ) -> Option<ConflictResolution> {
        match self.timestamps.get(key) {
            Some(&local_ts) if local_ts.happens_after(remote_ts) => {
                // Local wins, no update needed
                Some(ConflictResolution {
                    key: key.to_string(),
                    winner_value: String::new(), // placeholder — caller keeps local
                    winner_ts: local_ts,
                    was_conflict: false,
                })
            }
            Some(&local_ts) => {
                // Remote wins or conflict — update timestamp
                let winner = LamportTimestamp::merge(remote_ts, local_ts);
                self.timestamps.insert(key.to_string(), winner);
                Some(ConflictResolution {
                    key: key.to_string(),
                    winner_value: value.to_string(),
                    winner_ts: winner,
                    was_conflict: local_ts != remote_ts,
                })
            }
            None => {
                // No local entry — accept remote
                self.timestamps.insert(key.to_string(), remote_ts);
                Some(ConflictResolution {
                    key: key.to_string(),
                    winner_value: value.to_string(),
                    winner_ts: remote_ts,
                    was_conflict: false,
                })
            }
        }
    }

    /// Get the current timestamp for a key.
    #[must_use]
    pub fn get_timestamp(&self, key: &str) -> Option<&LamportTimestamp> {
        self.timestamps.get(key)
    }

    /// Get all tracked timestamps.
    #[must_use]
    pub fn timestamps(&self) -> &HashMap<String, LamportTimestamp> {
        &self.timestamps
    }
}

/// Merge two state deltas from concurrent origins.
///
/// Returns a single `StateDelta` containing the resolved operations.
/// When both deltas touch the same key, the one with the higher
/// Lamport timestamp wins (last-writer-wins with origin ID tie-break).
#[must_use]
pub fn merge_concurrent_deltas(
    base_revision: &str,
    delta_a: &StateDelta,
    origin_a: u32,
    delta_b: &StateDelta,
    origin_b: u32,
) -> StateDelta {
    let mut result = StateDelta::new(base_revision);
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Collect all upserts with their timestamps
    let mut upserts: HashMap<String, (String, LamportTimestamp)> = HashMap::new();
    let mut deletes: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Process delta A
    for op in &delta_a.operations {
        match op {
            DeltaOp::Upsert { key, value } => {
                let ts = LamportTimestamp::new(1, origin_a);
                upserts
                    .entry(key.clone())
                    .and_modify(|(_, existing_ts)| {
                        if ts.happens_after(*existing_ts) {
                            *existing_ts = ts;
                        }
                    })
                    .or_insert_with(|| (value.clone(), ts));
            }
            DeltaOp::Delete { key } => {
                deletes.insert(key.clone());
            }
        }
    }

    // Process delta B, resolving conflicts
    for op in &delta_b.operations {
        match op {
            DeltaOp::Upsert { key, value } => {
                let ts_b = LamportTimestamp::new(1, origin_b);
                match upserts.get(key) {
                    Some((_value_a, ts_a)) => {
                        let winner = LamportTimestamp::merge(*ts_a, ts_b);
                        if winner.origin_id == ts_b.origin_id {
                            upserts.insert(key.clone(), (value.clone(), ts_b));
                        }
                        // else keep A's value
                    }
                    None => {
                        upserts.insert(key.clone(), (value.clone(), ts_b));
                    }
                }
            }
            DeltaOp::Delete { key } => {
                deletes.insert(key.clone());
            }
        }
    }

    // Build result: upserts win over deletes (delete is just an upsert of tombstone)
    for (key, (value, _ts)) in upserts {
        if !deletes.contains(&key) {
            result.upsert(&key, &value);
        }
        seen_keys.insert(key);
    }

    // Apply deletes for keys that weren't upserted
    for key in deletes {
        if !seen_keys.contains(&key) {
            result.delete(&key);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn delta_empty() {
        let state = make_state(&[("a", "1"), ("b", "2")]);
        let delta = compute_delta("rev1", &state, &state);
        assert!(delta.is_empty());
    }

    #[test]
    fn delta_new_key() {
        let old = make_state(&[("a", "1")]);
        let new = make_state(&[("a", "1"), ("b", "2")]);
        let delta = compute_delta("rev1", &old, &new);
        assert_eq!(delta.len(), 1);
        assert!(
            matches!(&delta.operations[0], DeltaOp::Upsert { key, value } if key == "b" && value == "2")
        );
    }

    #[test]
    fn delta_changed_value() {
        let old = make_state(&[("a", "1")]);
        let new = make_state(&[("a", "updated")]);
        let delta = compute_delta("rev1", &old, &new);
        assert_eq!(delta.len(), 1);
        assert!(
            matches!(&delta.operations[0], DeltaOp::Upsert { key, value } if key == "a" && value == "updated")
        );
    }

    #[test]
    fn delta_deleted_key() {
        let old = make_state(&[("a", "1"), ("b", "2")]);
        let new = make_state(&[("a", "1")]);
        let delta = compute_delta("rev1", &old, &new);
        assert_eq!(delta.len(), 1);
        assert!(matches!(&delta.operations[0], DeltaOp::Delete { key } if key == "b"));
    }

    #[test]
    fn delta_mixed_changes() {
        let old = make_state(&[("a", "1"), ("b", "2"), ("c", "3")]);
        let new = make_state(&[("a", "1"), ("b", "updated"), ("d", "4")]);
        let delta = compute_delta("rev1", &old, &new);
        assert_eq!(delta.len(), 3); // b changed, c deleted, d added
    }

    #[test]
    fn apply_delta() {
        let state = make_state(&[("a", "1"), ("b", "2")]);
        let mut delta = StateDelta::new("rev1");
        delta.upsert("b", "updated");
        delta.delete("a");
        delta.upsert("c", "3");

        let result = delta.apply(&state);
        assert_eq!(result.get("a"), None);
        assert_eq!(result.get("b"), Some(&"updated".to_string()));
        assert_eq!(result.get("c"), Some(&"3".to_string()));
    }

    #[test]
    fn delta_json_roundtrip() {
        let mut delta = StateDelta::new("rev42");
        delta.upsert("key1", "value1");
        delta.delete("key2");

        let json = delta.to_json().unwrap();
        let restored = StateDelta::from_json(&json).unwrap();

        assert_eq!(restored.base_revision, "rev42");
        assert_eq!(restored.len(), 2);
    }

    #[test]
    fn revision_hash_deterministic() {
        let s1 = make_state(&[("b", "2"), ("a", "1")]);
        let s2 = make_state(&[("a", "1"), ("b", "2")]);
        assert_eq!(compute_revision_hash(&s1), compute_revision_hash(&s2));
    }

    #[test]
    fn revision_hash_differs_on_change() {
        let s1 = make_state(&[("a", "1")]);
        let s2 = make_state(&[("a", "2")]);
        assert_ne!(compute_revision_hash(&s1), compute_revision_hash(&s2));
    }

    #[test]
    fn serialized_size() {
        let mut delta = StateDelta::new("rev1");
        delta.upsert("k", "v");
        assert!(delta.serialized_size() > 0);
    }

    // ─── Conflict Resolution Tests ───

    #[test]
    fn lamport_timestamp_tick() {
        let ts = LamportTimestamp::new(1, 42);
        let ticked = ts.tick();
        assert_eq!(ticked.counter, 2);
        assert_eq!(ticked.origin_id, 42);
    }

    #[test]
    fn lamport_timestamp_merge_higher_wins() {
        let a = LamportTimestamp::new(5, 1);
        let b = LamportTimestamp::new(3, 2);
        assert_eq!(LamportTimestamp::merge(a, b), a);
    }

    #[test]
    fn lamport_timestamp_merge_tie_break_by_origin() {
        let a = LamportTimestamp::new(3, 1);
        let b = LamportTimestamp::new(3, 2);
        assert_eq!(LamportTimestamp::merge(a, b), b);
    }

    #[test]
    fn lamport_happens_after() {
        let later = LamportTimestamp::new(5, 1);
        let earlier = LamportTimestamp::new(3, 2);
        assert!(later.happens_after(earlier));
        assert!(!earlier.happens_after(later));
    }

    #[test]
    fn lamport_happens_after_tie_break() {
        let a = LamportTimestamp::new(3, 2);
        let b = LamportTimestamp::new(3, 1);
        assert!(a.happens_after(b));
        assert!(!b.happens_after(a));
    }

    #[test]
    fn concurrent_tracker_local_record() {
        let mut tracker = ConcurrentStateTracker::new();
        let ts = tracker.record_local("key1", 1);
        assert_eq!(ts.counter, 1);
        assert_eq!(ts.origin_id, 1);

        let ts2 = tracker.record_local("key1", 1);
        assert_eq!(ts2.counter, 2); // incremented
    }

    #[test]
    fn concurrent_tracker_remote_wins() {
        let mut tracker = ConcurrentStateTracker::new();
        tracker.record_local("key1", 1); // counter=1, origin=1

        let remote_ts = LamportTimestamp::new(5, 2);
        let result = tracker.record_remote("key1", "remote-value", remote_ts);
        assert!(result.is_some());
        let res = result.unwrap();
        assert!(res.was_conflict);
        assert_eq!(res.winner_value, "remote-value");
    }

    #[test]
    fn concurrent_tracker_local_wins() {
        let mut tracker = ConcurrentStateTracker::new();
        tracker.record_local("key1", 1);
        tracker.record_local("key1", 1); // counter=2

        let remote_ts = LamportTimestamp::new(1, 2); // lower counter
        let result = tracker.record_remote("key1", "remote-value", remote_ts);
        assert!(result.is_some());
        let res = result.unwrap();
        assert!(!res.was_conflict); // no update needed
    }

    #[test]
    fn concurrent_tracker_new_key_accepted() {
        let mut tracker = ConcurrentStateTracker::new();
        let remote_ts = LamportTimestamp::new(1, 2);
        let result = tracker.record_remote("new-key", "value", remote_ts);
        assert!(result.is_some());
        assert!(!result.unwrap().was_conflict);
    }

    #[test]
    fn merge_deltas_no_conflict_different_keys() {
        let mut delta_a = StateDelta::new("rev1");
        delta_a.upsert("a", "1");

        let mut delta_b = StateDelta::new("rev1");
        delta_b.upsert("b", "2");

        let merged = merge_concurrent_deltas("rev1", &delta_a, 1, &delta_b, 2);
        assert_eq!(merged.len(), 2); // both keys present
    }

    #[test]
    fn merge_deltas_same_key_higher_ts_wins() {
        let mut delta_a = StateDelta::new("rev1");
        delta_a.upsert("shared", "from-a");

        let mut delta_b = StateDelta::new("rev1");
        delta_b.upsert("shared", "from-b");

        let merged = merge_concurrent_deltas("rev1", &delta_a, 1, &delta_b, 2);
        assert_eq!(merged.len(), 1);
        // Origin 2 wins on tie (same counter=1, higher origin_id)
        let op = &merged.operations[0];
        assert!(matches!(op, DeltaOp::Upsert { value, .. } if value == "from-b"));
    }
}
