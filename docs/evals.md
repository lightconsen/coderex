# Evals — the hill-climb loop

Two things live in `evals/`:

- **Scenario evals** (`eval-runner`) — in-process assertions on agent behaviour
  against mock model responses. See `crates/test-utils/src/eval.rs`.
- **The hill-climb loop** (`run-eval`) — graded cases run against a real
  command, in variants, with a report that says which variant helped.

This document is about the loop.

## The idea

A *flow* is a directory of cases. A *variant* is a way of doing the work.
The loop is:

```bash
run-eval --flow smoke --approve-harness      # pin the harness
run-eval --flow smoke --variant baseline     # measure where you are
#   … change the thing you are measuring …
run-eval --flow smoke --variant v1           # measure the change
open .coderex/hillclimb/smoke/report.html    # did it help?
```

`v2` is another variant of the same harness, not another program. That is the
point of the design: everything a run does is data, declared in `flow.json`.

## A flow

```json
{
  "name": "artifact-publish",
  "description": "can the agent publish a page and read it back",
  "command": "coderex --headless --prompt {prompt} --model {model} --cwd {cwd}",
  "reps": 3,
  "concurrency": 2,
  "timeout_s": 300,
  "cases": [
    {
      "id": "publishes-a-page",
      "prompt": "publish a page that says hello",
      "expect": {
        "exit_code": 0,
        "output_contains": ["artifact"],
        "output_not_contains": ["error"],
        "files_contain": { "notes.md": "hello" },
        "files_absent": ["tmp-scratch.txt"]
      }
    }
  ]
}
```

Placeholders the command can use: `{id}`, `{prompt}`, `{variant}`, `{flow}`,
`{cwd}`, `{model}`. Each case runs in a fresh directory of its own
(`<flow>/<variant>/<case>/run-<rep>/`), which is what `{cwd}` points at and
what the file assertions are resolved against.

A case whose `expect` is empty passes as soon as the command runs — useful when
the thing being measured is duration or cost. The report marks those
`(ungraded)` rather than counting them as assertions that held.

## What the layout looks like

```
.coderex/hillclimb/<flow>/
  flow.json          the harness
  _state.json        the approved harness hash
  report.html        generated
  baseline/
    results.jsonl
    <case>/run-<rep>/   scratch, assertions resolve here
  v1/
    results.jsonl
  ref/<id>/
    results.jsonl
```

## Why the harness is pinned

`--approve-harness` records a SHA-256 over every file the harness reads. A
later run whose harness does not match is refused:

```
harness changed since last approved run (files: flow.json) — review the
change, then run with --approve-harness to approve it
```

A variant is only comparable to its baseline if the same harness measured
both. Without the pin, "v1 is better than baseline" could mean "the tests
changed". Variant output directories are excluded from the hash, so running a
flow never invalidates its own approval.

## Resuming

Records are appended to `results.jsonl` as they land, and a run skips cases
that already have a record for that variant and repetition. An interrupted run
picks up where it stopped, and a variant can be extended case by case. Use
`--force` to re-run everything, or `--case <id>` to run one case.

## Options

```
--flow DIR          the flow to run (under .coderex/hillclimb/)
--flow-dir PATH     the flow's directory, wherever it is checked in
--variant ID        'baseline' or 'v<N>'
--model ID          passed to the command as {model}
--repo DIR          the repository the flow lives in (default: here)
--reps N            repetitions per case (default: the flow's)
--concurrency N     cases at once (default: the flow's)
--timeout-s N       per case; 0 means no ceiling
--case ID           run one case
--force             re-run cases that already have a record
--approve-harness   pin the harness before running
```

`run-eval` exits non-zero when any case in that run failed, so it can gate CI.

## The checked-in smoke flow

`evals/flows/smoke/` is a minimal flow that lives in the repository rather
than under `.coderex/hillclimb/`. `--flow-dir` points the runner at it
directly, and the flow name is taken from `flow.json` when `--flow` is not
given:

```bash
cargo build -p coderex-cli --bin coderex --bin static-anthropic-server
cargo run -p coderex-evals --bin run-eval -- \
  --flow-dir evals/flows/smoke \
  --variant baseline \
  --approve-harness \
  --force
```

It needs no network and no API key. Each case's command is
`evals/flows/smoke/run-case.sh`, which starts the repository's
`static-anthropic-server` fixture on a free loopback port with
`tests/responses.json`, points `ANTHROPIC_BASE_URL` at it, and runs the real
`coderex` binary:

- `hello-from-fixture` exercises a streamed text turn through the fixture.
- `local-status-command` runs `/status`, which needs no provider call at all.

Run output (`baseline/`, `report.html`, `_state.json`) is ignored by the flow's
`.gitignore`; the harness hash covers only checked-in files.

The `evals` job in `.github/workflows/ci.yml` builds the two binaries and runs
the command above, then uploads `report.html` and `baseline/results.jsonl` as
the `eval-report` artifact (on failure too).

## Safety

The thing being measured can write to the repository the flow lives in, so:

- every id that becomes a path (flow, variant, case) is checked for `..`,
  separators, and leading dots, and refused rather than renamed;
- file assertions resolve inside the case's run directory and cannot climb out
  of it;
- each repetition starts from an empty directory, so a case cannot measure
  what the previous one left behind;
- the report escapes case output, so a case that prints `</script>` cannot
  change the page.
