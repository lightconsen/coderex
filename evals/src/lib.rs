//! CodeRex evals — the hill-climbing loop.
//!
//! A flow is a directory of cases; a variant is a way of doing the work; the
//! loop is: run the cases against the baseline, change something, run them
//! again as `v1`, and read the report to see whether it helped. `v2` is
//! another variant of the same harness, not another program — that is the
//! whole idea, and it is why everything a run does is declared as data.
//!
//! What the loop needs to be worth trusting is here too. The harness is hashed
//! and pinned, so a run cannot quietly mean a different test than the baseline
//! it is compared against. Results are appended as they happen, so an
//! interrupted run resumes. Every id that reaches a path is checked, and every
//! case runs in a directory of its own, because the thing being measured can
//! write to the repository the flow lives in.

#![warn(missing_docs)]

pub mod grade;
pub mod harness;
pub mod paths;
pub mod report;
pub mod run;

pub use grade::{RunOutput, Verdict, expects_something, grade};
pub use harness::{
    Case, Expectations, HARNESS_FILE, HILLCLIMB_DIR, Harness, REPORT_FILE, RESULTS_FILE,
    STATE_FILE, Variant, flow_dir,
};
pub use paths::HarnessError;
pub use run::{Record, RunOptions, RunOutcome, all_records, run};
