//! The report — sessions, aggregated, as a page.
//!
//! Self-contained on purpose: no external stylesheet, no font fetch, no
//! script. Someone reads this offline, on a machine that may be behind
//! anything, and it has to work. Everything printed is escaped, because a
//! prompt is whatever the person typed and an error message is whatever a tool
//! said.

use crate::aggregate::{Aggregate, Finding, FindingKind};
use crate::facts::SessionFacts;
use std::fmt::Write as _;

/// How many sessions the report names in its tables.
const SESSIONS_SHOWN: usize = 25;

/// Render the report.
#[must_use]
pub fn render(aggregate: &Aggregate, findings: &[Finding], sessions: &[SessionFacts]) -> String {
    let mut html = String::new();

    html.push_str(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>CodeRex Insights</title><style>",
    );
    html.push_str(
        ":root{--bg:#f8fafc;--ink:#334155;--head:#0f172a;--muted:#64748b;--line:#e2e8f0;\
         --chip:#eef2ff;--warn:#b45309}\
         body{background:var(--bg);color:var(--ink);font:15px/1.55 -apple-system,\
         BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif;margin:0;padding:2rem 1rem}\
         .container{max-width:800px;margin:0 auto}\
         h1{color:var(--head);font-size:1.55rem;margin:0 0 .2rem}\
         h2{color:var(--head);font-size:1.05rem;margin:2rem 0 .5rem}\
         .muted{color:var(--muted)}\
         .stats-row{display:flex;flex-wrap:wrap;gap:1.5rem;margin:1.2rem 0}\
         .stat-value{color:var(--head);font-size:1.4rem;font-weight:600}\
         .stat-label{color:var(--muted);font-size:.8rem;text-transform:uppercase;letter-spacing:.03em}\
         .at-a-glance{background:#fff;border:1px solid var(--line);border-radius:10px;\
         padding:1rem 1.2rem;margin:1rem 0}\
         .at-a-glance h3{margin:.6rem 0 .2rem;font-size:.95rem;color:var(--head)}\
         table{border-collapse:collapse;width:100%;margin:.4rem 0}\
         th,td{text-align:left;padding:.35rem .5rem;border-bottom:1px solid var(--line)}\
         th{color:var(--muted);font-size:.8rem;text-transform:uppercase}\
         td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}\
         code{background:var(--chip);padding:.08rem .28rem;border-radius:4px;font-size:.9em}\
         .suggestion{border-left:3px solid var(--warn);padding-left:.7rem;margin:.8rem 0}\
         .fact{margin:.8rem 0}\
         ul{padding-left:1.1rem;margin:.3rem 0}\
         </style></head><body><div class=\"container\">",
    );

    html.push_str("<h1>CodeRex Insights</h1>");
    let _ = write!(
        html,
        "<p class=\"muted\">{} session(s), {} message(s). Generated {}.</p>",
        aggregate.sessions,
        aggregate.messages,
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    );

    if aggregate.sessions == 0 {
        html.push_str(
            "<p>No sessions were found to read. Insights reads the transcripts under the \
             workspace's <code>.coderex/transcripts</code>.</p></div></body></html>",
        );
        return html;
    }

    html.push_str("<div class=\"stats-row\">");
    for (value, label) in [
        (aggregate.sessions.to_string(), "sessions"),
        (aggregate.messages.to_string(), "messages"),
        (aggregate.total_tool_calls().to_string(), "tool calls"),
        (
            format!("{}", aggregate.total_tool_errors()),
            "tool failures",
        ),
    ] {
        let _ = write!(
            html,
            "<div class=\"stat\"><div class=\"stat-value\">{value}</div>\
             <div class=\"stat-label\">{label}</div></div>"
        );
    }
    html.push_str("</div>");

    // At a glance: what is working, what is hindering, what to try.
    let (working, hindering): (Vec<&Finding>, Vec<&Finding>) = findings
        .iter()
        .partition(|finding| finding.kind == FindingKind::Fact);

    html.push_str("<div class=\"at-a-glance\"><h2 style=\"margin-top:0\">At a glance</h2>");
    html.push_str("<h3>What's happening</h3><ul>");
    for finding in &working {
        let _ = write!(
            html,
            "<li><strong>{}</strong> — {}</li>",
            escape(&finding.title),
            escape(&finding.detail)
        );
    }
    html.push_str("</ul>");

    if !hindering.is_empty() {
        html.push_str("<h3>Worth a look</h3>");
        for finding in &hindering {
            let _ = write!(
                html,
                "<div class=\"suggestion\"><strong>{}</strong><br>{}</div>",
                escape(&finding.title),
                escape(&finding.detail)
            );
        }
    }
    html.push_str("</div>");

    // Tools.
    html.push_str(
        "<h2>Tools</h2><table><thead><tr><th>Tool</th><th class=\"num\">Calls</th>\
         <th class=\"num\">Failures</th><th class=\"num\">Failure rate</th></tr></thead><tbody>",
    );
    for (tool, calls) in aggregate.busiest_tools(15) {
        let errors = aggregate.tool_errors.get(tool).copied().unwrap_or(0);
        let rate = aggregate
            .tool_error_rate(tool)
            .map_or_else(|| "—".to_string(), |rate| format!("{:.0}%", rate * 100.0));
        let _ = write!(
            html,
            "<tr><td><code>{}</code></td><td class=\"num\">{calls}</td>\
             <td class=\"num\">{errors}</td><td class=\"num\">{rate}</td></tr>",
            escape(tool)
        );
    }
    html.push_str("</tbody></table>");

    // Sessions.
    html.push_str(
        "<h2>Sessions</h2><table><thead><tr><th>Session</th><th>Opened with</th>\
         <th class=\"num\">Messages</th><th class=\"num\">Tools</th>\
         <th class=\"num\">Failures</th></tr></thead><tbody>",
    );
    for session in sessions.iter().take(SESSIONS_SHOWN) {
        let _ = write!(
            html,
            "<tr><td><code>{}</code></td><td>{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            escape(&session.session_id),
            escape(&session.first_prompt),
            session.messages,
            session.total_tool_calls(),
            session.total_tool_errors()
        );
    }
    html.push_str("</tbody></table>");
    if sessions.len() > SESSIONS_SHOWN {
        let _ = write!(
            html,
            "<p class=\"muted\">… and {} more.</p>",
            sessions.len() - SESSIONS_SHOWN
        );
    }

    html.push_str(
        "<h2>About this report</h2><p class=\"muted\">Built from the transcripts on this \
         machine. Nothing here is uploaded anywhere.</p>",
    );

    html.push_str("</div></body></html>");
    html
}

/// Escape text for HTML.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::aggregate;
    use std::collections::BTreeMap;

    fn session(id: &str, prompt: &str) -> SessionFacts {
        SessionFacts {
            session_id: id.to_string(),
            last_active: None,
            messages: 12,
            by_role: BTreeMap::from([("user".to_string(), 4)]),
            first_prompt: prompt.to_string(),
            tool_calls: BTreeMap::from([("Read".to_string(), 6)]),
            tool_errors: BTreeMap::new(),
            compactions: 0,
            unreadable_lines: 0,
        }
    }

    fn report() -> String {
        let sessions = vec![
            session("sess_1", "fix the parser"),
            session("sess_2", "add tests"),
        ];
        let total = aggregate(&sessions);
        let findings = crate::aggregate::findings(&total);
        render(&total, &findings, &sessions)
    }

    #[test]
    fn the_report_is_self_contained() {
        let html = report();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>CodeRex Insights</title>"), "{html}");
        // No external fetches: it has to open offline.
        assert!(!html.contains("http://"), "{html}");
        assert!(!html.contains("https://"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn the_report_names_the_sessions_and_their_tools() {
        let html = report();
        assert!(html.contains("sess_1"), "{html}");
        assert!(html.contains("fix the parser"), "{html}");
        assert!(html.contains("Read"), "{html}");
        assert!(html.contains("2 session(s)"), "{html}");
    }

    #[test]
    fn no_sessions_is_a_sentence_not_an_empty_page() {
        let html = render(&aggregate(&[]), &[], &[]);
        assert!(html.contains("No sessions were found"), "{html}");
        assert!(html.ends_with("</html>"));
    }

    #[test]
    fn a_prompt_that_looks_like_html_cannot_change_the_page() {
        let sessions = vec![session("s", "<script>alert(1)</script>")];
        let total = aggregate(&sessions);
        let html = render(&total, &crate::aggregate::findings(&total), &sessions);

        assert!(!html.contains("<script>alert"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn suggestions_are_set_apart_from_facts() {
        let sessions = vec![SessionFacts {
            session_id: "s".into(),
            last_active: None,
            messages: 10,
            by_role: BTreeMap::new(),
            first_prompt: "x".into(),
            tool_calls: BTreeMap::from([("Bash".to_string(), 100)]),
            tool_errors: BTreeMap::from([("Bash".to_string(), 40)]),
            compactions: 0,
            unreadable_lines: 0,
        }];
        let total = aggregate(&sessions);
        let html = render(&total, &crate::aggregate::findings(&total), &sessions);

        assert!(html.contains("class=\"suggestion\""), "{html}");
        assert!(html.contains("Worth a look"), "{html}");
        assert!(html.contains("40 of 100"), "{html}");
    }
}
