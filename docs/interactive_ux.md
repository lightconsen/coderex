# Claude-Code-Style Interactive UX and Testing

## Goal

Define a Claude Code-like interactive experience for `coderex` TUI mode and
the engineering work needed to implement it safely and test it reliably.

This document covers:

- the desired interactive UX
- what should and should not be shown to the user
- required runtime/state/event changes
- UI changes needed across statusline, message area, permission UX, and overlays
- a PTY-based true interactive E2E testing strategy

## Non-Goals

- Exposing private chain-of-thought or raw internal reasoning
- Replacing the existing headless / `-p` E2E suite
- Rewriting the terminal UI from scratch

The target is to add a rich, user-visible activity model on top of the current
REPL, not to leak internal reasoning text.

## Product Goal

When the user is in interactive mode, `coderex` should feel alive while a task
is in progress.

Instead of showing only a final answer, the UI should communicate:

- what phase the agent is in
- what action it is currently performing
- how long the current phase or turn has been running
- when permission is blocking progress
- what work has completed so far

The target feeling is:

- responsive
- inspectable
- cancellable
- resumable
- testable

## Desired User Experience

### During a turn

The user should be able to see status updates such as:

- `Thinking...`
- `Planning next step...`
- `Reading src/lib.rs...`
- `Running cargo test -p coderex-cli...`
- `Applying patch to 2 files...`
- `Waiting for permission: Write`

Each of these should show a live elapsed timer while active.

Example:

```text
◐ Planning next step...  (3s)
◓ Reading src/lib.rs...  (5s)
◑ Running cargo test -p coderex-cli...  (19s)
```

### After a turn

The user should see a short completion state, for example:

```text
Completed in 27s
```

or

```text
Completed: edited 3 files, ran 1 test command (27s)
```

### What should not be shown

Do not display raw chain-of-thought or hidden reasoning text.

Allowed:

- safe activity labels
- plan step labels
- tool/action summaries
- timing and status

Not allowed:

- hidden reasoning paragraphs
- raw internal deliberation
- private safety logic or decision traces

## Current State

Current TUI already has foundational pieces:

- `statusline`
- `notification bar`
- `permission bar`
- `message area`
- `overlay stack`
- `thinking spinner`
- live elapsed timer while `thinking = true`

Current limitations:

- runtime mostly only displays `Thinking`
- no full phase model (`Planning`, `Running tests`, `Applying patch`, etc.)
- plan state is structurally present but not wired end-to-end
- no complete, true interactive PTY E2E test harness
- most current E2E coverage is headless or `-p`-driven

## Screen Model

The current interactive screen can expose up to 6 vertical regions:

```text
+--------------------------------------------------+
|                  Message Area                    |
+--------------------------------------------------+
|               Inline Command Palette             |
+--------------------------------------------------+
|                Notification Bar                  |
+--------------------------------------------------+
|                   Status Line                    |
+--------------------------------------------------+
|                    Input Line                    |
+--------------------------------------------------+
|                  Permission Bar                  |
+--------------------------------------------------+
```

Default startup normally shows only a subset:

```text
+--------------------------------------------------+
|                  Message Area                    |
+--------------------------------------------------+
|                   Status Line                    |
+--------------------------------------------------+
|                    Input Line                    |
+--------------------------------------------------+
|                  Permission Bar                  |
+--------------------------------------------------+
```

Compact mode can collapse this further.

## UX Principles

1. **Always show current progress**
2. **Never expose hidden reasoning**
3. **Make blocking states obvious**
4. **Prefer short, human-readable activity labels**
5. **Keep the user in control during long-running work**
6. **Make state transitions testable**

## Required Runtime State Model

The current `thinking: bool` is too coarse for the target UX.

Introduce a proper interactive state machine, for example:

```text
Idle
Thinking { started_at, label }
Planning { started_at, step_info }
RunningTool { tool_name, summary, started_at }
WaitingPermission { tool_name, reason, started_at }
ApplyingPatch { file_count, started_at }
RunningTests { command, started_at }
Completed { duration, summary }
Error { message }
Cancelled
```

At minimum the UI layer should be able to distinguish:

- active AI thinking
- plan generation
- tool execution
- permission wait
- patch application
- test execution
- completion
- error

## Required Event Model

To drive the above state machine, the engine/orchestrator path must emit
structured lifecycle events.

Recommended event set:

- `ThinkingStarted`
- `ThinkingUpdated { label }`
- `ThinkingFinished`
- `PlanStarted`
- `PlanStepStarted { step, total, label }`
- `PlanStepFinished { step }`
- `ToolStarted { name, summary }`
- `ToolProgress { message }`
- `ToolFinished { success }`
- `PermissionRequested { tool, summary, risk }`
- `PermissionResolved { approved }`
- `PatchApplyStarted { file_count }`
- `PatchApplyFinished`
- `TestsStarted { command }`
- `TestsFinished { success, summary }`
- `TurnFinished { duration, summary }`
- `TurnErrored { message }`
- `TurnCancelled`

These events should be safe for UI display.

## Activity Labeling Layer

The UI should not show raw tool inputs directly.

Instead, add a small summary layer that converts internal actions into safe,
human-readable labels.

Examples:

- `Read { file_path: "src/lib.rs" }` -> `Reading src/lib.rs...`
- `Grep { pattern: "QueryEngine" }` -> `Searching for QueryEngine...`
- `Bash { command: "cargo test -p coderex-cli" }` -> `Running cargo test -p coderex-cli...`
- patch apply across two files -> `Applying patch to 2 files...`

This layer should:

- redact sensitive values where necessary
- keep labels short
- be deterministic enough for tests

## Plan Model

To feel closer to Claude Code, planning should be more than a single label.

Introduce a lightweight plan object:

- `steps: Vec<PlanStep>`
- `current_step: usize`
- `total_steps: usize`
- `current_label: String`
- `completed_steps: usize`

Possible displays:

- statusline: current step only
- message area: plan events or plan summary
- overlay: full plan viewer

Example statusline:

```text
◐ Step 2/5: Running tests  (14s)
```

## UI Workstreams

### 1. Statusline upgrade

Extend the current statusline to show:

- phase label
- spinner
- live elapsed duration
- optional step progress
- short completion summary after each turn

Examples:

- `◐ Thinking...  (2s)`
- `◓ Planning next step...  (4s)`
- `◑ Step 2/5: Running tests  (21s)`
- `Waiting for permission: Write  (8s)`
- `Completed in 27s`

### 2. Message area system events

Add optional system event rows so the user can see what the agent did during the
turn.

Examples:

- `Planning next step...`
- `Read src/lib.rs`
- `Running cargo test -p coderex-cli`
- `Applied patch to 2 files`

These should not flood the transcript; keep them concise.

### 3. Permission UX

Permission handling should become visibly interactive.

Show:

- requested tool/action
- risk summary
- current wait state
- approval/denial result

At minimum:

- permission bar reflects current blocking state
- overlay/modal can present more detail when needed

### 4. Notification UX

Notification bar can be used for:

- short progress teasers
- recent success/failure
- background activity summaries

Use notifications for secondary events, not as the primary progress display.

### 5. Overlay UX

Overlays can support:

- full plan inspection
- permission details
- recent activity details
- failure details

## Cancel / Interrupt / Resume

A Claude Code-like UX must be interruptible.

Add support for:

- `Esc` or `Ctrl+C` to cancel the current stage when safe
- cancelling while waiting on permission
- cancelling long-running tool/test phases
- recovering cleanly to `Idle`
- preserving enough activity history to explain what happened

## Persistence (Optional but Valuable)

Later phases can add:

- persisted current plan
- persisted recent activity summary
- session resume with unfinished task context

This is not required for v1, but it materially improves product feel.

## Implementation Plan

### Phase 1: Minimal visible Claude-Code-like UX

Deliver the smallest version that already feels much better:

1. replace `thinking: bool` with a richer interactive phase model
2. wire lifecycle events from engine/orchestrator/tool execution
3. show `label + spinner + elapsed` in statusline
4. display permission wait as an explicit phase
5. show final turn duration on completion

Target examples:

- `Planning...`
- `Running tool...`
- `Waiting permission...`
- `Running tests...`
- live elapsed timer

### Phase 2: Activity history and plan visibility

1. add message-area system events
2. add tool summary/activity label layer
3. add plan object and step rendering
4. improve notification bar usage

### Phase 3: Full interaction loop

1. cancel/resume behavior
2. plan overlay
3. richer permission UX
4. background/foreground activity integration
5. optional persistence/resume

## Testing Strategy

This feature set needs a **true interactive PTY harness**, not just `-p` tests.

### Why PTY is required

Real interactive mode depends on:

- TTY detection
- raw mode
- cursor movement
- screen redraw
- alternate screen behavior
- key events rather than line-only stdin

So tests should launch `coderex` **without `-p`** inside a PTY and interact with
it like a user would.

### Harness capabilities

The harness should support:

1. start `coderex` in PTY mode
2. set terminal size
3. send text lines
4. send special keys (`Enter`, arrows, `Ctrl+C`, `Ctrl+D`, `Esc`)
5. read and normalize terminal output
6. wait for specific text or regex matches
7. detect stable screen state
8. assert file system side effects

### Assertion strategy

Do not rely only on full-screen snapshots.

Use three layers of assertion:

1. **region presence**
   - whether a region exists or is hidden
2. **region content**
   - whether the right text is visible in that region
3. **state transition**
   - whether content changes correctly over time

## Region-Level Assertions

### Message Area

Validate:

- welcome text
- user messages
- assistant replies
- system activity events
- tool summaries
- multi-turn history
- `/clear` behavior

### Status Line

Validate:

- current model
- idle/thinking/planning/tool/test labels
- live elapsed timer increases
- completion duration shows after turn
- compact mode hides it when appropriate

### Input Line

Validate:

- typed text
- editing behavior
- history recall
- clear-on-submit
- clear-on-cancel

### Permission Bar

Validate:

- safe/default state
- permission request state
- approval/denial state changes
- compact mode hiding

### Inline Command Palette

Validate:

- `/` opens palette
- filtering works
- selection state works
- dismissal works

### Notification Bar

Validate:

- progress/info/success/error notifications
- appearance/disappearance
- unseen count behavior if retained

### Overlay / Modal

Validate:

- visibility
- correct content
- proper close behavior
- input focus restoration

### Optional Split Pane

Validate:

- left-pane file/content rendering
- right-pane message continuity
- resize stability

## Priority Interactive Test Cases

Recommended first 12 tests:

1. startup renders prompt and default regions
2. single prompt gets a response
3. two-turn context is preserved
4. `/help` renders correctly
5. `/status` renders correctly
6. `/clear` clears transcript state
7. safe read action completes without broken state
8. write action triggers permission request
9. write action denial keeps session alive
10. tool failure recovers to usable prompt
11. `Ctrl+C` clears current input or cancels safely
12. one Kimi and one Qwen live interactive smoke test

## Full Interactive Test Matrix

### A. Startup / Exit

- startup succeeds
- empty Enter
- `Ctrl+C` clears input
- `Ctrl+D` exits cleanly
- immediate exit

### B. Basic Conversation

- single prompt response
- streaming visible
- Unicode input
- long prompt handling

### C. Multi-Turn Context

- 2-turn recall
- 5-turn continuity
- clear after history
- compact after history

### D. Slash Commands

- `/help`
- `/status`
- `/tokens`
- `/model`
- `/clear`
- `/compact`
- `/vim`
- unknown command

### E. Input Editing

- backspace
- left/right cursor movement
- up/down history
- multiline input if supported
- paste handling

### F. Permissions and Tools

- safe read
- write requires approval
- write approved
- write denied
- dangerous command blocked
- permission flow returns to usable state

### G. File Side Effects

- create file
- edit existing file
- respect `.gitignore`
- dirty worktree handling
- path handling safety

### H. Error Recovery

- missing file
- tool error recovery
- provider failure recovery
- timeout recovery
- invalid slash args

### I. UI / TTY

- small terminal startup
- resize
- long output scrolling
- ANSI normalization
- color-off mode if supported

### J. Live Provider Smoke

- Kimi interactive file read
- Qwen interactive file read
- provider switch consistency
- slow response still keeps UI alive

## Acceptance Criteria

The feature is complete enough for a first release when:

1. statusline shows live phase label + timer
2. permission waiting is explicit in the UI
3. tool/test phases are visible, not hidden
4. completion duration is visible after each turn
5. at least the 12 priority interactive PTY tests pass
6. Kimi and Qwen live interactive smoke tests pass

## Recommended First Deliverable

The most efficient first implementation is:

- a PTY harness
- a richer interactive phase model
- statusline phase labels
- explicit permission wait state
- explicit tool/test activity labels
- live elapsed timer
- 12 priority interactive tests

That is the smallest version that will already feel much closer to Claude Code.
