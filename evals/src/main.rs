//! Standalone eval runner for CodeRex.
//!
//! Run all registered eval scenarios and report pass/fail status.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin eval-runner
//! ```

use coderex_test_utils::{EvalAssertion, EvalScenario, run_eval};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║           CodeRex AI Eval Runner                             ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    let scenarios = vec![tool_call_eval(), no_tool_eval(), streaming_text_eval()];

    let mut passed = 0;
    let mut failed = 0;

    for scenario in scenarios {
        let result = run_eval(scenario).await;
        println!(
            "  {:.<50} {}",
            result.name,
            if result.passed {
                "✓ PASS"
            } else {
                "✗ FAIL"
            }
        );

        if !result.passed {
            failed += 1;
            for ar in &result.assertion_results {
                if !ar.passed {
                    println!("    ✗ {} — {}", format_assertion(&ar.assertion), ar.message);
                }
            }
        } else {
            passed += 1;
        }
    }

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!("  Results: {passed} passed, {failed} failed");
    println!("═══════════════════════════════════════════════════════════════");

    if failed > 0 {
        std::process::exit(1);
    }
}

fn format_assertion(a: &EvalAssertion) -> String {
    match a {
        EvalAssertion::ToolCalled { name } => format!("ToolCalled({name})"),
        EvalAssertion::ToolNotCalled { name } => format!("ToolNotCalled({name})"),
        EvalAssertion::OutputContains { text } => format!("OutputContains({text})"),
        EvalAssertion::OutputNotContains { text } => format!("OutputNotContains({text})"),
        EvalAssertion::TextDeltaCount { min } => format!("TextDeltaCount(>={min})"),
        EvalAssertion::EventSequence { types } => format!("EventSequence({types:?})"),
        EvalAssertion::TerminalReason { reason } => format!("TerminalReason({reason})"),
    }
}

fn tool_call_eval() -> EvalScenario {
    EvalScenario {
        name: "tool_call/read_file".into(),
        responses: vec![
            r#"[{"id":"tc_1","name":"Read","input":{"path":"hello.txt"}}]"#.into(),
            "Done reading the file.".into(),
        ],
        assertions: vec![
            EvalAssertion::ToolCalled {
                name: "Read".into(),
            },
            EvalAssertion::OutputContains {
                text: "stub ok: Read".into(),
            },
        ],
        tool_executor: None,
    }
}

fn no_tool_eval() -> EvalScenario {
    EvalScenario {
        name: "no_tool/simple_response".into(),
        responses: vec!["Hello, world!".into()],
        assertions: vec![
            EvalAssertion::ToolNotCalled {
                name: "Read".into(),
            },
            EvalAssertion::TextDeltaCount { min: 1 },
        ],
        tool_executor: None,
    }
}

fn streaming_text_eval() -> EvalScenario {
    EvalScenario {
        name: "streaming/text_only".into(),
        responses: vec!["This is a streaming response.".into()],
        assertions: vec![
            EvalAssertion::TextDeltaCount { min: 1 },
            EvalAssertion::ToolNotCalled {
                name: "Bash".into(),
            },
        ],
        tool_executor: None,
    }
}
