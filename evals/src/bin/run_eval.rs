//! `run-eval` — the hill-climb loop from the command line.
//!
//! ```bash
//! run-eval --flow smoke --approve-harness          # pin the harness
//! run-eval --flow smoke --variant baseline         # measure the baseline
//! run-eval --flow smoke --variant v1 --reps 3      # measure a change
//! run-eval --flow-dir evals/flows/smoke --variant baseline  # a checked-in flow
//! ```
//!
//! Everything else is in the flow: the command each case runs and what it must
//! do are declared in `flow.json` beside it.

use coderex_evals::{HarnessError, RunOptions, Variant, run};

fn main() {
    if let Err(error) = main_result() {
        eprintln!("run-eval: {error}");
        std::process::exit(1);
    }
}

fn main_result() -> Result<(), HarnessError> {
    let mut options = RunOptions::new(
        std::env::current_dir().unwrap_or_default(),
        "",
        Variant::Baseline,
    );
    let mut show_help = false;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        let mut value = || -> Result<String, HarnessError> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| HarnessError::Invalid(format!("{argument} needs a value")))
        };

        match argument {
            "--flow" => options.flow = value()?,
            "--flow-dir" => options.flow_dir = Some(std::path::PathBuf::from(value()?)),
            "--variant" => options.variant = Variant::parse(&value()?)?,
            "--model" => options.model = Some(value()?),
            "--repo" => options.repo = std::path::PathBuf::from(value()?),
            "--case" => options.only_case = Some(value()?),
            "--reps" => {
                options.reps = Some(
                    value()?
                        .parse()
                        .map_err(|_| HarnessError::Invalid("--reps needs a number".to_string()))?,
                );
            }
            "--concurrency" => {
                options.concurrency = Some(value()?.parse().map_err(|_| {
                    HarnessError::Invalid("--concurrency needs a number".to_string())
                })?);
            }
            "--timeout-s" => {
                options.timeout_s = Some(value()?.parse().map_err(|_| {
                    HarnessError::Invalid("--timeout-s needs a number".to_string())
                })?);
            }
            "--approve-harness" => options.approve_harness = true,
            "--force" => options.force = true,
            "--help" | "-h" => show_help = true,
            other => {
                return Err(HarnessError::Invalid(format!(
                    "unknown argument '{other}' — try --help"
                )));
            }
        }
        index += 1;
    }

    if show_help || (options.flow.is_empty() && options.flow_dir.is_none()) {
        print_help();
        return Ok(());
    }

    if options.approve_harness {
        let dir_label = options
            .flow_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let label = if options.flow.is_empty() {
            dir_label.as_str()
        } else {
            options.flow.as_str()
        };
        println!("Approving the harness for flow '{label}'.");
    }

    let outcome = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| HarnessError::Run(error.to_string()))?
        .block_on(run(&options))?;

    println!(
        "flow '{}' variant '{}': {} ran, {} passed ({:.0}%), {} resumed",
        outcome.flow,
        outcome.variant,
        outcome.records.len(),
        outcome.passed(),
        outcome.pass_rate() * 100.0,
        outcome.resumed
    );

    for record in outcome.records.iter().filter(|record| !record.passed) {
        println!(
            "  FAIL {} (rep {}): {}",
            record.case_id,
            record.rep,
            record.failures.join("; ")
        );
        // The child's own error only exists in its output. Without this a
        // failing case reports "exit code 2" and nothing else, which is
        // exactly what happened to the live flow in CI.
        let tail = record.output_tail.trim();
        if !tail.is_empty() {
            let lines: Vec<&str> = tail.lines().collect();
            for line in lines.iter().skip(lines.len().saturating_sub(20)) {
                println!("        | {line}");
            }
        }
    }

    println!("report: {}", outcome.report_path.display());
    println!("records: {}", outcome.results_path.display());

    if outcome.records.iter().any(|record| !record.passed) {
        std::process::exit(1);
    }
    Ok(())
}

fn print_help() {
    println!(
        "run-eval — run a hill-climb flow\n\
         \n\
         usage: run-eval (--flow DIR | --flow-dir PATH) --variant ID [options]\n\
         \n\
         \x20 --flow DIR          the flow to run (under .coderex/hillclimb/)\n\
         \x20 --flow-dir PATH     the flow's directory, wherever it is checked in\n\
         \x20 --variant ID        'baseline' or 'v<N>'\n\
         \x20 --model ID          passed to the command as {{model}}\n\
         \x20 --repo DIR          the repository the flow lives in (default: here)\n\
         \x20 --reps N            repetitions per case (default: the flow's)\n\
         \x20 --concurrency N     cases at once (default: the flow's)\n\
         \x20 --timeout-s N       per case; 0 means no ceiling\n\
         \x20 --case ID           run one case\n\
         \x20 --force             re-run cases that already have a record\n\
         \x20 --approve-harness   pin the harness before running\n"
    );
}
