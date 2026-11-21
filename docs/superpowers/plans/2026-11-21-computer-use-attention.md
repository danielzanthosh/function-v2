# Computer Use Attention and Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make agent questions reliably actionable and prevent global input from escaping a selected background window context.

**Architecture:** The agent exposes a synthetic `request_user_choice` tool and emits a typed pending-choice terminal state with validated options and resumable history. The UI keeps its existing card, status text, and sound, adds a native macOS notification, and resumes with a tool result for the selected option. The agent loop tracks the selected window per run, injects it into low-level mouse/keyboard calls, and rejects legacy global input/focus calls.

**Tech Stack:** Rust, Tokio, GPUI, existing Objective-C/AppKit bindings, Swift ScreenCaptureKit helper, Cargo tests.

**Spec:** `docs/superpowers/specs/2026-11-21-computer-use-attention-design.md`

## Global Constraints

- Keep the Rust + GPUI app intact and keep native interactions in the existing macOS platform layer/helper.
- Do not bypass macOS permissions or add dependencies.
- Preserve global controls when a run has not selected a background window.
- Keep the current status indicator and in-app choice card as notification fallbacks.

## Review Focus

- Malformed choice tool arguments must fail visibly and must not continue the requested action.
- A stale/closed target window must fail before input is sent.
- Unscoped mouse or keyboard calls after target selection must not reach the physical foreground app.
- Notification suppression or denied permissions must not hide the in-app choice card.
- Selecting a choice must preserve prior conversation history and satisfy the pending tool call.

## File Map

- `crates/agent/src/lib.rs`: choice tool schema, state, history handoff, and per-run background input policy.
- `crates/agent/tests/lifecycle_tests.rs` or agent unit tests: red/green coverage for choice and routing behavior.
- `crates/ui/src/views/function_view.rs`: render typed choice state, notify macOS, and resume with the tool result.
- `crates/platform/src/lib.rs`: best-effort native notification function using Function's AppKit process.
- `crates/platform/src/macos_capture.swift`: only if the existing helper must expose notification behavior; prefer AppKit in the host process.
- `docs/superpowers/plans/2026-11-21-computer-use-attention.md`: record manual macOS acceptance results.

## Tasks

### 1. Add typed agent choice requests

- [x] Add an agent test where the provider calls `request_user_choice`; assert the agent emits typed question/options and stops before ordinary completion.
- [x] Add the virtual tool definition to model requests and validate 2–6 non-empty options plus a non-empty question.
- [x] Return the assistant tool call in resumable history and expose its call ID in the pending state.
- [x] Run agent tests; verify malformed arguments fail and valid requests end in the pending-choice state.

### 2. Notify and resume from the choice card

- [x] Test selected choice serialization; no notification payload test seam exists in the current AppKit wrapper.
- [x] Emit the typed state, show the existing choice card/status text, and post a best-effort AppKit notification from the Function process.
- [x] On button selection, append the tool result and selected option to history, then start the next agent turn.
- [x] Run agent/UI/platform tests and compile the app.

### 3. Enforce target-window input context

- [x] Add tests for window selection, low-level input target injection, explicit target switching, and rejection of global input/focus tools.
- [x] Track selected window ID locally inside each agent run; never share the context across runs.
- [x] Inject the selected ID into `computer_mouse` and `computer_keyboard` calls without one; reject global aliases and app-focus actions while selected.
- [x] Run agent, tools, and workspace tests.

### 4. Verify on real macOS apps

- [x] Build the current app and use real macOS apps (TextEdit and Chrome) as target/foreground windows.
- [ ] Start a target-window task, bring another app forward, and verify physical mouse/keyboard input there while the agent continues. Capture succeeded while Chrome stayed foreground; input was blocked because Function lacks Accessibility permission.
- [ ] Confirm a question produces a macOS alert plus buttons and choosing a button resumes the same task in the packaged app. The typed state/card/resumption paths compile and have agent tests; full UI interaction remains unverified.
- [x] Record the macOS observations here; the remaining permission-gated behavior is reported as unverified.

## Completion Checks

- `cargo test --workspace --locked`
- `cargo check -p function-app --features gpui/runtime_shaders --locked`
- `git diff --check`
- Manual real-app acceptance task above completed or clearly reported as blocked by platform/permission constraints.
