//! The report — what the loop produced, as a page.
//!
//! A hill-climb is only a loop if the previous round is readable. This renders
//! every variant side by side against the baseline: pass rate overall, pass
//! rate per case, and the actual output of anything that failed, so the next
//! change is made against evidence rather than memory.

use std::collections::BTreeMap;

use crate::harness::Harness;
use crate::run::Record;

/// Render the report for a flow.
#[must_use]
pub fn render(harness: &Harness, records: &[Record], harness_sha256: &str, flow: &str) -> String {
    let variants = variants_of(records);
    let mut html = String::new();

    html.push_str(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
    );
    html.push_str(&format!(
        "<title>{} · hillclimb</title>",
        escape(&harness.name)
    ));
    html.push_str(
        "<style>\
         :root{--bg:#f8fafc;--ink:#1e293b;--muted:#64748b;--line:#e2e8f0;\
         --pass:#047857;--fail:#b91c1c;--chip:#eef2ff}\
         body{background:var(--bg);color:var(--ink);font:15px/1.5 -apple-system,BlinkMacSystemFont,\
         'Segoe UI',system-ui,sans-serif;margin:0;padding:2rem 1rem}\
         main{max-width:960px;margin:0 auto}\
         h1{font-size:1.5rem;margin:0 0 .25rem}h2{font-size:1.05rem;margin:2rem 0 .5rem}\
         .muted{color:var(--muted)}table{border-collapse:collapse;width:100%;margin:.5rem 0}\
         th,td{text-align:left;padding:.4rem .6rem;border-bottom:1px solid var(--line)}\
         th{font-weight:600;color:var(--muted);font-size:.85rem;text-transform:uppercase}\
         td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}\
         .pass{color:var(--pass);font-weight:600}.fail{color:var(--fail);font-weight:600}\
         code{background:var(--chip);padding:.1rem .3rem;border-radius:4px;font-size:.9em}\
         pre{background:#0f172a;color:#e2e8f0;padding:.75rem;border-radius:8px;overflow-x:auto;\
         font-size:.82rem}\
         .failures{color:var(--fail);font-size:.9rem}\
         </style></head><body><main>",
    );

    html.push_str(&format!("<h1>{}</h1>", escape(&harness.name)));
    if !harness.description.is_empty() {
        html.push_str(&format!(
            "<p class=\"muted\">{}</p>",
            escape(&harness.description)
        ));
    }
    let short_hash = &harness_sha256[..12.min(harness_sha256.len())];
    html.push_str(&format!(
        "<p class=\"muted\">flow <code>{}</code> · {} case(s) · harness <code>{short_hash}</code> \
         · {} record(s)</p>",
        escape(flow),
        harness.cases.len(),
        records.len()
    ));

    if records.is_empty() {
        html.push_str("</main></body></html>");
        return html;
    }

    html.push_str(
        "<h2>Variants</h2><table><thead><tr><th>Variant</th><th class=\"num\">Records</th>\
         <th class=\"num\">Passed</th><th class=\"num\">Pass rate</th>\
         <th class=\"num\">Mean time</th><th class=\"num\">vs baseline</th></tr></thead><tbody>",
    );

    let baseline_rate = variants
        .iter()
        .find(|(name, _)| name == "baseline")
        .map(|(_, records)| pass_rate(records));

    for (variant, records) in &variants {
        let rate = pass_rate(records);
        let delta = match (baseline_rate, variant.as_str()) {
            (Some(_), "baseline") => "—".to_string(),
            (Some(baseline), _) => format!("{:+.1} pt", (rate - baseline) * 100.0),
            (None, _) => "—".to_string(),
        };

        html.push_str(&format!(
            "<tr><td><code>{}</code></td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num {}\">{:.0}%</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            escape(variant),
            records.len(),
            records.iter().filter(|record| record.passed).count(),
            if rate >= 0.999 { "pass" } else { "fail" },
            rate * 100.0,
            format_duration(mean_duration(records)),
            delta
        ));
    }
    html.push_str("</tbody></table>");

    // The per-case grid is the part that says *which* change helped.
    html.push_str("<h2>Cases</h2><table><thead><tr><th>Case</th>");
    for (variant, _) in &variants {
        html.push_str(&format!("<th class=\"num\">{}</th>", escape(variant)));
    }
    html.push_str("</tr></thead><tbody>");

    for case in &harness.cases {
        html.push_str(&format!("<tr><td><code>{}</code>", escape(&case.id)));
        if !case.description.is_empty() {
            html.push_str(&format!(
                "<br><span class=\"muted\">{}</span>",
                escape(&case.description)
            ));
        }
        html.push_str("</td>");

        for (_, records) in &variants {
            let for_case: Vec<&Record> = records
                .iter()
                .filter(|record| record.case_id == case.id)
                .collect();

            if for_case.is_empty() {
                html.push_str("<td class=\"num muted\">—</td>");
                continue;
            }
            let passed = for_case.iter().filter(|record| record.passed).count();
            let rate = passed as f64 / for_case.len() as f64;
            let ungraded = if case.expect == crate::harness::Expectations::default() {
                " (ungraded)"
            } else {
                ""
            };
            html.push_str(&format!(
                "<td class=\"num {}\">{}/{} {ungraded}</td>",
                if rate >= 0.999 { "pass" } else { "fail" },
                passed,
                for_case.len()
            ));
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table>");

    let failures: Vec<&Record> = records.iter().filter(|record| !record.passed).collect();
    if !failures.is_empty() {
        html.push_str("<h2>Failures</h2>");
        for record in failures.iter().take(50) {
            html.push_str(&format!(
                "<p><code>{}</code> on <code>{}</code> (rep {}):<br>\
                 <span class=\"failures\">{}</span></p>",
                escape(&record.case_id),
                escape(&record.variant),
                record.rep,
                escape(&record.failures.join("; "))
            ));
            if !record.output_tail.trim().is_empty() {
                html.push_str(&format!("<pre>{}</pre>", escape(record.output_tail.trim())));
            }
        }
    }

    html.push_str(&format!(
        "<h2>How to reproduce</h2><p class=\"muted\">Command per case: <code>{}</code></p>",
        escape(&harness.command)
    ));

    html.push_str("</main></body></html>");
    html
}

/// Variants in the records, baseline first, then numbered, then references.
fn variants_of(records: &[Record]) -> Vec<(String, Vec<Record>)> {
    let mut grouped: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    for record in records {
        grouped
            .entry(record.variant.clone())
            .or_default()
            .push(record.clone());
    }

    let mut variants: Vec<(String, Vec<Record>)> = grouped.into_iter().collect();
    variants.sort_by_key(|(name, _)| variant_order(name));
    variants
}

fn variant_order(name: &str) -> (u8, u32, String) {
    if name == "baseline" {
        return (0, 0, String::new());
    }
    if let Some(number) = name.strip_prefix('v') {
        if let Ok(number) = number.parse::<u32>() {
            return (1, number, String::new());
        }
    }
    (2, 0, name.to_string())
}

fn pass_rate(records: &[Record]) -> f64 {
    if records.is_empty() {
        return 0.0;
    }
    records.iter().filter(|record| record.passed).count() as f64 / records.len() as f64
}

fn mean_duration(records: &[Record]) -> u64 {
    if records.is_empty() {
        return 0;
    }
    records.iter().map(|record| record.duration_ms).sum::<u64>() / records.len() as u64
}

fn format_duration(millis: u64) -> String {
    if millis < 1000 {
        return format!("{millis} ms");
    }
    format!("{:.1} s", millis as f64 / 1000.0)
}

/// Escape text for HTML.
///
/// Case ids, prompts, and command output are all untrusted as far as the report
/// is concerned: a case that prints `</script>` must not change the page.
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
    use crate::harness::{Case, Expectations};

    fn harness() -> Harness {
        Harness {
            name: "smoke".into(),
            description: "does the thing still work".into(),
            command: "run {id}".into(),
            cases: vec![
                Case {
                    id: "c1".into(),
                    description: "the easy one".into(),
                    prompt: String::new(),
                    expect: Expectations {
                        exit_code: Some(0),
                        ..Expectations::default()
                    },
                },
                Case {
                    id: "c2".into(),
                    description: String::new(),
                    prompt: String::new(),
                    expect: Expectations::default(),
                },
            ],
            reps: 1,
            concurrency: 1,
            timeout_s: 1,
        }
    }

    fn record(case_id: &str, variant: &str, passed: bool) -> Record {
        Record {
            case_id: case_id.into(),
            variant: variant.into(),
            rep: 0,
            passed,
            graded: true,
            exit_code: Some(if passed { 0 } else { 1 }),
            duration_ms: 120,
            failures: if passed {
                Vec::new()
            } else {
                vec!["output does not contain \"ok\"".into()]
            },
            output_tail: "the command said this".into(),
        }
    }

    #[test]
    fn the_report_lists_variants_with_their_rates() {
        let records = vec![
            record("c1", "baseline", true),
            record("c2", "baseline", false),
            record("c1", "v1", true),
            record("c2", "v1", true),
        ];
        let html = render(&harness(), &records, "abcdef0123456789", "smoke");

        assert!(html.contains("baseline"), "{html}");
        assert!(html.contains("50%"), "{html}");
        assert!(html.contains("100%"), "{html}");
        assert!(html.contains("+50.0 pt"), "{html}");
    }

    #[test]
    fn a_case_with_no_expectations_is_marked_ungraded() {
        let records = vec![record("c2", "baseline", true)];
        let html = render(&harness(), &records, "abcdef", "smoke");
        assert!(html.contains("(ungraded)"), "{html}");
    }

    #[test]
    fn failures_are_shown_with_what_the_command_said() {
        let records = vec![record("c1", "baseline", false)];
        let html = render(&harness(), &records, "abcdef", "smoke");

        assert!(html.contains("output does not contain"), "{html}");
        assert!(html.contains("the command said this"), "{html}");
    }

    #[test]
    fn an_empty_run_still_renders() {
        let html = render(&harness(), &[], "abcdef", "smoke");
        assert!(html.ends_with("</html>"));
        assert!(html.contains("smoke"));
    }

    #[test]
    fn output_that_looks_like_html_cannot_escape_the_page() {
        let mut bad = record("c1", "baseline", false);
        bad.output_tail = "<script>alert(1)</script>".into();
        bad.failures = vec!["<img src=x onerror=alert(1)>".into()];

        let html = render(&harness(), &[bad], "abcdef", "smoke");
        assert!(!html.contains("<script>alert"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("onerror=alert(1)>"), "{html}");
    }

    #[test]
    fn variants_are_ordered_baseline_then_numbered_then_references() {
        let records = vec![
            record("c1", "v2", true),
            record("c1", "ref/other", true),
            record("c1", "baseline", true),
            record("c1", "v10", true),
            record("c1", "v1", true),
        ];
        let names: Vec<String> = variants_of(&records)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["baseline", "v1", "v2", "v10", "ref/other"]);
    }

    #[test]
    fn durations_are_readable() {
        assert_eq!(format_duration(120), "120 ms");
        assert_eq!(format_duration(2_500), "2.5 s");
    }
}
