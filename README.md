# open-wake

[![CI](https://github.com/RainyPixel/open-wake/actions/workflows/ci.yml/badge.svg)](https://github.com/RainyPixel/open-wake/actions/workflows/ci.yml)
[![Release](https://github.com/RainyPixel/open-wake/actions/workflows/release-please.yml/badge.svg)](https://github.com/RainyPixel/open-wake/releases)

`open-wake` lets a Codex CLI turn stop while local work continues. It can run a
small local command under a detached supervisor or observe an existing durable
condition. Codex resumes when the work finishes, reaches its deadline, or hits
an optional progress checkpoint.

It uses the Codex `Stop` hook instead of terminal input injection. The wake-up
path is therefore the same in a plain terminal, zellij, tmux, or another
terminal multiplexer.

Codex CLI is the only implemented agent adapter today. Planned integrations
and their acceptance criteria are tracked in [ROADMAP.md](ROADMAP.md).

## Why

Repeated model-side polling wastes tokens and fills the conversation with
status output. `open-wake` moves that wait into a local hook process:

1. The agent starts a local job with `open-wake run`, or arms a cheap predicate
   for work owned by an external supervisor.
2. The agent ends its turn.
3. Codex invokes the synchronous `Stop` hook.
4. `open-wake` checks local state until it is terminal, reaches a checkpoint,
   or times out.
5. The hook asks Codex to continue the same task with bounded evidence.

No model request is made while the hook is waiting. Model-visible predicate
output is bounded to 4 KiB. Supervised job logs stay on disk and are never
copied wholesale into the conversation.

## Requirements

- Linux or macOS
- Rust 1.88 or newer to build from source
- Codex CLI with hooks support

## Install

One-shot install and user setup:

```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/RainyPixel/open-wake/main/install.sh \
  | sh -s -- --scope user
```

Use `--scope project` instead to install the repository-local hook and skill.
The script downloads the native GitHub Release archive, verifies it against the
release `SHA256SUMS`, and atomically installs to `~/.local/bin`. Review
[`install.sh`](install.sh) before piping it to a shell. Pin a release when
reproducibility matters:

<!-- x-release-please-start-version -->
```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/RainyPixel/open-wake/v0.3.0/install.sh \
  | sh -s -- --version v0.3.0 --scope user
```
<!-- x-release-please-end -->

From a checkout:

```console
cargo install --path . --locked
```

Choose exactly one setup scope:

```console
# Writes the project hook/skill and enables that hook in the user Codex config.
open-wake setup --scope project

# Writes the user hook/skill and enables that hook in the user Codex config.
open-wake setup --scope user
```

`global` is accepted as an alias for `user`. Project setup uses
`open-wake hook`, so every contributor using the checked-in hook must have the
binary on `PATH`. User setup records the current executable's absolute path.

Setup merges its `Stop` entry with existing hooks and is idempotent. It enables
the hooks feature and the exact installed hook in the user's Codex
`config.toml`, while preserving other hook state and any existing trust hash.
It installs a standalone Codex skill rather than editing `AGENTS.md`; the skill
teaches the agent when and how to arm a condition. Use `--dry-run` to preview
writes and `--json` for a machine-readable report.

Restart Codex after setup. Open `/hooks`, verify that the hook is enabled,
review the exact command, and trust the installed definition if Codex requests
review. Setup never grants trust silently.

## Updates

```console
open-wake update --check
open-wake update
open-wake update --yes
```

`update` queries the latest GitHub Release, selects the current OS/architecture
asset, verifies its SHA-256 entry, checks the downloaded binary version, and
atomically replaces the current executable. Interactive confirmation is the
default; `--yes` is intended for explicit automation. Development binaries
inside `target/debug` or `target/release` are never self-updated.

`doctor` also reports a newer release as a warning and caches a successful
release lookup for 24 hours. Explicit `open-wake update --check` calls always
query GitHub. Set
`OPEN_WAKE_NO_UPDATE_CHECK=1` for a fully offline doctor run. There is no
background daemon, telemetry, or automatic update installation.

## Doctor

```console
open-wake doctor
open-wake doctor --scope project
open-wake doctor --scope user --json
```

Without `--scope`, doctor checks every detected `open-wake` installation. It
verifies:

- the current executable;
- Codex CLI and its hooks feature;
- the selected writable condition and supervised-job directories;
- an isolated `arm → hook handler → continuation` protocol smoke test;
- current-session condition and hook-lease liveness, including missed
  Stop-hook evidence and interrupted waiters;
- supervised-job heartbeats, including stale supervisors that may have left a
  child process running;
- the latest published GitHub release, unless offline checks are disabled;
- the selected hook command, timeout, and ownership marker;
- whether the exact managed hook is enabled in Codex state;
- the installed skill bytes;
- whether `open-wake` is on `PATH` for project scope.

Doctor never repairs configuration, kills processes, or deletes job records.
Failed checks exit non-zero and include an actionable fix. A stale job is
a warning with its ID and log path because heartbeat loss is not proof that the
child stopped. Hook trust remains a warning because setup deliberately leaves
security review to interactive `/hooks` rather than writing a trusted hash.
The isolated protocol smoke test proves the handler but cannot prove that the
current Codex host invoked the configured `Stop` hook. Doctor reports an
expired active condition with zero attempts as a failure and evidence that it
did not. The same diagnostic survives an upgrade from a legacy
`cancel_requested` record until `open-wake cancel` normalizes it. For supervised
jobs, a terminal or stale job paired with an armed zero-attempt condition is
reported immediately rather than waiting for the condition deadline. An
unavailable attached job is a failure because its command outcome cannot be
inferred safely. A `waiting` condition with a stale hook lease is also a
failure: finish the current turn so the next Stop invocation can recover the
same condition, or cancel only when abandoning its notifications.

## Polling and checkpoints

The public API deliberately separates two different intervals:

- `arm --poll-every` invokes a local read-only predicate. It never creates a
  model request.
- `run` and `arm --checkpoint-every` create a model-visible progress turn.
  Omit it when only terminal completion or timeout matters.

Checkpoints must be at least one minute and shorter than the overall timeout;
values below five minutes produce a warning because they can reintroduce
model-side polling. The former `--interval` and `--check-every` names are not
accepted as aliases. `run` has no public local polling interval: supervisor
result detection is an implementation detail.

## Run a local job

For a small local Unix build, test, or script, use `run`:

```console
open-wake run \
  --label "release build" \
  --timeout 1h \
  --checkpoint-every 15m \
  -- cargo build --release
```

`run` returns after the detached supervisor acknowledges startup. If that
acknowledgement fails after the condition is armed, `run` cancels the condition
and retains the failed job record instead of leaving the session occupied. The
launcher's exit does not mean the job finished. The job's stdout and stderr
share one persistent log. Print its absolute path with:

```console
open-wake logs
open-wake logs JOB_ID
```

`logs` intentionally has no tail, search, or follow mode. Use native tools on
the returned file, for example `tail -n 200 "$(open-wake logs)"` or
`rg 'error|warning' "$(open-wake logs)"`. This keeps large output outside the
model context and avoids duplicating standard system tools.

At each `--checkpoint-every` checkpoint, the same job and condition remain
active. Inspect the log, then finish the turn normally to keep waiting. There is no
`continue` command and the job is never restarted. `open-wake cancel` makes the
condition terminal immediately, releases the session for another condition,
and disables future wake-ups. It deliberately does not terminate the command.
Inspect the attached job before using `run` again so a cancelled job still in
progress is not duplicated accidentally.

A zero or non-zero command exit is terminal: both wake Codex, and the exact
exit code or Unix signal is recorded. A condition deadline also wakes Codex but
does not kill a still-running job.

## Observe externally supervised work

The predicate must be fast, read-only, and idempotent. Exit `0` means ready; any
other exit status means not ready yet.

```console
open-wake arm \
  --label "release build" \
  --timeout 1h \
  --poll-every 10s \
  --check-timeout 5s \
  --checkpoint-every 15m \
  -- sh -c 'test -f target/release/my-app'
```

After `arm` succeeds, the agent should tell the user what it registered and end
the turn immediately. It must not poll `status` in that turn.

Useful lifecycle commands:

```console
open-wake status
open-wake status --json
open-wake logs
open-wake cancel
open-wake update --check
open-wake uninstall --scope project
```

`status` always reports the condition and watcher-lease health when its record
is readable. If an attached job record is missing or corrupt, JSON output
includes `job_error` instead of hiding the condition behind a command failure.
For a live or interrupted hook, `watcher` also reports the Codex turn ID, hook
phase, phase/check timestamps, parent PID, and process group. A stale watcher in
phase `checking` means the hook disappeared while a predicate was in flight;
phase `delivering` means a continuation had already been durably prepared.

One condition can be active per Codex session. Ephemeral condition state uses
`$XDG_RUNTIME_DIR/open-wake` when that location is writable. In a restricted
agent sandbox it falls back to `/tmp/open-wake-$USER`. Override it with
`OPEN_WAKE_STATE_DIR` or `--state-dir`; explicit overrides are strict and never
fall back silently. State transitions use a per-session process lock, a
condition generation ID, and a hook-owner lease with a heartbeat. Only one
fresh hook lease may drive a generation; if Codex interrupts that hook, the
next Stop invocation can recover the same `waiting` condition after its lease
becomes stale. An old hook or failed launcher cannot overwrite, cancel, or
share predicate output with a replacement condition. Continuations use a
two-phase delivery record: success, timeout, failure, and checkpoint evidence
remain pending until the hook response has been written and flushed to Codex.
If the hook disappears first, the next Stop invocation retries the same bounded
continuation without rerunning the predicate. Legacy `cancel_requested` records
are terminal and an idempotent `cancel` normalizes them to `cancelled`.

Supervised job records and full logs persist under
`$XDG_STATE_HOME/open-wake/jobs` or `~/.local/state/open-wake/jobs` when the
selected location is writable. Otherwise they fall back to
`/var/tmp/open-wake-$USER/jobs`, keeping potentially large logs out of `/tmp`.
Override that location with `OPEN_WAKE_JOB_DIR` or `--job-dir`; explicit
overrides are strict. Records are not removed automatically.

## Choosing the execution authority

`run` is a small local Unix supervisor, not a deployment orchestrator. Use the
platform's real authority for system services, Kubernetes, CI, remote builds,
deployments, and production jobs. For those, use `arm` against durable status
or a purpose-built read-only status script.

## Safety boundary

The `Stop` hook may invoke a predicate many times with the agent's local user
permissions. Never put credentials in predicate arguments or output, and never
use a predicate that mutates production, retries an action, or performs cleanup.
Each predicate runs in its own process group; a per-check timeout kills the
whole group to avoid leaked descendants. A short-lived guard in a separate
process group also kills the active predicate group if the hook process alone
terminates abruptly and the guard remains alive. Host-wide process-tree or
cgroup termination is outside this fallback's control.

Codex owns the synchronous Stop-hook process and the only supported way to
start another model turn. If Codex terminates that process while it is still
checking, `open-wake` can retain diagnostics, reap the predicate, and recover
on a later Stop invocation, but it cannot manufacture a continuation after the
host has already completed the turn. This host lifecycle boundary is distinct
from recoverable pre-delivery interruption after a result has been prepared.

Condition and job directories are private (`0700`) and their records/logs are
`0600`. The on-disk job log is complete and currently has no automatic size
limit or retention policy, so choose its state directory with available disk
space in mind. Command arguments and combined output are persisted; do not put
credentials in either. Setup refuses to
replace a symlinked hook file. Uninstall removes only the managed hook entry and
unchanged embedded skill files; locally modified skill files are retained and
produce a non-zero exit.

The protocol and ownership invariants are documented in
[docs/protocol.md](docs/protocol.md).

## Development

```console
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cog check --from-latest-tag
lefthook validate
```

Run `lefthook install` once per clone to enable the local formatting,
Conventional Commit, clippy, and test gates. Release Please owns version bumps,
`CHANGELOG.md`, tags, and GitHub Releases; Cocogitto is validation-only in this
repository.

`open-wake` is licensed under the MIT License.
