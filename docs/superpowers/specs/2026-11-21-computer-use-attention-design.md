# Computer Use Attention and Isolation Design

## Intent

When Function needs a decision, it should notify the user, present clear choices, and resume the same agent run with the selected answer. Background computer-use workflows must keep input scoped to the selected application window so global compatibility tools cannot type or click in the user's foreground app.

## Goals

- Replace prose-only clarification detection with a validated agent choice request and preserve a text fallback for existing model responses.
- Deliver a macOS notification and keep the existing status indicator and in-app choice card.
- Latch a background window context during a run; route low-level mouse/keyboard calls to that window and reject legacy global input/focus calls until another target window is selected.
- Verify the flow with automated tests and a manual macOS foreground-app/input isolation check.

## Non-goals

- Replacing the Rust + GPUI application or the Swift native helper.
- Removing global desktop tools from workflows that have not selected a background window.
- Claiming background input works in every macOS application; unsupported applications and protected UI must report limitations.

## Approaches

1. Continue parsing assistant prose. This is the smallest change, but model formatting can silently break the choice UI.
2. Add an internal `request_user_choice` tool call that the agent validates and exposes as a typed pending-choice state. This reuses existing provider tool calling, has a clear validation boundary, and is the selected approach.
3. Add provider-specific structured-output APIs. This adds provider coupling and does not cover the existing provider set uniformly.

Use AppKit's native notification center from the Function process so alerts have Function's app identity. Keep the choice card as the reliable in-app fallback. Do not introduce a dependency.

## Constraints and decisions

- Keep choice requests to 2–6 non-empty strings and a non-empty question; invalid requests become a visible agent error instead of unsafe continuation.
- A choice answer must be added to the same conversation history before the agent resumes.
- The target window is scoped per agent run. Explicitly observing another window selects it; unscoped low-level input inherits the selected ID. Global input/focus calls are rejected after selection.
- User physical input remains untouched; only the native helper's app-directed input is used.

## Builder decisions

- Treat the AppKit notification as best-effort; the status text and in-app card remain available if macOS suppresses notifications.
- Preserve the prose parser only as a fallback for older provider/model replies.
