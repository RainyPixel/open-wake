# open-wake

[![CI](https://github.com/RainyPixel/open-wake/actions/workflows/ci.yml/badge.svg)](https://github.com/RainyPixel/open-wake/actions/workflows/ci.yml)
[![Release](https://github.com/RainyPixel/open-wake/actions/workflows/release.yml/badge.svg)](https://github.com/RainyPixel/open-wake/releases)

`open-wake` lets a Codex CLI turn stop while a local condition is checked.
When the condition succeeds or reaches its deadline, Codex receives one bounded
continuation message and resumes the same task.

It uses the Codex `Stop` hook instead of terminal input injection. The wake-up
path is therefore the same in a plain terminal, zellij, tmux, or another
terminal multiplexer.

Codex CLI is the only implemented agent adapter today. Planned integrations
and their acceptance criteria are tracked in [ROADMAP.md](ROADMAP.md).

## Why

Repeated model-side polling wastes tokens and fills the conversation with
status output. `open-wake` moves that wait into a local hook process:

1. The agent starts work under an independent supervisor.
2. The agent arms a cheap, read-only predicate and ends its turn.
3. Codex invokes the synchronous `Stop` hook.
4. `open-wake` checks the predicate locally until it exits `0` or times out.
5. The hook asks Codex for one continuation with the final result.

No model request is made while the hook is waiting. Predicate output is bounded
to 4 KiB and enters the conversation only with the final result.

## Requirements

- Linux or macOS
- Rust 1.85 or newer to build from source
- Codex CLI with the `hooks` feature enabled
- A separately supervised long-running job; `open-wake` observes it but does
  not keep an ordinary foreground command alive

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
  https://raw.githubusercontent.com/RainyPixel/open-wake/v0.1.0/install.sh \
  | sh -s -- --version v0.1.0 --scope user
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

`doctor` also reports a newer release as a warning. Set
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
- an isolated `arm → Stop hook → continuation` protocol smoke test;
- the latest published GitHub release, unless offline checks are disabled;
- the selected hook command, timeout, and ownership marker;
- the installed skill bytes;
- whether `open-wake` is on `PATH` for project scope.

Doctor never repairs configuration. Failed checks exit non-zero and include an
exact setup command. Hook trust remains a warning because Codex exposes that
review as interactive `/hooks` state rather than a stable non-interactive
readback.

## Use

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
open-wake cancel
open-wake update --check
open-wake uninstall --scope project
```

One condition can be active per Codex session. Runtime state lives under
`$XDG_RUNTIME_DIR/open-wake`, or a private user directory under the system temp
directory when `XDG_RUNTIME_DIR` is unavailable. Override it with
`OPEN_WAKE_STATE_DIR` or `--state-dir`.

## Starting durable work

`open-wake` deliberately does not guess how a build, deployment, or remote job
should be supervised. Use the platform's real authority when one exists:
systemd, launchd, Kubernetes, CI, a remote build service, or the application's
job runner. Arm a predicate against a durable status, exit-code file, or
purpose-built read-only status script.

For a small local Unix job, a detached wrapper can record its exit status:

```console
job_dir="$(mktemp -d -p /var/tmp open-wake-job.XXXXXX)"
nohup sh -c 'cargo build --release; printf "%s\n" "$?" >"$1/exit-code"' \
  sh "$job_dir" >"$job_dir/output.log" 2>&1 </dev/null &

open-wake arm --label "release build" --timeout 1h -- \
  sh -c 'test -f "$1/exit-code"' sh "$job_dir"
```

On continuation, inspect `exit-code` and the durable log before deciding whether
the task succeeded. The existence predicate only means the job finished.

## Safety boundary

The `Stop` hook may invoke a predicate many times with the agent's local user
permissions. Never put credentials in predicate arguments or output, and never
use a predicate that mutates production, retries an action, or performs cleanup.
Each predicate runs in its own process group; a per-check timeout kills the
whole group to avoid leaked descendants.

Condition files are private (`0700` directory, `0600` files). Setup refuses to
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
