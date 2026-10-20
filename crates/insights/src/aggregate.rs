//! What many sessions add up to — and what that suggests.
//!
//! Aggregation is arithmetic; the findings are the part worth arguing about.
//! A finding is only emitted when the underlying counts are big enough to mean
//! something, and each one says what it is based on, so a suggestion can be
//! checked against the sessions it came from rather than taken on faith.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::facts::SessionFacts;

/// Below this many tool calls, rates are noise.
const MIN_CALLS_FOR_A_RATE: usize = 20;

/// A tool has to fail this often, with this many calls, to be worth naming.
const NOTABLE_ERROR_RATE: f64 = 0.15;

/// A session this long is worth a second look.
const LONG_SESSION_MESSAGES: usize = 200;

/// What many sessions add up to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregate {
    /// How many sessions were read.
    pub sessions: usize,
    /// Messages across all of them.
    pub messages: usize,
    /// Messages the person sent.
    pub user_messages: usize,
    /// Tool calls by name.
    pub tool_calls: BTreeMap<String, usize>,
    /// Tool failures by name.
    pub tool_errors: BTreeMap<String, usize>,
    /// How many sessions were compacted at least once.
    pub compacted_sessions: usize,
    /// How many sessions ran long.
    pub long_sessions: usize,
    /// Sessions that called no tools at all.
    pub read_only_sessions: usize,
    /// The first prompts, for the report's examples.
    pub opening_prompts: Vec<String>,
}

/// Something the sessions suggest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// A short headline.
    pub title: String,
    /// Why it is being said, with the numbers behind it.
    pub detail: String,
    /// How sure this is: `fact` is arithmetic, `suggestion` is judgement.
    pub kind: FindingKind,
}

/// How much weight a finding carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingKind {
    /// Counted, not judged.
    Fact,
    /// Judgement, based on the counts.
    Suggestion,
}

/// Add up what the sessions contain.
#[must_use]
pub fn aggregate(sessions: &[SessionFacts]) -> Aggregate {
    let mut aggregate = Aggregate {
        sessions: sessions.len(),
        messages: 0,
        user_messages: 0,
        tool_calls: BTreeMap::new(),
        tool_errors: BTreeMap::new(),
        compacted_sessions: 0,
        long_sessions: 0,
        read_only_sessions: 0,
        opening_prompts: Vec::new(),
    };

    for session in sessions {
        aggregate.messages += session.messages;
        aggregate.user_messages += session.user_messages();

        for (tool, count) in &session.tool_calls {
            *aggregate.tool_calls.entry(tool.clone()).or_insert(0) += count;
        }
        for (tool, count) in &session.tool_errors {
            *aggregate.tool_errors.entry(tool.clone()).or_insert(0) += count;
        }

        if session.compactions > 0 {
            aggregate.compacted_sessions += 1;
        }
        if session.messages >= LONG_SESSION_MESSAGES {
            aggregate.long_sessions += 1;
        }
        if session.total_tool_calls() == 0 {
            aggregate.read_only_sessions += 1;
        }
        if !session.first_prompt.is_empty() && aggregate.opening_prompts.len() < 10 {
            aggregate.opening_prompts.push(session.first_prompt.clone());
        }
    }

    aggregate
}

impl Aggregate {
    /// Total tool calls.
    #[must_use]
    pub fn total_tool_calls(&self) -> usize {
        self.tool_calls.values().sum()
    }

    /// Total tool failures.
    #[must_use]
    pub fn total_tool_errors(&self) -> usize {
        self.tool_errors.values().sum()
    }

    /// The tools used most, most-used first.
    #[must_use]
    pub fn busiest_tools(&self, limit: usize) -> Vec<(&str, usize)> {
        let mut tools: Vec<(&str, usize)> = self
            .tool_calls
            .iter()
            .map(|(name, count)| (name.as_str(), *count))
            .collect();
        tools.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        tools.truncate(limit);
        tools
    }

    /// The tools that failed most, most-failed first.
    #[must_use]
    pub fn failing_tools(&self, limit: usize) -> Vec<(&str, usize)> {
        let mut tools: Vec<(&str, usize)> = self
            .tool_errors
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(name, count)| (name.as_str(), *count))
            .collect();
        tools.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        tools.truncate(limit);
        tools
    }

    /// The error rate for one tool, if it has been called enough to have one.
    #[must_use]
    // Display ratio; call counts are far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    pub fn tool_error_rate(&self, tool: &str) -> Option<f64> {
        let calls = self.tool_calls.get(tool).copied().unwrap_or(0);
        if calls < MIN_CALLS_FOR_A_RATE {
            return None;
        }
        Some(self.tool_errors.get(tool).copied().unwrap_or(0) as f64 / calls as f64)
    }
}

/// What the sessions suggest, in the order the report shows them.
#[must_use]
// Session counts feed display percentages; they are far below 2^53.
#[allow(clippy::cast_precision_loss)]
pub fn findings(aggregate: &Aggregate) -> Vec<Finding> {
    let mut findings = Vec::new();

    if aggregate.sessions == 0 {
        return findings;
    }

    if let Some((tool, count)) = aggregate.busiest_tools(1).first() {
        findings.push(Finding {
            title: "What mostly happens".to_string(),
            detail: format!(
                "{tool} is the tool this work leans on: {count} of {} calls across {} session(s).",
                aggregate.total_tool_calls(),
                aggregate.sessions
            ),
            kind: FindingKind::Fact,
        });
    }

    for (tool, errors) in aggregate.failing_tools(5) {
        let Some(rate) = aggregate.tool_error_rate(tool) else {
            continue;
        };
        if rate < NOTABLE_ERROR_RATE {
            continue;
        }
        findings.push(Finding {
            title: format!("{tool} fails more than the rest"),
            detail: format!(
                "{errors} of {} calls failed ({:.0}%). Worth asking whether the failures share a \
                 cause before changing how it is used.",
                aggregate.tool_calls[tool],
                rate * 100.0
            ),
            kind: FindingKind::Suggestion,
        });
    }

    if aggregate.compacted_sessions > 0 {
        let share = aggregate.compacted_sessions as f64 / aggregate.sessions as f64;
        findings.push(Finding {
            title: "Sessions hit the context limit".to_string(),
            detail: format!(
                "{} of {} session(s) ({:.0}%) were compacted at least once.",
                aggregate.compacted_sessions,
                aggregate.sessions,
                share * 100.0
            ),
            kind: FindingKind::Fact,
        });
    }

    if aggregate.long_sessions > 0 {
        findings.push(Finding {
            title: "Some sessions ran long".to_string(),
            detail: format!(
                "{} session(s) went past {LONG_SESSION_MESSAGES} messages. Long sessions are where \
                 a task is usually better split.",
                aggregate.long_sessions
            ),
            kind: FindingKind::Suggestion,
        });
    }

    if aggregate.read_only_sessions > 0 {
        findings.push(Finding {
            title: "Some sessions never used a tool".to_string(),
            detail: format!(
                "{} session(s) ran no tools at all — questions, or work that did not get as far \
                 as acting.",
                aggregate.read_only_sessions
            ),
            kind: FindingKind::Fact,
        });
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, calls: &[(&str, usize)], errors: &[(&str, usize)]) -> SessionFacts {
        SessionFacts {
            session_id: id.to_string(),
            last_active: None,
            messages: 10,
            by_role: BTreeMap::from([("user".to_string(), 3)]),
            first_prompt: format!("prompt for {id}"),
            tool_calls: calls
                .iter()
                .map(|(name, count)| ((*name).to_string(), *count))
                .collect(),
            tool_errors: errors
                .iter()
                .map(|(name, count)| ((*name).to_string(), *count))
                .collect(),
            compactions: 0,
            unreadable_lines: 0,
        }
    }

    #[test]
    fn sessions_add_up() {
        let sessions = vec![
            session("a", &[("Read", 5), ("Bash", 3)], &[("Bash", 1)]),
            session("b", &[("Read", 2)], &[]),
        ];
        let total = aggregate(&sessions);

        assert_eq!(total.sessions, 2);
        assert_eq!(total.messages, 20);
        assert_eq!(total.user_messages, 6);
        assert_eq!(total.tool_calls["Read"], 7);
        assert_eq!(total.total_tool_calls(), 10);
        assert_eq!(total.total_tool_errors(), 1);
        assert_eq!(total.opening_prompts.len(), 2);
    }

    #[test]
    fn the_busiest_and_failing_tools_are_ranked() {
        let sessions = vec![session(
            "a",
            &[("Read", 5), ("Bash", 9), ("Edit", 5)],
            &[("Edit", 2), ("Bash", 1)],
        )];
        let total = aggregate(&sessions);

        assert_eq!(total.busiest_tools(2), vec![("Bash", 9), ("Edit", 5)]);
        assert_eq!(total.failing_tools(2), vec![("Edit", 2), ("Bash", 1)]);
    }

    #[test]
    fn an_error_rate_needs_enough_calls_to_mean_something() {
        let thin = aggregate(&[session("a", &[("Bash", 2)], &[("Bash", 2)])]);
        assert_eq!(thin.tool_error_rate("Bash"), None);

        let enough = aggregate(&[session("a", &[("Bash", 40)], &[("Bash", 8)])]);
        assert_eq!(enough.tool_error_rate("Bash"), Some(0.2));
        assert_eq!(enough.tool_error_rate("Never"), None);
    }

    #[test]
    fn findings_stay_quiet_until_there_is_something_to_say() {
        assert!(findings(&aggregate(&[])).is_empty());

        // A handful of calls is not a pattern.
        let thin = aggregate(&[session("a", &[("Bash", 3)], &[("Bash", 1)])]);
        let titles: Vec<String> = findings(&thin).into_iter().map(|f| f.title).collect();
        assert!(
            !titles.iter().any(|title| title.contains("fails more")),
            "{titles:?}"
        );
        assert!(titles.iter().any(|title| title.contains("mostly happens")));
    }

    #[test]
    fn a_failing_tool_is_named_with_its_numbers() {
        let sessions = vec![session("a", &[("Bash", 100)], &[("Bash", 30)])];
        let findings = findings(&aggregate(&sessions));

        let failing = findings
            .iter()
            .find(|finding| finding.title.contains("Bash fails"))
            .expect("expected a finding about Bash");
        assert!(failing.detail.contains("30 of 100"), "{}", failing.detail);
        assert!(failing.detail.contains("30%"), "{}", failing.detail);
        assert_eq!(failing.kind, FindingKind::Suggestion);
    }

    #[test]
    fn compaction_and_length_are_reported() {
        let mut long = session("long", &[("Read", 1)], &[]);
        long.compactions = 2;
        long.messages = 500;
        let mut short = session("short", &[("Read", 1)], &[]);
        short.compactions = 0;

        let total = aggregate(&[long, short]);
        assert_eq!(total.compacted_sessions, 1);
        assert_eq!(total.long_sessions, 1);

        let titles: Vec<String> = findings(&total).into_iter().map(|f| f.title).collect();
        assert!(
            titles.iter().any(|t| t.contains("context limit")),
            "{titles:?}"
        );
        assert!(titles.iter().any(|t| t.contains("ran long")), "{titles:?}");
    }

    #[test]
    fn a_session_with_no_tools_is_counted_as_such() {
        let total = aggregate(&[session("a", &[], &[]), session("b", &[("Read", 1)], &[])]);
        assert_eq!(total.read_only_sessions, 1);

        let titles: Vec<String> = findings(&total).into_iter().map(|f| f.title).collect();
        assert!(
            titles.iter().any(|t| t.contains("never used a tool")),
            "{titles:?}"
        );
    }
}
