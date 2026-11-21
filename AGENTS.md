# Repository Guidelines

## Project Structure

This repository is a Rust 2021 Cargo workspace. The application entry point and dependency wiring are in `crates/app`; the other crates separate the GPUI interface (`ui`), agent loop (`agent`), tools (`tools`), memory (`memory`), model and speech providers (`providers`), OS integrations (`platform`), and configuration (`config`). Rust integration tests live under each crate's `tests/` directory. Shared images and platform resources are in `assets/`; product, architecture, and implementation notes are in `docs/`.

## Build, Test, and Development

Run commands from the workspace root (`function-v2/`):

- `cargo check --workspace` checks all crates without producing a release build.
- `cargo test --workspace` runs unit and integration tests across the workspace.
- `cargo run --bin function` builds and launches the desktop app.
- `cargo build --release --bin function` creates an optimized application build.
- `cargo fmt --all -- --check` checks Rust formatting; use `cargo fmt --all` to apply it.

GPUI, audio, and desktop integrations may need native platform dependencies. Report any commands you could not run in your pull request.

## Coding Style

Use standard Rust formatting (4-space indentation) and the workspace's Rust 2021 conventions. Name types and traits in `UpperCamelCase`, functions and modules in `snake_case`, and constants in `SCREAMING_SNAKE_CASE`. Follow nearby crate patterns, keep changes focused, return actionable errors, and preserve cross-platform behavior. Avoid logging secrets or sensitive screen and tool output.

## Testing

Add or update tests in the relevant crate, using `tests/` for integration behavior and inline unit tests when they fit the module. Name test functions for the behavior they verify. Run `cargo test --workspace` for behavior changes; at minimum run `cargo check --workspace` when native dependencies prevent tests. Include the commands and outcomes in the PR.

## Commits and Pull Requests

Recent history uses short, imperative subjects (for example, “Restore Graphite palette alongside Swift”); keep commit subjects similarly direct. Before a larger change, check existing issues and discuss the design. PRs should explain the problem and fix, include tests for behavior changes where practical, and provide screenshots or logs for UI/platform changes with personal data and secrets removed. Mention relevant documentation/configuration updates and any checks you could not run. Follow `SECURITY.md` for vulnerability reports.

## Configuration and Safety

Never commit API keys, credentials, local configuration, build output, or personal screenshots. Treat prompts, screenshots, and tool output as sensitive. Read the relevant `docs/` pages and `CONTRIBUTING.md` before changing user-visible behavior.
