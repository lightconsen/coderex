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

## The live-provider flow (opt-in)

`evals/flows/live/` drives the real `coderex` binary against a real
Anthropic-compatible provider. It is checked in beside the smoke flow, but it
is never part of the default `eval-runner` or the offline `evals` job, and the
smoke flow is unchanged.

Run it with `scripts/test-live-eval.sh`:

```bash
ANTHROPIC_API_KEY=... \
ANTHROPIC_BASE_URL=https://api.anthropic.com \
ANTHROPIC_MODEL=claude-sonnet-4-5 \
  scripts/test-live-eval.sh
```

Inputs (environment):

| Variable | Required | Meaning |
|----------|----------|---------|
| `ANTHROPIC_API_KEY` | yes | The script refuses to run without it, before building or calling a provider. |
| `ANTHROPIC_BASE_URL` | no | Anthropic-compatible endpoint. If unset, the CLI uses its own default; the script prints a note. |
| `ANTHROPIC_MODEL` | no | Passed to the CLI as `--model`. If unset, the CLI's configured default is used. |
| `CODEREX_BIN` | no | An already-built binary; otherwise the script builds `coderex` in debug mode. |

The flow has two cases, each with `reps: 1` and `concurrency: 1`:

- `live-text-turn` — a text-only prompt; output must contain `live-eval-ok`.
- `live-read-tool-turn` — `run-case.sh` writes `live-eval.txt` in the case's
  run directory, then the prompt asks the model to `Read` it; output must
  contain `live-tool-ok`.

`run-case.sh` invokes the same binary a person runs, with
`--print --brief --dangerously-skip-permissions`, and inherits the provider
environment. It sets `CODEREX_SKIP_ENV_SOURCE=1` so a developer's local
`export_env.*.sh` cannot silently redirect the run.

Outputs:

| Path | Meaning |
|------|---------|
| `evals/flows/live/_state.json` | Approved harness hash (the script passes `--approve-harness`). |
| `evals/flows/live/baseline/results.jsonl` | One record per case run, appended. |
| `evals/flows/live/report.html` | Rendered pass/fail report. |

The script exits non-zero when any case fails. Extra arguments are forwarded to
`run-eval`, so `--case live-text-turn`, `--reps`, and `--timeout-s` work as
usual.

**This flow has not been run from this host.** No live provider credentials
were configured during development, and adding the flow was verified only by
building and inspecting it; the first real execution must come from a
machine or workflow with credentials.

In CI it is the `live-provider-eval` job (plus the
`live-provider-eval-preflight` credential check) in
`.github/workflows/ci.yml`. Both are `workflow_dispatch` only and are skipped
on push and pull request; the preflight converts the presence of
`ANTHROPIC_API_KEY` into an output so the live job does not fail merely
because a repository has no secret. The job runs `scripts/test-live-eval.sh`
and uploads `report.html` and `baseline/results.jsonl` as `live-eval-report`.

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
