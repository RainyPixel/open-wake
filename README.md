# open-wake

[![CI](https://github.com/RainyPixel/open-wake/actions/workflows/ci.yml/badge.svg)](https://github.com/RainyPixel/open-wake/actions/workflows/ci.yml)
[![Release](https://github.com/RainyPixel/open-wake/actions/workflows/release.yml/badge.svg)](https://github.com/RainyPixel/open-wake/releases)

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
- Codex CLI with the `hooks` feature enabled

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

```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/RainyPixel/open-wake/v0.2.1/install.sh \
  | sh -s -- --version v0.2.1 --scope user
```

From a checkout:

```console
cargo install --path . --locked
```

Choose exactly one setup scope:

```console
# Writes .codex/hooks.json and .agents/skills/open-wake in this repository.
open-wake setup --scope project

# Writes ~/.codex/hooks.json and ~/.agents/skills/open-wake.
open-wake setup --scope user
```

`global` is accepted as an alias for `user`. Project setup uses
`open-wake hook`, so every contributor using the checked-in hook must have the
binary on `PATH`. User setup records the current executable's absolute path.

Setup merges its `Stop` entry with existing hooks and is idempotent. It installs
a standalone Codex skill rather than editing `AGENTS.md`; the skill teaches the
agent when and how to arm a condition. Use `--dry-run` to preview writes and
`--json` for a machine-readable report.

Restart Codex if the skill is not visible. Open `/hooks`, review the exact
command, and trust the installed definition before relying on it.

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
- an isolated `arm → Stop hook → continuation` protocol smoke test;
- supervised-job heartbeats, including stale supervisors that may have left a
  child process running;
- the latest published GitHub release, unless offline checks are disabled;
- the selected hook command, timeout, and ownership marker;
- the installed skill bytes;
- whether `open-wake` is on `PATH` for project scope.

Doctor never repairs configuration, kills processes, or deletes job records.
Failed checks exit non-zero and include an exact setup command. A stale job is
a warning with its ID and log path because heartbeat loss is not proof that the
child stopped. Hook trust remains a warning because Codex exposes that review
as interactive `/hooks` state rather than a stable non-interactive readback.

## Run a local job

For a small local Unix build, test, or script, use `run`:

```console
open-wake run \
  --label "release build" \
  --timeout 1h \
  --check-every 15m \
  -- cargo build --release
```

`run` returns after the detached supervisor acknowledges startup. It reports an
error instead of claiming success if that acknowledgement never arrives. The
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

At each `--check-every` checkpoint, the same job and condition remain active.
Inspect the log, then finish the turn normally to keep waiting. There is no
`continue` command and the job is never restarted. `open-wake cancel` disables
future wake-ups but deliberately does not terminate the command.

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
  --interval 10s \
  --check-timeout 5s \
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

One condition can be active per Codex session. Ephemeral condition state uses
`$XDG_RUNTIME_DIR/open-wake` when that location is writable. In a restricted
agent sandbox it falls back to `/tmp/open-wake-$USER`. Override it with
`OPEN_WAKE_STATE_DIR` or `--state-dir`; explicit overrides are strict and never
fall back silently.

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
whole group to avoid leaked descendants.

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
```

`open-wake` is licensed under the MIT License.
