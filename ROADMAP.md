# Roadmap

This document records direction, not release promises. A feature is considered
implemented only after its observable behavior has executable coverage. Codex
CLI is the only supported agent today; every other integration below is still
research.

## Current foundation

- A terminal-multiplexer-independent Codex `Stop` hook.
- One durable, session-scoped condition with bounded predicate output.
- Success, timeout, failure, replacement, and cancellation outcomes.
- Per-project or per-user setup, read-only doctor checks, and safe uninstall.
- Checksummed native release archives, one-shot installation, self-update, and
  update notices from `doctor`.

## Agent adapters

Before adding an adapter, document and test its capability matrix:

- how a turn can stop without cancelling the underlying session;
- how a later local event requests exactly one continuation;
- which stable session identifier binds the condition to that continuation;
- whether waiting happens outside model execution and consumes no model tokens;
- which project and user configuration scopes exist;
- how hook or plugin trust can be inspected by the user;
- which CLI versions are covered by a real end-to-end test.

| Agent | Status | First acceptance target |
| --- | --- | --- |
| Codex CLI | Implemented | Keep a real `arm → Stop → continuation` test across supported versions |
| OpenCode | Research | Prove a lifecycle hook and stable session correlation |
| Aider | Research | Prove a supported continuation path without terminal input injection |
| Gemini CLI | Research | Prove a lifecycle hook and stable session correlation |
| Qwen Code | Research | Prove a lifecycle hook and stable session correlation |
| Goose | Research | Prove a supported continuation path without terminal input injection |
| Cline CLI | Research | Prove a lifecycle hook and stable session correlation |
| OpenHands | Research | Define the boundary between its runtime authority and local wake state |
| GitHub Copilot CLI | Research | Prove a supported continuation path without terminal input injection |

The shared adapter interface should be extracted only after a second agent is
working end to end. That keeps the core based on demonstrated common behavior
instead of guessed abstractions.

## Reliability and ergonomics

- Add an opt-in `open-wake run` helper for small local jobs. It should detach
  safely, preserve the process-group exit status and a bounded log, and expose
  a read-only completion predicate. External supervisors remain the authority
  for CI, deployments, containers, and production jobs.
- Detect and explain stale conditions after crashes or host restarts, with an
  explicit garbage-collection command rather than silent deletion.
- Add a machine-readable event schema for arm, check, wake, timeout, and cancel
  outcomes while keeping model-visible output bounded.
- Cache update checks for a configurable interval so repeated `doctor` runs do
  not require a GitHub request. Keep checks disableable and install updates only
  after explicit user action.
- Add shell completions and man pages after the command surface stabilizes.

## Distribution and supply chain

- Publish signed release provenance and verify a signature in addition to the
  current SHA-256 manifest.
- Add macOS code signing and notarization.
- Add Windows process supervision and atomic executable replacement before
  publishing Windows binaries.
- Evaluate Homebrew, cargo-binstall, AUR, and other package channels without
  weakening checksum or version verification.
- Define tested upgrade and rollback paths for configuration, state formats,
  binary names, and adapter changes.

## Verification

- Build an adapter conformance suite for no-op, success, timeout, failure,
  cancellation, and duplicate-continuation prevention.
- Run end-to-end tests against a supported-version matrix of each real agent,
  not only protocol fixtures or mocked hook input.
- Add fault-injection coverage for killed supervisors, corrupted state,
  disappearing working directories, predicate process leaks, interrupted
  updates, and unavailable release services.
- Verify release installation and self-update from the public artifacts on
  every supported OS and architecture.

## Deliberate non-goals

- Sending keystrokes to tmux, zellij, or a terminal as the primary protocol.
- Running a permanent daemon by default.
- Treating a pane, volatile log line, or model conversation as job authority.
- Allowing predicates to mutate production, retry deployments, or perform
  cleanup.
